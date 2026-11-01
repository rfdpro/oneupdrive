//! Review initial differences without turning absence into deletion evidence.

use anyhow::{Context, Result, bail};
use oneup_plan::{
    Category,
    folder::{Next, plan_folder},
};
use oneup_rules::RuleSet;
use oneup_store::{Store, initial::local_key};
use serde_json::{Value, json};

pub fn collect(store: &mut Store, rules: &RuleSet) -> Result<()> {
    store.write_durable(|o| {
        let Some(root) = o.synced_get(oneup_sync::ROOT)? else { return Ok(()) };
        let (Some(local), Some(remote)) = (root.local, root.remote) else { return Ok(()) };
        o.initial_begin()?;
        let mut stack = vec![Next::Both { local: local.0, remote: remote.0, path: String::new() }];
        while let Some(next) = stack.pop() {
            let (path, inherit, local, remote) = match next {
                Next::Both { local, remote, path } => {
                    (path, None, o.local_children(local)?, o.remote_children(&remote)?)
                }
                Next::Local { id, path, inherit } => (path, inherit, o.local_children(id)?, Vec::new()),
                Next::Remote { id, path, inherit } => (path, inherit, Vec::new(), o.remote_children(&id)?),
            };
            let locals: std::collections::HashMap<_, _> = local.iter().map(|r| (r.name.clone(), r.file_id)).collect();
            let remotes: std::collections::HashMap<_, _> =
                remote.iter().map(|(r, _)| (r.name.clone(), r.id.clone())).collect();
            let plan = plan_folder(&path, inherit, local, remote);
            for entry in plan.entries {
                if oneup_sync::same_name(&entry.path, "/oneup.settings") {
                    continue;
                }
                if rules.excludes(&entry.path, entry.is_folder) {
                    continue;
                }
                let name = entry.path.rsplit('/').next().unwrap_or_default();
                match entry.category {
                    Category::Upload => {
                        if let Some(id) = locals.get(name) {
                            o.initial_put(&local_key(*id), &entry.path, entry.is_folder, false)?;
                        }
                    }
                    Category::Download => {
                        if let Some(id) = remotes.get(name) {
                            o.initial_put(&format!("r:{id}"), &entry.path, entry.is_folder, true)?;
                        }
                    }
                    _ => {}
                }
            }
            stack.extend(plan.next.into_iter().filter(|n| {
                let p = match n {
                    Next::Both { path, .. } | Next::Local { path, .. } | Next::Remote { path, .. } => path,
                };
                !rules.excludes(p, true)
            }));
        }
        o.initial_end()
    })?;
    Ok(())
}

pub fn page(dir: &std::path::Path, side: &str, after: &str) -> Result<Value> {
    if !matches!(side, "l" | "r" | "m") {
        bail!("choose local or OneDrive items");
    }
    let store = crate::setup::open_store(dir, 1024)?;
    let mut rows = store.read(|o| o.initial_page(side, after, 101))?;
    let more = rows.len() > 100;
    rows.truncate(100);
    let next = more.then(|| rows.last().map(|r| r.key.clone())).flatten();
    let mut items = Vec::with_capacity(rows.len());
    for r in rows {
        let path = store.read(|o| o.initial_path(&r.key))?.unwrap_or(r.path);
        items.push(json!({"key":r.key,"path":path,"folder":r.folder,"selected":r.selected}));
    }
    Ok(json!({"items":items,"next":next}))
}

/// Metadata only: acknowledging these rows never authorizes removal of
/// another provider's placeholders or changes the release gate's evidence.
pub fn collect_missing(dir: &std::path::Path) -> Result<()> {
    let mut store = crate::setup::open_store(dir, 1024)?;
    let check = crate::moveover::check(dir, &store)?.context("no OneDrive folder to check")?;
    if check.other_account || check.scanned.is_none() {
        bail!("check this OneDrive folder first");
    }
    let result = store.write(|o| {
        o.initial_clear_missing()?;
        Ok(oneup_plan::observe::observe_visit(o, 0, None, &Default::default(), &Default::default(), &mut |e| {
            if e.category == Category::Parked(oneup_plan::Park::OnlineOnlyNotInCloud)
                && let Some(id) = crate::resolve::local_at(o, &e.path)?
                && let Some(row) = o.local_get(id.0)?
            {
                let key = format!("m:{}:{}:{}", local_key(id.0), row.size, row.mtime);
                o.initial_put(&key, &e.path, e.is_folder, false)?;
            }
            Ok(())
        }))
    })?;
    result?;
    Ok(())
}

pub fn select(dir: &std::path::Path, key: &str, selected: bool) -> Result<()> {
    let mut store = crate::setup::open_store(dir, 1024)?;
    if !store.write_durable(|o| o.initial_select(key, selected))? {
        bail!("item already synced or no longer listed; use Files > Exclude for synced items");
    }
    Ok(())
}

