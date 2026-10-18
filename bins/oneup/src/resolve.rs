//! The user's answers for what can't sync (`crate::unsynced`): `oneup
//! unsynced resolve`, which the settings window's buttons run too.
//!
//! An answer is queued in the store's `meta` ([`KEY`]) and the agent is
//! nudged. Its next round takes the queue ([`apply_queued`]), checks each
//! answer again against the state then and carries it out through
//! `oneup_exec::resolve`, so what follows is planned, reviewed by the safety
//! gate (its circuit breakers included) and run like any change. Each answer
//! lands in the activity log, done or refused, and an answered item leaves
//! the record at once.
//!
//! A rename stays queued until a round runs it: a round can end before
//! running anything (a safety stop, a pause), and a rename isn't planned
//! again from marks as the planner's own operations are.
//!
//! Excluding an item changes the rules (`crate::filetree::set_sync`), as the
//! Files page does; it isn't queued.

use anyhow::Result;
use oneup_exec::resolve::{self as core, Change};
use oneup_store::{ActivityEntry, ActivityOutcome, Ops, Store};
use oneup_sync::{LocalId, Op, ROOT, RemoteId, Sid};
use serde_json::{Value, json};
use sha2::Digest;

use crate::unsynced::{Item, Record, Why};

/// In `meta`: the answers waiting for the agent (JSON).
pub const KEY: &str = "unsynced.resolve";

/// At most this many wait (each is one item, or one folder with what's in it).
pub const MAX: usize = 1_000;

/// What the user can do about an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Gone from this PC: delete it in OneDrive too (to its recycle bin).
    DeleteInOnedrive,
    /// Gone from this PC, in a backup pair: leave it in OneDrive, as a
    /// backup does with what this PC deletes.
    KeepInOnedrive,
    /// Gone from this PC: download it again.
    Restore,
    /// Gone from OneDrive: delete it here too (to the version store).
    DeleteHere,
    /// Gone from OneDrive: upload it again.
    Upload,
    Rename,
    /// Leave it out of sync with a rule.
    Exclude,
}

impl Choice {
    pub const ALL: [Choice; 7] = [
        Choice::DeleteInOnedrive,
        Choice::KeepInOnedrive,
        Choice::Restore,
        Choice::DeleteHere,
        Choice::Upload,
        Choice::Rename,
        Choice::Exclude,
    ];

    /// As the command line and the queue spell it.
    pub fn code(self) -> &'static str {
        match self {
            Choice::DeleteInOnedrive => "delete-in-onedrive",
            Choice::KeepInOnedrive => "keep-in-onedrive",
            Choice::Restore => "restore",
            Choice::DeleteHere => "delete-here",
            Choice::Upload => "upload",
            Choice::Rename => "rename",
            Choice::Exclude => "exclude",
        }
    }

    pub fn from_code(s: &str) -> Option<Choice> {
        Choice::ALL.into_iter().find(|c| c.code() == s)
    }

    /// The button.
    pub fn label(self) -> &'static str {
        match self {
            Choice::DeleteInOnedrive => "Delete in OneDrive",
            Choice::KeepInOnedrive => "Keep in OneDrive",
            Choice::Restore => "Restore",
            Choice::DeleteHere => "Delete here",
            Choice::Upload => "Upload",
            Choice::Rename => "Rename",
            Choice::Exclude => "Exclude",
        }
    }

    /// Removes something (recoverably): asked for in so many words.
    pub fn deletes(self) -> bool {
        matches!(self, Choice::DeleteInOnedrive | Choice::DeleteHere)
    }

    /// Fits many items at once (`--all`, the window's "all" buttons).
    pub fn in_bulk(self) -> bool {
        self != Choice::Rename
    }
}

/// What can be done about an item held for `why`. `backup`: the pair is a
/// backup, which never changes this PC and never deletes in OneDrive.
pub fn choices(why: Why, backup: bool) -> &'static [Choice] {
    match why {
        Why::GoneHere if backup => &[Choice::KeepInOnedrive],
        Why::GoneHere => &[Choice::DeleteInOnedrive, Choice::Restore],
        Why::GoneThere if backup => &[Choice::Upload],
        Why::GoneThere => &[Choice::DeleteHere, Choice::Upload],
        Why::NameClash | Why::InvalidName => &[Choice::Rename],
        Why::TooLarge | Why::Special | Why::NameTaken => &[Choice::Exclude],
        // A backup can't do what it waits for, and a folder still holding
        // items needs a look at them first (`inside`).
        Why::OneWay | Why::NotEmpty => &[],
    }
}

