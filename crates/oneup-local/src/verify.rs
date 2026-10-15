//! Content checks: hashing a scanned local file so it can be compared with
//! the QuickXorHash OneDrive stores for the cloud copy.
//!
//! Used only on request (`oneup plan --verify`), because reading every file
//! of a large tree costs real disk time. It is read-only like the rest of this
//! crate, with one extra rule that the scanner never needed:
//!
//! **Cloud placeholders are never read, hydrated or not.** A read of a
//! dehydrated placeholder makes its provider download the file, and no open
//! flag prevents that (`FILE_FLAG_OPEN_NO_RECALL` was tested and doesn't). A hydrated placeholder can be dehydrated by its
//! provider at any moment, so "it's on disk now" isn't proof either. So:
//! * nothing at all is read in a folder another sync app manages;
//! * elsewhere, placeholders are first *exposed* (Windows otherwise may
//!   disguise them as ordinary files, even on open handles: measured),
//!   and only ordinary files (no reparse point at all) are read,
//!   re-checked on the open handle, not just trusted from the scan.
//!
//! Every file is also checked to be the one the scan saw: same file ID
//! (opened by ID), same size and same last-write time, both before and after
//! reading. A file that changed in between is reported as [`Checked::Changed`]
//! rather than hashed into a misleading answer.

use std::os::windows::fs::MetadataExt as _;
use std::path::PathBuf;

use oneup_ntfs::FileId;
use oneup_ntfs::dirent::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
    FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT,
};
use oneup_ntfs::win::{self, Volume};
use oneup_store::{LocalKind, LocalRoot, LocalRow};

use crate::classify::is_cloud_tag;

/// Attributes that mean "not an ordinary file whose data is simply on disk".
/// Any of them is reason enough not to read it.
const NOT_ORDINARY: u32 = FILE_ATTRIBUTE_REPARSE_POINT
    | FILE_ATTRIBUTE_OFFLINE
    | FILE_ATTRIBUTE_RECALL_ON_OPEN
    | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
    | FILE_ATTRIBUTE_DIRECTORY;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] win::Error),
    /// The scanned folder is gone, or the path now leads to a different
    /// folder (another disk, or deleted and re-created). The scan doesn't
    /// describe it any more, so nothing in it is read.
    #[error("{} is no longer the folder that was scanned; scan it again", .0.display())]
    RootChanged(PathBuf),
    /// The folder is managed by another sync app. Any file there may be one
    /// of its cloud placeholders (possibly disguised as an ordinary file, or
    /// dehydrated between our check and our read), so nothing is read at all.
    #[error(
        "content checks aren't possible in a folder that {0} manages: its files are cloud files, and reading \
         them could make {0} download them"
    )]
    OtherEngine(String),
    #[error(transparent)]
    Placeholders(#[from] oneup_cfapi::PlaceholderModeError),
}

/// The outcome of checking one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checked {
    /// The QuickXorHash of the file's contents, which matched the scan's size
    /// and last-write time before and after reading.
    Hash([u8; oneup_hash::HASH_LEN]),
    /// A cloud placeholder or other non-ordinary file: deliberately not read.
    NotOrdinary,
    /// Gone, replaced or modified since the scan (or while it was read).
    Changed,
    /// Couldn't be opened or read (access denied, locked by another program).
    Unreadable,
}

/// Hashes files of one scanned folder. Holds the volume handle and a reusable
/// read buffer, so memory use is fixed regardless of file sizes.
pub struct Verifier {
    volume: Volume,
    buf: Vec<u8>,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier").field("buf", &self.buf.len()).finish_non_exhaustive()
    }
}

impl Verifier {
    /// Prepares to read files of the scanned folder `root`, after checking
    /// that its path still leads to the same folder (volume serial + file ID).
    pub fn open(root: &LocalRoot, buffer_bytes: usize) -> Result<Self, Error> {
        // Two independent guards against reading a placeholder. First, refuse
        // another engine's folder outright, whether the scan recorded one or
        // a sync root is registered there now. Second (for everywhere else),
        // make sure placeholders aren't disguised, so the per-file checks in
        // `hash` see the truth.
        if let Some(p) = &root.sync_root_provider {
            return Err(Error::OtherEngine(p.clone()));
        }
        let path = PathBuf::from(&root.path);
        if let Some(info) = oneup_cfapi::other_sync_root(&path) {
            return Err(Error::OtherEngine(info.provider_name));
        }
        oneup_cfapi::expose_placeholders()?;
        let changed = || Error::RootChanged(path.clone());
        let handle = win::open_dir(&path).map_err(|_| changed())?;
        let (serial, id) = win::file_id(&handle)?;
        if serial != root.volume_serial || id != FileId(root.root_file_id) {
            return Err(changed());
        }
        let volume = Volume::open_containing(&path)?;
        Ok(Self { volume, buf: vec![0; buffer_bytes.max(4096)] })
    }

