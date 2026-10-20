//! Recovery outside the rounds: putting back a OneDrive version, bringing
//! back an item from OneDrive's recycle bin, and answering a conflict
//! (the settings window's **Recovery** page, `oneup history`, `oneup
//! recycle-bin`, `oneup conflicts`).
//!
//! The first two only add to OneDrive, under its own preconditions, so
//! they don't go through the rounds: a restored version is uploaded as a
//! new version conditional on the file's eTag now (`If-Match`; Graph's
//! `restoreVersion` ignores it on OneDrive Personal), so the version it
//! replaces stays in the history; a restored item takes a free name and
//! replaces nothing. This PC then gets either by syncing, through the
//! safety gate, as any change made in OneDrive. Answering a conflict
//! removes or renames a copy on this PC, so it's planned operations
//! ([`conflict_ops`]) that the caller adds to a round, for the gate and
//! the effects layer like the rest.

use std::io::Seek as _;

use oneup_store::{Kind, LocalKind, Ops, RemoteRow};
use oneup_sync::rewind::Step;
use oneup_sync::{LocalId, LocalPre, NodeKind, Op, RemoteId, RemotePre, Sid};

use crate::cloud::{Cloud, CloudError, Hashing};

/// Makes version `version` of file `id` its current one in OneDrive. It's
/// downloaded into `staging` (a file OneUpDrive owns, outside the synced
/// folder), then uploaded as a new version under `If-Match` with the
/// file's eTag now, and OneDrive's copy checked by its hash. Returns the
/// file as OneDrive now has it.
pub async fn restore_version<C: Cloud>(
    cloud: &C,
    id: &RemoteId,
    version: &str,
    staging: &mut std::fs::File,
) -> Result<RemoteRow, CloudError> {
    let now = cloud.item(id).await?.ok_or(CloudError::Changed)?;
    if now.kind != Kind::File {
        return Err(CloudError::Other("folders have no versions".into()));
    }
    let parent = RemoteId(now.parent_id.clone().ok_or(CloudError::Changed)?);
    let pre = RemotePre { id: id.clone(), etag: now.etag.clone().ok_or(CloudError::Changed)? };
    staging.set_len(0)?;
    staging.rewind()?;
    let mut hashing = Hashing::new(&mut *staging);
    let len = cloud.download_version(id, version, &mut hashing).await?;
    let hash = hashing.hash.finalize();
    staging.rewind()?;
    let size = i64::try_from(len).map_err(|e| CloudError::Other(e.to_string()))?;
    let (row, sent) = cloud.upload(&parent, &now.name, Some(&pre), size, staging).await?;
    if row.qxh != Some(sent.unwrap_or(hash)) || row.size != size {
        return Err(CloudError::Other("OneDrive's copy doesn't match the version sent".into()));
    }
    Ok(row)
}

/// The `n`th name [`restore_deleted`] tries after `name` itself:
/// `name (restored N).ext`, as the version store's restore names them.
pub fn restored_name(name: &str, n: u32, kind: NodeKind) -> String {
    match name.rfind('.').filter(|&i| i > 0 && kind == NodeKind::File) {
        Some(i) => format!("{} (restored {n}){}", &name[..i], &name[i..]),
        None => format!("{name} (restored {n})"),
    }
}

/// Brings deleted item `id` back from OneDrive's recycle bin into folder
/// `parent` as `name`, or, if that's taken, as `name (restored N)`.
/// Nothing is replaced. [`CloudError::Changed`]: it's no longer in the
/// recycle bin (emptied, or kept past its time), or the folder is gone.
pub async fn restore_deleted<C: Cloud>(
    cloud: &C,
    id: &RemoteId,
    parent: &RemoteId,
    name: &str,
    kind: NodeKind,
) -> Result<RemoteRow, CloudError> {
    for n in 0..20 {
        let candidate = if n == 0 { name.to_string() } else { restored_name(name, n, kind) };
        match cloud.restore(id, parent, &candidate).await {
            Err(CloudError::NameTaken) => continue,
            r => return r,
        }
    }
    Err(CloudError::NameTaken)
}

/// Two copies of a file that changed on both sides: the one under the
/// file's name, and the copy kept under a conflict name beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub folder: Sid,
    pub original: Copy,
    pub copy: Copy,
}

/// One side of a [`Conflict`], as this PC has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub sid: Sid,
    pub name: String,
    pub local: LocalId,
    pub size: i64,
    /// Last-write time (FILETIME).
    pub mtime: i64,
}

/// The answer to a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    /// The file under its name; the conflict copy goes (into the version
    /// store here, OneDrive's recycle bin there).
    Original,
    /// The conflict copy, under the file's name; the other goes the same way.
    Copy,
}

/// The file name a conflict copy's name was made from
/// ([`oneup_sync::conflict_candidate`]: `stem (conflict N).ext` from
/// `stem.ext`); `None` if `name` isn't one.
pub fn conflict_original(name: &str) -> Option<String> {
    let at = name.rfind(" (conflict ")?;
    let rest = &name[at + " (conflict ".len()..];
    let close = rest.find(')')?;
    let (n, ext) = (&rest[..close], &rest[close + 1..]);
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || n.starts_with('0') {
        return None;
    }
    // What follows is the extension, or nothing.
    if !(ext.is_empty() || (ext.starts_with('.') && !ext.contains(['(', ')']))) {
        return None;
    }
    let stem = &name[..at];
    (!stem.is_empty()).then(|| format!("{stem}{ext}"))
}

