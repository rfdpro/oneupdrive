//! Local effects in Files On-Demand mode (`docs/architecture/files-on-demand.md`
//! § What changes in the engine): the sync folder is OneUpDrive's own
//! Cloud Files sync root, and cloud content arrives as placeholders, never
//! as file writes (`data-safety.md` invariant 12). Every change of placeholder
//! content goes through a share-none handle on the planned item, checked by
//! file ID and stamp (invariant 5), and dehydration only through
//! `VERIFY_IN_SYNC` (invariant 11).

use std::path::PathBuf;

use oneup_cfapi::identity::Identity;
use oneup_cfapi::provider::{self, Exclusive, NewPlaceholder};
use oneup_gate::{Approved, OldCopy};
use oneup_ntfs::FileId;
use oneup_ntfs::dirent::DirEntry;
use oneup_ntfs::win;
use oneup_sync::{Hash, LocalId, LocalPre, Op};

use crate::local::{Done, Error, LocalFs, Result, check_placeholder, long};

/// The identity a pre-release build (0.1.28) gave the excluded folders it
/// marked for File Explorer (shelved; [`LocalFs::unmark_excluded_folder`]
/// clears them): never a synced item's, which encode an OneDrive item ID
/// (`oneup_cfapi::identity`).
pub const EXCLUDED_MARK: &[u8] = b"oneup:excluded-folder";

/// What a placeholder becomes: its identity, size and last-write time
/// (FILETIME).
#[derive(Debug, Clone)]
pub struct Version {
    pub identity: Identity,
    pub modified: i64,
}

impl LocalFs {
    /// Where item `id` is, if it's one of our **dehydrated** placeholders
    /// (judged by attributes, read through an attribute-only handle, which
    /// never downloads anything).
    pub(crate) fn dehydrated_path(&self, id: FileId) -> Result<Option<PathBuf>> {
        if !self.on_demand() {
            return Ok(None);
        }
        let probe = self.volume().open_attributes_by_id(id).map_err(|_| Error::Changed)?;
        let entry = win::stat_handle(&probe, id)?.ok_or(Error::Changed)?;
        let dehydrated = entry.attributes & oneup_ntfs::dirent::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0
            && entry.attributes & oneup_ntfs::dirent::FILE_ATTRIBUTE_REPARSE_POINT != 0
            && !entry.is_dir();
        Ok(if dehydrated { Some(long(&win::final_path(&probe)?)) } else { None })
    }

    /// Opens `pre` if it's one of our dehydrated placeholders: with
    /// attribute access only (see `win::open_path_for_dehydrated`), proved to
    /// be that item and still the planned stamp.
    pub(crate) fn open_if_dehydrated(&self, pre: &LocalPre) -> Result<Option<(win::Handle, DirEntry, PathBuf)>> {
        let id = FileId(pre.id.0);
        let Some(path) = self.dehydrated_path(id)? else { return Ok(None) };
        let h = win::open_path_for_dehydrated(&path)?;
        if win::file_id(&h)?.1 != id {
            return Err(Error::Changed);
        }
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        check_placeholder(&entry, pre.stamp, true)?;
        if !self.is_inside_pub(&path) {
            return Err(Error::Outside);
        }
        Ok(Some((h, entry, path)))
    }

    /// While the returned hold lives, the open placeholder's content can't
    /// arrive (every fetch of it fails). `None` unless, under that hold, it
    /// is still dehydrated with the same identity: then it has no content
    /// here to lose. Also `None` while a program's fetch of it is under way:
    /// its old version's content is on its way, though none shows yet
    /// (invariant 13). It is then marked in sync (a rename may have cleared
    /// the bit; with no content, that bit protects nothing).
    pub(crate) fn hold_dehydrated(&self, h: &win::Handle) -> Option<provider::Hold> {
        let before = provider::info(&h.as_file()).ok()??;
        let hold = provider::hold(&before.identity)?;
        let now = provider::info(&h.as_file()).ok()??;
        if now.on_disk != 0 || now.identity != before.identity {
            return None;
        }
        if !now.in_sync {
            provider::set_in_sync(&h.as_file()).ok()?;
        }
        Some(hold)
    }

