//! Commands the shell runs itself.
//!
//! Two kinds live here, and the difference matters. `cd`, `set` and `exit`
//! change the shell's own state and *cannot* be external programs — a child
//! process changing its own directory would leave the parent exactly where it
//! was. The rest (`ls`, `cat`, `echo`) are here because Windows has no
//! reliable equivalent to reach for: `dir` and `type` are `cmd` builtins, not
//! programs, so a shell that shells out for them is depending on `cmd`.
//!
//! Every builtin writes through a handed-in writer rather than to stdout
//! directly, which is what lets `ls | findstr rs` and `ls > out.txt` work at
//! all.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::parse::Command;
use crate::fmt;

/// What a builtin did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub status: i32,
    /// The shell should stop reading input.
    pub exit: bool,
}

impl Outcome {
    fn ok() -> Outcome {
        Outcome {
            status: 0,
            exit: false,
        }
    }

    fn failed() -> Outcome {
        Outcome {
            status: 1,
            exit: false,
        }
    }
}

/// The shell state a builtin may change.
pub struct State {
    pub cwd: PathBuf,
    /// Where `cd -` goes back to.
    pub previous: Option<PathBuf>,
    pub vars: std::collections::HashMap<String, String>,
}

pub const NAMES: &[&str] = &[
    "cd", "pwd", "exit", "echo", "ls", "dir", "cat", "type", "clear", "cls", "set", "which",
    "help", "mkdir", "rm", "del", "touch", "cp", "copy", "mv", "move",
];

pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name.to_ascii_lowercase().as_str())
}

/// Run a builtin, or return `None` if this is not one.
///
/// Takes an input stream as well as the two output ones: without it `cat`
/// cannot read from a pipe, and a builtin that cannot appear on the right of
/// a `|` is only half a builtin.
pub fn run(
    state: &mut State,
    cmd: &Command,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<Outcome> {
    let name = cmd.program()?.to_ascii_lowercase();
    if !is_builtin(&name) {
        return None;
    }
    let args = cmd.args();

    let result = match name.as_str() {
        "cd" => cd(state, args, err),
        "pwd" => {
            let _ = writeln!(out, "{}", state.cwd.display());
            Outcome::ok()
        }
        "exit" => Outcome {
            status: args.first().and_then(|a| a.parse().ok()).unwrap_or(0),
            exit: true,
        },
        "echo" => {
            let _ = writeln!(out, "{}", args.join(" "));
            Outcome::ok()
        }
        "ls" | "dir" => ls(state, args, out, err),
        "cat" | "type" => cat(state, args, input, out, err),
        "clear" | "cls" => {
            // Erase everything and put the cursor home. The emulator on the
            // other end understands this; so does any other terminal.
            let _ = write!(out, "\x1b[2J\x1b[H");
            Outcome::ok()
        }
        "set" => set(state, args, out),
        "which" => which(state, args, out, err),
        "mkdir" => mkdir(state, args, err),
        "rm" | "del" => remove(state, args, err),
        "touch" => touch(state, args, err),
        "cp" | "copy" => transfer(state, args, err, Transfer::Copy),
        "mv" | "move" => transfer(state, args, err, Transfer::Move),
        "help" => {
            let _ = writeln!(out, "{}", help_text());
            Outcome::ok()
        }
        _ => Outcome::failed(),
    };
    Some(result)
}

/// Resolve a path argument against the shell's directory.
fn resolve(state: &State, arg: &str) -> PathBuf {
    let p = Path::new(arg);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        state.cwd.join(p)
    }
}

fn cd(state: &mut State, args: &[String], err: &mut dyn Write) -> Outcome {
    let target = match args.first().map(String::as_str) {
        // Bare `cd` goes home, as it does everywhere except `cmd`, where it
        // prints the current directory instead. Home is the more useful of
        // the two and `pwd` already covers the other.
        None => match std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            Ok(home) => PathBuf::from(home),
            Err(_) => {
                let _ = writeln!(err, "cd: no home directory set");
                return Outcome::failed();
            }
        },
        Some("-") => match state.previous.clone() {
            Some(prev) => prev,
            None => {
                let _ = writeln!(err, "cd: no previous directory");
                return Outcome::failed();
            }
        },
        Some(arg) => resolve(state, arg),
    };

    // Canonicalised so the prompt shows a real path rather than one full of
    // `..`, and so `cd -` goes somewhere meaningful.
    let target = target.canonicalize().unwrap_or(target);
    if !target.is_dir() {
        let _ = writeln!(err, "cd: not a directory: {}", target.display());
        return Outcome::failed();
    }

    state.previous = Some(std::mem::replace(&mut state.cwd, strip_unc(target)));
    Outcome::ok()
}

