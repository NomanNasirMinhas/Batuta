//! An embedded terminal: a ConPTY, a VT interpreter, and a screen.
//!
//! Built bottom-up, and the layering matters: the pipe cannot be tested
//! without the emulator, because ConPTY opens by asking a question and waits
//! for the answer. `grid.rs` is the part that answers.

pub mod grid;
pub mod keys;
pub mod pty;
pub mod session;
