//! Items deleted from OneDrive, as far as the Synced tree's history
//! reaches (`oneup recycle-bin`, the settings window's **Recovery** page).
//!
//! OneDrive Personal's recycle bin can't be listed through documented
//! Graph: the `recycleBin` resource is SharePoint's (sites and SharePoint
//! Embedded containers), and a drive has no such relationship. A deleted
//! item can be brought back by its ID (`POST /items/{id}/restore`,
//! documented for OneDrive Personal only). So this lists the items whose
//! records went with their OneDrive IDs, which OneDrive no longer lists:
//! deleted on either side and synced, or removed by a rewind. What was
//! deleted before the history began, or never synced, isn't listed (the
//! recycle bin on OneDrive's website has it). Bringing one back
//! (`crate::restores::deleted`) changes only OneDrive, and takes a free
//! name; this PC gets it by syncing.

use oneup_store::Ops;
use oneup_sync::rewind::path_then;
use oneup_sync::{NodeKind, RemoteId};
use serde_json::{Value, json};

/// Listed at most.
pub const MAX: usize = 1_000;

/// A deleted item.
#[derive(Debug, Clone)]
pub struct Deleted {
    /// Its place in the history: what `--restore` names.
    pub seq: i64,
    /// Its record (gone).
    pub sid: oneup_sync::Sid,
    pub at_ms: i64,
    /// Where it was, relative to the synced folder (`/docs/a.txt`).
    pub path: String,
    pub name: String,
    pub kind: NodeKind,
    pub size: i64,
    pub id: RemoteId,
    /// The OneDrive folder it goes back into, if that's still there.
    pub folder: Option<RemoteId>,
    /// Deleted from another folder or under another name (moved on the web
    /// and deleted there, say), or from where isn't known for certain:
    /// OneDrive would bring it back there, so it isn't restored from here.
    pub elsewhere: bool,
}

impl Deleted {
    /// Whether **Restore** can bring it back where it was.
    pub fn restorable(&self) -> bool {
        self.folder.is_some() && !self.elsewhere
    }
}

/// Items deleted from OneDrive, newest first.
pub fn list(o: &Ops<'_>, limit: usize) -> oneup_store::Result<Vec<Deleted>> {
    let mut out = Vec::new();
    for r in o.history_removed(limit.min(MAX), None)? {
        let Some(id) = r.was.remote.clone() else { continue };
        // Its folder now: the one it was in, if that's still synced.
        let folder = match r.was.parent {
            Some(p) => o.synced_get(p)?.and_then(|b| b.remote),
            None => None,
        };
        // OneDrive restores it where it was deleted from, whatever is
        // asked (measured).
        let elsewhere = match &folder {
            Some(f) => o.deleted_from(&id)?.is_none_or(|(p, n)| p != *f || !oneup_sync::same_name(&n, &r.was.name)),
            None => false,
        };
        let parent_path = match r.was.parent {
            Some(p) => path_then(o, o, p, r.at_ms - 1)?,
            None => String::new(),
        };
        out.push(Deleted {
            seq: r.seq,
            sid: r.sid,
            at_ms: r.at_ms,
            path: format!("{parent_path}/{}", r.was.name),
            name: r.was.name.clone(),
            kind: r.was.kind,
            size: r.was.size,
            id,
            folder,
            elsewhere,
        });
    }
    Ok(out)
}

pub fn to_json(d: &Deleted) -> Value {
    json!({
        "seq": d.seq,
        "at_ms": d.at_ms,
        "path": d.path,
        "folder": d.kind == NodeKind::Folder,
        "size": d.size,
        "restorable": d.restorable(),
        "elsewhere": d.elsewhere,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use oneup_store::{Kind, RemoteRow, Store};
    use oneup_sync::{Base, LocalId, NodeKind, ROOT, RemoteId};

    fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size: 1,
            etag: Some("e".into()),
            qxh: None,
            mtime_ms: None,
        }
    }

    fn base(sid: u64, parent: Option<u64>, name: &str, kind: NodeKind, remote: &str) -> Base {
        Base {
            sid,
            parent,
            name: name.into(),
            kind,
            size: 1,
            hash: None,
            local: Some(LocalId(u128::from(sid) + 100)),
            local_mtime: 1,
            remote: Some(RemoteId(remote.into())),
            remote_etag: "e".into(),
        }
    }

    /// An item moved on the web into another folder and deleted there
    /// (delta says so) is listed without Restore: OneDrive would bring it
    /// back there. One deleted where it was has Restore.
    #[test]
    fn an_item_deleted_elsewhere_is_listed_without_restore() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.history_start(0)?;
            o.upsert_item(&row("root", None, "", Kind::Root), false)?;
            o.upsert_item(&row("docs", Some("root"), "docs", Kind::Folder), false)?;
            o.synced_put(&base(ROOT, None, "", NodeKind::Folder, "root"))?;
            o.synced_put(&base(1, Some(ROOT), "docs", NodeKind::Folder, "docs"))?;
            for (sid, name, from) in [(2, "a.txt", "trash"), (3, "b.txt", "docs")] {
                let id = format!("r{sid}");
                o.upsert_item(&row(&id, Some("docs"), name, Kind::File), false)?;
                o.synced_put(&base(sid, Some(1), name, NodeKind::File, &id))?;
                o.delete_subtree(&id)?;
                o.note_deleted_from(&id, from)?;
                o.synced_remove(sid)?;
            }
            Ok(())
        })
        .unwrap();
        let listed = s.read(|o| super::list(o, 10)).unwrap();
        let by = |n: &str| listed.iter().find(|d| d.name == n).unwrap();
        assert!(by("a.txt").elsewhere && !by("a.txt").restorable());
        assert_eq!(super::to_json(by("a.txt"))["restorable"], false);
        assert!(!by("b.txt").elsewhere && by("b.txt").restorable());
    }
}
