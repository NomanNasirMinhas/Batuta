//! The platform-independent heart of Batuta: the index, its search engine,
//! folder-size rollups and duplicate detection.
//!
//! Nothing here touches the Windows API, so all of it is testable on synthetic
//! data without a raw volume or elevation.

pub mod dupes;
pub mod index;
pub mod names;
pub mod search;
pub mod snapshot;
pub mod watch;

#[cfg(test)]
mod tests;
#[cfg(any(test, feature = "testtree"))]
pub mod testtree;

pub use dupes::{find_duplicates, ContentSource, DupeGroup, DupeOptions};
pub use index::{Index, IndexBuilder, VolumeInfo, NO_NODE};
pub use names::NameArena;
pub use snapshot::SnapshotError;
pub use watch::{Applied, Change, WatchStats};
