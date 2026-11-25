//! On-demand search over stored paths and metadata. The index handles
//! ordinary whole-drive name queries. A filtered or sorted search keeps
//! only the requested prefix, and reads file attributes for displayed
//! OneDrive results only.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::path::Path;

use anyhow::{Result, bail};
use oneup_rules::ENGINE_DIR;
use oneup_store::{Kind, LocalKind, LocalRow, Ops, RemoteRow, Skip};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{LIMIT_NOTE, MatchMode, Pair, Placement, RECALL_ON_DATA_ACCESS, item_json};

pub const MAX_RESULTS: usize = 500;

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Availability {
    #[default]
    Any,
    Offline,
    OnlineOnly,
    Missing,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Sort {
    #[default]
    Relevance,
    Newest,
    Largest,
    Path,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Filters {
    folder: Option<bool>,
    extension: String,
    after_ms: Option<i64>,
    min_bytes: Option<i64>,
    availability: Availability,
    sort: Sort,
}

#[derive(Deserialize)]
pub struct Folder {
    rel: String,
    local_id: Option<String>,
    remote_id: Option<String>,
}

impl Folder {
    fn local(&self, o: &Ops<'_>) -> oneup_store::Result<Option<u128>> {
        let Some(id) = self.local_id.as_ref().and_then(|id| id.parse().ok()) else { return Ok(None) };
        if !o.local_get(id)?.is_some_and(|r| r.kind == LocalKind::Folder && r.skip == Skip::None && !r.name_lossy) {
            return Ok(None);
        }
        Ok(o.local_path_of(id)?
            .and_then(|path| path.strip_prefix('/').filter(|p| oneup_sync::same_name(p, &self.rel)).map(|_| id)))
    }

    fn remote<'a>(&'a self, o: &Ops<'_>, base: &str) -> oneup_store::Result<Option<&'a str>> {
        let Some(id) = &self.remote_id else { return Ok(None) };
        if !o.get_item(id)?.is_some_and(|r| r.kind == Kind::Folder)
            || o.item_state(id)? == oneup_store::ItemState::Excluded
        {
            return Ok(None);
        }
        let path = o.path_of(id)?;
        let prefix = format!("{}/", base.trim_end_matches('/'));
        Ok(path
            .as_deref()
            .and_then(|p| p.strip_prefix(&prefix))
            .filter(|rel| oneup_sync::same_name(rel, &self.rel))
            .map(|_| id.as_str()))
    }
}

#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    drive: bool,
    #[serde(default)]
    paths: bool,
    #[serde(default)]
    suggest: bool,
    #[serde(default)]
    suggest_only: bool,
    #[serde(default)]
    scope: Option<Folder>,
    #[serde(default)]
    filters: Filters,
    limit: usize,
}

#[derive(Clone, Copy)]
struct Metadata {
    folder: bool,
    size: Option<i64>,
    modified: Option<i64>,
    availability: Availability,
}

fn local_availability(row: &LocalRow) -> Availability {
    if row.attributes & RECALL_ON_DATA_ACCESS != 0 { Availability::OnlineOnly } else { Availability::Offline }
}

impl Metadata {
    fn local(row: &LocalRow) -> Self {
        Self {
            folder: row.kind == LocalKind::Folder,
            size: (row.kind == LocalKind::File).then_some(row.size),
            modified: (row.mtime > 0).then(|| crate::space::unix_ms(row.mtime)),
            availability: local_availability(row),
        }
    }

    fn remote(row: &RemoteRow) -> Self {
        Self {
            folder: row.kind == Kind::Folder,
            size: (row.kind == Kind::File).then_some(row.size),
            modified: row.mtime_ms,
            availability: Availability::Missing,
        }
    }
}

impl Filters {
    fn accepts(&self, name: &str, meta: Metadata) -> bool {
        self.folder.is_none_or(|folder| meta.folder == folder)
            && (self.extension.is_empty()
                || (!meta.folder
                    && Path::new(name)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|ext| oneup_sync::same_name(ext, &self.extension))))
            && self.after_ms.is_none_or(|after| meta.modified.is_some_and(|m| m >= after))
            && self.min_bytes.is_none_or(|min| meta.size.is_some_and(|s| s >= min))
    }
}

enum Source {
    Local,
    Remote(RemoteRow, bool),
}

#[derive(Eq, PartialEq, Ord, PartialOrd)]
struct Key {
    priority: u8,
    value: Reverse<Option<i64>>,
    path: String,
    id: String,
}

struct Hit {
    key: Key,
    path: String,
    meta: Metadata,
    source: Source,
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for Hit {}
impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Hit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key.cmp(&other.key)
    }
}

struct Results<'a> {
    request: &'a Request,
    query: oneup_store::search::NameQuery<'a>,
    needle: String,
    spelling: Option<Vec<char>>,
    suggestion: Option<String>,
    hits: BinaryHeap<Hit>,
}