    /// Hashes the file the scan recorded as `row`, if it is still that file
    /// and an ordinary one. Never fails as a whole: every problem becomes a
    /// [`Checked`] outcome the report can count.
    pub fn hash(&mut self, row: &LocalRow) -> Checked {
        // The scan's own metadata already rules most placeholders out without
        // opening anything.
        if row.kind != LocalKind::File || row.attributes & NOT_ORDINARY != 0 || is_cloud_tag(row.reparse_tag) {
            return Checked::NotOrdinary;
        }
        let handle = match self.volume.open_file_by_id(FileId(row.file_id)) {
            Ok(h) => h,
            Err(e) if e.is_access_denied() => return Checked::Unreadable,
            // Most often "not found": deleted since the scan. Any other
            // failure is also best reported as "changed, scan again".
            Err(_) => return Checked::Changed,
        };
        // Re-check on the handle: the file may have become a placeholder (or
        // been replaced by one under the same ID) since the scan.
        match win::attribute_tag(&handle) {
            Ok((attributes, tag)) if attributes & NOT_ORDINARY == 0 && !is_cloud_tag(tag) => {}
            Ok(_) => return Checked::NotOrdinary,
            Err(_) => return Checked::Unreadable,
        }
        let file = handle.into_file();
        let same = |f: &std::fs::File| {
            f.metadata().is_ok_and(|m| {
                m.file_attributes() & NOT_ORDINARY == 0
                    && i64::try_from(m.len()).is_ok_and(|len| len == row.size)
                    && i64::try_from(m.last_write_time()).is_ok_and(|t| t == row.mtime)
            })
        };
        if !same(&file) {
            return Checked::Changed;
        }
        let hash = match oneup_hash::hash_reader(&file, &mut self.buf) {
            Ok(h) => h,
            // A byte-range lock held by another program, or an I/O error.
            Err(_) => return Checked::Unreadable,
        };
        // Written to while we read it: the hash describes no real version.
        if !same(&file) || i64::try_from(hash.len()).ok() != Some(row.size) {
            return Checked::Changed;
        }
        Checked::Hash(hash.finalize())
    }

    /// Reads the file the scan recorded as `row` (a rule file), with the same
    /// guards as [`Verifier::hash`]: only an ordinary file, still the one the
    /// scan saw, unchanged while read. At most `max` bytes.
    pub fn read_small(&mut self, row: &LocalRow, max: u64) -> Small {
        if row.kind != LocalKind::File || row.attributes & NOT_ORDINARY != 0 || is_cloud_tag(row.reparse_tag) {
            return Small::Checked(Checked::NotOrdinary);
        }
        if u64::try_from(row.size).map_or(true, |s| s > max) {
            return Small::TooLarge;
        }
        let handle = match self.volume.open_file_by_id(FileId(row.file_id)) {
            Ok(h) => h,
            Err(e) if e.is_access_denied() => return Small::Checked(Checked::Unreadable),
            Err(_) => return Small::Checked(Checked::Changed),
        };
        match win::attribute_tag(&handle) {
            Ok((attributes, tag)) if attributes & NOT_ORDINARY == 0 && !is_cloud_tag(tag) => {}
            Ok(_) => return Small::Checked(Checked::NotOrdinary),
            Err(_) => return Small::Checked(Checked::Unreadable),
        }
        let mut file = handle.into_file();
        let same = |f: &std::fs::File| {
            f.metadata().is_ok_and(|m| {
                i64::try_from(m.len()).is_ok_and(|len| len == row.size)
                    && i64::try_from(m.last_write_time()).is_ok_and(|t| t == row.mtime)
            })
        };
        if !same(&file) {
            return Small::Checked(Checked::Changed);
        }
        let mut out = Vec::new();
        if std::io::Read::read_to_end(&mut std::io::Read::take(&mut file, max + 1), &mut out).is_err() {
            return Small::Checked(Checked::Unreadable);
        }
        if !same(&file) || i64::try_from(out.len()).ok() != Some(row.size) {
            return Small::Checked(Checked::Changed);
        }
        Small::Read(out)
    }
}

/// The outcome of [`Verifier::read_small`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Small {
    Read(Vec<u8>),
    TooLarge,
    /// Not read, for this reason (never [`Checked::Hash`]).
    Checked(Checked),
}
