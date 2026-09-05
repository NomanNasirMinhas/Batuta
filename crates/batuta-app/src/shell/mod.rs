//! Batuta's own shell: a command interpreter, not a terminal.
//!
//! The terminal emulator lives in `tui::terminal`. This is the program that
//! runs inside it: it parses what you type, handles its own builtins, and
//! launches everything else.

pub mod builtins;
pub mod line;
pub mod parse;
pub mod repl;
pub mod run;
