//! NTFS structures and Windows volume access for Batuta.
//!
//! The crate is split so that everything which merely *parses bytes* is
//! platform independent and unit-testable, while the parts that need a raw
//! volume handle (and therefore Administrator) are isolated behind `cfg(windows)`.
//!
//! Parsing layers, bottom up:
//!
//! - [`boot`]     — the volume geometry in the boot sector
//! - [`runlist`]  — data runs, the cluster map of a non-resident attribute
//! - [`record`]   — MFT file records: fixups, headers, attributes
//! - [`mft`]      — turning a stream of records into flat [`mft::MftEntry`] rows

pub mod boot;
pub mod error;
pub mod mft;
pub mod record;
pub mod runlist;
pub mod usn;

#[cfg(windows)]
pub mod volume;

#[cfg(any(test, feature = "testutil"))]
pub mod testutil;

pub use boot::BootSector;
pub use error::{FileRef, NtfsError, Result};
pub use mft::{EntryBatch, EntryInfo, MftParser, Parsed};

#[cfg(windows)]
pub use volume::{is_elevated, MftScanner, ScanStats, Volume};
