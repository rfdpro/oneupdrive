//! The effects layer: the only code that changes user content.
//!
//! Everything here takes an [`oneup_gate::Approved`] token, which only the
//! safety gate can issue, so no change to user content can bypass the gate
//! (AGENTS.md, prime directive 2). Each effect re-checks its preconditions
//! on the very handle it then acts through (`data-safety.md` invariant 5):
//!
//! * the item is opened **by file ID**, exclusively for files, so nobody can
//!   write, rename or delete it between the check and the change;
//! * its size and last-write time must still be the planned stamp, and it
//!   must be an ordinary file or folder (never a link, another provider's
//!   placeholder or other reparse point; in Files On-Demand mode, our own
//!   placeholders count as ordinary, see `ondemand.rs`);
//! * it, and the destination, must lie inside the sync folder and outside
//!   OneUpDrive's own folder;
//! * names are created **only if free**: a rename never replaces anything,
//!   and a download replaces a file only by first moving the old one into
//!   the local version store, putting it back if placing the new one fails.
//!
//! "Recycling" moves the item into the version store under
//! `<sync folder>\.oneup\versions` (same volume, so it's a rename: instant,
//! atomic, and restorable by moving it back). Content is removed for good
//! in two cases only:
//! * trimming that store, by the rule in [`versions`];
//! * an old copy the gate marked [`oneup_gate::OldCopy::DropIfSynced`] (a
//!   file replaced by a newer cloud version, or a batch the user chose to
//!   discard), and then only if, read under the exclusive handle, it hashes
//!   to the last synced version, which OneDrive has. It is deleted through
//!   that same handle. Anything else is kept (`data-safety.md` invariant 1).
//!
//! Two effects aren't operations of a round: [`release::release_root`]
//! finishes another program's stalled unlinking, for the gate's
//! [`oneup_gate::release::Release`] token, and [`sharing::apply`] removes a
//! sharing link or sets its expiry in OneDrive, for the gate's
//! [`oneup_gate::sharing::PermissionChange`] token (who can open an item,
//! never its content).
//!
//! A failed check is [`Error::Changed`], which the executor treats as "skip;
//! plan again from fresh state next round".
//!
//! Effects of more than one step (keeping an item in the version store,
//! replacing a file by a download) have crash points between their steps
//! under the test-only `crash-test` feature (`LocalFs::crash_after`), so
//! the executor's crash tests also stop them half done.

pub mod name;
pub mod sharing;
pub mod versions;

#[cfg(windows)]
mod database;
#[cfg(windows)]
mod local;
#[cfg(windows)]
mod ondemand;
#[cfg(windows)]
pub mod release;

#[cfg(windows)]
pub use database::{DbState, UploadCheck, Wait, staged_format};
#[cfg(all(windows, feature = "crash-test"))]
pub use local::Crashed;
#[cfg(windows)]
pub use local::{ChangedMeanwhile, Done, Error, LocalFs, Locator, Staged, UploadSource, Witnessed};
#[cfg(windows)]
pub use ondemand::{EXCLUDED_MARK, Version};
pub use oneup_ntfs::dirent::DirEntry;
pub use versions::{Slot, Trim};