    /// Whether the open item is a placeholder in sync: read before one of
    /// our moves, which may clear the bit, so [`LocalFs::resync_moved`]
    /// restores only what the move cleared.
    pub(crate) fn is_in_sync(&self, h: &win::Handle) -> bool {
        matches!(provider::info(&h.as_file()), Ok(Some(i)) if i.in_sync)
    }

    /// After one of our own moves of a placeholder that was in sync
    /// ([`LocalFs::is_in_sync`] before it; never one a program edited, whose
    /// edit isn't sent yet: invariant 12): the placeholder moved through
    /// `h`, now at `at`, is in sync again. `h` stays open meanwhile:
    /// share-none with data access for one with content (so no one changed
    /// it), attribute access for a dehydrated one (no content to change).
    /// The bit is set through a second, attribute-only handle, which has
    /// the access it needs.
    pub(crate) fn resync_moved(&self, h: &win::Handle, at: &std::path::Path) {
        if !matches!(provider::info(&h.as_file()), Ok(Some(i)) if !i.in_sync) {
            return;
        }
        match provider::open(at) {
            Ok(f) => {
                if let Err(e) = provider::set_in_sync(&f) {
                    tracing::debug!(error = %e, "a moved placeholder wasn't marked in sync");
                }
            }
            Err(e) => tracing::debug!(error = %e, "a moved placeholder couldn't be reopened"),
        }
    }

    /// Opens the planned item `pre` and proves it is that item (file ID),
    /// still the planned stamp, inside the sync folder: share-none with data
    /// access when it has content here; attribute access only when it's a
    /// dehydrated placeholder (data access would download it).
    fn open_ours(&self, pre: &LocalPre) -> Result<(win::Handle, DirEntry, PathBuf)> {
        if let Some(found) = self.open_if_dehydrated(pre)? {
            return Ok(found);
        }
        let id = FileId(pre.id.0);
        let path = {
            let probe = self.volume().open_attributes_by_id(id).map_err(|_| Error::Changed)?;
            long(&win::final_path(&probe)?)
        };
        let h = win::open_path_for_placeholder(&path, pre.stamp.is_none())?;
        if win::file_id(&h)?.1 != id {
            return Err(Error::Changed);
        }
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        check_placeholder(&entry, pre.stamp, true)?;
        if !self.is_inside_pub(&path) {
            return Err(Error::Outside);
        }
        Ok((h, entry, path))
    }

    /// For an approved new-file [`Op::Download`] or [`Op::MkdirLocal`]: a
    /// dehydrated placeholder (in sync) instead of downloaded content.
    pub fn place_placeholder(&self, a: &Approved, v: &Version) -> Result<Done> {
        let (to, name, folder) = match a.op() {
            Op::Download { to, name, replace: None, .. } => (*to, name, false),
            Op::MkdirLocal { to, name, .. } => (*to, name, true),
            _ => return Err(Error::WrongOp),
        };
        let (_pin, dir) = self.pin_folder_pub(to)?;
        let name = self.checked_name_pub(to, name)?;
        let identity = v.identity.encode();
        let item = NewPlaceholder { name, identity: &identity, size: v.identity.size, modified: v.modified, folder };
        match provider::create(&dir, &[item]) {
            Ok(_) => {}
            Err(e) if e.is_exists() => return Err(Error::NameTaken),
            Err(e) => return Err(placeholder_error(e)),
        }
        // Attribute access only: data access would download the new
        // placeholder at once.
        let h = if folder {
            win::open_path_for_placeholder(&dir.join(name), true)?
        } else {
            win::open_path_for_dehydrated(&dir.join(name))?
        };
        let id = win::file_id(&h)?.1;
        let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
        Ok(Done::Placed { parent: to, entry, kept: None })
    }

    /// Whether the open placeholder has some but not all of its `size`
    /// bytes here: a download cut short (see [`LocalFs::open_partial`]).
    pub(crate) fn is_partial(&self, h: &win::Handle, size: i64) -> bool {
        matches!(provider::info(&h.as_file()), Ok(Some(i)) if i.on_disk > 0 && i.on_disk < size)
    }

