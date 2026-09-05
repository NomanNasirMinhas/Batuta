//! Running a parsed pipeline.
//!
//! ## Inherit when you can, pipe when you must
//!
//! A command with no pipe and no redirection is spawned with **inherited**
//! stdio, so it gets the pseudo-console the shell itself is running in. That
//! is what makes `vim`, `less` and anything else that redraws the screen work
//! at all — a piped child sees a pipe, decides it is not interactive, and
//! behaves completely differently.
//!
//! Pipes and files are only introduced where the line actually asks for them.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command as Proc, Stdio};

use filedescriptor::{FileDescriptor, Pipe};

use super::builtins::{self, State};
use super::parse::{Command, Pipeline, Redirect};

/// Where a stage's output should go.
enum Sink {
    /// The console the shell is attached to.
    Inherit,
    File(std::fs::File),
    Pipe(FileDescriptor),
}

impl Sink {
    fn stdio(self) -> anyhow::Result<Stdio> {
        Ok(match self {
            Sink::Inherit => Stdio::inherit(),
            Sink::File(f) => Stdio::from(f),
            Sink::Pipe(fd) => fd.as_stdio()?,
        })
    }

    /// A reader for a builtin, which is handed bytes rather than a handle.
    fn reader(self) -> Box<dyn Read> {
        match self {
            Sink::Inherit => Box::new(std::io::stdin()),
            Sink::File(f) => Box::new(f),
            Sink::Pipe(fd) => Box::new(fd),
        }
    }

    /// A writer for a builtin, which produces text rather than a process.
    fn writer(self) -> Box<dyn Write> {
        match self {
            Sink::Inherit => Box::new(std::io::stdout()),
            Sink::File(f) => Box::new(f),
            Sink::Pipe(fd) => Box::new(fd),
        }
    }

    fn try_clone(&self) -> Option<Sink> {
        match self {
            Sink::Inherit => Some(Sink::Inherit),
            Sink::File(f) => f.try_clone().ok().map(Sink::File),
            Sink::Pipe(fd) => fd.try_clone().ok().map(Sink::Pipe),
        }
    }
}

fn open_out(path: &std::path::Path, append: bool) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
}

