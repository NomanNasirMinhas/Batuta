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
        Ok(Session {
            pty: Pty::spawn(cwd, cols, rows)?,
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
        let mut s = Session::open(&std::env::temp_dir(), 80, 24).expect("open a shell");

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

    #[test]
    fn resizing_reaches_both_the_screen_and_the_shell() {
        let mut s = Session::open(&std::env::temp_dir(), 80, 24).expect("open");
        s.resize(100, 30);
        assert_eq!(s.grid.size(), (100, 30));
    }
}
