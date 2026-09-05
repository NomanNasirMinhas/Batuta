//! A live terminal: a shell, its output parsed into a screen, and the answers
//! sent back to it.
//!
//! The loop is deliberately one-way-at-a-time and never blocks. Output is
//! drained from the reader thread, fed through the parser, and whatever the
//! program asked for is written straight back — because a terminal that reads
//! but never answers stalls the program on its first question, which is
//! exactly how this looked when it was broken.

use std::path::{Path, PathBuf};

use super::grid::Grid;
use super::pty::{Chunk, Pty};

pub struct Session {
    pty: Pty,
    parser: vte::Parser,
    pub grid: Grid,
    /// The shell has exited. The pane says so rather than looking frozen.
    pub ended: bool,
    pub cwd: PathBuf,
}

impl Session {
    pub fn open(cwd: &Path, cols: u16, rows: u16) -> anyhow::Result<Session> {
        let (program, args) = super::pty::shell_command();
        Session::open_command(&program, &args, cwd, cols, rows)
    }

    /// Open a session running a named program, for tests that need a known
    /// one rather than whatever the default resolves to.
    pub fn open_command(
        program: &str,
        args: &[String],
        cwd: &Path,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Session> {
        Ok(Session {
            pty: Pty::spawn_command(program, args, cwd, cols, rows)?,
            parser: vte::Parser::new(),
            grid: Grid::new(cols as usize, rows as usize),
            ended: false,
            cwd: cwd.to_path_buf(),
        })
    }

    /// Take everything the shell has produced since last time.
    ///
    /// Never blocks: the UI thread calls this every frame, and waiting on a
    /// shell that is thinking would freeze the whole interface.
    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.pty.output.try_recv() {
                Ok(Chunk::Output(bytes)) => {
                    self.parser.advance(&mut self.grid, &bytes);
                    changed = true;
                }
                Ok(Chunk::Ended) => {
                    self.ended = true;
                    changed = true;
                    break;
                }
                Err(_) => break,
            }
        }

        // Answer whatever was asked. This has to happen even when the caller
        // ignores `changed`, or the shell waits forever on a question we have
        // already parsed.
        let replies = self.grid.take_replies();
        if !replies.is_empty() {
            let _ = self.pty.write(&replies);
        }
        changed
    }

    pub fn send(&mut self, bytes: &[u8]) {
        // Typing anywhere brings the view back to the live screen: output
        // arriving where the user is not looking is worse than losing their
        // scroll position.
        self.grid.follow();
        let _ = self.pty.write(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols as usize, rows as usize);
        self.pty.resize(cols, rows);
    }

    /// The title the shell set, which is usually its working directory.
    pub fn title(&self) -> Option<&str> {
        self.grid.title.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End to end, against a real shell: spawn, parse, answer, and read back
    /// what it printed.
    ///
    /// This is the test that catches the failure worth catching. Everything
    /// below it can be individually correct while the terminal still shows an
    /// empty screen forever, because the shell is blocked waiting for an
    /// answer nobody sent.
    #[test]
    fn a_real_shell_prints_something_we_can_read() {
        let mut s = Session::open_command("cmd.exe", &[], &std::env::temp_dir(), 80, 24)
            .expect("open a shell");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        let mut sent_at: Option<std::time::Instant> = None;

        while std::time::Instant::now() < deadline {
            s.pump();

            let screen: String = s
                .grid
                .visible()
                .iter()
                .flat_map(|line| line.iter().map(|c| c.ch))
                .collect();

            // Wait for the shell to have drawn *something* before typing: one
            // still starting up discards its input. Deliberately not looking
            // for a ">" - a customised prompt may well end in any character,
            // and this one ends in U+276F.
            if sent_at.is_none() && screen.trim().len() > 20 {
                std::thread::sleep(std::time::Duration::from_millis(400));
                s.send(b"echo batuta-session-probe\r\n");
                sent_at = Some(std::time::Instant::now());
            }

            // Twice: once echoed as it is typed, once as the result. One
            // occurrence would only prove the shell reflected the keystrokes.
            if sent_at.is_some() && screen.matches("batuta-session-probe").count() >= 2 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let screen: String = s
            .grid
            .visible()
            .iter()
            .map(|line| line.iter().map(|c| c.ch).collect::<String>())
            .collect::<Vec<_>>()
            .join("|");
        panic!("the shell never produced readable output. screen: {screen:?}");
    }

    /// The whole stack at once: our shell, running under our pseudo-console,
    /// read back through our VT interpreter.
    ///
    /// The three pieces are tested apart elsewhere; this is the only test that
    /// says they fit together. It needs the release binary, because that is
    /// what `shell_command` launches — so it skips rather than fails when
    /// nobody has built one, which is the case on a bare `cargo test`.
    #[test]
    fn our_own_shell_runs_inside_our_own_terminal() {
        let exe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/release/batuta.exe");
        let Ok(exe) = exe.canonicalize() else {
            eprintln!("skipping: no release binary to run as the shell");
            return;
        };

        let dir = std::env::temp_dir().join(format!("batuta-own-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"hello from a file\n").unwrap();

        let mut s = Session::open_command(
            &exe.display().to_string(),
            &["shell".to_string()],
            &dir,
            100,
            20,
        )
        .expect("open our shell");

        // A builtin, a listing, a file read, and a pipe between two builtins.
        // The pipe is the one that matters: it is the piece that a shell made
        // only of `Command::spawn` cannot do.
        let script = [
            "echo hello world\r",
            "ls\r",
            "cat notes.txt\r",
            "echo piped | cat\r",
        ];

        let mut sent = 0usize;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        let screen = loop {
            s.pump();
            let screen: String = s
                .grid
                .visible()
                .iter()
                .flat_map(|l| l.iter().map(|c| c.ch))
                .collect();

            if sent == script.len() && screen.contains("piped") {
                std::thread::sleep(std::time::Duration::from_millis(500));
                s.pump();
                break s
                    .grid
                    .visible()
                    .iter()
                    .map(|l| l.iter().map(|c| c.ch).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            if std::time::Instant::now() > deadline {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("the shell never got through the script. screen:\n{screen}");
            }
            // Wait for the banner before typing: a shell still starting up has
            // not put the console in raw mode yet and drops what it is sent.
            if sent < script.len() && screen.trim().len() > 10 {
                std::thread::sleep(std::time::Duration::from_millis(350));
                s.send(script[sent].as_bytes());
                sent += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        };
        let _ = std::fs::remove_dir_all(&dir);

        // The prompt is ours, not a system shell's.
        assert!(
            screen.contains("batuta shell"),
            "no banner from our shell:\n{screen}"
        );
        assert!(
            screen.contains("hello world"),
            "`echo` printed nothing:\n{screen}"
        );
        // `ls` sorts directories first and marks them; both come from our
        // builtin rather than from anything on the system.
        assert!(
            screen.contains("subdir") && screen.contains("notes.txt"),
            "`ls` did not list the directory:\n{screen}"
        );
        assert!(
            screen.contains("hello from a file"),
            "`cat` did not read the file:\n{screen}"
        );
        // Twice: once echoed as it was typed, once as what came out of `cat`.
        assert!(
            screen.matches("piped").count() >= 2,
            "the pipe carried nothing:\n{screen}"
        );
    }

    #[test]
    fn resizing_reaches_both_the_screen_and_the_shell() {
        let mut s =
            Session::open_command("cmd.exe", &[], &std::env::temp_dir(), 80, 24).expect("open");
        s.resize(100, 30);
        assert_eq!(s.grid.size(), (100, 30));
    }
}
