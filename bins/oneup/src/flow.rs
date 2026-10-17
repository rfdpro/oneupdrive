//! Which way a folder pair's changes travel ([`Flow`]): both ways (the
//! default), or one way, as a backup of this PC: what's made or changed
//! here reaches OneDrive, deletions here never do, and nothing in OneDrive
//! changes this PC (`oneup_sync::Flow::Backup`).

use anyhow::Result;
use oneup_store::Store;
pub use oneup_sync::Flow;

/// In the data folder's `meta`: `backup`, or absent for both ways.
pub const KEY_FLOW: &str = "sync.flow";

pub fn get(store: &Store) -> Result<Flow> {
    Ok(match store.read(|o| o.meta_get(KEY_FLOW))?.as_deref() {
        Some("backup") => Flow::Backup,
        _ => Flow::TwoWay,
    })
}

pub fn name(flow: Flow) -> &'static str {
    match flow {
        Flow::TwoWay => "both ways",
        Flow::Backup => "backup (this PC → OneDrive only)",
    }
}

/// Sets the flow. Going back to both ways first lets go of what the backup
/// kept in OneDrive after this PC deleted it (their Synced rows go, deepest
/// first), so syncing both ways brings those files back here instead of
/// reading the old deletions as ones to make in OneDrive now. Returns how
/// many were let go. Run while the pair's agent is stopped.
pub fn set(store: &mut Store, flow: Flow) -> Result<usize> {
    let was = get(store)?;
    let mut released = 0;
    if was == Flow::Backup && flow == Flow::TwoWay {
        released = store.write(|o| {
            let mut n = 0;
            for sid in o.synced_gone_locally()? {
                // A folder whose rows below it stay (something in it is
                // still here) keeps its own row too.
                if sid != oneup_sync::ROOT && !o.synced_has_children(sid)? && o.synced_remove(sid)? {
                    n += 1;
                }
            }
            Ok(n)
        })?;
    }
    store.write(|o| match flow {
        Flow::Backup => o.meta_set(KEY_FLOW, "backup"),
        Flow::TwoWay => o.meta_delete(KEY_FLOW),
    })?;
    Ok(released)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_sync::{Base, LocalId, NodeKind, ROOT, RemoteId};

    fn base(sid: u64, parent: Option<u64>, name: &str, kind: NodeKind, local: u128, remote: &str) -> Base {
        Base {
            sid,
            parent,
            name: name.into(),
            kind,
            size: 0,
            hash: None,
            local: Some(LocalId(local)),
            local_mtime: 0,
            remote: Some(RemoteId(remote.into())),
            remote_etag: "1".into(),
        }
    }

    #[test]
    fn leaving_backup_lets_go_of_what_this_pc_deleted() {
        let mut s = Store::open_in_memory().unwrap();
        assert_eq!(get(&s).unwrap(), Flow::TwoWay);
        assert_eq!(set(&mut s, Flow::Backup).unwrap(), 0);
        assert_eq!(get(&s).unwrap(), Flow::Backup);
        s.write(|o| {
            o.synced_put(&base(ROOT, None, "", NodeKind::Folder, 1, "r0"))?;
            o.synced_put(&base(2, Some(ROOT), "gone", NodeKind::Folder, 2, "r2"))?;
            o.synced_put(&base(3, Some(2), "a.txt", NodeKind::File, 3, "r3"))?;
            o.synced_put(&base(4, Some(ROOT), "kept", NodeKind::Folder, 4, "r4"))?;
            o.synced_put(&base(5, Some(4), "b.txt", NodeKind::File, 5, "r5"))?;
            o.synced_put(&base(6, Some(4), "c.txt", NodeKind::File, 6, "r6"))?;
            for id in [2, 3, 4, 6] {
                o.note_local_deleted(id)?;
            }
            Ok(())
        })
        .unwrap();
        // 3, then its folder 2, and 6; folder 4 still holds b.txt.
        assert_eq!(set(&mut s, Flow::TwoWay).unwrap(), 3);
        assert_eq!(get(&s).unwrap(), Flow::TwoWay);
        let left: Vec<u64> = s.read(|o| o.synced_all()).unwrap().into_iter().map(|b| b.sid).collect();
        assert_eq!(left, vec![ROOT, 4, 5]);
    }
}
