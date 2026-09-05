//! Noticing that a newer release has been published.
//!
//! Deliberately small. It asks GitHub which release is newest, compares that
//! with the version this binary was built as, and says so. It does not
//! download anything, does not run anything, and cannot update itself.
//!
//! ## How the version is found out
//!
//! `https://github.com/OWNER/REPO/releases/latest` is a redirect to the tag
//! page of the newest release, so the answer is the `Location` header and
//! there is no JSON to parse and no API rate limit to run into. Redirects are
//! therefore switched *off*: the whole point is to read where it was going to
//! send us.
//!
//! ## What is sent
//!
//! One GET to `github.com`, carrying a user agent naming this program and its
//! version. No path, no query, no machine identifier, nothing about the index.
//! There is nothing to send: the answer does not depend on who is asking.
//!
//! ## When
//!
//! At most once a day, remembered in `%LOCALAPPDATA%\Batuta`. Off entirely
//! with `check_updates = false` in the config or `BATUTA_NO_UPDATE_CHECK=1` in
//! the environment. The daemon never checks — it runs as LocalSystem and has
//! nobody to tell.

use std::path::PathBuf;

/// Where releases are published.
const OWNER_REPO: &str = "NomanNasirMinhas/Batuta";

/// How long an answer is trusted before asking again.
const MAX_AGE: u64 = 24 * 60 * 60;

/// Set to anything non-empty to switch the check off.
pub const OFF_ENV: &str = "BATUTA_NO_UPDATE_CHECK";

/// A released version, as the tags spell it: `v0.1.3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse `v1.2.3` or `1.2.3`.
///
/// Strict about the shape, because the alternative is announcing an update to
/// a version that does not exist. Anything else — a pre-release suffix, a
/// missing component, a redirect that landed somewhere unexpected — is not a
/// version, and no answer is better than a wrong one.
pub fn parse(text: &str) -> Option<Version> {
    let text = text.trim();
    let text = text
        .strip_prefix('v')
        .or(text.strip_prefix('V'))
        .unwrap_or(text);
    let mut parts = text.split('.');
    let mut next = || parts.next()?.parse::<u32>().ok();
    let (major, minor, patch) = (next()?, next()?, next()?);
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// The version this binary was built as.
///
/// `BATUTA_VERSION` is stamped in by the release workflow, which works the
/// version out from the published tags rather than from `Cargo.toml`. Without
/// it a locally built binary reports whatever the manifest says, which is the
/// truthful answer for a build that is not a release.
pub fn stamped() -> &'static str {
    option_env!("BATUTA_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn current() -> Option<Version> {
    parse(stamped())
}

/// Is `latest` worth telling someone running `current` about?
pub fn is_newer(latest: Version, current: Version) -> bool {
    latest > current
}

/// Where the last answer is remembered.
///
/// Under `%LOCALAPPDATA%`, not the index directory: that one can be locked
/// down to Administrators by setup, and a failed write there would mean asking
/// GitHub on every single launch.
fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("Batuta").join("update-check"))
}

/// `(when it was checked, what was found)`.
fn read_cache() -> Option<(u64, String)> {
    let text = std::fs::read_to_string(cache_path()?).ok()?;
    let mut lines = text.lines();
    let at: u64 = lines.next()?.trim().parse().ok()?;
    let tag = lines.next()?.trim().to_string();
    Some((at, tag))
}

fn write_cache(tag: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Best effort throughout: a cache that cannot be written costs an extra
    // request, which is not worth reporting to anybody.
    let _ = std::fs::write(path, format!("{}\n{}\n", now(), tag));
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Has the user turned this off?
pub fn disabled() -> bool {
    std::env::var_os(OFF_ENV).is_some_and(|v| !v.is_empty())
}

/// The newest published version, if it is newer than this one.
///
/// Returns `None` for every uninteresting outcome — already current, switched
/// off, no network, GitHub unreachable, an answer that did not parse. A failed
/// update check is not a problem the user has, so it is never reported as one.
pub fn check(allowed: bool) -> Option<Version> {
    if !allowed || disabled() {
        return None;
    }
    let current = current()?;

    let fresh = match read_cache() {
        Some((at, tag)) if now().saturating_sub(at) < MAX_AGE => Some(tag),
        _ => None,
    };

    let tag = match fresh {
        Some(tag) => tag,
        None => {
            let tag = latest_tag()?;
            write_cache(&tag);
            tag
        }
    };

    let latest = parse(&tag)?;
    is_newer(latest, current).then_some(latest)
}

/// Ask GitHub which release is newest, and return its tag.
#[cfg(windows)]
fn latest_tag() -> Option<String> {
    use std::ffi::c_void;
    use windows_sys::Win32::Networking::WinHttp::*;

    /// A NUL-terminated UTF-16 string, kept alive for the call that uses it.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Closes its handle however the function returns, including early.
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    let agent = wide(&format!("Batuta/{}", stamped()));
    let host = wide("github.com");
    let path = wide(&format!("/{OWNER_REPO}/releases/latest"));
    let verb = wide("GET");

    unsafe {
        let session = Handle(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ));
        if session.0.is_null() {
            return None;
        }

        // Bounded on every phase. This runs on a background thread, but an
        // unbounded wait would keep a process alive long after the window it
        // was reporting to had gone.
        WinHttpSetTimeouts(session.0, 5_000, 5_000, 5_000, 5_000);

        let connect = Handle(WinHttpConnect(session.0, host.as_ptr(), 443, 0));
        if connect.0.is_null() {
            return None;
        }

        let request = Handle(WinHttpOpenRequest(
            connect.0,
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return None;
        }

        // The redirect *is* the answer, so following it would throw away the
        // only thing being asked for — and would download a web page for no
        // reason.
        let disable: u32 = WINHTTP_DISABLE_REDIRECTS;
        WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_DISABLE_FEATURE,
            &disable as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );

        if WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0 {
            return None;
        }
        if WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0 {
            return None;
        }

        // 4 KB is far more than a URL, and the call fails cleanly rather than
        // truncating if a header were somehow longer.
        let mut buf = [0u16; 2048];
        let mut len = (buf.len() * std::mem::size_of::<u16>()) as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_LOCATION,
            std::ptr::null(),
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
        ) == 0
        {
            return None;
        }

        let chars = len as usize / std::mem::size_of::<u16>();
        let location = String::from_utf16_lossy(&buf[..chars]);
        tag_from_location(&location)
    }
}