impl Results<'_> {
    fn matches(&self, name: &str, path: &str) -> bool {
        let target = if self.request.paths { path } else { name };
        !self.request.suggest_only
            && match &self.query {
                oneup_store::search::NameQuery::Text(_) => oneup_sync::fold(target).contains(&self.needle),
                _ => self.query.matches_text(target),
            }
    }

    fn correction<'a>(&self, name: &'a str) -> Option<&'a str> {
        let needle = self.spelling.as_ref()?;
        if self.suggestion.is_some() || !self.hits.is_empty() {
            return None;
        }
        name.split(|c: char| !c.is_alphanumeric())
            .filter(|part| part.chars().count().abs_diff(needle.len()) <= 1)
            .find(|part| one_edit(needle, &oneup_sync::fold(part).chars().collect::<Vec<_>>()))
    }

    fn add(&mut self, id: String, path: String, name: &str, meta: Metadata, source: Source) {
        let (priority, value) = match self.request.filters.sort {
            Sort::Newest => (0, meta.modified),
            Sort::Largest => (0, meta.size),
            Sort::Path => (0, None),
            Sort::Relevance => {
                let name = oneup_sync::fold(name);
                (
                    if name == self.needle {
                        0
                    } else if name.starts_with(&self.needle) {
                        1
                    } else {
                        2
                    },
                    None,
                )
            }
        };
        let key = Key { priority, value: Reverse(value), path: oneup_sync::fold(&path), id };
        if self.hits.len() <= self.request.limit {
            self.hits.push(Hit { key, path, meta, source });
        } else if let Some(mut worst) = self.hits.peek_mut()
            && key < worst.key
        {
            *worst = Hit { key, path, meta, source };
        }
    }
}

/// A single insertion, deletion, substitution, or adjacent transposition.
fn one_edit(left: &[char], right: &[char]) -> bool {
    let common = left.iter().zip(right).take_while(|(a, b)| a == b).count();
    let (a, b) = (&left[common..], &right[common..]);
    match (a, b) {
        ([x, rest @ ..], [y, other @ ..]) if a.len() == b.len() => {
            rest == other
                || matches!((rest, other), ([next, rest @ ..], [then, other @ ..])
                if next == y && then == x && rest == other)
        }
        ([_, rest @ ..], _) if a.len() == b.len() + 1 => rest == b,
        (_, [_, rest @ ..]) if b.len() == a.len() + 1 => a == rest,
        _ => false,
    }
}

fn joined(base: &str, rel: &str) -> String {
    if base.is_empty() { rel.to_owned() } else { format!("{}/{rel}", base.trim_end_matches('/')) }
}

pub fn find(dir: &Path, mut request: Request, mode: MatchMode) -> Result<Value> {
    request.limit = request.limit.clamp(1, MAX_RESULTS);
    request.filters.extension = request.filters.extension.trim().trim_start_matches('.').to_owned();
    let q = request.q.trim();
    if q.len() > 4096 {
        bail!("Search text is too long.");
    }
    let query = super::name_query(q, mode, false)?;
    let mut results = Results {
        request: &request,
        query,
        needle: oneup_sync::fold(q),
        spelling: (request.suggest
            && !matches!(mode, MatchMode::Regex)
            && (4..=32).contains(&q.chars().count())
            && q.chars().all(char::is_alphabetic))
        .then(|| oneup_sync::fold(q).chars().collect()),
        suggestion: None,
        hits: BinaryHeap::new(),
    };
    let mut store = crate::setup::open_store(dir, 4096)?;
    let (available, local_root) =
        store.snapshot(|o| Ok((collect(o, &mut results)?, o.local_root()?.map(|root| root.path))))?;
    if !available {
        bail!("Search folder moved or is no longer listed.");
    }
    let more = results.hits.len() > request.limit;
    let suggestion = results.hits.is_empty().then_some(results.suggestion).flatten();
    let pair = if request.drive && !results.hits.is_empty() {
        oneup_cfapi::expose_placeholders()?;
        Some(Pair::load(dir, &store))
    } else {
        None
    };
    let items: Vec<Value> = results
        .hits
        .into_sorted_vec()
        .into_iter()
        .take(request.limit)
        .map(|hit| {
            let local = if matches!(&hit.source, Source::Local) {
                local_root.as_deref().and_then(|root| crate::filetree::search_local_path(root, &hit.path))
            } else {
                None
            };
            let remote = match hit.source {
                Source::Remote(row, excluded) => {
                    if let Some(pair) = &pair {
                        return item_json(&pair.item(&row, excluded, hit.path));
                    }
                    Some(row.id)
                }
                Source::Local => None,
            };
            let placement = match hit.meta.availability {
                Availability::Offline => Placement::Here,
                Availability::OnlineOnly => Placement::OnlineOnly,
                _ => Placement::NotHereYet,
            };
            json!({
                "id": hit.key.id, "rel": hit.path, "folder": hit.meta.folder, "placement": placement.name(),
                "size": hit.meta.size, "modified_ms": hit.meta.modified, "local": local, "remote": remote
            })
        })
        .collect();
    Ok(json!({ "items": items, "more": more, "suggestion": suggestion, "note": LIMIT_NOTE }))
}

