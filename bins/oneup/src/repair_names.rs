//! Bounded name-repair previews. Proposals become ordinary fingerprinted
//! rename requests, never filesystem writes. Names already present in either
//! tree, queued renames and earlier proposals all reserve their destinations.

use std::collections::HashSet;

use anyhow::Result;
use oneup_exec::resolve::{self as core, Refused};
use oneup_plan::names;
use oneup_store::{LocalKind, Store};
use sha2::{Digest, Sha256};

use crate::resolve::{self, Choice, Prepared, Request};
use crate::unsynced::Item;

fn windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    let numbered = stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT"));
    // Windows also reserves these superscript digits in device names.
    oneup_effects::name::is_valid_name(name, true) && !matches!(numbered, Some("¹" | "²" | "³"))
}

fn clean(name: &str) -> String {
    let mut name: String =
        name.chars()
            .map(|c| {
                if c.is_control() || matches!(c, '"' | '*' | ':' | '<' | '>' | '?' | '/' | '\\' | '|') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
    name = name.trim_start_matches(' ').trim_end_matches([' ', '.']).to_string();
    while let Some(i) = name.to_ascii_lowercase().find("_vti_") {
        name.replace_range(i..i + 5, "_vti-");
    }
    if name.is_empty() {
        name = "file".into();
    }
    if names::is_invalid(&name) || !windows_name(&name) {
        name.insert(0, '_');
    }
    if names::not_synced(&name).is_some() {
        name = format!("_{name}_");
    }
    name
}

fn truncate(s: &str, units: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= units
        })
        .collect()
}

fn candidate(base: &str, number: usize, units: usize) -> Option<String> {
    let suffix = if number == 0 { String::new() } else { format!(" ({number})") };
    let (mut stem, mut extension) = match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (base, String::new()),
    };
    if suffix.encode_utf16().count() + extension.encode_utf16().count() >= units {
        stem = base;
        extension.clear();
    }
    let available = units.checked_sub(suffix.encode_utf16().count() + extension.encode_utf16().count())?;
    if available == 0 {
        return None;
    }
    let stem = truncate(stem, available).trim_end_matches([' ', '.']).to_string();
    let mut name = format!("{stem}{suffix}{extension}");
    // Truncation can turn an ordinary stem into a device name.
    if names::is_invalid(&name) || !windows_name(&name) {
        name = format!("_{}", truncate(&name, units.saturating_sub(1)));
    }
    (name.encode_utf16().count() <= units
        && !names::is_invalid(&name)
        && names::not_synced(&name).is_none()
        && windows_name(&name))
    .then_some(name)
}

