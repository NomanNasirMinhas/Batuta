//! Telling a double-clicked launch from a launch inside a shell.
//!
//! When `batuta.exe` is started from Explorer — or spawned by the hotkey
//! helper — Windows gives it a console of its own that disappears the instant
//! the process exits. For a run that printed a report, that would flash past
//! unread. For the search UI, which draws its own screen and should vanish
//! when dismissed, holding the window open is exactly wrong.
//!
//! The distinction available here is whether any other process is attached to
//! this console; what to do about it is the caller's decision.

/// Are we the only process attached to this console?
///
/// True for a double-click, for the elevated relaunch, and for the UI spawned
/// by the hotkey helper — all of which get a fresh console. False when run
/// from `cmd`, PowerShell or a terminal, where the shell is attached too.
#[cfg(windows)]
pub fn owns_console_alone() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;

    let mut pids = [0u32; 8];
    let n = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    // Zero means there is no console at all (a detached helper), which is not
    // a window anyone is looking at either.
    n == 1
}

#[cfg(not(windows))]
pub fn owns_console_alone() -> bool {
    false
}

/// Hold the window open so the user can read what happened.
///
/// Reads raw bytes rather than a line. A console that has been through raw
/// mode can deliver Enter as a bare carriage return, and `read_line` would
/// then block waiting for a line feed that never arrives — leaving a window
/// showing a prompt that cannot be dismissed.
pub fn wait_for_enter() {
    use std::io::{Read, Write};
    println!();
    print!("Press Enter to close...");
    let _ = std::io::stdout().flush();

    let mut stdin = std::io::stdin().lock();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) => return, // stdin closed
            Ok(_) if byte[0] == b'\r' || byte[0] == b'\n' => return,
            Ok(_) => continue,
            Err(_) => return,
        }
    }
}

/// Maximise the console window we were given.
///
/// Only worth doing for a window opened for us — the hotkey helper's new
/// console, or a double-click. Resizing a terminal the user already had open
/// would be rude.
#[cfg(windows)]
pub fn maximize() {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MAXIMIZE};

    let hwnd = unsafe { GetConsoleWindow() };
    if !hwnd.is_null() {
        unsafe { ShowWindow(hwnd, SW_MAXIMIZE) };
    }
}

#[cfg(not(windows))]
pub fn maximize() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_does_not_panic_however_it_was_launched() {
        // Under `cargo test` the harness owns the console, so this is false;
        // only the call itself is under test.
        let _ = owns_console_alone();
    }

    #[test]
    fn maximizing_is_safe_without_a_console_window() {
        // The hotkey helper runs detached with no console at all.
        maximize();
    }
}
