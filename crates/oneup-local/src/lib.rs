//! OneUpDrive's **Local tree**: a read-only scan of a local folder into the
//! state database.
//!
//! * [`classify`]: pure rules deciding what each directory entry is and
//!   whether its contents are listed (exclusions, links, placeholders).
//! * [`coexist`]: detects whether a folder belongs to another sync engine
//!   (the official OneDrive client, or any Cloud Files provider).
//! * [`scan`] (Windows only): the resumable scanner itself.
//! * [`observe`]: keeps a finished scan current from the change journal and
//!   records deletion evidence (the Windows driver plus a pure core).
//! * [`verify`] (Windows only): opt-in hashing of scanned files, to compare
//!   them with the cloud. Reads ordinary files only, never cloud placeholders.
//!
//! **Read-only by construction.** Directories are opened with
//! `FILE_LIST_DIRECTORY` only and listed with `FileIdExtdDirectoryInfo`, which
//! returns names, sizes, timestamps, attributes and file IDs without opening a
//! single file. The scan never reads file contents, so cloud-only files are
//! never downloaded, and folders that a cloud provider hasn't populated yet are
//! not listed at all (listing them would make the provider fetch their
//! children). [`verify`] reads contents only when asked, and only of files
//! that aren't placeholders.

pub mod classify;
pub mod coexist;
pub mod observe;
#[cfg(windows)]
pub mod scan;
#[cfg(windows)]
pub mod verify;

#[cfg(windows)]
pub use scan::{Config, Error, Progress, Replaced, Scan, Start, Summary};
