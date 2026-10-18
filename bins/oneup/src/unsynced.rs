//! What can't sync until the user does something about it: an item Windows
//! or OneDrive won't take as it is (a hard link, a name OneDrive refuses, a
//! file over its size limit), or one whose name is held by something that
//! isn't synced. Trying again doesn't help, so the agent stops trying
//! (`sync::waits_for_a_change`) and the round counts as up to date; this
//! record is what tells the user, in `oneup status`, `oneup unsynced` and
//! the tray.
//!
//! Kept in the store's `meta` table ([`KEY`]), bounded to [`MAX`] items.
//! Items leave it once they sync, once they're gone, and once the rules
//! exclude them (those are left out on purpose, not stuck). Excluded items
//! aren't listed at all.

use std::path::Path;

use anyhow::Result;
use oneup_rules::RuleSet;
use oneup_store::Store;
use serde_json::{Value, json};

/// In `meta`: the record (JSON).
pub const KEY: &str = "unsynced";

/// At most this many items are kept (a folder of thousands of hard links
/// shouldn't make every round rewrite a large record).
pub const MAX: usize = 1_000;

/// Why an item can't sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// A hard link, or another item Windows can't make a placeholder of.
    Special,
    /// Its name is held by something that isn't synced (a link, say).
    NameTaken,
    /// OneDrive doesn't accept the name.
    InvalidName,
    /// Larger than OneDrive's limit.
    TooLarge,
    /// Held by sync: gone from this PC with no recorded deletion.
    GoneHere,
    /// Held by sync: gone from OneDrive with no recorded deletion.
    GoneThere,
    /// Held by sync: names in one folder that differ only in case.
    NameClash,
    /// Held by sync: a backup pair would have to change this PC.
    OneWay,
    /// A folder to delete that still holds something: often items the
    /// rules leave out, which sync doesn't list.
    NotEmpty,
}

impl Why {
    pub const ALL: [Why; 9] = [
        Why::Special,
        Why::NameTaken,
        Why::InvalidName,
        Why::TooLarge,
        Why::GoneHere,
        Why::GoneThere,
        Why::NameClash,
        Why::OneWay,
        Why::NotEmpty,
    ];

    /// Held by sync (`Op::Hold`): planned again only with its folder, so
    /// its entry is refreshed by every full plan instead, and kept until
    /// then even when the path isn't here (it may be missing here). One
    /// gone from this PC also leaves as soon as its path is back (restored
    /// from the Recycle Bin, say): it no longer is.
    pub fn held(self) -> bool {
        matches!(self, Why::GoneHere | Why::GoneThere | Why::NameClash | Why::OneWay | Why::NotEmpty)
    }

    /// Why sync holds an item: `gone_here` for a missing one, which side
    /// lost it.
    pub fn of_hold(r: oneup_sync::HoldReason, gone_here: bool) -> Self {
        match r {
            oneup_sync::HoldReason::MissingWithoutEvidence if gone_here => Why::GoneHere,
            oneup_sync::HoldReason::MissingWithoutEvidence => Why::GoneThere,
            oneup_sync::HoldReason::NameClash => Why::NameClash,
            oneup_sync::HoldReason::OneWay => Why::OneWay,
        }
    }

    pub fn from_code(s: &str) -> Option<Why> {
        Why::ALL.into_iter().find(|w| w.code() == s)
    }