pub fn prepare(store: &Store, items: &[&Item], backup: bool) -> Result<Prepared> {
    anyhow::ensure!(items.len() <= resolve::MAX, crate::t!("At most {n} name repairs at once", n = resolve::MAX));
    let mut items = items.to_vec();
    items.sort_by(|a, b| a.path.cmp(&b.path));
    items.dedup_by(|a, b| a.path == b.path);
    let out = store.read(|o| {
        let mut out = Prepared::default();
        let mut reserved = HashSet::new();
        let pending = resolve::queued(o)?;
        for req in &pending {
            if req.choice == Choice::Rename
                && let (Some(id), Some(name)) = (resolve::local_at(o, &req.path)?, req.name.as_ref())
                && let Some(row) = o.local_get(id.0)?
            {
                reserved.insert((row.parent_id, oneup_sync::fold(name)));
            }
        }
        let remote = o.meta_get(crate::sync::KEY_REMOTE)?.unwrap_or_default();
        for item in items {
            let worked = (|| -> oneup_store::Result<std::result::Result<Request, String>> {
                if !resolve::choices(item.why, backup).contains(&Choice::RepairNames) {
                    return Ok(Err(crate::t!("name repair isn't offered for this item")));
                }
                if pending.iter().any(|r| r.path == item.path) {
                    return Ok(Err(crate::t!("an answer is already queued")));
                }
                if out.asked.len() + pending.len() >= resolve::MAX {
                    return Ok(Err(crate::t!(
                        "the answer queue is full; let OneUpDrive carry out queued answers first"
                    )));
                }
                let Some(id) = resolve::local_at(o, &item.path)? else {
                    return Ok(Err(crate::t!("it isn't on this PC any more")));
                };
                let Some(row) = o.local_get(id.0)? else { return Ok(Err(crate::t!("it isn't on this PC any more"))) };
                if row.kind == LocalKind::Root {
                    return Ok(Err(crate::t!("the sync folder cannot be renamed")));
                }
                let parent = item.path.rsplit_once('/').map_or("", |(parent, _)| parent);
                let prefix = format!("{}{parent}/", remote.trim_end_matches('/'));
                let units = names::MAX_PATH_UNITS.saturating_sub(names::path_units(&prefix)).min(255);
                let base = clean(&row.name);
                for n in 0..=resolve::MAX {
                    let Some(name) = candidate(&base, n, units) else { break };
                    if reserved.contains(&(row.parent_id, oneup_sync::fold(&name))) {
                        continue;
                    }
                    match core::rename(o, id, &name) {
                        Ok(change) => {
                            reserved.insert((row.parent_id, oneup_sync::fold(&name)));
                            return Ok(Ok(Request {
                                path: item.path.clone(),
                                choice: Choice::Rename,
                                name: Some(name),
                                id: Some(change.fingerprint()),
                            }));
                        }
                        Err(core::Error::Refused(Refused::NameTaken | Refused::NameTakenThere | Refused::SameName)) => {
                            continue;
                        }
                        Err(core::Error::Refused(why)) => return Ok(Err(why.to_string())),
                        Err(core::Error::Store(e)) => return Err(e),
                    }
                }
                Ok(Err(crate::t!(
                    "no available name within the path limit; rename the item or its folder individually"
                )))
            })()?;
            match worked {
                Ok(req) => out.asked.push(req),
                Err(why) => out.refused.push(format!("{}: {why}", item.path)),
            }
        }
        out.items = out.asked.len();
        Ok(out)
    })?;
    let mut out = out;
    let mut code = Sha256::new();
    code.update(b"repair-names");
    for req in &out.asked {
        code.update(req.path.as_bytes());
        code.update([0]);
        code.update(req.id.as_deref().unwrap_or_default().as_bytes());
        code.update([0]);
    }
    out.code = code.finalize()[..8].iter().map(|b| format!("{b:02x}")).collect();
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::unsynced::{Record, Why};
    use oneup_store::{LocalRoot, LocalRow, Skip};
    use oneup_sync::{LocalId, ROOT};

    fn row(id: u128, parent: Option<u128>, name: &str, kind: LocalKind) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: parent,
            name: name.into(),
            name_lossy: false,
            kind,
            size: 12,
            mtime: 34,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    fn store() -> Store {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.local_begin(
                &LocalRoot {
                    path: r"C:\test".into(),
                    volume_serial: 7,
                    root_file_id: 1,
                    rules: String::new(),
                    journal: None,
                    started_at_ms: 0,
                    finished_at_ms: None,
                    sync_root_provider: None,
                },
                &row(1, None, "", LocalKind::Root),
            )?;
            o.upsert_item(
                &oneup_store::RemoteRow {
                    id: "root".into(),
                    parent_id: None,
                    name: "".into(),
                    kind: oneup_store::Kind::Root,
                    size: 0,
                    etag: None,
                    qxh: None,
                    mtime_ms: None,
                },
                false,
            )?;
            o.synced_put(&oneup_sync::Base {
                sid: ROOT,
                parent: None,
                name: "".into(),
                kind: oneup_sync::NodeKind::Folder,
                size: 0,
                hash: None,
                local: Some(LocalId(1)),
                local_mtime: 0,
                remote: Some(oneup_sync::RemoteId("root".into())),
                remote_etag: String::new(),
            })
        })
        .unwrap();
        s
    }

    fn add(s: &mut Store, id: u128, parent: u128, name: &str, kind: LocalKind) {
        s.write(|o| o.local_put(&row(id, Some(parent), name, kind)).map(|_| ())).unwrap();
    }

    fn items(paths: &[&str]) -> Vec<Item> {
        paths.iter().map(|p| Item { path: p.to_string(), why: Why::InvalidName }).collect()
    }

    fn preview(s: &Store, items: &[Item]) -> Prepared {
        prepare(s, &items.iter().collect::<Vec<_>>(), false).unwrap()
    }

    fn record(s: &mut Store, items: &[Item]) {
        let value: Vec<_> = items.iter().map(|i| serde_json::json!([i.path, i.why.code()])).collect();
        s.write(|o| o.meta_set(crate::unsynced::KEY, &serde_json::json!({"items": value}).to_string())).unwrap();
    }

    #[test]
    fn repairs_all_invalid_name_classes_and_preserves_unicode_and_extensions() {
        for (before, after) in [
            ("1.", "1"),
            (" lead. ", "lead"),
            ("a?b|c\n.txt", "a_b_c_.txt"),
            ("NUL.txt", "_NUL.txt"),
            ("COM¹.txt.", "_COM¹.txt"),
            ("LPT².", "_LPT²"),
            ("COM1", "_COM1"),
            (".lock", "_.lock"),
            ("x_VTI_y", "x_vti-y"),
            (".oneup.", "_.oneup"),
            ("...", "file"),
            ("~$a.", "_~$a_"),
            ("a.tmp.", "_a.tmp_"),
            ("日本語😀.", "日本語😀"),
        ] {
            assert_eq!(candidate(&clean(before), 0, 255).as_deref(), Some(after), "{before}");
        }
        let long = format!("{}.txt", "😀".repeat(200));
        for n in 0..10 {
            let proposed = candidate(&clean(&long), n, 255).unwrap();
            assert!(proposed.encode_utf16().count() <= 255 && proposed.ends_with(".txt"));
        }
        assert_eq!(candidate("CONX", 0, 3).as_deref(), Some("_CO"));
        assert!(candidate("a.txt", 1, 1).is_none());
    }

    #[test]
    fn reserves_local_remote_pending_and_batch_names_without_overwriting() {
        let mut s = store();
        add(&mut s, 2, 1, "1.", LocalKind::File);
        add(&mut s, 3, 1, "1 ", LocalKind::File);
        add(&mut s, 4, 1, "1", LocalKind::File);
        add(&mut s, 5, 1, "other.", LocalKind::File);
        s.write(|o| {
            o.upsert_item(
                &oneup_store::RemoteRow {
                    id: "taken".into(),
                    parent_id: Some("root".into()),
                    name: "1 (1)".into(),
                    kind: oneup_store::Kind::File,
                    size: 1,
                    etag: Some("e".into()),
                    qxh: None,
                    mtime_ms: None,
                },
                false,
            )
        })
        .unwrap();
        resolve::ask(
            &mut s,
            &[Request { path: "/other.".into(), choice: Choice::Rename, name: Some("1 (2)".into()), id: None }],
        )
        .unwrap();
        let selected = items(&["/1.", "/1 ", "/other."]);
        let p = preview(&s, &selected);
        assert_eq!(p.asked.iter().map(|r| r.name.as_deref().unwrap()).collect::<Vec<_>>(), ["1 (3)", "1 (4)"]);
        assert_eq!(p.refused, ["/other.: an answer is already queued"]);
        let mut reversed = selected;
        reversed.reverse();
        assert_eq!(preview(&s, &reversed).code, p.code);
        assert_eq!(s.read(|o| o.local_get(2)).unwrap().unwrap().name, "1.", "preview changes nothing");
    }

    #[test]
    fn nested_repairs_and_case_clashes_get_distinct_ordinary_rename_operations() {
        let mut s = store();
        add(&mut s, 2, 1, "folder.", LocalKind::Folder);
        add(&mut s, 3, 2, "child.", LocalKind::File);
        add(&mut s, 4, 1, "A.txt", LocalKind::File);
        add(&mut s, 5, 1, "a.txt", LocalKind::File);
        let mut selected = items(&["/folder.", "/folder./child.", "/A.txt", "/a.txt"]);
        for i in &mut selected[2..] {
            i.why = Why::NameClash;
        }
        record(&mut s, &selected);
        let p = preview(&s, &selected);
        assert_eq!(p.asked.len(), 4, "renaming a folder doesn't repair its children's names");
        assert_eq!(p.asked[0].name.as_deref(), Some("A (1).txt"));
        assert_eq!(p.asked[1].name.as_deref(), Some("a (2).txt"));
        assert!(p.asked.iter().all(|r| r.choice == Choice::Rename));
        resolve::ask(&mut s, &p.asked).unwrap();
        let applied = resolve::apply_queued(&mut s, false, &|_| true).unwrap();
        assert_eq!(applied.ops.len(), 4);
        assert!(applied.log.is_empty());
    }

    #[test]
    fn changed_sources_or_destinations_refuse_the_previewed_operations() {
        let mut s = store();
        add(&mut s, 2, 1, "1.", LocalKind::File);
        let selected = items(&["/1."]);
        record(&mut s, &selected);
        let p = preview(&s, &selected);
        s.write(|o| {
            let mut r = o.local_get(2)?.unwrap();
            r.mtime += 1;
            o.local_put(&r).map(|_| ())
        })
        .unwrap();
        assert_ne!(preview(&s, &selected).code, p.code, "preview code binds source stamp");
        resolve::ask(&mut s, &p.asked).unwrap();
        let applied = resolve::apply_queued(&mut s, false, &|_| true).unwrap();
        assert!(applied.ops.is_empty());
        assert!(applied.log[0].detail.contains("changed since you chose"));
        let p = preview(&s, &selected);
        resolve::ask(&mut s, &p.asked).unwrap();
        add(&mut s, 3, 1, "1", LocalKind::File);
        let applied = resolve::apply_queued(&mut s, false, &|_| true).unwrap();
        assert!(applied.ops.is_empty());
        assert!(applied.log[0].detail.contains("has that name"));
        assert_eq!(crate::unsynced::load(&s).unwrap(), Record { items: selected, more: false });
    }

    #[test]
    fn path_budget_includes_the_paired_cloud_folder_and_refuses_impossible_names() {
        let mut s = store();
        add(&mut s, 2, 1, "longfilename.txt.", LocalKind::File);
        let selected = items(&["/longfilename.txt."]);
        s.write(|o| o.meta_set(crate::sync::KEY_REMOTE, &format!("/{}", "a".repeat(390)))).unwrap();
        let p = preview(&s, &selected);
        let name = p.asked[0].name.as_ref().unwrap();
        assert!(name.encode_utf16().count() <= 9);
        s.write(|o| o.meta_set(crate::sync::KEY_REMOTE, &format!("/{}", "a".repeat(400)))).unwrap();
        let p = preview(&s, &selected);
        assert!(p.asked.is_empty() && p.refused.len() == 1);
    }

    #[test]
    fn previews_only_what_fits_in_the_bounded_answer_queue() {
        let mut s = store();
        add(&mut s, 2, 1, "1.", LocalKind::File);
        add(&mut s, 3, 1, "2.", LocalKind::File);
        let waiting: Vec<_> = (0..resolve::MAX - 1)
            .map(|n| Request { path: format!("/waiting-{n}"), choice: Choice::Restore, name: None, id: None })
            .collect();
        resolve::ask(&mut s, &waiting).unwrap();
        let selected = items(&["/1.", "/2."]);
        let p = preview(&s, &selected);
        assert_eq!(p.asked.len(), 1);
        assert!(p.refused[0].contains("queue is full"));
        resolve::ask(&mut s, &p.asked).unwrap();
        assert_eq!(s.read(resolve::queued).unwrap().len(), resolve::MAX);
        let too_many = vec![&selected[0]; resolve::MAX + 1];
        assert!(prepare(&s, &too_many, false).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn a_real_batch_of_602_files_repairs_through_the_gate_and_keeps_a_racing_destination() {
        let dir = std::env::temp_dir().join(format!("oneup-bulk-repair-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exact = std::fs::canonicalize(&dir).unwrap();
        let selected: Vec<_> = (1..=602)
            .map(|n| {
                std::fs::write(exact.join(format!("{n}.")), format!("original {n}")).unwrap();
                Item { path: format!("/{n}."), why: Why::InvalidName }
            })
            .collect();
        let mut scan = oneup_local::Scan::open(
            Store::open_in_memory().unwrap(),
            &dir,
            oneup_local::Config::new(oneup_rules::ExcludeSet::none()),
        )
        .unwrap();
        scan.run().unwrap();
        let mut s = scan.into_store();
        let rules = oneup_rules::RuleSet::new(&[] as &[&str], &[] as &[&str]).unwrap();
        crate::unsynced::update(&mut s, &dir, &rules, selected.iter().map(|i| (i.path.clone(), i.why)), [], true)
            .unwrap();
        assert_eq!(crate::unsynced::load(&s).unwrap().items.len(), 602);
        let p = preview(&s, &selected);
        assert_eq!(p.asked.len(), 602);
        resolve::ask(&mut s, &p.asked).unwrap();
        let root = s.read(|o| o.local_root()).unwrap().unwrap();
        let fs = oneup_effects::LocalFs::open(&dir, root.volume_serial, root.root_file_id, "bulk-repair-test", true)
            .unwrap();
        let applied = resolve::apply_queued(&mut s, false, &|id| fs.is_present(id)).unwrap();
        assert_eq!(applied.ops.len(), 602);
        // A different executable supplies a user's file after the preview.
        let status = std::process::Command::new("cmd.exe")
            .current_dir(&dir)
            .args(["/d", "/c", "echo", "newcomer>1"])
            .status()
            .unwrap();
        assert!(status.success());
        let review = oneup_gate::review_in(
            &oneup_gate::Policy { max_count: 10_000, ..Default::default() },
            &oneup_gate::Context { synced_items: 1_000_000, ..Default::default() },
            applied.ops.into_iter().map(|(op, _)| op).collect(),
        );
        assert_eq!(review.approved.len(), 602);
        let mut collisions = 0;
        for op in &review.approved {
            match fs.apply(op) {
                Ok(_) => {}
                Err(oneup_effects::Error::NameTaken) => collisions += 1,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(collisions, 1);
        assert_eq!(std::fs::read_to_string(exact.join("1.")).unwrap(), "original 1");
        assert!(std::fs::read_to_string(exact.join("1")).unwrap().starts_with("newcomer"));
        for n in 2..=602 {
            assert_eq!(std::fs::read_to_string(exact.join(n.to_string())).unwrap(), format!("original {n}"));
            assert!(!exact.join(format!("{n}.")).exists());
        }
        drop(fs);
        std::fs::remove_dir_all(exact).unwrap();
    }
}
