//! The decrypted view of a vault (see the crate docs).
//!
//! [`project`] first checks the keys against the vault's signed
//! configuration (`vault.cryptomator`), so keys that belong to another
//! vault (remembered ones, after the vault in OneDrive was replaced) never
//! encrypt anything into it. Then it walks the vault from its top folder:
//! each folder's entries
//! are in the storage folder `d/XX/…` named after the folder's ID, each
//! entry's name decrypts with that ID, and a folder's ID is in its
//! `dir.c9r`. The small files (`dir.c9r`, a long name's `name.c9s`) are
//! kept in a bounded [`Cache`]; an evicted file is read again when needed.
//!
//! Cost: the walk reads the vault's part of the cloud listing, one row per
//! encrypted item, every time it runs (the agent runs it when a round
//! starts). Fine for vaults of up to about 100,000 items; larger ones would
//! need it to follow changed folders only.
//!
//! [`apply`] writes the view into the vault pair's store as its Remote
//! tree. Removals need evidence: a decrypted item whose encrypted item is
//! still in the listing but couldn't be placed (an unreadable name, a
//! folder whose ID can't be read) keeps its last row and is reported.

use std::collections::{HashMap, HashSet, VecDeque};

use oneup_store::{Kind, RemoteRow, Store};
use oneup_vault::config::{self, Config};
use oneup_vault::content::content_size;
use oneup_vault::names::{self, CONTENTS_FILE, DIR_FILE, DIR_ID_BACKUP, NAME_FILE, SHORT_EXT, SYMLINK_FILE};

use crate::{Error, Node, NodeKind, Nodes, Result, Vault};

/// Where the vault's encrypted items come from: the cloud listing, and
/// downloads of small files.
pub trait Source: Send + Sync {
    /// The listed children of a cloud folder.
    fn children(&self, id: &str) -> Result<Vec<RemoteRow>>;
    /// First child by ID matching an ASCII storage name, ignoring ASCII case.
    /// Must look up the name directly rather than listing its siblings.
    fn child_named(&self, parent: &str, name: &str) -> Result<Option<RemoteRow>>;
    /// A listed item.
    fn item(&self, id: &str) -> Result<Option<RemoteRow>>;
    /// The content of a small file (at most a few KiB).
    fn fetch(&self, item: &RemoteRow) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

/// Small files already read, at most one version per item ID. FIFO eviction
/// bounds both entry overhead and retained string allocations across refreshes.
#[derive(Debug)]
pub struct Cache {
    entries: HashMap<String, Cached>,
    order: VecDeque<String>,
    bytes: usize,
    entry_limit: usize,
    byte_limit: usize,
}

#[derive(Debug)]
struct Cached {
    etag: String,
    text: String,
    bytes: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self::with_limits(4096, 2 * 1024 * 1024)
    }
}

impl Cache {
    fn with_limits(entry_limit: usize, byte_limit: usize) -> Self {
        Self { entries: HashMap::new(), order: VecDeque::new(), bytes: 0, entry_limit, byte_limit }
    }

    fn insert(&mut self, id: &str, etag: &str, text: &str) {
        let etag = etag.to_owned();
        let text = text.to_owned();
        if let Some(old) = self.entries.get_mut(id) {
            // Replacing a version keeps its FIFO position and both ID strings.
            let ids = old.bytes - old.etag.capacity() - old.text.capacity();
            self.bytes -= old.bytes;
            *old = Cached { bytes: ids + etag.capacity() + text.capacity(), etag, text };
            self.bytes += old.bytes;
        } else {
            let key = id.to_owned();
            let queued = id.to_owned();
            let bytes = key.capacity() + queued.capacity() + etag.capacity() + text.capacity();
            self.entries.insert(key, Cached { etag, text, bytes });
            self.order.push_back(queued);
            self.bytes += bytes;
        }
        while self.entries.len() > self.entry_limit || self.bytes > self.byte_limit {
            let Some(id) = self.order.pop_front() else { break };
            if let Some(old) = self.entries.remove(&id) {
                self.bytes -= old.bytes;
            }
        }
    }

    async fn text(&mut self, src: &impl Source, item: &RemoteRow, max: usize) -> Result<String> {
        if item.size > max as i64 {
            return Err(Error::Fetch(format!("{} is too big", item.name)));
        }
        let etag = item.etag.as_deref().unwrap_or_default();
        if let Some(cached) = self.entries.get(&item.id).filter(|c| c.etag == etag) {
            if cached.text.len() > max {
                return Err(Error::Fetch(format!("{} is too big", item.name)));
            }
            return Ok(cached.text.clone());
        }
        let bytes = src.fetch(item).await?;
        if bytes.len() > max {
            return Err(Error::Fetch(format!("{} is too big", item.name)));
        }
        let text = String::from_utf8(bytes).map_err(|_| Error::Fetch(format!("{} isn't text", item.name)))?;
        self.insert(&item.id, etag, &text);
        Ok(text)
    }
}