    /// Reopens the planned placeholder `pre` at `path`, which has only part
    /// of its content here (a download cut short), share-none, for a change
    /// that needs none of that content: reading the rest would ask for a
    /// version the cloud may no longer have. With whole-file hydration a
    /// program can't write to a placeholder until all of it is here
    /// (measured: writes fail and leave nothing modified), so its bytes are
    /// only what OneUpDrive transferred of its identity's version, verified
    /// before the transfer, which OneDrive keeps. Proved under the new
    /// handle: still `pre`, in sync, nothing written here, still partial,
    /// and an identity of ours for the planned size. The returned hold
    /// keeps the rest from arriving: opening with data access asks for it,
    /// and the hold refuses that at once, before the hydration service sees
    /// it. `h`, the attribute handle it was found through, is closed first
    /// (it would make the share-none open fail).
    pub(crate) fn open_partial(
        &self,
        h: win::Handle,
        pre: &LocalPre,
        path: &std::path::Path,
    ) -> Result<(win::Handle, provider::Hold)> {
        let Some((size, _)) = pre.stamp else { return Err(Error::WrongOp) };
        let before = provider::info(&h.as_file()).map_err(placeholder_error)?.ok_or(Error::NotOrdinary)?;
        drop(h);
        let hold = provider::hold(&before.identity);
        let x = win::open_path_for_placeholder(path, false)?;
        let id = FileId(pre.id.0);
        if win::file_id(&x)?.1 != id {
            return Err(Error::Changed);
        }
        let entry = win::stat_handle(&x, id)?.ok_or(Error::Changed)?;
        check_placeholder(&entry, pre.stamp, true)?;
        if !self.is_inside_pub(path) {
            return Err(Error::Outside);
        }
        let now = provider::info(&x.as_file()).map_err(placeholder_error)?.ok_or(Error::NotOrdinary)?;
        let only_ours = now.identity == before.identity
            && now.in_sync
            && now.modified == 0
            && now.on_disk > 0
            && now.on_disk < size
            && Identity::decode(&now.identity).is_some_and(|i| i.size == size);
        if !only_ours {
            return Err(Error::Changed);
        }
        Ok((x, hold))
    }

    /// For an approved [`Op::Download`] that replaces a local file (a cloud
    /// edit): the placeholder takes the new version. Dehydrated, it just
    /// changes identity. Partly here (a download cut short), it holds only
    /// bytes of the old cloud version, which OneDrive keeps: dehydrated with
    /// the new identity without reading it ([`LocalFs::open_partial`]), no
    /// copy kept. Hydrated, its content is the last synced version
    /// (checked under the share-none handle against `synced`, with Windows'
    /// own `VERIFY_IN_SYNC` on top); a copy goes to the version store unless
    /// the token says [`OldCopy::DropIfSynced`] and it proved to be that
    /// version (OneDrive's history has it). Then it's dehydrated with the new
    /// identity. An ordinary file (not yet converted) is refused: it's
    /// replaced the full-file way.
    pub fn replace_placeholder(&self, a: &Approved, v: &Version, synced: Option<&Hash>) -> Result<Done> {
        let Op::Download { to, replace: Some(pre), .. } = a.op() else { return Err(Error::WrongOp) };
        let (h, _entry, path) = self.open_ours(pre)?;
        let info = provider::info(&h.as_file()).map_err(placeholder_error)?.ok_or(Error::NotOrdinary)?;
        let identity = v.identity.encode();
        let mut kept = None;
        if info.on_disk == 0 {
            // Held: its old content can't arrive and be edited meanwhile.
            let _hold = self.hold_dehydrated(&h).ok_or(Error::Changed)?;
            provider::update_dehydrated(&h.as_file(), &identity, v.identity.size, v.modified)
                .map_err(placeholder_error)?;
        } else if pre.stamp.is_some_and(|(size, _)| info.on_disk < size) {
            return self.replace_partial(h, pre, *to, &path, v);
        } else {
            let proved = self.is_synced_pub(&h, pre, synced)?;
            if a.old_copy() != OldCopy::DropIfSynced || !proved {
                // A copy of the old content, as an ordinary file in the
                // store (never a placeholder that could need the cloud).
                kept = Some(self.copy_to_store(&h, &path)?);
            }
            // Its content is the last synced version (proved) or kept above:
            // in sync again (an earlier move may have cleared the bit), then
            // dehydrated through this same share-none handle.
            provider::set_in_sync(&h.as_file()).map_err(placeholder_error)?;
            oneup_cfapi::ours::note(pre.id.0);
            let x = Exclusive::from_file(h.into_file());
            x.dehydrate(Some((&identity, v.identity.size, v.modified))).map_err(placeholder_error)?;
            drop(x);
            let h = win::open_path_for_dehydrated(&path)?;
            let id = win::file_id(&h)?.1;
            let entry = win::stat_handle(&h, id)?.ok_or(Error::Changed)?;
            return Ok(Done::Placed { parent: *to, entry, kept });
        }
        let entry = win::stat_handle(&h, FileId(pre.id.0))?.ok_or(Error::Changed)?;
        Ok(Done::Placed { parent: *to, entry, kept })
    }