fn collect(o: &Ops<'_>, results: &mut Results<'_>) -> oneup_store::Result<bool> {
    let request = results.request;
    let local_root = o.local_root()?.map(|r| r.root_file_id);
    let paired = local_root.map(|id| o.synced_by_local(id)).transpose()?.flatten().and_then(|b| b.remote);
    let paired_path = paired.as_ref().map(|id| o.path_of(&id.0)).transpose()?.flatten();
    let prefix = paired_path.as_ref().map(|base| format!("{}/", base.trim_end_matches('/')));
    let scope = request.scope.as_ref().filter(|_| !request.drive);
    let rel = scope.map_or("", |folder| folder.rel.as_str());
    let (local_id, remote_id) = if let Some(scope) = scope {
        let local_id = scope.local(o)?;
        let remote_id =
            paired_path.as_deref().map(|base| scope.remote(o, base)).transpose()?.flatten().map(str::to_owned);
        if local_id.is_none() && remote_id.is_none() {
            return Ok(false);
        }
        (local_id, remote_id)
    } else if request.drive {
        (None, o.remote_root()?.map(|row| row.id))
    } else {
        (local_root, paired.map(|id| id.0))
    };
    if let Some(id) = local_id {
        o.visit_local_search(id, |tail, row| {
            let path = joined(rel, &tail);
            let meta = Metadata::local(&row);
            if row.name_lossy
                || row.kind == LocalKind::Link
                || path.split('/').next().is_some_and(|p| oneup_sync::same_name(p, ENGINE_DIR))
                || !request.filters.accepts(&row.name, meta)
                || (request.filters.availability != Availability::Any
                    && meta.availability != request.filters.availability)
            {
                return Ok(true);
            }
            if results.matches(&row.name, &path) {
                results.add(format!("l:{}", row.file_id), path, &row.name, meta, Source::Local);
            } else if let Some(correction) = results.correction(&row.name) {
                results.suggestion = Some(correction.to_owned());
            }
            Ok(!request.suggest_only || results.suggestion.is_none())
        })?;
    }
    if request.suggest_only && results.suggestion.is_some() {
        return Ok(true);
    }
    if let Some(id) = remote_id {
        o.visit_remote_search(&id, |tail, row, excluded| {
            let path = joined(if request.drive { "/" } else { rel }, &tail);
            let mut meta = Metadata::remote(&row);
            if (!request.drive && path.split('/').next().is_some_and(|p| oneup_sync::same_name(p, ENGINE_DIR)))
                || !request.filters.accepts(&row.name, meta)
            {
                return Ok(true);
            }
            let matches = results.matches(&row.name, &path);
            let correction = if matches { None } else { results.correction(&row.name) };
            if !matches && correction.is_none() {
                return Ok(true);
            }
            let filter = request.filters.availability;
            // For a synced pair, the local record supplies metadata if both
            // trees contain the same path. Drive scope keeps cloud metadata.
            let check_local = if request.drive { filter != Availability::Any } else { local_id.is_some() };
            let relative = if request.drive {
                prefix.as_deref().and_then(|p| oneup_sync::strip_folded_prefix(&path, p))
            } else {
                Some(path.as_str())
            };
            if check_local
                && let (Some(root), Some(relative)) = (local_root, relative)
                && let Some(local) = crate::filetree::row_at(o, root, relative)?
                && local.kind != LocalKind::Link
                && !local.name_lossy
            {
                if !request.drive {
                    return Ok(true);
                }
                meta.availability = local_availability(&local);
            }
            if filter != Availability::Any && meta.availability != filter {
                return Ok(true);
            }
            if matches {
                let name = row.name.clone();
                results.add(row.id.clone(), path, &name, meta, Source::Remote(row, excluded));
            } else if let Some(correction) = correction {
                results.suggestion = Some(correction.to_owned());
            }
            Ok(!request.suggest_only || results.suggestion.is_none())
        })?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_store::LocalRoot;
    use oneup_sync::{Base, LocalId, NodeKind, ROOT, RemoteId};

    fn fixture(dir: &Path) {
        let mut store = crate::setup::open_store(dir, 1024).unwrap();
        let local = |id, parent, name: &str, kind, size, time, attributes| LocalRow {
            file_id: id,
            parent_id: parent,
            name: name.into(),
            name_lossy: false,
            kind,
            size,
            mtime: crate::space::filetime(time),
            attributes,
            reparse_tag: 0,
            skip: Skip::None,
        };
        store
            .write(|o| {
                o.local_begin(
                    &LocalRoot {
                        path: dir.join("test-root").to_string_lossy().into_owned(),
                        volume_serial: 0,
                        root_file_id: 1,
                        rules: String::new(),
                        journal: None,
                        started_at_ms: 0,
                        finished_at_ms: Some(0),
                        sync_root_provider: None,
                    },
                    &local(1, None, "", LocalKind::Root, 0, 0, 0),
                )?;
                for row in [
                    local(2, Some(1), "Acme", LocalKind::Folder, 0, 0, 0),
                    local(3, Some(2), "invoice.PDF", LocalKind::File, 2_000_000, 100, 0),
                    local(4, Some(2), "online.pdf", LocalKind::File, 12_000_000, 200, RECALL_ON_DATA_ACCESS),
                    local(5, Some(1), "other.pdf", LocalKind::File, 4_000_000, 300, 0),
                ] {
                    o.local_put(&row)?;
                }
                for (id, parent, name, kind, size, mtime_ms) in [
                    ("root", None, "", Kind::Root, 0, None),
                    ("acme", Some("root"), "Acme", Kind::Folder, 0, None),
                    ("duplicate", Some("acme"), "invoice.PDF", Kind::File, 20_000_000, Some(10)),
                    ("missing", Some("acme"), "receipt.pdf", Kind::File, 6_000_000, Some(500)),
                ] {
                    o.upsert_item(
                        &RemoteRow {
                            id: id.into(),
                            parent_id: parent.map(str::to_owned),
                            name: name.into(),
                            kind,
                            size,
                            etag: None,
                            qxh: None,
                            mtime_ms,
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
                    remote_etag: String::new(),
                })
            })
            .unwrap();
    }

    fn search(dir: &Path, args: Value, mode: MatchMode) -> Value {
        find(dir, serde_json::from_value(args).unwrap(), mode).unwrap()
    }

    #[test]
    fn advanced_search_filters_the_scope_before_sorting_and_limiting() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        fixture(dir);
        let mut args = json!({
            "limit": 1, "scope": { "rel": "Acme", "local_id": "2", "remote_id": "acme" },
            "filters": { "extension": ".pdf", "after_ms": 50, "min_bytes": 2_000_000, "sort": "largest" }
        });
        let first = search(dir, args.clone(), MatchMode::Text);
        assert_eq!(first["items"][0]["rel"], "Acme/online.pdf");
        assert!(first["items"][0]["local"].as_str().unwrap().ends_with(r"\Acme\online.pdf"));
        assert_eq!(first["more"], true);
        args["limit"] = json!(5);
        let more = search(dir, args.clone(), MatchMode::Text);
        let paths: Vec<_> = more["items"].as_array().unwrap().iter().map(|row| row["rel"].as_str().unwrap()).collect();
        assert_eq!(paths, ["Acme/online.pdf", "Acme/receipt.pdf", "Acme/invoice.PDF"]);
        assert_eq!(more["more"], false);
        args["filters"]["sort"] = json!("path");
        let by_path = search(dir, args.clone(), MatchMode::Text);
        assert_eq!(by_path["items"][0]["rel"], "Acme/invoice.PDF");
        args["filters"]["sort"] = json!("newest");
        let newest = search(dir, args.clone(), MatchMode::Text);
        assert_eq!(newest["items"][0]["rel"], "Acme/receipt.pdf");
        assert_eq!(newest["items"][0]["remote"], "missing");
        assert!(newest["items"][0]["local"].is_null());
        args["filters"]["availability"] = json!("offline");
        let offline = search(dir, args, MatchMode::Text);
        assert_eq!(offline["items"].as_array().unwrap().len(), 1);
        assert_eq!(offline["items"][0]["rel"], "Acme/invoice.PDF");
        assert_eq!(offline["items"][0]["size"], 2_000_000);
    }

    #[test]
    fn path_query_and_spelling_suggestion_use_stored_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        fixture(dir);
        let path = search(dir, json!({"q":"acme invoice", "paths":true, "limit":50}), MatchMode::Words);
        assert_eq!(path["items"][0]["rel"], "Acme/invoice.PDF");
        let hint = search(dir, json!({"q":"invioce", "suggest":true, "limit":50}), MatchMode::Text);
        assert_eq!(hint["items"], json!([]));
        assert_eq!(hint["suggestion"], "invoice");
        let stale = find(
            dir,
            serde_json::from_value(json!({
                "limit":50, "scope":{"rel":"wrong", "local_id":"2", "remote_id":"acme"}
            }))
            .unwrap(),
            MatchMode::Text,
        );
        assert!(stale.unwrap_err().to_string().contains("Search folder moved"));
    }
}