/// The decrypted view: rows for the vault pair's Remote tree (parents
/// before children), how each maps to encrypted items, and what couldn't be
/// read. Problems name the encrypted items, never decrypted names: they
/// end up in the agent's log.
#[derive(Debug, Default)]
pub struct Projection {
    pub rows: Vec<RemoteRow>,
    pub nodes: Nodes,
    pub problems: Vec<String>,
}

fn named<'a>(rows: &'a [RemoteRow], name: &str) -> Option<&'a RemoteRow> {
    rows.iter().find(|r| r.name.eq_ignore_ascii_case(name))
}

/// The cloud folder holding the entries of the folder with `dir_id`.
fn storage(src: &impl Source, vault: &Vault, d: Option<&RemoteRow>, dir_id: &str) -> Result<Option<String>> {
    let Some(d) = d else { return Ok(None) };
    let path = names::dir_path(&vault.key, dir_id)?;
    let mut parts = path.split('/').skip(1); // "d"
    let (Some(a), Some(b)) = (parts.next(), parts.next()) else { return Ok(None) };
    let Some(first) = src.child_named(&d.id, a)?.filter(|r| r.kind == Kind::Folder) else {
        return Ok(None);
    };
    Ok(src.child_named(&first.id, b)?.filter(|r| r.kind == Kind::Folder).map(|r| r.id))
}

/// Works out the decrypted view (see the module docs).
pub async fn project(src: &impl Source, vault: &Vault, cache: &mut Cache) -> Result<Projection> {
    let top = src.item(&vault.root)?.ok_or(Error::NoVault)?;
    let top_children = src.children(&top.id)?;
    check_keys(src, vault, cache, &top_children).await?;
    let d = named(&top_children, "d").filter(|r| r.kind == Kind::Folder).cloned();
    let mut out = Projection::default();
    out.rows.push(RemoteRow {
        id: top.id.clone(),
        parent_id: None,
        name: String::new(),
        kind: Kind::Root,
        size: 0,
        etag: top.etag.clone(),
        qxh: None,
        mtime_ms: top.mtime_ms,
    });
    let mut queue = VecDeque::from([(top.id.clone(), names::ROOT_DIR_ID.to_string())]);
    out.nodes.insert(
        top.id.clone(),
        Node {
            kind: NodeKind::Root,
            parent: None,
            long: false,
            content: None,
            dir_id: Some(names::ROOT_DIR_ID.into()),
            storage: None,
        },
    );
    let mut seen_dirs = HashSet::new();
    while let Some((pid, dir_id)) = queue.pop_front() {
        // A folder ID twice would loop (a copied dir.c9r): the second is
        // shown empty.
        if !seen_dirs.insert(dir_id.clone()) {
            out.problems.push(format!("a folder ID is used twice ({pid}); one copy is shown empty"));
            continue;
        }
        let Some(store_id) = storage(src, vault, d.as_ref(), &dir_id)? else { continue };
        if let Some(n) = out.nodes.get_mut(&pid) {
            n.storage = Some(store_id.clone());
        }
        for entry in src.children(&store_id)? {
            if entry.name.eq_ignore_ascii_case(DIR_ID_BACKUP) {
                continue;
            }
            match entry_of(src, vault, cache, &entry, &dir_id).await {
                Ok(Some(e)) => {
                    let node = Node {
                        kind: if e.dir_id.is_some() { NodeKind::Dir } else { NodeKind::File },
                        parent: Some(pid.clone()),
                        long: e.long,
                        content: e.content.as_ref().map(|c| c.id.clone()),
                        dir_id: e.dir_id.clone(),
                        storage: None,
                    };
                    if let Some(c) = &e.content
                        && content_size(u64::try_from(c.size).unwrap_or(0)).is_none()
                    {
                        out.problems.push(format!("{}: not a valid encrypted file", entry.name));
                    }
                    out.rows.push(match &e.content {
                        Some(c) => RemoteRow {
                            id: entry.id.clone(),
                            parent_id: Some(pid.clone()),
                            name: e.name,
                            kind: Kind::File,
                            size: content_size(u64::try_from(c.size).unwrap_or(0))
                                .and_then(|s| i64::try_from(s).ok())
                                .unwrap_or(-1),
                            etag: c.etag.clone(),
                            qxh: c.qxh,
                            mtime_ms: c.mtime_ms,
                        },
                        None => RemoteRow {
                            id: entry.id.clone(),
                            parent_id: Some(pid.clone()),
                            name: e.name,
                            kind: Kind::Folder,
                            size: 0,
                            etag: entry.etag.clone(),
                            qxh: None,
                            mtime_ms: entry.mtime_ms,
                        },
                    });
                    if let Some(id) = &e.dir_id {
                        queue.push_back((entry.id.clone(), id.clone()));
                    }
                    out.nodes.insert(entry.id.clone(), node);
                }
                Ok(None) => {}
                Err(why) => out.problems.push(format!("{}: {why}", entry.name)),
            }
        }
    }
    // A file whose encrypted size is impossible can't be shown (reported
    // above).
    out.rows.retain(|r| r.size >= 0);
    Ok(out)
}

