//! Locking down the data directory (step 4b).
//!
//! `index.bin` lists every file belonging to every user on the machine, and
//! `%ProgramData%` is readable by all local accounts by default — so without
//! this, any user can read everyone else's file listing.
//!
//! The descriptor mirrors the one already used for the named pipe in
//! `pipe.rs`: full access for SYSTEM and Administrators, full access for the
//! account that ran setup, and nobody else. `P` protects the DACL so no
//! inherited entry from `%ProgramData%` can widen it back out again.

/// Build the SDDL for the data directory.
///
/// `OICI` makes the entries inheritable, so `index.bin` and any future file in
/// the directory get the same protection rather than only the folder itself.
pub fn sddl(user_sid: &str) -> String {
    format!("D:P(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)(A;OICI;GA;;;{user_sid})")
}

#[cfg(windows)]
pub use imp::restrict;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::io;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Apply the restrictive DACL to `dir`.
    pub fn restrict(dir: &Path, user_sid: &str) -> io::Result<()> {
        let text = wide(&sddl(user_sid));
        let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut dacl: *mut ACL = ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        let got =
            unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) };
        if got == 0 || present == 0 {
            unsafe { LocalFree(psd as *mut _) };
            return Err(io::Error::other("security descriptor carried no DACL"));
        }

        let mut path = wide(&dir.display().to_string());
        let rc = unsafe {
            SetNamedSecurityInfoW(
                path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null_mut(),
            )
        };
        unsafe { LocalFree(psd as *mut _) };

        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-2205097650-901772425-1229515317-1001";

    #[test]
    fn the_descriptor_names_exactly_the_three_principals() {
        let s = sddl(SID);
        assert!(s.contains(";SY)"), "SYSTEM missing: {s}");
        assert!(s.contains(";BA)"), "Administrators missing: {s}");
        assert!(s.contains(SID), "the installing user missing: {s}");
        assert_eq!(
            s.matches("(A;").count(),
            3,
            "no other principal may appear: {s}"
        );
    }

    #[test]
    fn the_dacl_is_protected_from_inheritance() {
        // Without P, %ProgramData%'s inherited "all users can read" entry
        // would flow back in and undo the whole point of the step.
        assert!(sddl(SID).starts_with("D:P"), "{}", sddl(SID));
    }

    #[test]
    fn the_entries_are_inheritable_so_the_index_file_is_covered() {
        // Protecting the folder but not its contents would leave index.bin
        // itself readable.
        assert!(sddl(SID).contains("OICI"), "{}", sddl(SID));
    }

    #[cfg(windows)]
    #[test]
    fn windows_accepts_the_descriptor() {
        // The same check pipe.rs uses: a malformed SDDL would otherwise only
        // surface when setup ran on a real machine.
        use std::ptr;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;

        let text: Vec<u16> = sddl(SID).encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "Windows rejected the SDDL");
        assert!(!psd.is_null());
        unsafe { LocalFree(psd as *mut _) };
    }
}