#[cfg(not(windows))]
fn latest_tag() -> Option<String> {
    None
}

/// The tag out of a `.../releases/tag/v1.2.3` redirect.
///
/// A repository with no releases at all redirects to `/releases` instead, and
/// that has no tag in it — which is why this insists on the `tag/` segment
/// rather than just taking the last one.
fn tag_from_location(location: &str) -> Option<String> {
    let location = location.trim().trim_end_matches('/');
    let (_, tag) = location.rsplit_once("/tag/")?;
    if tag.is_empty() || tag.contains('/') {
        return None;
    }
    Some(tag.to_string())
}

/// Start a check on another thread, to be collected later.
///
/// Never blocks the caller: an interface that paused on a network request
/// before drawing its first frame would be a worse program than one that never
/// mentioned updates at all.
pub fn spawn(allowed: bool) -> std::sync::mpsc::Receiver<Option<Version>> {
    let (tx, rx) = std::sync::mpsc::channel();
    if !allowed || disabled() {
        // Answer immediately rather than starting a thread that would only
        // report nothing. The receiver still behaves the same way.
        let _ = tx.send(None);
        return rx;
    }
    std::thread::spawn(move || {
        // The receiver is gone if the UI exited first; nothing to do about it.
        let _ = tx.send(check(true));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    #[test]
    fn a_tag_parses_with_or_without_its_v() {
        assert_eq!(parse("v0.1.3"), Some(v(0, 1, 3)));
        assert_eq!(parse("0.1.3"), Some(v(0, 1, 3)));
        assert_eq!(parse("  v10.20.30  "), Some(v(10, 20, 30)));
    }

    #[test]
    fn anything_not_exactly_three_numbers_is_not_a_version() {
        // Announcing an update to a version that does not exist is worse than
        // saying nothing, so this is deliberately strict.
        for text in [
            "v1.2",
            "1.2.3.4",
            "v1.2.x",
            "",
            "v",
            "latest",
            "releases",
            "v1.2.3-rc1",
            "-1.2.3",
        ] {
            assert_eq!(parse(text), None, "{text:?} should not parse");
        }
    }

    #[test]
    fn versions_compare_by_component_not_as_text() {
        // The trap: "0.10.0" sorts before "0.9.0" as a string.
        assert!(is_newer(v(0, 10, 0), v(0, 9, 0)));
        assert!(is_newer(v(1, 0, 0), v(0, 99, 99)));
        assert!(is_newer(v(0, 1, 4), v(0, 1, 3)));
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        assert!(!is_newer(v(0, 1, 3), v(0, 1, 3)));
    }

    #[test]
    fn an_older_published_version_is_never_announced() {
        // Running a build newer than the last release — which is every local
        // build between releases — must not be told to downgrade.
        assert!(!is_newer(v(0, 1, 2), v(0, 1, 3)));
    }

    #[test]
    fn the_tag_comes_out_of_the_redirect() {
        assert_eq!(
            tag_from_location("https://github.com/o/r/releases/tag/v0.1.4").as_deref(),
            Some("v0.1.4")
        );
        assert_eq!(
            tag_from_location("https://github.com/o/r/releases/tag/v0.1.4/").as_deref(),
            Some("v0.1.4")
        );
    }

    #[test]
    fn a_repository_with_no_releases_yields_no_tag() {
        // GitHub redirects to the releases list instead, and taking its last
        // segment would produce the "version" `releases`.
        assert_eq!(tag_from_location("https://github.com/o/r/releases"), None);
        assert_eq!(tag_from_location(""), None);
        assert_eq!(tag_from_location("https://example.com/"), None);
    }

    #[test]
    fn the_whole_chain_survives_a_redirect_that_makes_no_sense() {
        // Location, tag, version: any link failing has to end the check, not
        // produce a wrong answer.
        for location in [
            "https://github.com/o/r/releases",
            "https://github.com/o/r/releases/tag/",
            "https://github.com/o/r/releases/tag/nightly",
        ] {
            let version = tag_from_location(location).and_then(|t| parse(&t));
            assert_eq!(version, None, "{location} should produce nothing");
        }
    }

    #[test]
    fn this_binary_knows_what_version_it_is() {
        // A binary that cannot parse its own version would silently never
        // report an update, and nothing else would notice.
        assert!(
            current().is_some(),
            "the crate version has to be a plain MAJOR.MINOR.PATCH"
        );
    }

    #[test]
    fn a_check_that_is_not_allowed_asks_nothing_and_answers_nothing() {
        assert_eq!(check(false), None);
        // And the spawned form answers rather than hanging, so a caller
        // waiting on it is not left waiting forever.
        assert_eq!(
            spawn(false).recv_timeout(std::time::Duration::from_secs(5)),
            Ok(None)
        );
    }
}
