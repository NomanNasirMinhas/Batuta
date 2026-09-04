//! Reading a file into the editor, and writing it back without damaging it.
//!
//! ## What counts as text
//!
//! Valid UTF-8 is not the test. `NUL` is a legal codepoint and plenty of
//! binaries decode without error, so a UTF-8 check alone happily opens a `.exe`
//! and shows mojibake. The gates run cheapest-and-most-decisive first, and the
//! `NUL` scan is the one that does most of the work: NUL appears in nearly
//! every binary format and essentially never in real text.
//!
//! UTF-16 is refused by name rather than half-supported. **This is a real gap
//! on Windows** — anything Notepad saved as "Unicode", anything PowerShell 5.1
//! redirected to a file — so the message says so instead of claiming the file
//! is corrupt. Decoding it would be easy; writing it back with the right BOM,
//! endianness and terminators is where files get mangled, and an editor that
//! damages a file it offered to open is worse than one that declines.
//!
//! ## Writing it back unchanged
//!
//! Opening a file and saving it untouched must produce identical bytes. The
//! per-line terminators, the trailing newline and the BOM all live on
//! [`Buffer`] for that reason; this module only detects them on the way in and
//! asks for the bytes on the way out.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::buffer::{Buffer, Ending};

/// Files above this are reported rather than loaded. The buffer keeps one
/// `String` per line and edits a line in place, so this bounds the worst case.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// A single line longer than this is both unusable in a line editor and strong
/// evidence the file is not really text — a minified bundle, or a blob.
const MAX_LINE: usize = 1024 * 1024;

/// Five million one-byte lines cost far more in `String` headers than the file
/// costs on disk, so line count is capped as well as bytes.
const MAX_LINES: usize = 2_000_000;

/// Above this share of control characters, treat it as binary. Catches
/// NUL-free binaries and escape-sequence streams that decode as valid UTF-8.
const MAX_CONTROL_RATIO: f64 = 0.003;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

#[derive(Debug)]
pub enum Loaded {
    Text(Box<Buffer>),
    /// Not editable here, with the reason to show the user.
    Rejected(String),
}

/// What the file looked like when it was read, so a save can notice that
/// something else has written to it since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub len: u64,
    pub mtime: Option<SystemTime>,
}

