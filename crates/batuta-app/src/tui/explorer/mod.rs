//! The directory explorer: a tree, a path bar, and a text editor.
//!
//! Built bottom-up: the document and the file layer first, because they are
//! the parts that can destroy someone's work and they are testable without a
//! terminal. The panes that use them come next.

pub mod buffer;
pub mod file;
pub mod find;
pub mod state;
pub mod tree;
