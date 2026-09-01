//! Adding the install directory to the user's `PATH` (step 6).
//!
//! **Never use `setx` for this.** It truncates the value at 1024 characters
//! and expands `%VAR%` references in place, so a long or variable-containing
//! `PATH` comes back mangled or shortened — a well-known way to destroy
//! someone's environment. The registry value is edited directly instead, and
//! its type is preserved so a `REG_EXPAND_SZ` `PATH` stays expandable.
//!
//! The string manipulation is separated from the registry access so the part
//! that could silently drop entries is testable.

/// Normalise one `PATH` entry for comparison: case-insensitive, trailing
/// separators and surrounding quotes ignored.
fn normalise(entry: &str) -> String {
    entry
        .trim()
        .trim_matches('"')
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase()
}

/// Whether `path` already lists `entry`.
pub fn contains_entry(path: &str, entry: &str) -> bool {
    let want = normalise(entry);
    !want.is_empty()
        && path
            .split(';')
            .filter(|s| !s.trim().is_empty())
            .any(|s| normalise(s) == want)
}

/// Append `entry` to `path`, or `None` when it is already there.
///
/// Existing entries are never reordered, rewritten or dropped: the original
/// string is kept verbatim and only extended.
pub fn with_entry(path: &str, entry: &str) -> Option<String> {
    if entry.trim().is_empty() || contains_entry(path, entry) {
        return None;
    }
    let trimmed = path.trim_end_matches(';');
    Some(if trimmed.is_empty() {
        entry.to_string()
    } else {
        format!("{trimmed};{entry}")
    })
}

/// Remove `entry` from `path`, or `None` when it was not present.
pub fn without_entry(path: &str, entry: &str) -> Option<String> {
    if !contains_entry(path, entry) {
        return None;
    }
    let want = normalise(entry);
    let kept: Vec<&str> = path
        .split(';')
        .filter(|s| !s.trim().is_empty() && normalise(s) != want)
        .collect();
    Some(kept.join(";"))
}

#[cfg(windows)]
pub use imp::{add, remove};

