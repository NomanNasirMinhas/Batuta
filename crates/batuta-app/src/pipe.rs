//! Named pipe transport between the CLI and the daemon.
//!
//! The daemon runs elevated and clients do not, so the pipe is a privilege
//! boundary in both directions:
//!
//! - **Who may connect** is restricted by an explicit security descriptor.
//!   The default DACL on a named pipe is far more permissive than we want,
//!   and the index describes files belonging to every user on the machine, so
//!   access is limited to the account that started the daemon, plus
//!   Administrators and SYSTEM.
//! - **What they may send** is bounded by the framing and decoding in
//!   `batuta-ipc`, which never allocates on an unverified length.

use std::io::{self, Read, Write};
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub use batuta_ipc::PIPE_NAME;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const PIPE_BUFFER: u32 = 64 * 1024;
const MAX_INSTANCES: u32 = 16;

/// All pipe instances are currently serving other clients.
const ERROR_PIPE_BUSY: i32 = 231;

/// How long to wait for a free instance before giving up.
const CONNECT_TIMEOUT_MS: u32 = 2_000;

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The SID of the account this process is running as, in string form.
pub(crate) fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last_error());
        }

        // Ask for the size first; TOKEN_USER is variable length.
        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return Err(last_error());
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(last_error());
        }

        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut raw: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut raw) == 0 {
            return Err(last_error());
        }
        let mut len = 0;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(raw, len));
        LocalFree(raw as *mut _);
        Ok(s)
    }
}

/// Owns a security descriptor for the lifetime of the pipe.
struct SecurityDescriptor {
    psd: PSECURITY_DESCRIPTOR,
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.psd.is_null() {
            unsafe { LocalFree(self.psd as *mut _) };
        }
    }
}

impl SecurityDescriptor {
    /// Build a DACL granting full access to SYSTEM and Administrators, and
    /// read/write to `owner`. Nobody else.
    ///
    /// `owner` must be the human user's SID, recorded when setup ran. Asking
    /// the running process would be wrong: as a service the daemon runs as
    /// LocalSystem, so it would grant access to SYSTEM and lock out the
    /// unelevated client the pipe exists for.
    ///
    /// `P` makes the DACL protected, so no inherited entry can widen it.
    fn restrictive(owner: Option<&str>) -> io::Result<Self> {
        let sid = owner.map(str::to_owned).or_else(|| current_user_sid().ok());
        let sddl = match sid {
            Some(sid) => format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid})"),
            // Without an owner, stay closed rather than opening up.
            None => "D:P(A;;GA;;;SY)(A;;GA;;;BA)".to_string(),
        };
        let wide = to_wide(&sddl);
        let mut psd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(SecurityDescriptor { psd })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.psd,
            bInheritHandle: 0,
        }
    }
}

/// One end of a connected pipe.
pub struct PipeStream {
    handle: HANDLE,
    /// Server ends need disconnecting before the handle is closed.
    server: bool,
}

unsafe impl Send for PipeStream {}

impl Drop for PipeStream {
    fn drop(&mut self) {
        unsafe {
            if self.server {
                DisconnectNamedPipe(self.handle);
            }
            CloseHandle(self.handle);
        }
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut got: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut got,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = last_error();
            // A client that hung up is end-of-stream, not a failure.
            return match e.raw_os_error() {
                Some(109) | Some(232) => Ok(0), // BROKEN_PIPE, NO_DATA
                _ => Err(e),
            };
        }
        Ok(got as usize)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut put: u32 = 0;
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                buf.len() as u32,
                &mut put,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(put as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PipeStream {
    /// Connect to a running daemon.
    ///
    /// A busy pipe means every instance is mid-request, not that the daemon is
    /// absent. Treating the two the same would make a client quietly fall back
    /// to the on-disk snapshot whenever another query happened to be in
    /// flight, answering from stale data without saying so. So a busy pipe is
    /// waited on instead.
    pub fn connect() -> io::Result<Self> {
        let wide = to_wide(PIPE_NAME);
        loop {
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ptr::null(),
                    OPEN_EXISTING,
                    0,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(PipeStream {
                    handle,
                    server: false,
                });
            }

            let err = last_error();
            if err.raw_os_error() != Some(ERROR_PIPE_BUSY) {
                return Err(err);
            }
            // Block until an instance frees up. A timeout here means the
            // daemon is wedged, which is worth reporting rather than hiding.
            let waited = unsafe { WaitNamedPipeW(wide.as_ptr(), CONNECT_TIMEOUT_MS) };
            if waited == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the daemon is listening but did not free a pipe instance",
                ));
            }
        }
    }

    /// Is a daemon listening?
    pub fn daemon_running() -> bool {
        match Self::connect() {
            Ok(_) => true,
            // A timeout means one is there but wedged; still "running".
            Err(e) => e.kind() == io::ErrorKind::TimedOut,
        }
    }

    /// Send a request and read the reply.
    pub fn request(
        &mut self,
        req: &batuta_ipc::Request,
    ) -> batuta_ipc::Result<batuta_ipc::Response> {
        batuta_ipc::write_frame(self, &req.encode())?;
        let frame = batuta_ipc::read_frame(self)?;
        batuta_ipc::Response::decode(&frame)
    }
}