/// Checks the vault's keys against its configuration, and that names are
/// still shortened as this pair stores them.
async fn check_keys(src: &impl Source, vault: &Vault, cache: &mut Cache, top: &[RemoteRow]) -> Result<()> {
    let item = named(top, config::FILE)
        .filter(|r| r.kind == Kind::File)
        .ok_or_else(|| Error::Fetch(format!("the vault's {} is missing", config::FILE)))?;
    let token = cache.text(src, item, 16 * 1024).await?;
    let c = Config::verify(&token, &vault.key).map_err(|e| match e {
        oneup_vault::Error::Tampered(_) => Error::WrongKeys,
        other => other.into(),
    })?;
    if c.shortening_threshold != vault.threshold {
        return Err(Error::Fetch("the vault's name shortening changed; add the vault again".into()));
    }
    Ok(())
}

/// One decrypted entry.
struct Entry {
    name: String,
    long: bool,
    /// A file's encrypted contents.
    content: Option<RemoteRow>,
    /// A folder's ID.
    dir_id: Option<String>,
}

/// The entry an encrypted item stands for; `None` for what isn't one (a
/// symbolic link, which isn't synced, or a stray file).
async fn entry_of(
    src: &impl Source,
    vault: &Vault,
    cache: &mut Cache,
    item: &RemoteRow,
    dir_id: &str,
) -> Result<Option<Entry>> {
    let long = item.name.ends_with(SHORT_EXT) && item.kind == Kind::Folder;
    if !long && !item.name.ends_with(names::EXT) {
        return Ok(None);
    }
    let (encrypted, inside) = if long {
        let inside = src.children(&item.id)?;
        let Some(n) = named(&inside, NAME_FILE).filter(|r| r.kind == Kind::File) else {
            return Err(Error::Fetch("a long name's name.c9s is missing".into()));
        };
        let encrypted = cache.text(src, n, 4096).await?;
        // See `names::shortened`: a name.c9s from another entry would show
        // that entry twice.
        if names::shortened(&encrypted) != item.name {
            return Err(Error::Fetch("a long name's name.c9s doesn't belong to its folder".into()));
        }
        (encrypted, Some(inside))
    } else {
        (item.name.clone(), None)
    };
    let name = names::decrypt_name(&vault.key, &encrypted, dir_id)?;
    if item.kind == Kind::File {
        return Ok((!long).then(|| Entry { name, long: false, content: Some(item.clone()), dir_id: None }));
    }
    let inside = match inside {
        Some(i) => i,
        None => src.children(&item.id)?,
    };
    if let Some(dir) = named(&inside, DIR_FILE).filter(|r| r.kind == Kind::File) {
        let id = cache.text(src, dir, 64).await?;
        if id.is_empty() || id.len() > 36 || !id.is_ascii() {
            return Err(Error::Fetch("a folder's dir.c9r doesn't hold a folder ID".into()));
        }
        return Ok(Some(Entry { name, long, content: None, dir_id: Some(id) }));
    }
    if let Some(c) = named(&inside, CONTENTS_FILE).filter(|r| r.kind == Kind::File && long) {
        return Ok(Some(Entry { name, long, content: Some(c.clone()), dir_id: None }));
    }
    if named(&inside, SYMLINK_FILE).is_some() {
        return Err(Error::Fetch("a symbolic link, which isn't synced".into()));
    }
    // Being created (its dir.c9r not there yet), or damaged.
    Err(Error::Fetch("incomplete".into()))
}

/// What [`apply`] changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub changed: usize,
    pub removed: usize,
    /// Rows kept although they couldn't be placed (their encrypted item
    /// is still listed).
    pub kept: usize,
}

