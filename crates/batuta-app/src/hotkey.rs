//! The global hotkey helper (step 5).
//!
//! This cannot live in the daemon. A service runs as LocalSystem in session 0,
//! which has no desktop and cannot own a hotkey in the logged-in user's
//! session. Windows `.lnk` hotkeys only honour `Ctrl+Alt+<key>` reliably. So a
//! small resident process registers the combination, parks in `GetMessageW`,
//! and launches the UI when it fires. It costs one thread blocked in the
//! kernel and no CPU at all while idle.
//!
//! It is started per-user from `HKCU\...\Run`, not `HKLM`: a hotkey belongs to
//! one desktop session, and a machine-wide entry would start a copy for every
//! user who logs in, only one of which could actually own the combination.

/// Modifier bits accepted by `RegisterHotKey`.
pub mod modifiers {
    pub const ALT: u32 = 0x0001;
    pub const CONTROL: u32 = 0x0002;
    pub const SHIFT: u32 = 0x0004;
    pub const WIN: u32 = 0x0008;
    /// Fire once per press rather than repeating while held.
    pub const NOREPEAT: u32 = 0x4000;
}

/// The registry value name used for the autostart entry.
pub const RUN_VALUE: &str = "Batuta Search Hotkey";

/// Attempts spent retrying briskly before backing off to a slow poll.
const FAST_RETRIES: u32 = 30;

/// How long to wait before trying a refused registration again.
///
/// Signing in starts a dozen programs at once, all claiming their shortcuts,
/// and whoever asks second is simply refused. That is a transient collision,
/// not a permanent one, so the first minute is retried briskly. After that a
/// slow poll costs nothing and means the hotkey starts working the moment the
/// other program lets go, instead of staying dead until setup is run again.
pub fn retry_delay(attempt: u32) -> std::time::Duration {
    if attempt < FAST_RETRIES {
        std::time::Duration::from_secs(2)
    } else {
        std::time::Duration::from_secs(30)
    }
}

/// A parsed hotkey: modifier bits and a virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Combo {
    pub modifiers: u32,
    pub key: u32,
}

/// Parse a combination like `Ctrl+Alt+Space`.
pub fn parse(spec: &str) -> Option<Combo> {
    let mut mods = modifiers::NOREPEAT;
    let mut key = None;

    for part in spec.split('+') {
        match part.trim().to_ascii_lowercase().as_str() {
            "" => return None,
            "ctrl" | "control" => mods |= modifiers::CONTROL,
            "alt" => mods |= modifiers::ALT,
            "shift" => mods |= modifiers::SHIFT,
            "win" | "super" => mods |= modifiers::WIN,
            "space" => key = Some(0x20),
            other => {
                // A single letter or digit maps to its ASCII code, which is
                // also its virtual-key code.
                let mut chars = other.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_alphanumeric() => {
                        key = Some(c.to_ascii_uppercase() as u32)
                    }
                    _ => return None,
                }
            }
        }
    }

    let key = key?;
    // A bare key with no modifier would swallow that key system-wide.
    if mods & (modifiers::CONTROL | modifiers::ALT | modifiers::SHIFT | modifiers::WIN) == 0 {
        return None;
    }
    Some(Combo {
        modifiers: mods,
        key,
    })
}

#[cfg(windows)]
pub use imp::{autostart_disable, autostart_enable, is_autostart_enabled, run};