/// Whether `name` is a conflict copy's name of `of`: `stem (conflict N).ext`.
pub fn is_conflict_copy(name: &str, of: &str) -> bool {
    conflict_original(name).is_some_and(|o| oneup_sync::same_name(&o, of))
}

/// The conflicts in Synced folder `folder`: each synced file with a synced
/// conflict copy beside it, both on this PC as recorded. One indexed read
/// of the folder's records, one pass over them (each name parsed once,
/// matched through the folded names), and each pair's two local rows.
pub fn conflicts_in(o: &Ops<'_>, folder: Sid) -> oneup_store::Result<Vec<Conflict>> {
    let kids = o.synced_children(folder)?;
    let files: std::collections::HashMap<String, &oneup_sync::Base> =
        kids.iter().filter(|b| b.kind == NodeKind::File).map(|b| (oneup_sync::fold(&b.name), b)).collect();
    let mut out = Vec::new();
    for copy in kids.iter().filter(|b| b.kind == NodeKind::File) {
        let Some(name) = conflict_original(&copy.name) else { continue };
        let Some(original) = files.get(&oneup_sync::fold(&name)).filter(|b| b.sid != copy.sid) else { continue };
        let (Some(a), Some(b)) = (local_copy(o, original)?, local_copy(o, copy)?) else { continue };
        out.push(Conflict { folder, original: a, copy: b });
    }
    out.sort_by(|a, b| a.original.name.cmp(&b.original.name).then(a.copy.name.cmp(&b.copy.name)));
    Ok(out)
}

fn local_copy(o: &Ops<'_>, b: &oneup_sync::Base) -> oneup_store::Result<Option<Copy>> {
    let Some(l) = b.local else { return Ok(None) };
    let Some(row) = o.local_get(l.0)? else { return Ok(None) };
    if row.kind != LocalKind::File {
        return Ok(None);
    }
    Ok(Some(Copy { sid: b.sid, name: row.name, local: l, size: row.size, mtime: row.mtime }))
}

/// The operations that answer conflict `c` with `keep`, from what the
/// store holds now: the copy not kept goes into the version store as a
/// deletion made here ([`Step::Discard`]; the next round deletes it from
/// OneDrive, into its recycle bin), and, keeping the conflict copy, that
/// copy is renamed to the file's name once the other has freed it (the
/// claims keep that order). Each runs under its own precondition (the
/// file's size and last-write time as listed), so a copy changed since is
/// skipped. The answer is the user's approval of the discarding step: the
/// caller passes its key as approved (`oneup_gate::rewind_keys`). `None`:
/// they're no longer as listed.
pub fn conflict_ops(o: &Ops<'_>, c: &Conflict, keep: Keep) -> oneup_store::Result<Option<Vec<Op>>> {
    let now = conflicts_in(o, c.folder)?;
    if !now.contains(c) {
        return Ok(None);
    }
    let Some(dir) = o.synced_get(c.folder)?.and_then(|b| b.local) else { return Ok(None) };
    let pre = |x: &Copy| LocalPre { id: x.local, stamp: Some((x.size, x.mtime)) };
    let discard = |x: &Copy| Op::rewind(x.sid, Step::Discard { local: pre(x) });
    Ok(Some(match keep {
        Keep::Original => vec![discard(&c.copy)],
        Keep::Copy => vec![
            discard(&c.original),
            // A rename here, like the user's own: the next round takes it
            // to OneDrive.
            Op::RenameLocal { local: pre(&c.copy), dir, name: c.original.name.clone() },
        ],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_and_restored_names() {
        assert!(is_conflict_copy("report (conflict 1).docx", "report.docx"));
        assert!(is_conflict_copy("REPORT (conflict 12).DOCX", "report.docx"));
        assert!(!is_conflict_copy("report (conflict).docx", "report.docx"));
        assert!(!is_conflict_copy("other (conflict 1).docx", "report.docx"));
        // Any number: names aren't guessed one by one.
        assert!(is_conflict_copy("report (conflict 150).docx", "report.docx"));
        assert_eq!(conflict_original("a.b (conflict 2).txt").as_deref(), Some("a.b.txt"));
        assert_eq!(conflict_original(".gitignore (conflict 1)").as_deref(), Some(".gitignore"));
        assert_eq!(conflict_original("notes (conflict 3)").as_deref(), Some("notes"));
        assert_eq!(conflict_original("x (conflict 1) y.txt"), None);
        assert_eq!(conflict_original(" (conflict 1).txt"), None);
        assert_eq!(restored_name("a.txt", 2, NodeKind::File), "a (restored 2).txt");
        assert_eq!(restored_name("dir.d", 1, NodeKind::Folder), "dir.d (restored 1)");
    }
}