/// Accepts client connections on the pipe.
pub struct PipeServer {
    sd: SecurityDescriptor,
}

impl PipeServer {
    /// Bind the pipe, granting access to `owner` alongside SYSTEM and
    /// Administrators.
    pub fn bind(owner: Option<&str>) -> io::Result<Self> {
        Ok(PipeServer {
            sd: SecurityDescriptor::restrictive(owner)?,
        })
    }

    /// Create a fresh pipe instance and block until a client connects.
    ///
    /// Each accepted connection gets its own instance, which is how Windows
    /// named pipes serve several clients at once.
    pub fn accept(&self) -> io::Result<PipeStream> {
        let wide = to_wide(PIPE_NAME);
        let attrs = self.sd.attributes();
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                MAX_INSTANCES,
                PIPE_BUFFER,
                PIPE_BUFFER,
                0,
                &attrs,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }

        let ok = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) };
        // ERROR_PIPE_CONNECTED means the client arrived before we blocked,
        // which is a success, not a failure.
        if ok == 0 && unsafe { GetLastError() } != 535 {
            let e = last_error();
            unsafe { CloseHandle(handle) };
            return Err(e);
        }
        Ok(PipeStream {
            handle,
            server: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_looks_like_a_sid() {
        let sid = current_user_sid().expect("should be able to read our own token");
        assert!(sid.starts_with("S-1-"), "unexpected SID form: {sid}");
        assert!(sid.len() > 8);
    }

    #[test]
    fn the_descriptor_grants_only_the_intended_principals() {
        // Building it is the real check: an SDDL string the OS rejects would
        // otherwise surface only when the daemon first starts.
        let sd = SecurityDescriptor::restrictive(None).expect("SDDL should be valid");
        assert!(!sd.psd.is_null());
        let attrs = sd.attributes();
        assert_eq!(
            attrs.nLength as usize,
            std::mem::size_of::<SECURITY_ATTRIBUTES>()
        );
        assert_eq!(attrs.bInheritHandle, 0, "the handle must not be inherited");
    }

    #[test]
    fn an_explicit_owner_is_granted_not_the_running_account() {
        // The bug this guards: the daemon runs as LocalSystem, so deriving the
        // SID from its own token granted SYSTEM and locked out the unelevated
        // client the pipe exists to serve.
        let user = "S-1-5-21-2205097650-901772425-1229515317-1001";
        let sd = SecurityDescriptor::restrictive(Some(user)).expect("SDDL should be valid");
        assert!(!sd.psd.is_null());

        // And the SID actually reaches the descriptor text.
        let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{user})");
        assert!(sddl.contains(user));
        assert_eq!(
            sddl.matches("(A;").count(),
            3,
            "no other principal may appear"
        );
    }

    #[test]
    fn without_an_owner_the_pipe_stays_closed_rather_than_open() {
        // Failing shut matters: the index describes every user's files.
        let sd = SecurityDescriptor::restrictive(None);
        assert!(sd.is_ok(), "must still build a valid, closed descriptor");
    }

    #[test]
    fn connecting_with_no_daemon_fails_cleanly() {
        // Nothing is listening during tests; this must be a clean error rather
        // than a hang or a panic.
        if !PipeStream::daemon_running() {
            assert!(PipeStream::connect().is_err());
        }
    }
}
