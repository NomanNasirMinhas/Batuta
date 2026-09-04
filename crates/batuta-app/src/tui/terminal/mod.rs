//! An embedded terminal: a ConPTY, a VT interpreter, and a screen.
//!
//! Built bottom-up, and the layering matters: the pipe cannot be tested
//! without the emulator, because ConPTY opens by asking a question and waits
//! for the answer. `grid.rs` is the part that answers.

// Complete and tested, but not yet reachable from the UI, so every public item
// reads as dead until the pane lands. This comes off in the change that wires
// `Mode::Terminal` into `handle_key`.
#![allow(dead_code)]

pub mod grid;
#[cfg(windows)]
pub mod pty;
#[cfg(windows)]
pub mod session;