impl Stamp {
    pub fn of(path: &Path) -> io::Result<Stamp> {
        let md = std::fs::metadata(path)?;
        Ok(Stamp {
            len: md.len(),
            mtime: md.modified().ok(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Saved {
    Written,
    /// Someone else wrote to the file after it was loaded. Refused rather than
    /// silently overwriting their work.
    ChangedOnDisk,
    /// The read-only attribute is set. Clearing it is not the editor's call.
    ReadOnly,
}

/// Decide what a file's bytes are, and split them into an editable document.
pub fn inspect(bytes: &[u8]) -> Loaded {
    fn no(reason: &str) -> Loaded {
        Loaded::Rejected(reason.to_string())
    }

    // Named encodings first, so the message can say what the file actually is
    // rather than "not valid UTF-8".
    if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) || bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF])
    {
        return no("This is a UTF-32 file. Batuta only edits UTF-8.");
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return no("This is a UTF-16 file - common on Windows, from Notepad's \
             \"Unicode\" or a PowerShell redirect. Batuta only edits UTF-8.");
    }

    // The decisive one, and cheap: a NUL almost always means binary, and it is
    // exactly the case a UTF-8 check waves through.
    if memchr::memchr(0, bytes).is_some() {
        return no("This file contains NUL bytes, so it is not text.");
    }

    let bom = bytes.starts_with(&BOM);
    let body = if bom { &bytes[BOM.len()..] } else { bytes };

    let Ok(text) = std::str::from_utf8(body) else {
        return no("This file is not valid UTF-8, so Batuta cannot edit it.");
    };

    // A binary can be NUL-free and valid UTF-8; a wall of control characters
    // is the remaining tell.
    if !text.is_empty() {
        let controls = text
            .bytes()
            .filter(|b| (*b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r')) || *b == 0x7F)
            .count();
        if controls as f64 / text.len() as f64 > MAX_CONTROL_RATIO {
            return no("This file is mostly control characters, so it is not text.");
        }
    }

    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    // Ties and empty files go to CRLF: this is a Windows-only program, and an
    // LF default would surprise more people here than it pleased.
    let dominant = if lf > crlf { Ending::Lf } else { Ending::Crlf };

    // `str::lines()` is unusable for this: it strips `\r\n` and `\n` alike with
    // no way to tell which, and loses the trailing-newline distinction
    // entirely. A lone `\r` is content, not a terminator - treating it as one
    // would restructure every such file on the first save.
    let mut lines: Vec<String> = Vec::new();
    let mut eols: Vec<Ending> = Vec::new();
    let mut trailing_newline = false;

    for piece in text.split_inclusive('\n') {
        match piece.strip_suffix('\n') {
            Some(rest) => {
                trailing_newline = true;
                match rest.strip_suffix('\r') {
                    Some(body) => {
                        lines.push(body.to_string());
                        eols.push(Ending::Crlf);
                    }
                    None => {
                        lines.push(rest.to_string());
                        eols.push(Ending::Lf);
                    }
                }
            }
            None => {
                trailing_newline = false;
                lines.push(piece.to_string());
                eols.push(dominant);
            }
        }
    }
    // An empty file yields no pieces at all, and a document is never zero
    // lines. Its `trailing_newline` must stay false so saving it untouched
    // writes zero bytes rather than one terminator.
    if lines.is_empty() {
        lines.push(String::new());
        eols.push(dominant);
    }

    if lines.len() > MAX_LINES {
        return no("This file has too many lines to edit here.");
    }
    if lines.iter().any(|l| l.len() > MAX_LINE) {
        return no("This file has a line too long to edit here.");
    }

    Loaded::Text(Box::new(Buffer::from_parts(
        lines,
        eols,
        trailing_newline,
        bom,
        dominant,
    )))
}

/// Read a file, deciding whether it is editable text.
pub fn load(path: &Path) -> io::Result<Loaded> {
    // Checked before reading, so a 4 GB file is never pulled into memory to
    // find out it is too big.
    let size = std::fs::metadata(path)?.len();
    if size > MAX_BYTES {
        return Ok(Loaded::Rejected(format!(
            "This file is {:.1} MB. Batuta edits files up to {} MB.",
            size as f64 / (1024.0 * 1024.0),
            MAX_BYTES / (1024 * 1024)
        )));
    }
    Ok(inspect(&std::fs::read(path)?))
}

/// The temp file a save writes before taking the original's place.
///
/// Beside the target on purpose: a replace cannot cross volumes, and the
/// system temp directory frequently is one.
fn temp_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mut tmp = path.to_path_buf();
    tmp.set_file_name(format!(".{name}.batuta-{}.tmp", std::process::id()));
    tmp
}

/// Write `bytes` to `path`, preserving what the original file was.
///
/// The contents go to a temp file, are flushed to the device, and only then
/// take the original's place. Writing in place would truncate the file first,
/// so a crash between the truncate and the write destroys the user's data —
/// and with `panic = "abort"` a crash is a realistic outcome of any bug in a
/// new editor, not a hypothetical.
pub fn save(path: &Path, bytes: &[u8], expected: Option<Stamp>) -> io::Result<Saved> {
    use std::io::Write;

    if path.exists() {
        let md = std::fs::metadata(path)?;
        if md.permissions().readonly() {
            return Ok(Saved::ReadOnly);
        }
        // In a program whose whole premise is watching a live filesystem, not
        // checking this is the difference between an editor and a way to lose
        // someone else's work.
        if let Some(expected) = expected {
            if Stamp::of(path)? != expected {
                return Ok(Saved::ChangedOnDisk);
            }
        }
    } else {
        // Nothing to preserve and nothing to lose.
        std::fs::write(path, bytes)?;
        return Ok(Saved::Written);
    }

    let tmp = temp_beside(path);
    let write = (|| -> io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        // Without this the replace can commit while the data is still in the
        // cache, and a power cut leaves an atomically-renamed empty file —
        // the exact failure this whole dance exists to prevent, except the
        // original is gone too.
        f.sync_all()
    })();

    if let Err(e) = write.and_then(|()| replace_file(path, &tmp)) {
        // No destructor runs under `panic = "abort"`, so cleanup is explicit.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(Saved::Written)
}

#[cfg(windows)]
fn replace_file(target: &Path, replacement: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// The replace could not delete the original.
    const ERROR_UNABLE_TO_REMOVE_REPLACED: i32 = 1175;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_ACCESS_DENIED: i32 = 5;

    let (t, r) = (wide(target), wide(replacement));
    // `ReplaceFileW`, not a rename. A rename deletes the destination and moves
    // the temp into its place, so the file ends up with the temp's freshly
    // inherited ACL, creation time and short name. Any permission the user
    // deliberately set on that file is silently discarded. `ReplaceFileW`
    // exists precisely to merge the new data into the old identity.
    // Antivirus scanners, search indexers and backup agents open a file
    // moments after it is written, and while they hold it the replace cannot
    // delete the original. That is transient and common enough that failing
    // the save outright would make editing feel unreliable, so it is retried
    // briefly. Anything still failing after that is a real error and is
    // reported: quietly falling back to a plain rename would reintroduce the
    // permission loss this function exists to avoid.
    let mut last = io::Error::other("replace failed");
    for attempt in 0..5 {
        let ok = unsafe {
            ReplaceFileW(
                t.as_ptr(),
                r.as_ptr(),
                ptr::null(),
                0,
                ptr::null(),
                ptr::null(),
            )
        };
        if ok != 0 {
            return Ok(());
        }

        last = io::Error::last_os_error();
        let transient = matches!(
            last.raw_os_error(),
            Some(ERROR_UNABLE_TO_REMOVE_REPLACED)
                | Some(ERROR_SHARING_VIOLATION)
                | Some(ERROR_ACCESS_DENIED)
        );
        if !transient {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1)));
    }
    Err(last)
}

#[cfg(not(windows))]
fn replace_file(target: &Path, replacement: &Path) -> io::Result<()> {
    std::fs::rename(replacement, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8]) -> Buffer {
        match inspect(bytes) {
            Loaded::Text(b) => *b,
            Loaded::Rejected(why) => panic!("expected text, rejected: {why}"),
        }
    }