    /// [`LocalFs::replace_placeholder`] for one partly here: dehydrated with
    /// the new identity through the share-none handle (`VERIFY_IN_SYNC` on
    /// top), never read.
    fn replace_partial(
        &self,
        h: win::Handle,
        pre: &LocalPre,
        to: LocalId,
        path: &std::path::Path,
        v: &Version,
    ) -> Result<Done> {
        let (x, hold) = self.open_partial(h, pre, path)?;
        oneup_cfapi::ours::note(pre.id.0);
        let x = Exclusive::from_file(x.into_file());
        x.dehydrate(Some((&v.identity.encode(), v.identity.size, v.modified))).map_err(placeholder_error)?;
        drop(x);
        drop(hold);
        let h = win::open_path_for_dehydrated(path)?;
        let entry = win::stat_handle(&h, FileId(pre.id.0))?.ok_or(Error::Changed)?;
        Ok(Done::Placed { parent: to, entry, kept: None })
    }

    /// After a verified upload of `pre`: the file becomes (or stays) a
    /// placeholder in sync with `identity`, if it's still exactly what was
    /// uploaded. Best effort for the caller: if it changed meanwhile, the
    /// next round uploads again.
    pub fn mark_uploaded(&self, pre: &LocalPre, v: &Version) -> Result<DirEntry> {
        let (h, _entry, path) = self.open_ours(pre)?;
        let identity = v.identity.encode();
        let info = provider::info(&h.as_file()).map_err(placeholder_error)?;
        oneup_cfapi::ours::note(pre.id.0);
        let x = Exclusive::from_file(h.into_file());
        if let Some(i) = info {
            // Bytes written here stay "modified" otherwise, and could never
            // be freed (measured, `oneup-cfapi` tests).
            if i.modified > 0 && i.on_disk >= v.identity.size {
                x.revalidate(&identity, v.identity.size).map_err(placeholder_error)?;
            } else {
                x.mark_in_sync(&identity).map_err(placeholder_error)?;
            }
        } else {
            x.convert(&identity).map_err(placeholder_error)?;
        }
        drop(x);
        let h = win::open_path_for_placeholder(&path, pre.stamp.is_none())?;
        win::stat_handle(&h, FileId(pre.id.0))?.ok_or(Error::Changed)
    }

    /// After a move that gave the cloud item a new ID (a vault entry
    /// rebuilt): the placeholder `pre` names it from now on. A dehydrated one
    /// stays dehydrated (attribute access only: opening it for data would
    /// download it); one with content goes as after an upload. Best effort.
    pub fn renew_identity(&self, pre: &LocalPre, v: &Version) -> Result<()> {
        match self.open_if_dehydrated(pre)? {
            Some((h, _, _)) => {
                // Held, and in sync again (the move cleared the bit; with no
                // content here, it protects nothing).
                let _hold = self.hold_dehydrated(&h).ok_or(Error::Changed)?;
                provider::update_dehydrated(&h.as_file(), &v.identity.encode(), v.identity.size, v.modified)
                    .map_err(placeholder_error)
            }
            None => self.mark_uploaded(pre, v).map(|_| ()),
        }
    }

    /// The hash of one of our placeholders without reading it, when its
    /// identity vouches for the content: dehydrated, or in sync with all of
    /// it on disk. `None`: read it the ordinary way (it's hydrated and was
    /// edited), or not one of ours.
    pub(crate) fn identity_hash(&self, h: &win::Handle, size: i64) -> Option<Hash> {
        let info = provider::info(&h.as_file()).ok()??;
        let id = Identity::decode(&info.identity)?;
        let vouches = id.size == size && (info.on_disk == 0 || (info.in_sync && info.on_disk >= size));
        vouches.then_some(id.hash)
    }

