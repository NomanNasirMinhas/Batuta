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

/// The program to run, and its arguments.
///
/// Batuta's own shell by default: `batuta shell` is a hidden subcommand that
/// is a full command interpreter. Running it *inside* the pseudo-console
/// rather than in-process is what keeps interactive programs working —
/// anything it launches inherits a real console, so `vim` behaves as `vim`
/// should.
///
/// Returned split rather than as one string because the program and its
/// arguments are separate things to `CommandBuilder`; joining them would make
/// it look for a file whose name contains a space and an argument.
///
/// `BATUTA_SHELL` overrides, for anyone who would rather have PowerShell.
pub fn shell_command() -> (String, Vec<String>) {
    if let Ok(custom) = std::env::var("BATUTA_SHELL") {
        let mut parts = custom.split_whitespace().map(str::to_string);
        if let Some(program) = parts.next() {
            return (program, parts.collect());
        }
    }
    match std::env::current_exe() {
        Ok(exe) => (exe.display().to_string(), vec!["shell".to_string()]),
        Err(_) => ("powershell.exe".to_string(), Vec::new()),
    }
}

impl Pty {
    /// Start a named program in `cwd`, sized `cols` by `rows`.
    ///
    /// Taking the program rather than always resolving it is what lets the
    /// tests drive a known, fast one: under `cargo test` the running
    /// executable is the test binary, so the default would try to run the
    /// harness as a shell.
    pub fn spawn_command(
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Pty> {
        let pair = NativePtySystem::default().openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
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

    fn probe_pty() -> Pty {
        // `cmd` rather than the default: under `cargo test` the running
        // executable is the harness, not `batuta.exe`.
        Pty::spawn_command("cmd.exe", &[], &std::env::temp_dir(), 80, 24).expect("spawn a shell")
    }

    #[test]
    fn the_shell_defaults_to_our_own_and_stays_overridable() {
        // Not asserting on the variable itself: tests share a process, and
        // setting one would leak into whatever runs alongside.
        let (program, args) = shell_command();
        assert!(
            args == ["shell"] || std::env::var("BATUTA_SHELL").is_ok(),
            "should run Batuta's own shell by default, got {program:?} {args:?}"
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
        let mut pty = probe_pty();
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
        let pty = probe_pty();
        pty.resize(120, 40);
        // Zero is what a terminal dragged shut reports for a frame, and it
        // must not take the process down.
        pty.resize(0, 0);
    }
}
