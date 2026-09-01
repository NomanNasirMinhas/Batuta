//! Quick Setup: one guided run that leaves behind a working installation.
//!
//! The decision logic is deliberately separated from anything that touches the
//! machine. [`flow`] asks the questions and returns a [`flow::SetupPlan`];
//! `apply` consumes one. The questions reach the user through [`prompt::Ask`],
//! implemented twice: by the console line protocol in [`prompt`], and by the
//! ratatui dialogs in [`wizard`]. That split is what makes the branching, the
//! validation and the generated command lines testable without a console,
//! without elevation, and without changing anything.

pub mod acl;
pub mod apply;
pub mod drives;
pub mod flow;
pub mod pathenv;
pub mod procs;
pub mod prompt;
pub mod task;
pub mod wizard;
