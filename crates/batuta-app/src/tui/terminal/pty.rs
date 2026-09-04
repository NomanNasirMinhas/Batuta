//! A pseudo-console and the shell running inside it.
//!
//! ConPTY is the Windows API for this: it gives the child a real console to
//! talk to, turns what the child does into a VT byte stream, and lets that
//! stream be read from a pipe. Without it a program asking "how big is my
//! console" gets no answer, and nothing full-screen works at all.
//!
//! ## Why this is not hand-rolled
//!
//! It was, first. `CreatePseudoConsole` succeeded, the attribute list sized
//! and initialised correctly, `UpdateProcThreadAttribute` reported success
//! with a valid `HPCON`, `STARTUPINFOEXW.cb` matched `sizeof`, and
//! `CreateProcessW` returned 1 — and the child still attached to the
//! *parent's* console rather than the pseudo-console, so its output never
//! reached the pipe. Every documented precondition was satisfied and the
//! failure was silent.
//!
//! `portable-pty` is wezterm's, it gets this sequence right, and a terminal
//! that does not reliably attach is not worth the lines saved. Same reasoning
//! as using `vte` for the parser rather than writing the state machine.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, NativePtySystem, PtyPair, PtySize, PtySystem};

/// What the reader thread reports.
pub enum Chunk {
    Output(Vec<u8>),
    /// The stream closed, which means the shell exited.
    Ended,
}

pub struct Pty {
    pair: PtyPair,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pub output: Receiver<Chunk>,
}

/// The shell to run.
///
/// `BATUTA_SHELL` wins, otherwise PowerShell. "Open a terminal here" should
/// give you the shell you actually use, and on Windows 10 and 11 that is
/// overwhelmingly PowerShell rather than `cmd`.
fn shell_command() -> String {
    std::env::var("BATUTA_SHELL").unwrap_or_else(|_| "powershell.exe".to_string())
}

impl Pty {
    /// Start a shell in `cwd`, sized `cols` by `rows`.
    pub fn spawn(cwd: &std::path::Path, cols: u16, rows: u16) -> anyhow::Result<Pty> {
        let pair = NativePtySystem::default().openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(shell_command());
        cmd.cwd(cwd);
        let child = pair.slave.spawn_command(cmd)?;

        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Chunk::Output(buf[..n].to_vec())).is_err() {
                            // The UI has gone; nothing left to report to.
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(Chunk::Ended);
        });

        Ok(Pty {
            pair,
            writer,
            child,
            output: rx,
        })
    }

    /// Send bytes to the shell's input.
    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut w = self
            .writer
            .lock()
            .map_err(|_| std::io::Error::other("pty writer poisoned"))?;
        w.write_all(bytes)?;
        w.flush()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.pair.master.resize(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

/// Only the startup test asks this: everything else learns the shell has gone
/// from the output stream closing, which is the same fact arriving earlier.
#[cfg(test)]
impl Pty {
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // A shell sitting at a prompt will not notice its input closing, so it
        // is ended rather than left orphaned holding a working directory open.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_is_overridable_and_defaults_to_powershell() {
        // Not asserting on the variable itself: tests share a process, and
        // setting one would leak into whatever runs alongside.
        assert!(
            shell_command().contains("powershell") || std::env::var("BATUTA_SHELL").is_ok(),
            "the default should be the shell people actually use"
        );
    }

    #[test]
    fn a_shell_starts_and_the_pseudo_console_talks_to_us() {
        // What this layer can prove on its own, and no more.
        //
        // It deliberately does *not* check that a typed command echoes back.
        // ConPTY opens by asking the terminal where the cursor is and then
        // waits for the answer before emitting anything further, and answering
        // is the emulator's job, not the pipe's. A shell will therefore never
        // reach a prompt with only this layer running, and a test that
        // demanded one would be asserting something impossible by design.
        // `session.rs` carries that end-to-end test, with a parser attached.
        let mut pty = Pty::spawn(&std::env::temp_dir(), 80, 24).expect("spawn a shell");
        assert!(pty.alive(), "the shell should still be running");

        let first = pty
            .output
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the pseudo-console should say something");

        match first {
            Chunk::Output(bytes) => assert!(!bytes.is_empty()),
            Chunk::Ended => panic!("the shell exited immediately"),
        }
    }

    #[test]
    fn resizing_a_live_pty_does_not_fail() {
        let pty = Pty::spawn(&std::env::temp_dir(), 80, 24).expect("spawn");
        pty.resize(120, 40);
        // Zero is what a terminal dragged shut reports for a frame, and it
        // must not take the process down.
        pty.resize(0, 0);
    }
}