/// Find an executable, the way a shell does.
///
/// Exported because `which` answers the same question, and two
/// implementations of "where would this run from" would eventually disagree.
pub fn find_program(state: &State, name: &str) -> Option<PathBuf> {
    let has_sep = name.contains('\\') || name.contains('/');
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|e| e.to_ascii_lowercase())
        .collect();

    let mut roots: Vec<PathBuf> = Vec::new();
    if has_sep {
        // A path, relative to where the shell is rather than where the
        // process happens to be.
        roots.push(state.cwd.join(name));
    } else {
        // Windows looks in the current directory first, and people rely on
        // that for `.\build.bat`-style scripts sitting in the folder.
        roots.push(state.cwd.join(name));
        if let Ok(path) = std::env::var("PATH") {
            for dir in path.split(';').filter(|d| !d.is_empty()) {
                roots.push(PathBuf::from(dir).join(name));
            }
        }
    }

    for root in roots {
        if root.is_file() {
            return Some(root);
        }
        for ext in &exts {
            let candidate = PathBuf::from(format!("{}{ext}", root.display()));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Work out this stage's three streams.
///
/// `2>&1` is handled by cloning whatever stdout resolved to, which is why it
/// has to be decided after the redirections rather than alongside them.
fn streams(
    cmd: &Command,
    state: &State,
    stdin: Option<Sink>,
    stdout: Option<Sink>,
) -> anyhow::Result<(Sink, Sink, Sink)> {
    let mut input = stdin.unwrap_or(Sink::Inherit);
    let mut output = stdout.unwrap_or(Sink::Inherit);
    let mut errors = Sink::Inherit;
    let mut err_to_out = false;

    for redirect in &cmd.redirects {
        match redirect {
            Redirect::In { path } => {
                let full = state.cwd.join(path);
                input = Sink::File(
                    std::fs::File::open(&full)
                        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", full.display()))?,
                );
            }
            Redirect::Out { path, append } => {
                let full = state.cwd.join(path);
                output = Sink::File(
                    open_out(&full, *append)
                        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", full.display()))?,
                );
            }
            Redirect::Err { path, append } => {
                let full = state.cwd.join(path);
                errors = Sink::File(
                    open_out(&full, *append)
                        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", full.display()))?,
                );
            }
            Redirect::ErrToOut => err_to_out = true,
        }
    }

    if err_to_out {
        // Duplicated rather than reopened: a second handle to the same file
        // would have its own write position and the two streams would
        // overwrite each other.
        errors = output
            .try_clone()
            .ok_or_else(|| anyhow::anyhow!("cannot duplicate the output stream for 2>&1"))?;
    }

    Ok((input, output, errors))
}

/// Run a pipeline. Returns the exit status of the last stage.
pub fn run(state: &mut State, pipeline: &Pipeline) -> anyhow::Result<builtins::Outcome> {
    if pipeline.commands.is_empty() {
        return Ok(builtins::Outcome {
            status: 0,
            exit: false,
        });
    }

    let mut children: Vec<std::process::Child> = Vec::new();
    let mut carried: Option<Sink> = None;
    let mut last = builtins::Outcome {
        status: 0,
        exit: false,
    };

    for (i, cmd) in pipeline.commands.iter().enumerate() {
        let is_last = i + 1 == pipeline.commands.len();

        // Every stage but the last hands its output to the next one.
        let (stage_out, next_in) = if is_last {
            (None, None)
        } else {
            let pipe = Pipe::new()?;
            (Some(Sink::Pipe(pipe.write)), Some(Sink::Pipe(pipe.read)))
        };

        let (input, output, errors) = streams(cmd, state, carried.take(), stage_out)?;
        carried = next_in;

        let Some(name) = cmd.program() else { continue };

        // Builtins run here rather than being spawned, because half of them
        // exist precisely to change this process's own state. The branch owns
        // the streams outright: nothing falls through to the spawn path, so
        // there is no question of who consumed them.
        if builtins::is_builtin(name) {
            let mut inp = input.reader();
            let mut out = output.writer();
            let mut err = errors.writer();
            match builtins::run(state, cmd, &mut inp, &mut out, &mut err) {
                Some(outcome) => {
                    let _ = out.flush();
                    last = outcome;
                    if last.exit {
                        return Ok(last);
                    }
                }
                None => {
                    // `is_builtin` and `run` disagreeing would be a bug here,
                    // not the user's problem, and not worth ending the
                    // process over when `panic = "abort"` means exactly that.
                    let _ = writeln!(err, "{name}: not runnable as a builtin");
                    last = builtins::Outcome {
                        status: 1,
                        exit: false,
                    };
                }
            }
            continue;
        }

        let Some(program) = find_program(state, name) else {
            anyhow::bail!("not found: {name}");
        };

        let mut proc = Proc::new(program);
        proc.args(cmd.args())
            .current_dir(&state.cwd)
            .stdin(input.stdio()?)
            .stdout(output.stdio()?)
            .stderr(errors.stdio()?);
        for (k, v) in &state.vars {
            proc.env(k, v);
        }

        match proc.spawn() {
            Ok(child) => children.push(child),
            Err(e) => anyhow::bail!("could not run {name}: {e}"),
        }
    }

    // Waited in order, and only after every stage exists: waiting on the
    // first before the second is spawned deadlocks as soon as the first
    // fills the pipe.
    for mut child in children {
        if let Ok(status) = child.wait() {
            last.status = status.code().unwrap_or(-1);
        }
    }

    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::parse::{self, map_env};
    use std::collections::HashMap;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batuta-run-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(cwd: &std::path::Path) -> State {
        State {
            cwd: cwd.to_path_buf(),
            previous: None,
            vars: HashMap::new(),
        }
    }

    fn exec(st: &mut State, line: &str) -> anyhow::Result<builtins::Outcome> {
        let env = map_env(HashMap::new());
        let pipeline = parse::parse(line, &env)?;
        run(st, &pipeline)
    }

    #[test]
    fn a_builtin_writing_to_a_file_actually_lands_there() {
        let dir = scratch("redirect");
        let mut st = state(&dir);

        exec(&mut st, "echo hello > out.txt").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).unwrap().trim(),
            "hello"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn appending_adds_and_truncating_replaces() {
        // Getting these the wrong way round destroys a log.
        let dir = scratch("append");
        let mut st = state(&dir);

        exec(&mut st, "echo one > log.txt").unwrap();
        exec(&mut st, "echo two >> log.txt").unwrap();
        let text = std::fs::read_to_string(dir.join("log.txt")).unwrap();
        assert!(text.contains("one") && text.contains("two"), "{text:?}");

        exec(&mut st, "echo three > log.txt").unwrap();
        let text = std::fs::read_to_string(dir.join("log.txt")).unwrap();
        assert!(!text.contains("one"), "truncating should replace: {text:?}");
        assert!(text.contains("three"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pipeline_carries_output_from_one_stage_to_the_next() {
        // Two builtins, so this exercises the pipe rather than a program.
        let dir = scratch("pipe");
        std::fs::write(dir.join("data.txt"), b"alpha\nbeta\n").unwrap();
        let mut st = state(&dir);

        exec(&mut st, "cat data.txt | cat > copy.txt").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("copy.txt")).unwrap(),
            "alpha\nbeta\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn input_can_come_from_a_file() {
        let dir = scratch("stdin");
        std::fs::write(dir.join("in.txt"), b"from a file\n").unwrap();
        let mut st = state(&dir);

        exec(&mut st, "cat < in.txt > out.txt").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("out.txt")).unwrap(),
            "from a file\n",
            "stdin should have been redirected from the file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_program_is_reported_by_name() {
        let dir = scratch("missing");
        let mut st = state(&dir);
        let e = exec(&mut st, "definitely-not-a-real-program").unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_redirect_target_is_reported_rather_than_ignored() {
        let dir = scratch("badin");
        let mut st = state(&dir);
        let e = exec(&mut st, "cat < nope.txt").unwrap_err();
        assert!(e.to_string().contains("cannot read"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_program_runs_and_its_output_can_be_captured() {
        let dir = scratch("real");
        let mut st = state(&dir);

        // `cmd` is on every Windows machine and exits immediately.
        exec(&mut st, "cmd /c echo from-a-program > out.txt").unwrap();
        let text = std::fs::read_to_string(dir.join("out.txt")).unwrap();
        assert!(text.contains("from-a-program"), "{text:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stderr_can_be_folded_into_stdout() {
        let dir = scratch("errout");
        let mut st = state(&dir);

        // Writing to a closed handle makes `cmd` complain on stderr.
        exec(&mut st, "cmd /c dir nonexistent-path-here > both.txt 2>&1").unwrap();
        let text = std::fs::read_to_string(dir.join("both.txt")).unwrap();
        assert!(
            !text.trim().is_empty(),
            "the error should have reached the file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_programs_exit_code_comes_back() {
        let dir = scratch("status");
        let mut st = state(&dir);
        let outcome = exec(&mut st, "cmd /c exit 3").unwrap();
        assert_eq!(outcome.status, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finding_a_program_prefers_the_current_directory() {
        let dir = scratch("which");
        std::fs::write(dir.join("local-tool.bat"), b"@echo off\n").unwrap();
        let st = state(&dir);

        let found = find_program(&st, "local-tool").expect("should find it by extension");
        assert!(found.ends_with("local-tool.bat"), "{found:?}");
        assert!(find_program(&st, "cmd").is_some(), "and still find PATH");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_pipeline_does_nothing_quietly() {
        let mut st = state(&std::env::temp_dir());
        let outcome = exec(&mut st, "   ").unwrap();
        assert_eq!(outcome.status, 0);
        assert!(!outcome.exit);
    }
}