    fn rejected(bytes: &[u8]) -> String {
        match inspect(bytes) {
            Loaded::Rejected(why) => why,
            Loaded::Text(_) => panic!("expected a rejection"),
        }
    }

    /// The property that matters most in this whole module.
    #[test]
    fn loading_then_saving_an_untouched_file_is_byte_identical() {
        let cases: &[&[u8]] = &[
            b"",
            b"one line",
            b"one line\n",
            b"a\nb\nc",
            b"a\nb\nc\n",
            b"a\r\nb\r\nc",
            b"a\r\nb\r\nc\r\n",
            b"\n",
            b"\r\n",
            b"a\r\nb\nc\r\n",
            b"mac\rstyle\rcontent\n",
            &[0xEF, 0xBB, 0xBF, b'a', b'\n'],
            &[0xEF, 0xBB, 0xBF],
            "h\u{e9}llo \u{2192} w\u{f6}rld\r\n".as_bytes(),
        ];
        for original in cases {
            assert_eq!(
                text(original).to_bytes(),
                *original,
                "round trip changed {original:?}"
            );
        }
    }

    #[test]
    fn mixed_endings_survive_an_edit_to_one_line() {
        // Normalising would rewrite every line, turning a one-character fix
        // into a whole-file diff.
        let mut b = text(b"a\r\nb\nc\r\n");
        assert!(b.mixed_endings());
        b.goto(super::super::buffer::Cursor::new(0, 1));
        b.insert_char('!');
        assert_eq!(b.to_bytes(), b"a!\r\nb\nc\r\n");
    }

    #[test]
    fn a_lone_carriage_return_is_content_not_a_terminator() {
        // Treating it as a line break restructures the file on first save.
        let b = text(b"mac\rstyle\n");
        assert_eq!(b.len(), 1, "one line, with a CR inside it");
        assert_eq!(b.to_bytes(), b"mac\rstyle\n");
    }

    #[test]
    fn a_trailing_newline_does_not_become_a_phantom_line() {
        let b = text(b"a\n");
        assert_eq!(b.len(), 1);
        assert_eq!(b.line(0), "a");
    }

    #[test]
    fn an_empty_file_stays_empty_when_saved() {
        let b = text(b"");
        assert_eq!(b.len(), 1, "a document is never zero lines");
        assert_eq!(b.to_bytes(), b"", "and must not gain a terminator");
    }