#[cfg(windows)]
mod imp {
    use super::*;
    use std::io;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_SZ,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open(access: u32) -> io::Result<HKEY> {
        let sub = wide("Environment");
        let mut key: HKEY = ptr::null_mut();
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut key) };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(key)
    }

    /// Read `Path` and the type it is stored as.
    fn read(key: HKEY) -> (String, u32) {
        let name = wide("Path");
        let mut kind: u32 = REG_SZ;
        let mut len: u32 = 0;
        unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                ptr::null_mut(),
                &mut len,
            );
        }
        if len == 0 {
            return (String::new(), REG_EXPAND_SZ);
        }
        let mut buf = vec![0u8; len as usize];
        let rc = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                buf.as_mut_ptr(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS {
            return (String::new(), REG_EXPAND_SZ);
        }
        let units: Vec<u16> = buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
        (String::from_utf16_lossy(&units[..end]), kind)
    }

    fn write(key: HKEY, value: &str, kind: u32) -> io::Result<()> {
        let name = wide("Path");
        let data = wide(value);
        let bytes: Vec<u8> = data.iter().flat_map(|u| u.to_le_bytes()).collect();
        let rc = unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                kind,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }

    /// Tell running programs the environment changed. Shells that are already
    /// open will not pick it up regardless; only newly started ones will.
    fn broadcast() {
        let env = wide("Environment");
        let mut out: usize = 0;
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                env.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                2000,
                &mut out,
            );
        }
    }

    /// Add `dir` to the user's `PATH`. Returns whether anything changed.
    pub fn add(dir: &Path) -> io::Result<bool> {
        let entry = dir.display().to_string();
        let key = open(KEY_READ | KEY_WRITE)?;
        let (current, kind) = read(key);
        let result = match with_entry(&current, &entry) {
            // Preserve the existing type: rewriting a REG_EXPAND_SZ PATH as
            // REG_SZ would stop every %VAR% in it from expanding.
            Some(next) => write(key, &next, kind).map(|_| true),
            None => Ok(false),
        };
        unsafe { RegCloseKey(key) };
        if matches!(result, Ok(true)) {
            broadcast();
        }
        result
    }

    /// Remove `dir` from the user's `PATH`. Returns whether anything changed.
    pub fn remove(dir: &Path) -> io::Result<bool> {
        let entry = dir.display().to_string();
        let key = open(KEY_READ | KEY_WRITE)?;
        let (current, kind) = read(key);
        let result = match without_entry(&current, &entry) {
            Some(next) => write(key, &next, kind).map(|_| true),
            None => Ok(false),
        };
        unsafe { RegCloseKey(key) };
        if matches!(result, Ok(true)) {
            broadcast();
        }
        result
    }

    // Silence an unused-import warning on the HANDLE alias in some configs.
    #[allow(dead_code)]
    fn _unused(_: HANDLE) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = r"C:\Windows;C:\Windows\system32;%USERPROFILE%\.cargo\bin";

    #[test]
    fn appending_preserves_every_existing_entry_verbatim() {
        // The failure mode that matters: silently dropping or reordering
        // something already on the user's PATH.
        let next = with_entry(P, r"C:\ProgramData\Batuta\bin").unwrap();
        assert!(next.starts_with(P), "existing value must be kept intact");
        assert!(next.ends_with(r";C:\ProgramData\Batuta\bin"));
        assert_eq!(next.split(';').count(), P.split(';').count() + 1);
    }

    #[test]
    fn variable_references_are_left_unexpanded() {
        // setx would expand these in place; we must not.
        let next = with_entry(P, r"C:\Batuta").unwrap();
        assert!(next.contains("%USERPROFILE%"), "{next}");
    }

    #[test]
    fn appending_twice_is_a_no_op() {
        let entry = r"C:\ProgramData\Batuta\bin";
        let once = with_entry(P, entry).unwrap();
        assert_eq!(with_entry(&once, entry), None, "must not duplicate");
    }

    #[test]
    fn presence_ignores_case_and_trailing_separators() {
        let path = r"C:\Windows;C:\ProgramData\Batuta\bin\";
        assert!(contains_entry(path, r"C:\ProgramData\Batuta\bin"));
        assert!(contains_entry(path, r"c:\programdata\batuta\BIN"));
        assert!(!contains_entry(path, r"C:\ProgramData\Batuta"));
    }

    #[test]
    fn an_empty_path_is_handled_without_a_leading_separator() {
        assert_eq!(with_entry("", r"C:\Batuta").unwrap(), r"C:\Batuta");
        assert_eq!(with_entry(";;", r"C:\Batuta").unwrap(), r"C:\Batuta");
    }

    #[test]
    fn a_trailing_separator_does_not_create_an_empty_entry() {
        let next = with_entry(r"C:\Windows;", r"C:\Batuta").unwrap();
        assert_eq!(next, r"C:\Windows;C:\Batuta");
        assert!(!next.contains(";;"));
    }

    #[test]
    fn removal_takes_out_only_the_entry_asked_for() {
        let entry = r"C:\ProgramData\Batuta\bin";
        let with = with_entry(P, entry).unwrap();
        let back = without_entry(&with, entry).unwrap();
        assert_eq!(back, P, "removing must restore the original exactly");
        assert_eq!(without_entry(P, entry), None, "absent entry is a no-op");
    }

    #[test]
    fn removal_also_tidies_empty_segments() {
        let messy = r"C:\Windows;;C:\Batuta;";
        let back = without_entry(messy, r"C:\Batuta").unwrap();
        assert_eq!(back, r"C:\Windows");
    }

    #[test]
    fn an_empty_entry_is_never_added() {
        assert_eq!(with_entry(P, ""), None);
        assert_eq!(with_entry(P, "   "), None);
    }
}