    /// How it's saved, and how the window and `oneup unsynced resolve` name it.
    pub fn code(self) -> &'static str {
        match self {
            Why::Special => "special",
            Why::NameTaken => "name_taken",
            Why::InvalidName => "invalid_name",
            Why::TooLarge => "too_large",
            Why::GoneHere => "gone_here",
            Why::GoneThere => "gone_there",
            Why::NameClash => "name_clash",
            Why::OneWay => "one_way",
            Why::NotEmpty => "not_empty",
        }
    }

    /// What it means and what to do, in a few words.
    /// A few words for a list's column; [`Why::label`] says the rest.
    pub fn title(self) -> &'static str {
        match self {
            Why::Special => "Special item",
            Why::NameTaken => "Name taken",
            Why::InvalidName => "Name not allowed",
            Why::TooLarge => "Too large",
            Why::GoneHere => "Gone from this PC",
            Why::GoneThere => "Gone from OneDrive",
            Why::NameClash => "Name differs by case",
            Why::OneWay => "Backup only",
            Why::NotEmpty => "Folder not empty",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Why::Special => "a hard link or other item OneDrive can't hold",
            Why::NameTaken => "an unsynced item (like a link) has its name",
            Why::InvalidName => "OneDrive doesn't allow this name; rename it",
            Why::TooLarge => "over OneDrive's 250 GB limit",
            Why::GoneHere => "missing here with no recorded deletion; delete it in OneDrive or put it back",
            Why::GoneThere => "missing in OneDrive with no recorded deletion; delete it here or restore it there",
            Why::NameClash => "another item differs only in case; OneDrive allows one",
            Why::OneWay => "a backup never changes this PC",
            Why::NotEmpty => "deleted on one side, but the other side's folder still holds (likely excluded) items",
        }
    }
}

impl From<oneup_plan::park::Why> for Why {
    fn from(w: oneup_plan::park::Why) -> Self {
        match w {
            oneup_plan::park::Why::InvalidName => Why::InvalidName,
            oneup_plan::park::Why::TooLarge => Why::TooLarge,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Relative to the synced folders (`/docs/a.txt`).
    pub path: String,
    pub why: Why,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub items: Vec<Item>,
    /// More were found than [`MAX`].
    pub more: bool,
}

/// The record; unreadable counts as empty (it's our own, and is filled
/// again as rounds run into the items).
pub fn load(store: &Store) -> Result<Record> {
    let v = store.read(|o| o.meta_get(KEY))?;
    let Some(v) = v.and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { return Ok(Record::default()) };
    let items = v["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let why = Why::from_code(i[1].as_str()?)?;
            Some(Item { path: i[0].as_str()?.to_string(), why })
        })
        .collect();
    Ok(Record { items, more: v["more"].as_bool().unwrap_or(false) })
}

fn save(store: &mut Store, r: &Record) -> Result<()> {
    let items: Vec<Value> = r.items.iter().map(|i| json!([i.path, i.why.code()])).collect();
    let text = json!({ "items": items, "more": r.more }).to_string();
    store.write(|o| o.meta_set(KEY, &text))?;
    Ok(())
}

/// After a round: adds what it found it can't sync (`found`), and drops
/// what it synced (`synced`), what the rules now exclude, and what's no
/// longer in `root` (looked up by attributes only, never opened). `full`:
/// the round planned every folder, so held items it didn't find are
/// settled. Writes the record only when it changed.
pub fn update<'a>(
    store: &mut Store,
    root: &Path,
    rules: &RuleSet,
    found: impl IntoIterator<Item = (String, Why)>,
    synced: impl IntoIterator<Item = &'a str>,
    full: bool,
) -> Result<()> {
    let before = load(store)?;
    let mut r = before.clone();
    if full {
        r.items.retain(|i| !i.why.held());
    }
    // A first sync can do millions: looked up, not searched.
    let synced: std::collections::HashSet<String> = synced.into_iter().map(str::to_ascii_lowercase).collect();
    r.items.retain(|i| !synced.contains(&i.path.to_ascii_lowercase()));
    for (path, why) in found {
        if let Some(i) = r.items.iter_mut().find(|i| i.path.eq_ignore_ascii_case(&path)) {
            i.why = why;
        } else if r.items.len() < MAX {
            r.items.push(Item { path, why });
        } else {
            r.more = true;
        }
    }
    r.items.retain(|i| {
        let full = i.path.trim_start_matches('/').split('/').fold(root.to_path_buf(), |p, x| p.join(x));
        match std::fs::symlink_metadata(&full) {
            Ok(_) if i.why == Why::GoneHere => false,
            Ok(m) => !rules.excludes(&i.path, m.is_dir()),
            Err(e) => i.why.held() || e.kind() != std::io::ErrorKind::NotFound,
        }
    });
    if r.items.len() < MAX / 2 {
        // Room again: the next rounds that run into the rest list them.
        r.more = false;
    }
    if r != before {
        save(store, &r)?;
    }
    Ok(())
}