    /// Whether the open placeholder has all its content on disk (reading it
    /// then never asks the cloud). Not a placeholder: true, unless its
    /// attributes or reparse tag say it is one after all (then nothing
    /// vouches for its content). Removing the OneDrive copy relies on this
    /// ([`LocalFs::has_content`]), so any doubt answers false.
    pub(crate) fn fully_on_disk(&self, h: &win::Handle, size: i64) -> bool {
        match provider::info(&h.as_file()) {
            Ok(Some(i)) => i.on_disk >= size,
            Ok(None) => win::attribute_tag(h).is_ok_and(|(attributes, tag)| !looks_like_placeholder(attributes, tag)),
            Err(_) => false,
        }
    }

    /// Whether the open placeholder holds no content here (dehydrated).
    pub(crate) fn is_dehydrated(&self, h: &win::Handle) -> bool {
        matches!(provider::info(&h.as_file()), Ok(Some(i)) if i.on_disk == 0)
    }

    /// Turns a placeholder just moved into the version store into an
    /// ordinary file, so a kept copy never depends on the cloud.
    pub(crate) fn revert_kept(&self, kept: &std::path::Path, size: i64) {
        let Ok(f) = provider::open(kept) else { return };
        if let Err(e) = provider::revert(&f, size) {
            tracing::warn!(error = %e, "a kept placeholder couldn't be made an ordinary file");
        }
    }

    /// Whether the item `pre` is still here as planned, with all its
    /// content (a placeholder: fully on disk; a folder: there). What
    /// removing its OneDrive copy needs first ([`Op::PruneRemote`]).
    pub fn has_content(&self, pre: &LocalPre) -> Result<bool> {
        let (h, entry, _) = match self.open_ours(pre) {
            Ok(found) => found,
            Err(Error::Changed) => return Ok(false),
            Err(e) => return Err(e),
        };
        Ok(entry.is_dir() || self.fully_on_disk(&h, i64::try_from(entry.size).unwrap_or(i64::MAX)))
    }

    /// After its OneDrive copy was removed: the placeholder `pre` becomes
    /// an ordinary item, so it no longer depends on OneDrive. Refused
    /// (nothing changed) unless all its content is here. Not a placeholder:
    /// nothing to do.
    pub fn make_ordinary(&self, pre: &LocalPre) -> Result<()> {
        let (h, entry, _) = self.open_ours(pre)?;
        if provider::info(&h.as_file()).map_err(placeholder_error)?.is_none() {
            return Ok(());
        }
        let size = if entry.is_dir() { 0 } else { i64::try_from(entry.size).unwrap_or(i64::MAX) };
        provider::revert(&h.as_file(), size).map_err(placeholder_error)
    }

    /// For an approved [`Op::Dehydrate`] ("Free up space"): frees the
    /// placeholder's content, keeping the placeholder (`data-safety.md`
    /// invariant 11). Only if, read under a share-none handle, the content
    /// hashes to the op's hash, which the caller checked is the cloud's
    /// current version, and Windows still has it in sync (`VERIFY_IN_SYNC`).
    pub fn free_up(&self, a: &Approved) -> Result<Done> {
        let Op::Dehydrate { local, hash } = a.op() else { return Err(Error::WrongOp) };
        let Some((size, _)) = local.stamp else { return Err(Error::WrongOp) };
        let (h, _entry, path) = self.open_ours(local)?;
        let info = provider::info(&h.as_file()).map_err(placeholder_error)?.ok_or(Error::NotOrdinary)?;
        if info.on_disk == 0 {
            return Err(Error::Changed); // already free
        }
        if info.on_disk < size || !info.in_sync {
            return Err(Error::Changed);
        }
        let mut buf = vec![0u8; 1 << 20];
        let got = oneup_hash::hash_reader(&*h.as_file(), &mut buf)?;
        if i64::try_from(got.len()).ok() != Some(size) || got.finalize() != *hash {
            tracing::warn!("a file to free up differs from its cloud version, so it was kept");
            return Err(Error::Changed);
        }
        oneup_cfapi::ours::note(local.id.0);
        Exclusive::from_file(h.into_file()).dehydrate(None).map_err(placeholder_error)?;
        let h = win::open_path_for_dehydrated(&path)?;
        let entry = win::stat_handle(&h, FileId(local.id.0))?.ok_or(Error::Changed)?;
        Ok(Done::Freed { entry })
    }