/// Writes the view into the vault pair's store (see the module docs).
pub fn apply(store: &mut Store, src: &impl Source, p: &Projection) -> Result<Applied> {
    let existing: HashMap<String, RemoteRow> =
        store.read(|o| o.all_items())?.into_iter().map(|(r, _)| (r.id.clone(), r)).collect();
    let wanted: HashSet<&str> = p.rows.iter().map(|r| r.id.as_str()).collect();
    // A file's encrypted contents that now belong to another entry: it was
    // moved there (a name that became too long to store as it is), which is
    // evidence enough that the old entry is gone.
    let moved_into: HashSet<&str> =
        p.nodes.iter().filter_map(|(id, n)| n.content.as_deref().filter(|c| *c != id.as_str())).collect();
    let mut gone = Vec::new();
    let mut kept = 0;
    for id in existing.keys().filter(|id| !wanted.contains(id.as_str())) {
        if moved_into.contains(id.as_str()) {
            gone.push(id.clone());
        } else if src.item(id)?.is_some() {
            kept += 1;
        } else {
            gone.push(id.clone());
        }
    }
    let mut out = Applied { kept, ..Applied::default() };
    store.write(|o| {
        for r in &p.rows {
            if existing.get(&r.id) != Some(r) {
                o.upsert_item(r, false)?;
                out.changed += 1;
            }
        }
        // Evidence: the encrypted item is gone from the listing.
        for id in &gone {
            if o.get_item(id)?.is_some() {
                o.delete_subtree(id)?;
                out.removed += 1;
            }
        }
        Ok(())
    })?;
    Ok(out)
}

#[cfg(test)]
mod cache_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TextSource(AtomicUsize);

    impl Source for TextSource {
        fn children(&self, _: &str) -> Result<Vec<RemoteRow>> {
            unreachable!()
        }
        fn child_named(&self, _: &str, _: &str) -> Result<Option<RemoteRow>> {
            unreachable!()
        }
        fn item(&self, _: &str) -> Result<Option<RemoteRow>> {
            unreachable!()
        }
        async fn fetch(&self, item: &RemoteRow) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(item.etag.as_deref().unwrap_or_default().as_bytes().to_vec())
        }
    }

    fn item(id: &str, version: &str) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: None,
            name: "dir.c9r".into(),
            kind: Kind::File,
            size: version.len() as i64,
            etag: Some(version.into()),
            qxh: None,
            mtime_ms: None,
        }
    }

    fn bounded(c: &Cache) {
        assert!(c.entries.len() <= c.entry_limit);
        assert!(c.bytes <= c.byte_limit);
        assert_eq!(c.order.len(), c.entries.len());
        assert_eq!(c.bytes, c.entries.values().map(|v| v.bytes).sum::<usize>());
    }

    #[tokio::test]
    async fn versions_replace_in_place_and_eviction_refetches_correct_text() {
        let src = TextSource(AtomicUsize::new(0));
        let mut c = Cache::with_limits(2, 1024);
        for v in 0..1000 {
            let version = v.to_string();
            assert_eq!(c.text(&src, &item("a", &version), 64).await.unwrap(), version);
            bounded(&c);
            assert_eq!(c.entries.len(), 1);
        }
        assert_eq!(src.0.load(Ordering::Relaxed), 1000);
        assert_eq!(c.text(&src, &item("a", "999"), 64).await.unwrap(), "999");
        assert_eq!(src.0.load(Ordering::Relaxed), 1000, "current version is cached");
        for id in ["b", "c"] {
            c.text(&src, &item(id, id), 64).await.unwrap();
            bounded(&c);
        }
        assert!(!c.entries.contains_key("a"), "oldest entry evicted");
        assert_eq!(c.text(&src, &item("a", "999"), 64).await.unwrap(), "999");
        assert_eq!(src.0.load(Ordering::Relaxed), 1003);
        bounded(&c);
    }

    #[test]
    fn byte_budget_counts_ids_versions_and_content_and_handles_oversized_entries() {
        let mut c = Cache::with_limits(100, 100);
        for i in 0..1000 {
            c.insert(&i.to_string(), "version", &"x".repeat(40));
            bounded(&c);
            assert!(c.entries.len() < 3, "byte limit, not entry limit, evicts");
        }
        c.insert("999", "replacement", &"x".repeat(200));
        bounded(&c);
        assert!(c.entries.is_empty());
        for i in 0..1000 {
            c.insert("same", &i.to_string(), "text");
            bounded(&c);
            assert_eq!(c.entries.len(), 1);
        }
    }

    #[tokio::test]
    async fn fetched_and_cached_text_obey_the_callers_size_limit() {
        let src = TextSource(AtomicUsize::new(0));
        let mut c = Cache::default();
        let mut r = item("a", "too long");
        r.size = 1;
        assert!(c.text(&src, &r, 2).await.is_err(), "actual response size checked");
        assert!(c.entries.is_empty());
        assert_eq!(c.text(&src, &r, 64).await.unwrap(), "too long");
        assert!(c.text(&src, &r, 2).await.is_err(), "cache hit size checked too");
        bounded(&c);
    }
}
