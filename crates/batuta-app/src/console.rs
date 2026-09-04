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

/// Fraction of the work area a floating launcher window occupies.
const FLOAT_W: i32 = 72;
const FLOAT_H: i32 = 68;

/// Turn the console we were given into a borderless floating panel.
///
/// Only ever applied to a window opened *for* us — the hotkey helper's new
/// console, or a double-click. Restyling a terminal the user already had open
/// would be taking their window away from them.
///
/// A launcher should look like a launcher: no title bar to read, no resize
/// grips to catch, centred where the eye already is. The window styles are
/// stripped rather than the window being maximised, because a full-screen
/// console for a search box is the thing being replaced here.
///
/// This works on the classic console host, which owns a real window we can
/// restyle. Under a terminal that multiplexes tabs in its own process there is
/// no such window, and `GetConsoleWindow` returns either nothing or a hidden
/// stand-in; both cases are left alone rather than half-applied.
#[cfg(windows)]
pub fn float() {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, IsWindowVisible, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
        SystemParametersInfoW, GWL_STYLE, HWND_TOP, SPI_GETWORKAREA, SWP_FRAMECHANGED,
        SWP_SHOWWINDOW, WS_BORDER, WS_CAPTION, WS_DLGFRAME, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
        WS_SYSMENU, WS_THICKFRAME,
    };

    let hwnd = unsafe { GetConsoleWindow() };
    if hwnd.is_null() || unsafe { IsWindowVisible(hwnd) } == 0 {
        return;
    }

    // Everything that makes a window look like a document window.
    let chrome = (WS_CAPTION
        | WS_THICKFRAME
        | WS_MINIMIZEBOX
        | WS_MAXIMIZEBOX
        | WS_SYSMENU
        | WS_BORDER
        | WS_DLGFRAME) as isize;
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
    unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, style & !chrome) };

    // The work area, not the screen: covering the taskbar would make the
    // window hard to dismiss by anything but the keyboard.
    let mut work = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut work as *mut RECT as *mut core::ffi::c_void,
            0,
        )
    };
    if ok == 0 {
        // Without a work area there is no sane size to pick, but the chrome is
        // already gone; leave the window where it is rather than guessing.
        return;
    }

    let (aw, ah) = (work.right - work.left, work.bottom - work.top);
    let (w, h) = (aw * FLOAT_W / 100, ah * FLOAT_H / 100);
    let x = work.left + (aw - w) / 2;
    let y = work.top + (ah - h) / 2;

    unsafe {
        // SWP_FRAMECHANGED is required: without it the stripped styles are not
        // recalculated and the old frame stays drawn.
        SetWindowPos(
            hwnd,
            HWND_TOP,
            x,
            y,
            w,
            h,
            SWP_FRAMECHANGED | SWP_SHOWWINDOW,
        );
        SetForegroundWindow(hwnd);
    }
}

#[cfg(not(windows))]
pub fn float() {}

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
    fn floating_is_safe_without_a_console_window() {
        // Runs under a test harness with no console of its own, which is the
        // same path a terminal that owns no window takes.
        float();
    }
}