/// One answer, for the item at `path` in the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub path: String,
    pub choice: Choice,
    /// The new name, for a rename.
    pub name: Option<String>,
    /// What it concerned when chosen (`oneup_exec::resolve::Change::
    /// fingerprint`: every record, by its sid and linked IDs): it's carried
    /// out only if it still concerns exactly those, never another item that
    /// took the path since, nor more items inside a folder.
    pub id: Option<String>,
}

/// The answers waiting (unreadable counts as none: the user can answer
/// again).
pub fn queued(o: &Ops<'_>) -> oneup_store::Result<Vec<Request>> {
    let Some(v) = o.meta_get(KEY)?.and_then(|s| serde_json::from_str::<Value>(&s).ok()) else { return Ok(Vec::new()) };
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(Request {
                path: r["path"].as_str()?.to_string(),
                choice: Choice::from_code(r["choice"].as_str()?)?,
                name: r["name"].as_str().map(str::to_string),
                id: r["id"].as_str().map(str::to_string),
            })
        })
        .collect())
}

fn save(o: &Ops<'_>, q: &[Request]) -> oneup_store::Result<()> {
    if q.is_empty() {
        return o.meta_delete(KEY);
    }
    let v: Vec<Value> =
        q.iter().map(|r| json!({ "path": r.path, "choice": r.choice.code(), "name": r.name, "id": r.id })).collect();
    o.meta_set(KEY, &Value::from(v).to_string())
}

/// Adds answers to the queue, replacing one waiting for the same item.
/// Refused, adding none, if that would make more than [`MAX`] wait.
pub fn ask(store: &mut Store, asked: &[Request]) -> Result<()> {
    let waiting = store.write(|o| {
        let mut q = queued(o)?;
        q.retain(|r| !asked.iter().any(|a| a.path == r.path));
        if q.len() + asked.len() > MAX {
            return Ok(Some(q.len()));
        }
        q.extend(asked.iter().cloned());
        save(o, &q)?;
        Ok(None)
    })?;
    if let Some(n) = waiting {
        anyhow::bail!("{n} answers wait already (at most {MAX}); let OneUpDrive carry them out first");
    }
    Ok(())
}

/// Which item the record's `item` is now, as the window showed it: its
/// Synced record, or for a name to change its file (ID, size and
/// last-write time). `None` if it can't be found. An answer the window
/// gives for an item is refused if this changed since it was listed.
pub fn subject(o: &Ops<'_>, item: &Item) -> oneup_store::Result<Option<String>> {
    if matches!(item.why, Why::NameClash | Why::InvalidName) {
        let Some(l) = local_at(o, &item.path)? else { return Ok(None) };
        return Ok(o.local_get(l.0)?.map(|r| format!("l:{:x}:{}:{}", r.file_id, r.size, r.mtime)));
    }
    Ok(record_of(o, item)?.map(|sid| format!("s:{sid}")))
}

/// The parts of a record path (`/docs/a.txt`).
fn parts(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|p| !p.is_empty())
}

/// Of the items named like `want` as Windows compares names, the one
/// named exactly so, or the only one; none if that's ambiguous.
fn pick<T>(mut found: Vec<T>, name: impl Fn(&T) -> &str, want: &str) -> Option<T> {
    if let Some(i) = found.iter().position(|t| name(t) == want) {
        return Some(found.swap_remove(i));
    }
    (found.len() == 1).then(|| found.swap_remove(0))
}

/// The local item at record path `path`, from the Local tree.
pub fn local_at(o: &Ops<'_>, path: &str) -> oneup_store::Result<Option<LocalId>> {
    let Some(root) = o.local_root()? else { return Ok(None) };
    let mut cur = root.root_file_id;
    for part in parts(path) {
        match pick(o.local_named(cur, part)?, |n| n.name.as_str(), part) {
            Some(n) => cur = n.file_id,
            None => return Ok(None),
        }
    }
    Ok(Some(LocalId(cur)))
}

