//! Finding and stopping processes that are running a particular executable.
//!
//! Windows locks a running executable's file, so the installed `batuta.exe`
//! cannot be replaced while the service or the hotkey helper is still running
//! it. Re-running setup would otherwise fail with a sharing violation.
//!
//! Stopping the old hotkey helper matters for a second reason: only one
//! process can own a hotkey. Leaving the previous one alive would make the new
//! one's `RegisterHotKey` fail, and the shortcut would silently keep pointing
//! at the old binary.

use std::path::Path;

#[cfg(windows)]
pub use imp::{running_from, stop_running_from};

#[cfg(windows)]
mod imp {
    use super::*;

    use windows_sys::Win32::Foundation::{CloseHandle, MAX_PATH};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    fn utf16_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// The full image path of a process, if it can be read.
    fn image_path(pid: u32) -> Option<String> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; MAX_PATH as usize];
        let mut len = buf.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
        unsafe { CloseHandle(handle) };
        (ok != 0).then(|| utf16_to_string(&buf[..len as usize]))
    }

    fn same_file(a: &str, b: &Path) -> bool {
        // Compared as text rather than by canonicalising: the target may be
        // mid-replacement, and a case difference is the only variation that
        // realistically shows up between these two sources.
        a.eq_ignore_ascii_case(&b.display().to_string())
    }

    /// Process ids currently running `exe`, excluding this process.
    pub fn running_from(exe: &Path) -> Vec<u32> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot.is_null() {
            return Vec::new();
        }

        let me = std::process::id();
        let mut found = Vec::new();
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) };
        while ok != 0 {
            let pid = entry.th32ProcessID;
            let name = utf16_to_string(&entry.szExeFile);
            // Check the cheap name first; opening every process on the system
            // to read its path would be needlessly heavy.
            if pid != me && name.eq_ignore_ascii_case("batuta.exe") {
                if let Some(path) = image_path(pid) {
                    if same_file(&path, exe) {
                        found.push(pid);
                    }
                }
            }
            ok = unsafe { Process32NextW(snapshot, &mut entry) };
        }
        unsafe { CloseHandle(snapshot) };
        found
    }

    /// Stop everything running `exe`. Returns how many were stopped.
    ///
    /// These are only ever Batuta's own helpers, and only ones running the
    /// exact file about to be replaced.
    pub fn stop_running_from(exe: &Path) -> usize {
        let mut stopped = 0;
        for pid in running_from(exe) {
            let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
            if handle.is_null() {
                continue;
            }
            if unsafe { TerminateProcess(handle, 0) } != 0 {
                stopped += 1;
            }
            unsafe { CloseHandle(handle) };
        }
        if stopped > 0 {
            // Handles close asynchronously; give the file lock a moment to go.
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        stopped
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn path_comparison_ignores_case_only() {
            let dest = Path::new(r"C:\ProgramData\Batuta\bin\batuta.exe");
            assert!(same_file(r"C:\ProgramData\Batuta\bin\batuta.exe", dest));
            assert!(same_file(r"c:\programdata\batuta\bin\BATUTA.EXE", dest));
            assert!(!same_file(r"D:\Code\target\release\batuta.exe", dest));
        }

        #[test]
        fn this_process_is_never_reported() {
            // Setup must not target itself while replacing the binary.
            let me = std::env::current_exe().unwrap();
            assert!(!running_from(&me).contains(&std::process::id()));
        }

        #[test]
        fn an_unused_path_reports_nothing() {
            assert!(running_from(Path::new(r"C:\nothing\runs\from\here.exe")).is_empty());
        }
    }
}

#[cfg(not(windows))]
pub fn running_from(_exe: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn stop_running_from(_exe: &Path) -> usize {
    0
}
