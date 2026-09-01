//! Re-launching as Administrator.
//!
//! Reading the MFT, installing a service and creating a SYSTEM scheduled task
//! all need elevation. Rather than telling a non-technical user to find an
//! admin console, Quick Setup restarts itself through `ShellExecuteW` with the
//! `runas` verb, which is what raises the UAC prompt.

use anyhow::{bail, Result};

/// The user dismissed the UAC prompt.
const ERROR_CANCELLED: i32 = 1223;

/// `ShellExecuteW` returns a value above this on success. It is a legacy
/// convention: the return is an `HINSTANCE`-shaped error code below it.
const SHELL_SUCCESS_THRESHOLD: isize = 32;

#[cfg(windows)]
pub fn relaunch_as_admin(args: &[&str]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    let exe = std::env::current_exe()?;
    let verb = wide(std::ffi::OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let params = wide(std::ffi::OsStr::new(&args.join(" ")));

    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    let code = result as isize;
    if code > SHELL_SUCCESS_THRESHOLD {
        return Ok(());
    }
    if code as i32 == ERROR_CANCELLED {
        bail!("elevation was declined, so setup cannot continue");
    }
    bail!("could not restart as Administrator (code {code})")
}

#[cfg(not(windows))]
pub fn relaunch_as_admin(_args: &[&str]) -> Result<()> {
    bail!("elevation is only available on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_success_threshold_matches_the_shell_convention() {
        // ShellExecuteW reports failure as a small integer, not a null handle,
        // so anything at or below 32 is an error and must not read as success.
        assert_eq!(SHELL_SUCCESS_THRESHOLD, 32);
        assert!(ERROR_CANCELLED > SHELL_SUCCESS_THRESHOLD as i32);
    }
}