pub fn ready(dir: &std::path::Path) -> Result<()> {
    let store = crate::setup::open_store(dir, 1024)?;
    store.read(|o| o.meta_get(crate::sync::KEY_LOCAL))?.context("compare the folders first")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_store::{Kind, LocalKind, LocalRow, RemoteRow, Skip};
    use oneup_sync::{Base, LocalId, NodeKind, Op, ROOT, RemoteId, Seen, Trees};

    #[test]
    fn setup_choices_never_invent_deletions_and_unselected_content_stays_out_of_the_plan() {
        let mut s = Store::open_in_memory().unwrap();
        let row = |id, parent, name: &str, kind| LocalRow {
            file_id: id,
            parent_id: parent,
            name: name.into(),
            name_lossy: false,
            kind,
            size: 4,
            mtime: 3,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        };
        let high = u128::MAX - 10;
        s.write(|o| {
            o.local_put(&row(1, None, "", LocalKind::Root))?;
            o.local_put(&row(high, Some(1), "old", LocalKind::Folder))?;
            o.local_put(&row(high + 1, Some(high), "keep.txt", LocalKind::File))?;
            o.local_put(&row(high + 2, Some(high), "upload.txt", LocalKind::File))?;
            for (id, parent, name, kind) in
                [("root", None, "", Kind::Root), ("cloud", Some("root"), "cloud.txt", Kind::File)]
            {
                o.upsert_item(
                    &RemoteRow {
                        id: id.into(),
                        parent_id: parent.map(Into::into),
                        name: name.into(),
                        kind,
                        size: 4,
                        etag: Some("e1".into()),
                        qxh: None,
                        mtime_ms: None,
                    },
                    false,
                )?;
            }
            o.synced_put(&Base {
                sid: ROOT,
                parent: None,
                name: String::new(),
                kind: NodeKind::Folder,
                size: 0,
                hash: None,
                local: Some(LocalId(1)),
                local_mtime: 0,
                remote: Some(RemoteId("root".into())),
                remote_etag: "e1".into(),
            })
        })
        .unwrap();
        collect(&mut s, &RuleSet::default()).unwrap();
        assert!(
            matches!(s.write(|o| o.initial_select(&local_key(high + 2), true)), Err(oneup_store::Error::NotDurable)),
            "inclusion decisions require a durable transaction"
        );
        assert!(s.read(|o| Trees::local_children(o, LocalId(1))).unwrap().is_empty());
        assert!(matches!(s.read(|o| Trees::local(o, LocalId(high + 1))).unwrap(), Seen::Missing));
        s.write_durable(|o| {
            o.initial_select(&local_key(high + 2), true)?;
            o.initial_select("r:cloud", false)?;
            Ok(())
        })
        .unwrap();
        let visible = s.read(|o| Trees::local_children(o, LocalId(high))).unwrap();
        let revision = s.read(|o| o.meta_get("initial.revision")).unwrap();
        s.write_durable(|o| o.initial_select(&local_key(high + 2), true)).unwrap();
        assert_eq!(
            s.read(|o| o.meta_get("initial.revision")).unwrap(),
            revision,
            "unchanged choices do not re-plan the tree"
        );
        assert_eq!(visible.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["upload.txt"]);
        assert!(s.read(|o| Trees::remote_children(o, &RemoteId("root".into()))).unwrap().is_empty());
        // A new preview preserves answers, including the 128-bit IDs.
        collect(&mut s, &RuleSet::default()).unwrap();
        let ops = s.read(|o| oneup_sync::plan(o, [ROOT])).unwrap();
        assert!(ops.iter().all(|op| !matches!(op, Op::DeleteRemote { .. } | Op::RecycleLocal { .. })));
        assert!(ops.iter().any(|op| matches!(op, Op::MkdirRemote { .. })));
        assert_eq!(s.read(|o| o.local_get(high + 1)).unwrap().unwrap().name, "keep.txt");
        s.write_durable(|o| {
            o.local_update(&row(high + 1, Some(1), "moved.txt", LocalKind::File))?;
            o.initial_select(&local_key(high), true)?;
            Ok(())
        })
        .unwrap();
        assert!(
            s.read(|o| o.initial_held(&local_key(high + 1))).unwrap(),
            "choosing a folder must not select a former child moved elsewhere"
        );
        assert_eq!(s.read(|o| o.initial_path(&local_key(high + 1))).unwrap().as_deref(), Some("/moved.txt"));
        // Later-created files follow normal sync; they weren't in the review.
        s.write(|o| o.local_put(&row(high + 3, Some(1), "new.txt", LocalKind::File))).unwrap();
        assert!(matches!(s.read(|o| Trees::local(o, LocalId(high + 3))).unwrap(), Seen::Present(_)));
    }
}