    /// Turning Files On-Demand on: the synced item `pre` becomes a
    /// placeholder that keeps its content, in sync with `v`. Only if, under a
    /// share-none handle, it is still the planned stamp (for a file, the
    /// caller checked that the hash recorded for that stamp is the synced
    /// version's). Already a placeholder: nothing to do.
    pub fn convert_in_sync(&self, pre: &LocalPre, v: &Version) -> Result<()> {
        let (h, _, _) = self.open_ours(pre)?;
        if provider::info(&h.as_file()).map_err(placeholder_error)?.is_some() {
            return Ok(());
        }
        Exclusive::from_file(h.into_file()).convert(&v.identity.encode()).map_err(placeholder_error)
    }

    /// Converts a local folder to a placeholder in sync (after it was
    /// created in OneDrive), so placeholders below it are ordinary children
    /// of a placeholder folder. Best effort.
    pub fn convert_folder(&self, id: LocalId, v: &Version) -> Result<()> {
        // A folder an earlier build marked excluded, synced now (the rules
        // changed): its pin is cleared first.
        self.unmark_excluded_folder(id.0)?;
        let pre = LocalPre { id, stamp: None };
        let (h, _, _) = self.open_ours(&pre)?;
        if provider::info(&h.as_file()).map_err(placeholder_error)?.is_some() {
            return Ok(());
        }
        Exclusive::from_file(h.into_file()).convert(&v.identity.encode()).map_err(placeholder_error)
    }

    /// Clears a mark left by a pre-release build that marked excluded
    /// folders for File Explorer (since shelved): the folder's *excluded* pin back to
    /// unspecified. The folder stays a placeholder (in sync, with the mark's
    /// identity, which nothing reads for a folder), and is **never
    /// reverted**: reverting a marked folder has removed
    /// an online-only placeholder inside it (one the OneDrive app had left
    /// in an excluded folder; its content was still in OneDrive).
    /// Only such a mark; `Ok(true)` if its pin was changed.
    pub fn unmark_excluded_folder(&self, id: u128) -> Result<bool> {
        let Some(path) = self.folder_path(id)? else { return Ok(false) };
        let f = provider::open(&path).map_err(placeholder_error)?;
        match provider::info(&f).map_err(placeholder_error)? {
            Some(i) if i.identity == EXCLUDED_MARK && i.pin == provider::Pin::Excluded => {
                provider::set_pin(&f, provider::Pin::Unspecified).map_err(placeholder_error)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Where the folder `id` is, if it's a folder inside the sync folder
    /// (by attributes, through an attribute-only handle).
    fn folder_path(&self, id: u128) -> Result<Option<PathBuf>> {
        let Ok(probe) = self.volume().open_attributes_by_id(FileId(id)) else { return Ok(None) };
        let Some(entry) = win::stat_handle(&probe, FileId(id))? else { return Ok(None) };
        if !entry.is_dir() {
            return Ok(None);
        }
        let path = long(&win::final_path(&probe)?);
        Ok(self.is_inside_pub(&path).then_some(path))
    }
}

/// Whether a file's attributes or reparse tag mark a cloud placeholder or
/// content that isn't here (offline, recalled on open or data access).
fn looks_like_placeholder(attributes: u32, reparse_tag: u32) -> bool {
    use oneup_ntfs::dirent::{
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    };
    attributes & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN) != 0
        || oneup_cfapi::placeholder_state(attributes, reparse_tag).is_placeholder()
}

fn placeholder_error(e: provider::Error) -> Error {
    match e {
        provider::Error::Io(io) => Error::Io(io),
        provider::Error::NotPlaceholder => Error::NotOrdinary,
        // Hydrated, or Windows refused (e.g. VERIFY_IN_SYNC after an edit):
        // nothing was changed; plan again.
        provider::Error::Hydrated | provider::Error::Win { .. } => {
            tracing::debug!(error = %e, "a placeholder change was refused");
            Error::Changed
        }
    }
}