#[cfg(windows)]
mod imp {
    use super::*;
    use std::io;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND};
    use windows_sys::Win32::System::Console::FreeConsole;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const HOTKEY_ID: i32 = 1;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Register the combination and launch the UI whenever it fires.
    ///
    /// Never returns while it holds the hotkey.
    pub fn run(spec: &str, exe: &Path, log_path: &Path) -> io::Result<()> {
        // Explorer starts this from the Run key, which gives it a console the
        // helper has no use for and would otherwise leave sitting on the
        // desktop for the whole session. Detaching closes it. Nothing is
        // printed after this point, so the log file is where failures go.
        unsafe { FreeConsole() };

        let Some(combo) = parse(spec) else {
            let msg = format!("'{spec}' is not a usable shortcut");
            log(log_path, &msg);
            return Err(io::Error::other(msg));
        };

        // Losing the race at sign-in used to kill the helper outright, which
        // is why the shortcut needed setup re-run by hand to come back.
        let mut attempt = 0u32;
        loop {
            let ok = unsafe {
                RegisterHotKey(
                    ptr::null_mut::<HWND>() as HWND,
                    HOTKEY_ID,
                    combo.modifiers,
                    combo.key,
                )
            };
            if ok != 0 {
                if attempt > 0 {
                    log(
                        log_path,
                        &format!("{spec} registered after {attempt} retries"),
                    );
                }
                break;
            }
            if attempt == 0 {
                log(
                    log_path,
                    &format!("{spec} is held by another program; retrying until it is free"),
                );
            }
            std::thread::sleep(retry_delay(attempt));
            attempt = attempt.saturating_add(1);
        }

        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // Blocks in the kernel until a message arrives: no polling, no CPU.
        while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
            if msg.message == WM_HOTKEY {
                // Silently doing nothing on a keypress is the one failure the
                // user cannot diagnose, so it gets recorded.
                if let Err(e) = launch(exe) {
                    log(log_path, &format!("could not open the search bar: {e}"));
                }
            }
        }
        unsafe { UnregisterHotKey(ptr::null_mut::<HWND>() as HWND, HOTKEY_ID) };
        Ok(())
    }

    /// Append one line to the helper's log.
    ///
    /// A detached background process has nowhere else to report to, and a
    /// hotkey that quietly does nothing is otherwise impossible to explain.
    /// Best-effort throughout: logging must never take the helper down.
    fn log(path: &Path, message: &str) {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| crate::fmt::timestamp(d.as_secs() as u32))
            .unwrap_or_else(|_| "-".into());
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{stamp}  {message}");
        }
    }

    /// Open the UI in a console of its own.
    ///
    /// `CreateProcessW` directly, rather than `std::process::Command`, for one
    /// reason: this process has no console. `FreeConsole` above closed it, so
    /// `GetStdHandle` now answers with nothing, and `Command` — which always
    /// passes the three standard handles on to the child — asks
    /// `DuplicateHandle` to copy them and gets `ERROR_INVALID_HANDLE` back.
    /// Every press produced that and nothing else.
    ///
    /// Passing no handles is not something `Command` can express, and the two
    /// obvious workarounds are both worse. Inheriting the handles is what just
    /// failed. Redirecting to `NUL` would spawn successfully and then draw the
    /// interface into the null device, which is a blank window rather than an
    /// error — the harder failure to diagnose of the two.
    ///
    /// So: `bInheritHandles = FALSE` and a `STARTUPINFOW` with no
    /// `STARTF_USESTDHANDLES`. `CREATE_NEW_CONSOLE` then gives the child a
    /// fresh console and standard handles attached to it, which is what the
    /// interface needs to draw on.
    fn launch(exe: &Path) -> io::Result<()> {
        spawn_detached(exe, "ui")
    }

    /// Start `exe args` in its own console, inheriting nothing.
    fn spawn_detached(exe: &Path, args: &str) -> io::Result<()> {
        use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
        use windows_sys::Win32::System::Threading::{
            CreateProcessW, CREATE_NEW_CONSOLE, PROCESS_INFORMATION, STARTUPINFOW,
        };

        // `CreateProcessW` may write to the command line it is given, so it
        // cannot be a literal or a shared buffer.
        let mut cmdline = wide(&format!("\"{}\" {args}", exe.display()));

        // Zeroed, so `dwFlags` carries no STARTF_USESTDHANDLES and the handle
        // fields are ignored. That is the whole point of doing this by hand.
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        let started = unsafe {
            CreateProcessW(
                ptr::null(),
                cmdline.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                FALSE,
                CREATE_NEW_CONSOLE,
                ptr::null(),
                ptr::null(),
                &si,
                &mut pi,
            )
        };
        if started == 0 {
            return Err(io::Error::last_os_error());
        }
        // The child is not waited on; these two handles are all this process
        // holds of it, and leaking one per keypress would be a slow leak in a
        // program that runs for the whole session.
        unsafe {
            CloseHandle(pi.hProcess);
            CloseHandle(pi.hThread);
        }
        Ok(())
    }

    fn open_run(access: u32, create: bool) -> io::Result<HKEY> {
        let sub = wide(RUN_KEY);
        let mut key: HKEY = ptr::null_mut();
        let rc = if create {
            let mut disposition = 0u32;
            unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    sub.as_ptr(),
                    0,
                    ptr::null(),
                    0,
                    access,
                    ptr::null(),
                    &mut key,
                    &mut disposition,
                )
            }
        } else {
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut key) }
        };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(key)
    }

    /// The command line the Run entry stores.
    fn run_command(exe: &Path, spec: &str) -> String {
        format!("\"{}\" hotkey --combo \"{spec}\"", exe.display())
    }

    /// Start the helper at logon.
    pub fn autostart_enable(exe: &Path, spec: &str) -> io::Result<()> {
        let key = open_run(KEY_WRITE, true)?;
        let name = wide(RUN_VALUE);
        let value = wide(&run_command(exe, spec));
        let bytes: Vec<u8> = value.iter().flat_map(|u| u.to_le_bytes()).collect();
        let rc = unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        };
        unsafe { RegCloseKey(key) };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }

    /// Stop starting the helper at logon. Returns whether anything was removed.
    pub fn autostart_disable() -> io::Result<bool> {
        let Ok(key) = open_run(KEY_WRITE, false) else {
            return Ok(false);
        };
        let name = wide(RUN_VALUE);
        let rc = unsafe { RegDeleteValueW(key, name.as_ptr()) };
        unsafe { RegCloseKey(key) };
        Ok(rc == ERROR_SUCCESS)
    }

    pub fn is_autostart_enabled() -> bool {
        let Ok(key) = open_run(KEY_READ, false) else {
            return false;
        };
        let name = wide(RUN_VALUE);
        let mut len = 0u32;
        let rc = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut len,
            )
        };
        unsafe { RegCloseKey(key) };
        rc == ERROR_SUCCESS
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Start `exe args` the way Explorer starts the helper: its own
        /// console, and **no inherited handles**, then wait for it.
        ///
        /// The second half is what makes this faithful, and it is easy to get
        /// wrong. Under `cargo test` the harness hands a child pipe handles,
        /// and `FreeConsole` does not invalidate a pipe — so a probe started
        /// the ordinary way succeeds whether the code under test is fixed or
        /// broken, and proves nothing. Only a child whose standard handles
        /// really belong to the console it just closed reproduces the failure.
        fn spawn_probe_and_wait(exe: &Path, args: &str, env: &str) -> u32 {
            use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
            use windows_sys::Win32::System::Threading::{
                CreateProcessW, GetExitCodeProcess, WaitForSingleObject, CREATE_NEW_CONSOLE,
                CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
            };

            let mut cmdline = wide(&format!("\"{}\" {args}", exe.display()));

            // The child needs the marker variable, and a block passed here
            // replaces the environment wholesale, so the parent's is copied.
            let mut block: Vec<u16> = Vec::new();
            for (k, v) in std::env::vars() {
                block.extend(wide(&format!("{k}={v}")));
            }
            block.extend(wide(env));
            block.push(0);

            let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

            let started = unsafe {
                CreateProcessW(
                    ptr::null(),
                    cmdline.as_mut_ptr(),
                    ptr::null(),
                    ptr::null(),
                    FALSE,
                    CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
                    block.as_ptr() as *const std::ffi::c_void,
                    ptr::null(),
                    &si,
                    &mut pi,
                )
            };
            assert_ne!(started, 0, "could not start the probe child");

            let mut code = 1u32;
            unsafe {
                WaitForSingleObject(pi.hProcess, INFINITE);
                GetExitCodeProcess(pi.hProcess, &mut code);
                CloseHandle(pi.hProcess);
                CloseHandle(pi.hThread);
            }
            code
        }

        /// The bug this exists for: the helper detaches from its console at
        /// startup, and every launch afterwards failed with
        /// `ERROR_INVALID_HANDLE`, because the spawn was handing the child the
        /// standard handles that the detach had just closed. The hotkey
        /// registered, the hotkey fired, and nothing opened.
        #[test]
        fn the_ui_can_be_launched_after_detaching_from_the_console() {
            use windows_sys::Win32::System::Console::FreeConsole;

            let out = std::env::temp_dir().join("batuta-detached-spawn-probe.txt");

            // The child half: detach exactly as `run` does, then launch
            // something harmless exactly as a keypress would.
            if std::env::var_os("BATUTA_DETACHED_SPAWN_PROBE").is_some() {
                let freed = unsafe { FreeConsole() };
                let answer = match spawn_detached(Path::new("cmd.exe"), "/c exit 0") {
                    Ok(()) => format!("ok (FreeConsole returned {freed})"),
                    Err(e) => format!("{e} (FreeConsole returned {freed})"),
                };
                let _ = std::fs::write(&out, answer);
                return;
            }

            let _ = std::fs::remove_file(&out);
            let code = spawn_probe_and_wait(
                &std::env::current_exe().unwrap(),
                "the_ui_can_be_launched_after_detaching_from_the_console --nocapture",
                "BATUTA_DETACHED_SPAWN_PROBE=1",
            );
            assert_eq!(code, 0, "the detached child failed to run");

            let answer = std::fs::read_to_string(&out).expect("the child reported nothing");
            let _ = std::fs::remove_file(&out);
            assert!(
                answer.starts_with("ok"),
                "launching after FreeConsole failed, which is exactly what left the \
                 hotkey firing into nothing: {answer}"
            );
        }

        #[test]
        fn the_autostart_command_quotes_the_executable() {
            // An install directory with a space would otherwise be split.
            let cmd = run_command(
                Path::new(r"C:\Program Files\Batuta\batuta.exe"),
                "Ctrl+Space",
            );
            assert!(
                cmd.starts_with(r#""C:\Program Files\Batuta\batuta.exe""#),
                "{cmd}"
            );
            assert!(cmd.contains(r#"hotkey --combo "Ctrl+Space""#), "{cmd}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_offered_combinations_all_parse() {
        for spec in crate::setup::flow::HOTKEYS {
            assert!(parse(spec).is_some(), "{spec} should parse");
        }
    }

    #[test]
    fn modifiers_are_combined_correctly() {
        let c = parse("Ctrl+Space").unwrap();
        assert_eq!(c.key, 0x20);
        assert_eq!(c.modifiers & modifiers::CONTROL, modifiers::CONTROL);
        assert_eq!(c.modifiers & modifiers::ALT, 0);

        let c = parse("Ctrl+Alt+Space").unwrap();
        assert_eq!(c.modifiers & modifiers::CONTROL, modifiers::CONTROL);
        assert_eq!(c.modifiers & modifiers::ALT, modifiers::ALT);

        let c = parse("Alt+Space").unwrap();
        assert_eq!(c.modifiers & modifiers::ALT, modifiers::ALT);
        assert_eq!(c.modifiers & modifiers::CONTROL, 0);
    }

    #[test]
    fn repeats_are_suppressed() {
        // Holding the key down must open one window, not hundreds.
        assert_ne!(
            parse("Ctrl+Space").unwrap().modifiers & modifiers::NOREPEAT,
            0
        );
    }

    #[test]
    fn parsing_is_case_and_spacing_insensitive() {
        assert_eq!(parse("ctrl+space"), parse("Ctrl+Space"));
        assert_eq!(parse(" CTRL + SPACE "), parse("Ctrl+Space"));
        assert_eq!(parse("control+space"), parse("Ctrl+Space"));
    }

    #[test]
    fn a_refused_registration_is_retried_briskly_then_slowly() {
        // The sign-in scramble is over in well under a minute, so the fast
        // window has to cover it; after that the poll only has to be cheap.
        assert!(retry_delay(0) <= std::time::Duration::from_secs(2));
        let fast: std::time::Duration = (0..FAST_RETRIES).map(retry_delay).sum();
        assert!(
            fast >= std::time::Duration::from_secs(60),
            "fast retries should cover at least a minute, got {fast:?}"
        );
        assert!(retry_delay(FAST_RETRIES) > retry_delay(FAST_RETRIES - 1));
    }

    #[test]
    fn retrying_never_gives_up() {
        // A helper that stopped trying would go back to needing setup re-run.
        assert!(retry_delay(u32::MAX) > std::time::Duration::ZERO);
    }

    #[test]
    fn letters_and_digits_map_to_their_virtual_key_codes() {
        assert_eq!(parse("Ctrl+Alt+B").unwrap().key, b'B' as u32);
        assert_eq!(parse("ctrl+alt+b").unwrap().key, b'B' as u32);
        assert_eq!(parse("Win+7").unwrap().key, b'7' as u32);
    }

    #[test]
    fn a_combination_with_no_modifier_is_refused() {
        // Registering a bare key would swallow it system-wide.
        assert_eq!(parse("Space"), None);
        assert_eq!(parse("B"), None);
    }

    #[test]
    fn nonsense_is_refused_rather_than_half_parsed() {
        for bad in [
            "",
            "Ctrl+",
            "+Space",
            "Ctrl+Nope",
            "Ctrl++Space",
            "Ctrl+Alt",
        ] {
            assert_eq!(parse(bad), None, "{bad:?} should not parse");
        }
    }
}
