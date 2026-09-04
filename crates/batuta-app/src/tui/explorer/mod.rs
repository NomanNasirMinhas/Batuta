//! The directory explorer: a tree, a path bar, and a text editor.
//!
//! Built bottom-up: the document and the file layer first, because they are
//! the parts that can destroy someone's work and they are testable without a
//! terminal. The panes that use them come next.

// The pieces below are complete and tested but not yet reachable from the UI,
// so every public item reads as dead until the panes land. This allow comes
// off in the same change that wires `Mode::Explore` into `handle_key`.
#![allow(dead_code)]

pub mod buffer;
pub mod file;
