//! Makes the stamped release version participate in cargo's freshness check.
//!
//! `src/update.rs` reads `BATUTA_VERSION` through `option_env!`, which is
//! resolved when the crate is compiled and leaves no trace cargo knows about.
//! Without the line below, a cached build would keep whatever version was
//! stamped into it the first time — so re-running a release, or publishing two
//! versions from the same commit, would ship a binary claiming the wrong one
//! and unable to tell it was out of date.

fn main() {
    println!("cargo:rerun-if-env-changed=BATUTA_VERSION");
}
