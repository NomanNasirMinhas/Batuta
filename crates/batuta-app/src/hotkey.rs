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
    pub fn run(spec: &str, exe: &Path) -> io::Result<()> {
        let Some(combo) = parse(spec) else {
            return Err(io::Error::other(format!(
                "'{spec}' is not a usable shortcut"
            )));
        };

        let ok = unsafe {
            RegisterHotKey(
                ptr::null_mut::<HWND>() as HWND,
                HOTKEY_ID,
                combo.modifiers,
                combo.key,
            )
        };
        if ok == 0 {
            // Almost always because another program already owns it. Saying so
            // beats sitting silently on a shortcut that will never fire.
            return Err(io::Error::other(format!(
                "{spec} is already in use by another program"
            )));
        }

        let mut msg: MSG = unsafe { std::mem::zeroed() };
        // Blocks in the kernel until a message arrives: no polling, no CPU.
        while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
            if msg.message == WM_HOTKEY {
                let _ = launch(exe);
            }
        }
        unsafe { UnregisterHotKey(ptr::null_mut::<HWND>() as HWND, HOTKEY_ID) };
        Ok(())
    }

    /// Open the UI in its own console window.
    fn launch(exe: &Path) -> io::Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        std::process::Command::new(exe)
            .arg("ui")
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map(|_| ())
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