/// Canonicalising on Windows produces a `\\?\` prefix, which is correct and
/// unreadable. It is removed for display and for passing on to children,
/// which mostly do not expect it.
fn strip_unc(path: PathBuf) -> PathBuf {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

fn ls(state: &State, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> Outcome {
    let dir = match args.first() {
        Some(a) => resolve(state, a),
        None => state.cwd.clone(),
    };

    let mut entries: Vec<(String, bool, u64)> = match std::fs::read_dir(&dir) {
        Ok(read) => read
            .flatten()
            .map(|e| {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                (e.file_name().to_string_lossy().into_owned(), is_dir, size)
            })
            .collect(),
        Err(e) => {
            let _ = writeln!(err, "ls: {}: {e}", dir.display());
            return Outcome::failed();
        }
    };

    // Directories first, then by name, matching the explorer and Explorer.
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()))
    });

    for (name, is_dir, size) in entries {
        if is_dir {
            let _ = writeln!(out, "{:>10}  {name}\\", "<dir>");
        } else {
            let _ = writeln!(out, "{:>10}  {name}", fmt::bytes(size));
        }
    }
    Outcome::ok()
}

fn cat(
    state: &State,
    args: &[String],
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Outcome {
    // No arguments means "copy what you are given", which is what makes
    // `dir | cat` and `cat < file` work.
    if args.is_empty() {
        return match std::io::copy(input, out) {
            Ok(_) => Outcome::ok(),
            Err(e) => {
                let _ = writeln!(err, "cat: {e}");
                Outcome::failed()
            }
        };
    }
    let mut status = Outcome::ok();
    for arg in args {
        let path = resolve(state, arg);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let _ = out.write_all(&bytes);
            }
            Err(e) => {
                let _ = writeln!(err, "cat: {}: {e}", path.display());
                status = Outcome::failed();
            }
        }
    }
    status
}

fn set(state: &mut State, args: &[String], out: &mut dyn Write) -> Outcome {
    if args.is_empty() {
        let mut names: Vec<&String> = state.vars.keys().collect();
        names.sort();
        for name in names {
            let _ = writeln!(out, "{name}={}", state.vars[name]);
        }
        return Outcome::ok();
    }
    for arg in args {
        match arg.split_once('=') {
            Some((name, value)) => {
                state.vars.insert(name.to_string(), value.to_string());
            }
            None => {
                // `set NAME` with no value prints it, which is more useful
                // than treating it as an error.
                let value = state
                    .vars
                    .get(arg)
                    .cloned()
                    .or_else(|| std::env::var(arg).ok())
                    .unwrap_or_default();
                let _ = writeln!(out, "{arg}={value}");
            }
        }
    }
    Outcome::ok()
}