/// The OneDrive item at record path `path` (relative to the synced
/// OneDrive folder), from the mirror.
pub fn remote_at(o: &Ops<'_>, path: &str) -> oneup_store::Result<Option<RemoteId>> {
    let Some(mut cur) = o.synced_get(ROOT)?.and_then(|b| b.remote) else { return Ok(None) };
    for part in parts(path) {
        match pick(o.remote_named(&cur.0, part)?, |r| r.name.as_str(), part) {
            Some(r) => cur = RemoteId(r.id),
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// The Synced record of the record's `item`: found from OneDrive's side
/// for one gone from this PC (its path is OneDrive's), from this PC's
/// otherwise.
fn record_of(o: &Ops<'_>, item: &Item) -> oneup_store::Result<Option<Sid>> {
    let b = if item.why == Why::GoneHere {
        match remote_at(o, &item.path)? {
            Some(r) => o.synced_by_remote(&r.0)?,
            None => None,
        }
    } else {
        match local_at(o, &item.path)? {
            Some(l) => o.synced_by_local(l.0)?,
            None => None,
        }
    };
    Ok(b.map(|b| b.sid))
}

/// Works out `choice` for the record's `item` against the state now,
/// changing nothing. `present`: whether a local item still opens by its ID
/// in the sync folder (the agent asks the folder; the command line, which
/// only checks before queueing, says no). The inner error is why it's
/// refused, for people.
pub fn check(
    o: &Ops<'_>,
    item: &Item,
    choice: Choice,
    name: Option<&str>,
    backup: bool,
    present: &dyn Fn(LocalId) -> bool,
) -> oneup_store::Result<std::result::Result<Change, String>> {
    if !choices(item.why, backup).contains(&choice) || choice == Choice::Exclude {
        return Ok(Err(format!("not offered for \"{}\"", item.why.title())));
    }
    let worked = if choice == Choice::Rename {
        let name = name.unwrap_or_default().trim();
        if oneup_plan::names::is_invalid(name) {
            return Ok(Err("OneDrive doesn't allow that name".into()));
        }
        if oneup_plan::names::not_synced(name).is_some() {
            return Ok(Err("names like that are never synced".into()));
        }
        let Some(l) = local_at(o, &item.path)? else { return Ok(Err("it isn't on this PC any more".into())) };
        core::rename(o, l, name)
    } else {
        let Some(sid) = record_of(o, item)? else { return Ok(Err("it isn't synced any more".into())) };
        match choice {
            Choice::DeleteInOnedrive | Choice::KeepInOnedrive => core::delete_there(o, sid, present),
            Choice::Restore => core::restore(o, sid, present),
            Choice::DeleteHere => core::delete_here(o, sid),
            _ => core::upload(o, sid),
        }
    };
    match worked {
        Ok(c) => Ok(Ok(c)),
        Err(core::Error::Refused(r)) => Ok(Err(r.to_string())),
        Err(core::Error::Store(e)) => Err(e),
    }
}

/// What an answer did, for the activity log.
fn done(choice: Choice, items: usize, name: Option<&str>) -> String {
    let more = if items > 1 { format!(" ({items} items)") } else { String::new() };
    match choice {
        Choice::DeleteInOnedrive => format!("you chose to delete it in OneDrive{more}"),
        Choice::KeepInOnedrive => format!("you chose to keep it in OneDrive{more}"),
        Choice::Restore => format!("you chose to restore it from OneDrive{more}"),
        Choice::DeleteHere => format!("you chose to delete it on this PC{more}"),
        Choice::Upload => format!("you chose to upload it again{more}"),
        Choice::Rename => format!("you chose to rename it to {}", name.unwrap_or_default()),
        Choice::Exclude => "you chose to exclude it".into(),
    }
}

fn entry(outcome: ActivityOutcome, path: &str, detail: String) -> ActivityEntry {
    ActivityEntry {
        seq: 0,
        at_ms: crate::control::now_ms(),
        outcome,
        action: "resolve".into(),
        path: path.into(),
        detail,
    }
}

/// What a round takes from the queue ([`apply_queued`]).
#[derive(Debug, Default)]
pub struct Applied {
    /// The renames to add to the round, with the answers they come from.
    pub ops: Vec<(Op, Request)>,
    /// For the activity log.
    pub log: Vec<ActivityEntry>,
}

/// At the start of a round: carries out the answers waiting, each checked
/// against the state now in a transaction of its own. Everything but a
/// rename leaves the queue here; a rename comes back to be added to the
/// round and leaves once it ran ([`ran`]). Items answered leave the record.
pub fn apply_queued(store: &mut Store, backup: bool, present: &dyn Fn(LocalId) -> bool) -> Result<Applied> {
    let mut out = Applied::default();
    if store.read(|o| o.meta_get(KEY))?.is_none() {
        return Ok(out);
    }
    let record = crate::unsynced::load(store)?;
    let mut answered = Vec::new();
    let mut i = 0;
    loop {
        let step = store.write(|o| {
            let mut q = queued(o)?;
            let Some(req) = q.get(i).cloned() else { return Ok(None) };
            let item = crate::unsynced::find(&record, &req.path);
            let outcome = match item {
                None => Err("it isn't in the list any more".to_string()),
                Some(item) => match check(o, item, req.choice, req.name.as_deref(), backup, present)? {
                    Ok(c) if Some(c.fingerprint()) != req.id => {
                        Err("it changed since you chose; choose again".to_string())
                    }
                    Ok(c) => {
                        let items = c.items();
                        Ok((items, c.commit(o)?))
                    }
                    Err(why) => Err(why),
                },
            };
            let keep = matches!(outcome, Ok((_, Some(_))));
            if keep {
                i += 1;
            } else {
                q.remove(i);
                save(o, &q)?;
            }
            Ok(Some((req, outcome)))
        })?;
        let Some((req, outcome)) = step else { break };
        match outcome {
            // A rename is logged by the round that runs it, as any is
            // (it's worked out again each round until one does).
            Ok((_, Some(op))) => out.ops.push((op, req)),
            Ok((items, None)) => {
                let detail = done(req.choice, items, req.name.as_deref());
                out.log.push(entry(ActivityOutcome::Note, &req.path, detail));
                answered.push(req.path);
            }
            Err(why) => {
                let detail = format!("{}: not done; {why}", req.choice.label());
                out.log.push(entry(ActivityOutcome::Failed, &req.path, detail));
            }
        }
    }
    crate::unsynced::answered(store, &answered)?;
    Ok(out)
}

/// After a round: the renames it ran (done, skipped or failed; the round's
/// report says how) leave the queue. One the round didn't get to stays.
pub fn ran(store: &mut Store, applied: &[(Op, Request)], report: &oneup_exec::Report) -> Result<()> {
    let ran = |op: &Op| {
        report.done.contains(op)
            || report.skipped.iter().any(|(o, _)| o == op)
            || report.failed.iter().any(|(o, _)| o == op)
    };
    let gone: Vec<&Request> = applied.iter().filter(|(op, _)| ran(op)).map(|(_, r)| r).collect();
    if gone.is_empty() {
        return Ok(());
    }
    store.write(|o| {
        let mut q = queued(o)?;
        q.retain(|r| !gone.contains(&r));
        save(o, &q)
    })?;
    Ok(())
}

/// What a choice for some listed items would do ([`prepare`]).
#[derive(Debug, Default)]
pub struct Prepared {
    /// The answers to queue: one per item, a folder answering for what's in it.
    pub asked: Vec<Request>,
    /// How many items they concern, folders' contents included.
    pub items: usize,
    /// Those refused now, and why.
    pub refused: Vec<String>,
    /// Of all of it (16 hex digits): the same exactly when the same answers
    /// would concern the same items. What the window showed is carried out
    /// only if it still is (`--expect`).
    pub code: String,
}

/// The listed items `choice` is for: those at `paths` (as the record
/// spells them), or with `None` every item it fits.
pub fn select<'a>(
    record: &'a Record,
    choice: Option<Choice>,
    paths: Option<&[String]>,
    backup: bool,
) -> Result<Vec<&'a Item>> {
    let Some(paths) = paths else {
        let Some(c) = choice.filter(|c| c.in_bulk()) else {
            anyhow::bail!(
                "--all needs --delete-in-onedrive, --keep-in-onedrive, --restore, --delete-here, --upload or --exclude"
            );
        };
        return Ok(record.items.iter().filter(|i| choices(i.why, backup).contains(&c)).collect());
    };
    paths
        .iter()
        .map(|p| {
            crate::unsynced::find(record, p).ok_or_else(|| anyhow::anyhow!("{p} isn't in the list (`oneup unsynced`)"))
        })
        .collect()
}

/// Works out `choice` for `items` against the store as it is (changing
/// nothing): the answers to queue, each bound to its item, and how many
/// items they concern. An item inside another chosen folder goes with it.
pub fn prepare(store: &Store, items: &[&Item], choice: Choice, name: Option<&str>, backup: bool) -> Result<Prepared> {
    let mut items: Vec<&Item> = items.to_vec();
    items.sort_by(|x, y| x.path.cmp(&y.path));
    let mut tops: Vec<&Item> = Vec::new();
    for i in items {
        if !tops.iter().any(|t| below(&i.path, &t.path)) {
            tops.push(i);
        }
    }
    let mut out = Prepared::default();
    let mut code = sha2::Sha256::new();
    code.update(choice.code().as_bytes());
    for i in tops {
        // An exclusion is a rule for the path: its code is of the paths.
        let worked = if choice == Choice::Exclude {
            Ok((1, String::new()))
        } else {
            store.read(|o| Ok(check(o, i, choice, name, backup, &|_| false)?.map(|c| (c.items(), c.fingerprint()))))?
        };
        match worked {
            Ok((n, id)) => {
                out.items += n;
                code.update(i.path.as_bytes());
                code.update([0]);
                code.update(id.as_bytes());
                let name = name.map(|n| n.trim().to_string());
                let id = (!id.is_empty()).then_some(id);
                out.asked.push(Request { path: i.path.clone(), choice, name, id });
            }
            Err(why) => out.refused.push(format!("{}: {why}", i.path)),
        }
    }
    out.code = code.finalize()[..8].iter().map(|b| format!("{b:02x}")).collect();
    Ok(out)
}

/// Whether record path `path` is inside folder `top`.
pub fn below(path: &str, top: &str) -> bool {
    let (p, t) = (path.as_bytes(), top.trim_end_matches('/').as_bytes());
    p.len() > t.len() && p[..t.len()].eq_ignore_ascii_case(t) && p[t.len()] == b'/'
}

/// An item inside a folder held as not empty ([`inside`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inside {
    pub name: String,
    pub folder: bool,
    /// The rules leave it out (sync doesn't list it).
    pub excluded: bool,
}