    #[test]
    fn a_binary_that_happens_to_be_valid_utf8_is_still_binary() {
        // Exactly the case a UTF-8 check waves through.
        // All-ASCII apart from the NULs, so this really does decode cleanly -
        // which is the whole point of checking for NUL separately.
        let bytes = b"MZ\x00\x00PE\x00\x00header text";
        assert!(std::str::from_utf8(bytes).is_ok(), "precondition");
        assert!(rejected(bytes).contains("NUL"));
    }

    #[test]
    fn utf16_is_named_rather_than_called_corrupt() {
        // Common enough on Windows that "not valid UTF-8" would be unhelpful.
        let le = [0xFF, 0xFE, b'h', 0, b'i', 0];
        assert!(rejected(&le).contains("UTF-16"));
        let be = [0xFE, 0xFF, 0, b'h', 0, b'i'];
        assert!(rejected(&be).contains("UTF-16"));
    }

    #[test]
    fn a_nul_free_control_character_stream_is_rejected() {
        let mut bytes = vec![b'a'; 1000];
        bytes.extend(std::iter::repeat_n(0x07, 50));
        assert!(rejected(&bytes).contains("control"));
    }

    #[test]
    fn ordinary_text_is_not_mistaken_for_control_characters() {
        let bytes = b"fn main() {\n\tprintln!(\"hi\");\r\n}\n";
        assert_eq!(text(bytes).to_bytes(), bytes);
    }

    #[test]
    fn invalid_utf8_is_refused() {
        assert!(rejected(&[0xC3, 0x28, b'a']).contains("UTF-8"));
    }

    #[test]
    fn the_temp_file_sits_beside_its_target() {
        let tmp = temp_beside(Path::new(r"D:\work\notes.txt"));
        assert_eq!(tmp.parent().unwrap(), Path::new(r"D:\work"));
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".notes.txt.batuta-"), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batuta-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saving_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = scratch("save");
        let path = dir.join("notes.txt");
        std::fs::write(&path, b"old\r\n").unwrap();

        let stamp = Stamp::of(&path).unwrap();
        let mut b = match load(&path).unwrap() {
            Loaded::Text(b) => *b,
            Loaded::Rejected(w) => panic!("{w}"),
        };
        b.end();
        b.insert_str("er");

        assert_eq!(
            save(&path, &b.to_bytes(), Some(stamp)).unwrap(),
            Saved::Written
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"older\r\n");
        assert!(
            !temp_beside(&path).exists(),
            "the temp must not survive a successful save"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_changed_underneath_us_is_refused_not_overwritten() {
        let dir = scratch("stale");
        let path = dir.join("shared.txt");
        std::fs::write(&path, b"mine\n").unwrap();
        let stamp = Stamp::of(&path).unwrap();

        // Someone else writes to it. A different length is enough to see it
        // without depending on filesystem timestamp resolution.
        std::fs::write(&path, b"theirs, and longer\n").unwrap();

        assert_eq!(
            save(&path, b"mine\n", Some(stamp)).unwrap(),
            Saved::ChangedOnDisk
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"theirs, and longer\n",
            "their work must still be there"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_only_file_is_refused_rather_than_forced() {
        let dir = scratch("ro");
        let path = dir.join("locked.txt");
        std::fs::write(&path, b"keep\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();

        assert_eq!(save(&path, b"changed\n", None).unwrap(), Saved::ReadOnly);
        assert_eq!(std::fs::read(&path).unwrap(), b"keep\n");

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        // Undoing exactly what this test set, so the directory can be removed.
        // The lint is about widening permissions on Unix; here it is putting a
        // Windows attribute back the way it was found.
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_a_file_that_does_not_exist_yet_creates_it() {
        let dir = scratch("new");
        let path = dir.join("fresh.txt");
        assert_eq!(save(&path, b"hello\n", None).unwrap(), Saved::Written);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_file_is_reported_rather_than_loaded() {
        let dir = scratch("big");
        let path = dir.join("big.bin");
        std::fs::write(&path, vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();

        match load(&path).unwrap() {
            Loaded::Rejected(why) => assert!(why.contains("up to"), "{why}"),
            Loaded::Text(_) => panic!("should have been refused"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