fn which(state: &State, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> Outcome {
    let Some(name) = args.first() else {
        let _ = writeln!(err, "which: needs a name");
        return Outcome::failed();
    };
    if is_builtin(name) {
        let _ = writeln!(out, "{name}: a shell builtin");
        return Outcome::ok();
    }
    match super::run::find_program(state, name) {
        Some(path) => {
            let _ = writeln!(out, "{}", path.display());
            Outcome::ok()
        }
        None => {
            let _ = writeln!(err, "which: not found: {name}");
            Outcome::failed()
        }
    }
}

fn mkdir(state: &State, args: &[String], err: &mut dyn Write) -> Outcome {
    if args.is_empty() {
        let _ = writeln!(err, "mkdir: needs a name");
        return Outcome::failed();
    }
    let mut status = Outcome::ok();
    for arg in args {
        let path = resolve(state, arg);
        if let Err(e) = std::fs::create_dir_all(&path) {
            let _ = writeln!(err, "mkdir: {}: {e}", path.display());
            status = Outcome::failed();
        }
    }
    status
}

fn remove(state: &State, args: &[String], err: &mut dyn Write) -> Outcome {
    if args.is_empty() {
        let _ = writeln!(err, "rm: needs a name");
        return Outcome::failed();
    }
    let mut status = Outcome::ok();
    for arg in args {
        let path = resolve(state, arg);
        // Directories need saying so explicitly. Deleting a tree because a
        // name happened to be a folder is not a mistake worth allowing.
        let result = if path.is_dir() {
            let _ = writeln!(
                err,
                "rm: {} is a directory; remove it in the explorer",
                path.display()
            );
            status = Outcome::failed();
            continue;
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = result {
            let _ = writeln!(err, "rm: {}: {e}", path.display());
            status = Outcome::failed();
        }
    }
    status
}

#[derive(Clone, Copy, PartialEq)]
enum Transfer {
    Copy,
    Move,
}

impl Transfer {
    fn name(self) -> &'static str {
        match self {
            Transfer::Copy => "cp",
            Transfer::Move => "mv",
        }
    }
}

/// `cp` and `mv`. They differ in one call, so they are one function.
///
/// Windows ships neither as a program, so without these there is no way to
/// copy a file from a shell that lives inside a file explorer.
///
/// The last argument is the destination, as everywhere else. Several sources
/// need it to be a directory; one source may name either a directory to put
/// the file in or the new name itself.
///
/// Neither recurses into directories, for the same reason `rm` will not delete
/// one: a mistyped name that silently duplicates or relocates a tree is not
/// worth the convenience.
fn transfer(state: &State, args: &[String], err: &mut dyn Write, how: Transfer) -> Outcome {
    let name = how.name();
    if args.len() < 2 {
        let _ = writeln!(err, "{name}: needs a source and a destination");
        return Outcome::failed();
    }
    let (sources, target) = args.split_at(args.len() - 1);
    let target = resolve(state, &target[0]);
    let into_dir = target.is_dir();

    if sources.len() > 1 && !into_dir {
        let _ = writeln!(
            err,
            "{name}: {} is not a directory, so it cannot take several files",
            target.display()
        );
        return Outcome::failed();
    }

    let mut status = Outcome::ok();
    for arg in sources {
        let from = resolve(state, arg);
        if from.is_dir() {
            let _ = writeln!(
                err,
                "{name}: {} is a directory; only files are handled",
                from.display()
            );
            status = Outcome::failed();
            continue;
        }
        // A destination directory keeps the source's own file name.
        let to = if into_dir {
            match from.file_name() {
                Some(base) => target.join(base),
                None => {
                    let _ = writeln!(err, "{name}: {} has no name", from.display());
                    status = Outcome::failed();
                    continue;
                }
            }
        } else {
            target.clone()
        };

        let result = match how {
            Transfer::Copy => std::fs::copy(&from, &to).map(|_| ()),
            // `rename` fails across volumes, which is exactly the case someone
            // moving a file out of a downloads folder will hit; fall back to
            // copying and then removing the original.
            Transfer::Move => std::fs::rename(&from, &to).or_else(|e| {
                std::fs::copy(&from, &to)
                    .and_then(|_| std::fs::remove_file(&from))
                    .map_err(|_| e)
            }),
        };
        if let Err(e) = result {
            let _ = writeln!(err, "{name}: {} -> {}: {e}", from.display(), to.display());
            status = Outcome::failed();
        }
    }
    status
}

fn touch(state: &State, args: &[String], err: &mut dyn Write) -> Outcome {
    if args.is_empty() {
        let _ = writeln!(err, "touch: needs a name");
        return Outcome::failed();
    }
    let mut status = Outcome::ok();
    for arg in args {
        let path = resolve(state, arg);
        if path.exists() {
            continue;
        }
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(err, "touch: {}: {e}", path.display());
            status = Outcome::failed();
        }
    }
    status
}