/// Items the user answered (`crate::resolve`) leave the record, with what
/// sync held below them: none of it is held any more. Planned again, an
/// item that still can't sync comes back.
pub fn answered(store: &mut Store, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let before = load(store)?;
    let mut r = before.clone();
    let below = |path: &str, top: &str| {
        let (p, t) = (path.as_bytes(), top.trim_end_matches('/').as_bytes());
        p.len() > t.len() && p[..t.len()].eq_ignore_ascii_case(t) && p[t.len()] == b'/'
    };
    r.items.retain(|i| !paths.iter().any(|p| i.path.eq_ignore_ascii_case(p) || (i.why.held() && below(&i.path, p))));
    if r != before {
        save(store, &r)?;
    }
    Ok(())
}

/// For the agent's status: how many, and whether there are more.
pub fn count(r: &Record) -> Option<oneup_ipc::Unsynced> {
    (!r.items.is_empty()).then_some(oneup_ipc::Unsynced { count: r.items.len(), more: r.more })
}

/// `oneup unsynced`: the record, read from the database (with or without
/// the agent running).
pub fn show(store: &Store) -> Result<()> {
    let r = load(store)?;
    if r.items.is_empty() {
        println!("Everything that isn't excluded by your rules syncs.");
    } else {
        println!("These can't sync until you change them (trying again doesn't help):");
        for i in &r.items {
            println!("  {}  ({})", i.path, i.why.label());
        }
        let more = if r.more { ", and more not listed" } else { "" };
        println!("{} items{more}. Each is tried again as soon as it changes.", r.items.len());
        println!("`oneup unsynced resolve PATH` shows what you can do about one.");
    }
    println!(
        "Excluded items (your rules, `oneup rules`) aren't listed: they're left out on purpose. Explorer shows them as Sync pending, and the folders they're in too."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn items_leave_once_synced_gone_or_excluded() {
        let dir = std::env::temp_dir().join(format!("oneup-unsynced-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["link.txt", "bad.txt", "env.lock", "fixed.txt"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let mut s = Store::open_in_memory().unwrap();
        let none = RuleSet::new(&[] as &[&str], &[] as &[&str]).unwrap();
        let found = [
            ("/link.txt", Why::Special),
            ("/bad.txt", Why::InvalidName),
            ("/env.lock", Why::Special),
            ("/fixed.txt", Why::NameTaken),
            ("/gone.txt", Why::Special),
        ];
        update(&mut s, &dir, &none, found.iter().map(|(p, w)| (p.to_string(), *w)), [], false).unwrap();
        let paths = |s: &Store| load(s).unwrap().items.into_iter().map(|i| i.path).collect::<Vec<_>>();
        assert_eq!(paths(&s), ["/link.txt", "/bad.txt", "/env.lock", "/fixed.txt"], "not what isn't there");
        let rules = RuleSet::new(&[] as &[&str], &["*.lock"]).unwrap();
        update(&mut s, &dir, &rules, [], ["/FIXED.txt"], false).unwrap();
        assert_eq!(paths(&s), ["/link.txt", "/bad.txt"], "synced, or excluded since");
        std::fs::remove_file(dir.join("bad.txt")).unwrap();
        update(&mut s, &dir, &rules, [], [], false).unwrap();
        assert_eq!(paths(&s), ["/link.txt"]);
        // Held: kept though not here, until a full plan no longer holds it.
        update(&mut s, &dir, &rules, [("/gone.txt".to_string(), Why::GoneHere)], [], false).unwrap();
        update(&mut s, &dir, &rules, [], [], false).unwrap();
        assert_eq!(paths(&s), ["/link.txt", "/gone.txt"]);
        update(&mut s, &dir, &rules, [], [], true).unwrap();
        assert_eq!(paths(&s), ["/link.txt"]);
        // Gone from here, then put back: it leaves at once.
        update(&mut s, &dir, &rules, [("/back.txt".to_string(), Why::GoneHere)], [], false).unwrap();
        assert_eq!(paths(&s), ["/link.txt", "/back.txt"]);
        std::fs::write(dir.join("back.txt"), b"x").unwrap();
        update(&mut s, &dir, &rules, [], [], false).unwrap();
        assert_eq!(paths(&s), ["/link.txt"]);
        assert_eq!(count(&load(&s).unwrap()), Some(oneup_ipc::Unsynced { count: 1, more: false }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