/// What a folder held as not empty still holds ([`inside`]).
#[derive(Debug, Default)]
pub struct Contents {
    /// `local` (on this PC), `remote` (in OneDrive), or empty if it's gone.
    pub side: &'static str,
    /// How many items in all.
    pub total: usize,
    /// The first `limit` of them, by name.
    pub items: Vec<Inside>,
}

/// What's inside a folder held as not empty: on this PC if it's still
/// here, else in OneDrive, read from the trees as last seen (nothing is
/// opened or listed); at most `limit` items.
pub fn inside(o: &Ops<'_>, path: &str, limit: usize) -> oneup_store::Result<Contents> {
    if let Some(l) = local_at(o, path)? {
        let (total, kids) = o.local_children_first(l.0, limit)?;
        let items = kids
            .into_iter()
            .map(|k| Inside {
                folder: k.kind == oneup_store::LocalKind::Folder,
                excluded: k.skip == oneup_store::Skip::Excluded,
                name: k.name,
            })
            .collect();
        return Ok(Contents { side: "local", total, items });
    }
    if let Some(r) = remote_at(o, path)? {
        let (total, kids) = o.remote_children_first(&r.0, limit)?;
        let items = kids
            .into_iter()
            .map(|(k, excluded)| Inside { folder: k.kind == oneup_store::Kind::Folder, excluded, name: k.name })
            .collect();
        return Ok(Contents { side: "remote", total, items });
    }
    Ok(Contents::default())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn each_kind_offers_what_resolves_it_and_a_backup_never_changes_this_pc() {
        use Choice as C;
        assert_eq!(choices(Why::GoneHere, false), [C::DeleteInOnedrive, C::Restore]);
        assert_eq!(choices(Why::GoneHere, true), [C::KeepInOnedrive]);
        assert_eq!(choices(Why::GoneThere, false), [C::DeleteHere, C::Upload]);
        assert_eq!(choices(Why::GoneThere, true), [C::Upload]);
        for w in [Why::NameClash, Why::InvalidName] {
            assert_eq!(choices(w, false), [C::Rename]);
        }
        for w in [Why::TooLarge, Why::Special, Why::NameTaken] {
            assert_eq!(choices(w, true), [C::Exclude]);
        }
        assert!(choices(Why::OneWay, true).is_empty() && choices(Why::NotEmpty, false).is_empty());
        for c in Choice::ALL {
            assert_eq!(Choice::from_code(c.code()), Some(c));
        }
        // Only deletions ask in so many words.
        assert_eq!(Choice::ALL.iter().filter(|c| c.deletes()).count(), 2);
    }

    fn req(path: &str, choice: Choice) -> Request {
        Request { path: path.into(), choice, name: None, id: None }
    }

    #[test]
    fn answers_queue_once_per_item_and_unreadable_queues_read_empty() {
        let mut s = Store::open_in_memory().unwrap();
        ask(&mut s, &[req("/a.txt", Choice::Restore), req("/b", Choice::DeleteHere)]).unwrap();
        // Another item: names differing only in case can both be listed.
        ask(&mut s, &[req("/A.txt", Choice::DeleteInOnedrive), req("/b", Choice::Upload)]).unwrap();
        let q = s.read(queued).unwrap();
        assert_eq!(
            q,
            [req("/a.txt", Choice::Restore), req("/A.txt", Choice::DeleteInOnedrive), req("/b", Choice::Upload)]
        );
        s.write(|o| o.meta_set(KEY, "{nonsense")).unwrap();
        assert!(s.read(queued).unwrap().is_empty());
    }

    #[test]
    fn a_full_queue_refuses_more_answers_and_keeps_what_waits() {
        let mut s = Store::open_in_memory().unwrap();
        let many: Vec<Request> = (0..MAX).map(|n| req(&format!("/f{n}"), Choice::Restore)).collect();
        ask(&mut s, &many).unwrap();
        assert!(ask(&mut s, &[req("/one-more", Choice::Restore)]).is_err());
        // Replacing an answer that waits isn't one more.
        ask(&mut s, &[req("/f7", Choice::DeleteInOnedrive)]).unwrap();
        let q = s.read(queued).unwrap();
        assert_eq!(q.len(), MAX);
        assert!(!q.iter().any(|r| r.path == "/one-more"));
    }

    #[test]
    fn excluding_many_is_bound_to_the_items_shown() {
        let mut s = Store::open_in_memory().unwrap();
        let set = |s: &mut Store, record: &str| s.write(|o| o.meta_set(crate::unsynced::KEY, record)).unwrap();
        let code = |s: &Store| {
            let record = crate::unsynced::load(s).unwrap();
            let items = select(&record, Some(Choice::Exclude), None, false).unwrap();
            prepare(s, &items, Choice::Exclude, None, false).unwrap()
        };
        set(&mut s, r#"{"items":[["/a.iso","too_large"],["/b.iso","too_large"]]}"#);
        let shown = code(&s);
        assert_eq!((shown.items, shown.asked.len()), (2, 2));
        assert_eq!(code(&s).code, shown.code);
        set(&mut s, r#"{"items":[["/a.iso","too_large"],["/b.iso","too_large"],["/c.iso","too_large"]]}"#);
        assert_ne!(code(&s).code, shown.code, "one more since it was shown");
    }

    #[test]
    fn a_listed_path_is_found_as_spelled_first() {
        let s = Store::open_in_memory().unwrap();
        let mut s = s;
        let record = r#"{"items":[["/A.txt","name_clash"],["/a.txt","name_clash"],["/Docs","gone_here"]]}"#;
        s.write(|o| o.meta_set(crate::unsynced::KEY, record)).unwrap();
        let r = crate::unsynced::load(&s).unwrap();
        assert_eq!(crate::unsynced::find(&r, "/a.txt").unwrap().path, "/a.txt");
        assert_eq!(crate::unsynced::find(&r, "/A.txt").unwrap().path, "/A.txt");
        assert!(crate::unsynced::find(&r, "/A.TXT").is_none(), "two spelled so in another case");
        assert_eq!(crate::unsynced::find(&r, "/docs").unwrap().path, "/Docs");
    }

    #[test]
    fn a_refused_answer_is_logged_and_leaves_the_queue() {
        let mut s = Store::open_in_memory().unwrap();
        let record = r#"{"items":[["/gone.txt","gone_here"],["/big.iso","too_large"]],"more":false}"#;
        s.write(|o| o.meta_set(crate::unsynced::KEY, record)).unwrap();
        ask(
            &mut s,
            &[req("/gone.txt", Choice::Restore), req("/big.iso", Choice::Upload), req("/nope", Choice::Restore)],
        )
        .unwrap();
        let a = apply_queued(&mut s, false, &|_| false).unwrap();
        assert!(a.ops.is_empty());
        let why: Vec<&str> = a.log.iter().map(|e| e.detail.as_str()).collect();
        assert_eq!(
            why,
            [
                "Restore: not done; it isn't synced any more",
                "Upload: not done; not offered for \"Too large\"",
                "Restore: not done; it isn't in the list any more",
            ]
        );
        assert!(a.log.iter().all(|e| e.outcome == ActivityOutcome::Failed && e.action == "resolve"));
        assert!(s.read(queued).unwrap().is_empty());
    }

    fn base(
        sid: Sid,
        parent: Option<Sid>,
        name: &str,
        kind: oneup_sync::NodeKind,
        local: u128,
        remote: &str,
    ) -> oneup_sync::Base {
        oneup_sync::Base {
            sid,
            parent,
            name: name.into(),
            kind,
            size: 0,
            hash: None,
            local: Some(LocalId(local)),
            local_mtime: 0,
            remote: Some(RemoteId(remote.into())),
            remote_etag: "e1".into(),
        }
    }

    /// A store with the synced folder (local item 1, `root` in OneDrive)
    /// and, under it, a folder `old` with `a.txt`: synced, then gone from
    /// this PC (their local items aren't in the Local tree), still in
    /// OneDrive; and `old/b.txt`, gone from both.
    fn gone_here() -> Store {
        use oneup_sync::NodeKind;
        let mut s = Store::open_in_memory().unwrap();
        let row = |id: &str, parent: Option<&str>, name: &str, kind| oneup_store::RemoteRow {
            id: id.into(),
            parent_id: parent.map(Into::into),
            name: name.into(),
            kind,
            size: 0,
            etag: Some("e1".into()),
            qxh: None,
            mtime_ms: None,
        };
        s.write(|o| {
            o.local_put(&oneup_store::LocalRow {
                file_id: 1,
                parent_id: None,
                name: String::new(),
                name_lossy: false,
                kind: oneup_store::LocalKind::Root,
                size: 0,
                mtime: 0,
                attributes: 0x10,
                reparse_tag: 0,
                skip: oneup_store::Skip::None,
            })?;
            o.upsert_item(&row("root", None, "", oneup_store::Kind::Root), false)?;
            o.upsert_item(&row("r1", Some("root"), "old", oneup_store::Kind::Folder), false)?;
            o.upsert_item(&row("r2", Some("r1"), "a.txt", oneup_store::Kind::File), false)?;
            o.synced_put(&base(ROOT, None, "", NodeKind::Folder, 1, "root"))?;
            o.synced_put(&base(1, Some(ROOT), "old", NodeKind::Folder, 5, "r1"))?;
            o.synced_put(&base(2, Some(1), "a.txt", NodeKind::File, 6, "r2"))?;
            o.synced_put(&base(3, Some(1), "b.txt", NodeKind::File, 7, "r3"))?;
            let record = r#"{"items":[["/old","gone_here"],["/old/a.txt","gone_here"],["/x.iso","too_large"]]}"#;
            o.meta_set(crate::unsynced::KEY, record)
        })
        .unwrap();
        s
    }

    /// What `oneup unsynced resolve` queues for `paths`.
    fn prepared(s: &Store, paths: &[&str], choice: Choice) -> Prepared {
        let record = crate::unsynced::load(s).unwrap();
        let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        let items = select(&record, Some(choice), Some(&paths), false).unwrap();
        prepare(s, &items, choice, None, false).unwrap()
    }

    #[test]
    fn an_answer_notes_what_the_planner_acts_on_and_the_item_leaves_the_list() {
        use oneup_sync::{Seen, Trees};
        let mut s = gone_here();
        // Gone from both sides isn't counted: nothing to delete there.
        let p = prepared(&s, &["/OLD", "/old/a.txt"], Choice::DeleteInOnedrive);
        assert_eq!((p.asked.len(), p.items), (1, 2), "the folder answers for what's in it");
        ask(&mut s, &p.asked).unwrap();
        // Still on this PC after all (it opens by its ID): refused.
        let a = apply_queued(&mut s, false, &|id| id == LocalId(5)).unwrap();
        assert_eq!(a.log[0].detail, "Delete in OneDrive: not done; it's on this PC");
        assert_eq!(crate::unsynced::load(&s).unwrap().items.len(), 3);

        let asked = prepared(&s, &["/old"], Choice::DeleteInOnedrive).asked;
        ask(&mut s, &asked).unwrap();
        let a = apply_queued(&mut s, false, &|_| false).unwrap();
        assert_eq!(a.log.len(), 1);
        assert_eq!((a.log[0].outcome, a.log[0].path.as_str()), (ActivityOutcome::Note, "/old"));
        assert_eq!(a.log[0].detail, "you chose to delete it in OneDrive (2 items)");
        s.read(|o| {
            assert_eq!(o.local(LocalId(5))?, Seen::Deleted);
            assert_eq!(o.local(LocalId(6))?, Seen::Deleted);
            assert_eq!(o.local(LocalId(7))?, Seen::Missing, "gone from both: no evidence");
            assert!(o.dirty_scope()?.folders.contains(&1), "planned again");
            Ok(())
        })
        .unwrap();
        let left: Vec<String> = crate::unsynced::load(&s).unwrap().items.into_iter().map(|i| i.path).collect();
        assert_eq!(left, ["/x.iso"], "the folder and what it held leave the list");
        assert!(s.read(queued).unwrap().is_empty());
    }

    #[test]
    fn an_answer_is_for_the_item_chosen_not_another_at_its_path_since() {
        use oneup_sync::{NodeKind, Seen, Trees};
        let mut s = gone_here();
        let p = prepared(&s, &["/old"], Choice::DeleteInOnedrive);
        assert_eq!(p.asked[0].id.as_ref().map(String::len), Some(32));
        ask(&mut s, &p.asked).unwrap();
        // Before the agent gets to it, another synced item is at that path.
        s.write(|o| {
            o.synced_remove(1)?;
            o.synced_put(&base(9, Some(ROOT), "old", NodeKind::Folder, 8, "r1"))
        })
        .unwrap();
        let a = apply_queued(&mut s, false, &|_| false).unwrap();
        assert_eq!(a.log[0].detail, "Delete in OneDrive: not done; it changed since you chose; choose again");
        assert_eq!(s.read(|o| o.local(LocalId(8))).unwrap(), Seen::Missing);
        // An answer queued without saying which item (by an older version) too.
        ask(&mut s, &[req("/old", Choice::DeleteInOnedrive)]).unwrap();
        let a = apply_queued(&mut s, false, &|_| false).unwrap();
        assert_eq!(a.log[0].outcome, ActivityOutcome::Failed);
    }

    #[test]
    fn a_folder_answer_is_for_what_it_held_when_chosen_not_more() {
        let mut s = gone_here();
        let p = prepared(&s, &["/old"], Choice::DeleteInOnedrive);
        assert_eq!(p.items, 2);
        // The same choice gives the same code; the window's preview relies on it.
        assert_eq!(prepared(&s, &["/old"], Choice::DeleteInOnedrive).code, p.code);
        ask(&mut s, &p.asked).unwrap();
        // Before the agent gets to it, b.txt (gone from both sides when
        // chosen) is back in OneDrive: deleting the folder would now take it
        // too, which wasn't shown.
        let row = oneup_store::RemoteRow {
            id: "r3".into(),
            parent_id: Some("r1".into()),
            name: "b.txt".into(),
            kind: oneup_store::Kind::File,
            size: 0,
            etag: Some("e1".into()),
            qxh: None,
            mtime_ms: None,
        };
        s.write(|o| o.upsert_item(&row, false)).unwrap();
        assert_ne!(prepared(&s, &["/old"], Choice::DeleteInOnedrive).code, p.code);
        let a = apply_queued(&mut s, false, &|_| false).unwrap();
        assert_eq!(a.log[0].detail, "Delete in OneDrive: not done; it changed since you chose; choose again");
        assert_eq!(crate::unsynced::load(&s).unwrap().items.len(), 3, "still listed");
    }

    #[test]
    fn restoring_drops_the_records_and_a_rename_waits_until_a_round_runs_it() {
        let mut s = gone_here();
        let asked = prepared(&s, &["/old"], Choice::Restore).asked;
        ask(&mut s, &asked).unwrap();
        let a = apply_queued(&mut s, false, &|_| false).unwrap();
        assert_eq!(a.log[0].detail, "you chose to restore it from OneDrive (3 items)");
        s.read(|o| {
            assert!(o.synced_get(1)?.is_none() && o.synced_get(2)?.is_none());
            assert!(o.dirty_scope()?.folders.contains(&ROOT), "where it reads as new");
            Ok(())
        })
        .unwrap();

        let rename = Request { name: Some("y".into()), ..req("/x", Choice::Rename) };
        let other = Request { name: Some("w".into()), ..req("/z", Choice::Rename) };
        s.write(|o| save(o, &[rename.clone(), other.clone()])).unwrap();
        let op = |n: u128| Op::RenameLocal {
            local: oneup_sync::LocalPre { id: LocalId(n), stamp: None },
            dir: LocalId(1),
            name: "y".into(),
        };
        let applied = [(op(7), rename), (op(8), other.clone())];
        let report = oneup_exec::Report { skipped: vec![(op(7), oneup_exec::Skip::Changed)], ..Default::default() };
        ran(&mut s, &applied, &report).unwrap();
        assert_eq!(s.read(queued).unwrap(), [other], "not run yet: still waiting");
    }
}