fn help_text() -> String {
    "batuta shell\n\
     \n\
     builtins: cd, pwd, ls/dir, cat/type, echo, set, which, mkdir, touch,\n\
     cp/copy, mv/move, rm/del, clear/cls, exit, help\n\
     \n\
     anything else is run as a program. pipes (a | b), redirection\n\
     (> >> < 2> 2>&1), quoting and $VAR / %VAR% / ~ all work.\n\
     Tab completes paths, Up and Down walk the history."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::parse::{self, map_env};

    fn state(cwd: &Path) -> State {
        State {
            cwd: cwd.to_path_buf(),
            previous: None,
            vars: std::collections::HashMap::new(),
        }
    }

    fn run_line(state: &mut State, line: &str) -> (String, String, Outcome) {
        piped(state, line, b"")
    }

    fn piped(state: &mut State, line: &str, stdin: &[u8]) -> (String, String, Outcome) {
        let env = map_env(std::collections::HashMap::new());
        let pipeline = parse::parse(line, &env).expect("parses");
        let cmd = pipeline.commands[0].clone();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut input = std::io::Cursor::new(stdin.to_vec());
        let outcome = run(state, &cmd, &mut input, &mut out, &mut err).expect("a builtin");
        (
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
            outcome,
        )
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batuta-sh-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cp_copies_a_file_and_leaves_the_original() {
        let dir = scratch("cp");
        std::fs::write(dir.join("a.txt"), b"contents").unwrap();
        let mut st = state(&dir);

        let (_, err, out) = run_line(&mut st, "cp a.txt b.txt");
        assert_eq!(out.status, 0, "{err}");
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"contents");
        assert!(dir.join("a.txt").exists(), "the source must survive a copy");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cp_into_a_directory_keeps_the_file_name() {
        let dir = scratch("cpdir");
        std::fs::create_dir_all(dir.join("into")).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"y").unwrap();
        let mut st = state(&dir);

        let (_, err, out) = run_line(&mut st, "cp a.txt b.txt into");
        assert_eq!(out.status, 0, "{err}");
        assert!(dir.join("into/a.txt").exists());
        assert!(dir.join("into/b.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn several_sources_need_a_directory_to_go_into() {
        // Otherwise each would overwrite the last and only the final one would
        // survive, silently.
        let dir = scratch("cpmany");
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("b.txt"), b"y").unwrap();
        let mut st = state(&dir);

        let (_, err, out) = run_line(&mut st, "cp a.txt b.txt c.txt");
        assert_ne!(out.status, 0);
        assert!(err.contains("not a directory"), "{err}");
        assert!(!dir.join("c.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mv_moves_a_file_and_removes_the_original() {
        let dir = scratch("mv");
        std::fs::write(dir.join("a.txt"), b"contents").unwrap();
        let mut st = state(&dir);

        let (_, err, out) = run_line(&mut st, "mv a.txt b.txt");
        assert_eq!(out.status, 0, "{err}");
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"contents");
        assert!(!dir.join("a.txt").exists(), "the source must be gone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn neither_cp_nor_mv_touches_a_directory() {
        let dir = scratch("cpdirsrc");
        std::fs::create_dir_all(dir.join("tree/inner")).unwrap();
        let mut st = state(&dir);

        for line in ["cp tree elsewhere", "mv tree elsewhere"] {
            let (_, err, out) = run_line(&mut st, line);
            assert_ne!(out.status, 0, "{line} should refuse");
            assert!(err.contains("is a directory"), "{err}");
        }
        assert!(dir.join("tree/inner").exists(), "nothing should have moved");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cp_and_mv_say_so_when_given_one_argument() {
        let dir = scratch("cpargs");
        let mut st = state(&dir);
        for line in ["cp a.txt", "mv a.txt"] {
            let (_, err, out) = run_line(&mut st, line);
            assert_ne!(out.status, 0);
            assert!(err.contains("destination"), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtins_are_recognised_whatever_the_case() {
        assert!(is_builtin("cd"));
        assert!(is_builtin("CD"));
        assert!(is_builtin("Ls"));
        assert!(!is_builtin("cargo"));
    }

    #[test]
    fn cd_changes_the_shells_own_directory() {
        // The reason this cannot be a program: a child changing its directory
        // would leave the shell exactly where it started.
        let dir = scratch("cd");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        let mut st = state(&dir);

        let (_, err, out) = run_line(&mut st, "cd inner");
        assert_eq!(out.status, 0, "{err}");
        assert!(st.cwd.ends_with("inner"), "{:?}", st.cwd);

        let (shown, _, _) = run_line(&mut st, "pwd");
        assert!(shown.trim().ends_with("inner"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cd_dash_goes_back_where_you_were() {
        let dir = scratch("cdback");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        let mut st = state(&dir);
        let start = st.cwd.clone();

        run_line(&mut st, "cd inner");
        let (_, err, out) = run_line(&mut st, "cd -");
        assert_eq!(out.status, 0, "{err}");
        assert_eq!(
            st.cwd.canonicalize().ok(),
            start.canonicalize().ok(),
            "cd - should return to the previous directory"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cd_into_nothing_reports_it_and_stays_put() {
        let dir = scratch("cdmiss");
        let mut st = state(&dir);
        let before = st.cwd.clone();

        let (_, err, out) = run_line(&mut st, "cd nowhere");
        assert_ne!(out.status, 0);
        assert!(err.contains("not a directory"), "{err}");
        assert_eq!(st.cwd, before, "a failed cd must not move the shell");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_canonicalised_path_does_not_keep_the_unc_prefix() {
        // `\\?\C:\...` is correct and unreadable, and children mostly do not
        // expect it.
        let dir = scratch("unc");
        let mut st = state(&dir);
        run_line(&mut st, "cd .");
        assert!(
            !st.cwd.display().to_string().starts_with(r"\\?\"),
            "{:?}",
            st.cwd
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ls_lists_directories_first() {
        let dir = scratch("ls");
        std::fs::create_dir_all(dir.join("zzz_dir")).unwrap();
        std::fs::write(dir.join("aaa.txt"), b"hello").unwrap();
        let mut st = state(&dir);

        let (out, err, outcome) = run_line(&mut st, "ls");
        assert_eq!(outcome.status, 0, "{err}");
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("zzz_dir"), "directories first: {out}");
        assert!(lines[1].contains("aaa.txt"));
        assert!(lines[1].contains("5 B"), "sizes shown: {out}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cat_prints_a_file_and_reports_a_missing_one() {
        let dir = scratch("cat");
        std::fs::write(dir.join("a.txt"), b"contents").unwrap();
        let mut st = state(&dir);

        let (out, _, outcome) = run_line(&mut st, "cat a.txt");
        assert_eq!(out, "contents");
        assert_eq!(outcome.status, 0);

        let (_, err, outcome) = run_line(&mut st, "cat missing.txt");
        assert_ne!(outcome.status, 0);
        assert!(err.contains("missing.txt"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cat_with_no_arguments_copies_what_it_is_given() {
        // Without this a builtin cannot sit on the right of a pipe, which is
        // most of what a builtin is for.
        let mut st = state(Path::new("."));
        let (out, _, outcome) = piped(&mut st, "cat", b"through the pipe");
        assert_eq!(out, "through the pipe");
        assert_eq!(outcome.status, 0);
    }

    #[test]
    fn echo_joins_its_arguments() {
        let mut st = state(Path::new("."));
        let (out, _, _) = run_line(&mut st, "echo one two three");
        assert_eq!(out, "one two three\n");
    }

    #[test]
    fn exit_asks_the_shell_to_stop_and_carries_its_code() {
        let mut st = state(Path::new("."));
        let (_, _, outcome) = run_line(&mut st, "exit");
        assert!(outcome.exit);
        assert_eq!(outcome.status, 0);

        let (_, _, outcome) = run_line(&mut st, "exit 3");
        assert!(outcome.exit);
        assert_eq!(outcome.status, 3);
    }

    #[test]
    fn set_remembers_a_variable_and_lists_them() {
        let mut st = state(Path::new("."));
        run_line(&mut st, "set GREETING=hello");
        assert_eq!(st.vars.get("GREETING").map(String::as_str), Some("hello"));

        let (out, _, _) = run_line(&mut st, "set");
        assert!(out.contains("GREETING=hello"), "{out}");
    }

    #[test]
    fn rm_refuses_a_directory_rather_than_deleting_a_tree() {
        // Removing a whole tree because a name happened to be a folder is not
        // a mistake worth allowing from a one-word command.
        let dir = scratch("rm");
        std::fs::create_dir_all(dir.join("keep")).unwrap();
        std::fs::write(dir.join("keep").join("inside.txt"), b"x").unwrap();
        let mut st = state(&dir);

        let (_, err, outcome) = run_line(&mut st, "rm keep");
        assert_ne!(outcome.status, 0);
        assert!(err.contains("is a directory"), "{err}");
        assert!(dir.join("keep").join("inside.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn touch_creates_a_file_and_leaves_an_existing_one_alone() {
        let dir = scratch("touch");
        std::fs::write(dir.join("has.txt"), b"keep me").unwrap();
        let mut st = state(&dir);

        run_line(&mut st, "touch new.txt has.txt");
        assert!(dir.join("new.txt").exists());
        assert_eq!(std::fs::read(dir.join("has.txt")).unwrap(), b"keep me");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_emits_the_sequence_a_terminal_understands() {
        let mut st = state(Path::new("."));
        let (out, _, _) = run_line(&mut st, "clear");
        assert!(out.contains("\x1b[2J"), "should erase the screen");
        assert!(out.contains("\x1b[H"), "and put the cursor home");
    }

    #[test]
    fn which_knows_its_own_builtins() {
        let mut st = state(Path::new("."));
        let (out, _, outcome) = run_line(&mut st, "which cd");
        assert_eq!(outcome.status, 0);
        assert!(out.contains("builtin"), "{out}");
    }

    #[test]
    fn something_that_is_not_a_builtin_is_left_alone() {
        let mut st = state(Path::new("."));
        let env = map_env(std::collections::HashMap::new());
        let pipeline = parse::parse("cargo build", &env).unwrap();
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut i = std::io::empty();
        assert!(run(&mut st, &pipeline.commands[0], &mut i, &mut o, &mut e).is_none());
    }
}
