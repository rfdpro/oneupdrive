//! The core decision: given one folder's children on both sides, categorise
//! each of them and say which subfolders to look into next.
//!
//! Pure: rows in, decisions out, so every rule is unit-tested here and the
//! driver ([`crate::observe`]) only has to walk the store.
//!
//! Matching is by name, **case-insensitively**, like OneDrive and Windows. Two
//! local names that differ only in case (possible in case-sensitive folders)
//! are a [`Conflict::NameClash`]: OneDrive could hold only one of them.
//! Unicode normalisation is *not* folded: "é" precomposed and "e + ́" are
//! different names to NTFS, and are treated as different here too.

use std::collections::BTreeMap;

use oneup_store::{Kind, LocalKind, LocalRow, RemoteRow, Skip};

use crate::names;
use crate::report::{Category, Conflict, Entry, NotSynced, Park, Unchecked};

/// Windows attribute bits used to recognise placeholders. Duplicated here on
/// purpose: the planner must not depend on the FFI crates.
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
/// `IO_REPARSE_TAG_CLOUD` and its variants `CLOUD_1`..`CLOUD_F`.
const CLOUD_TAG_MASK: u32 = 0xFFFF_0FFF;
const IO_REPARSE_TAG_CLOUD: u32 = 0x9000_001A;

/// Whether the file is a Cloud Files placeholder (hydrated or not).
fn is_placeholder(r: &LocalRow) -> bool {
    r.reparse_tag & CLOUD_TAG_MASK == IO_REPARSE_TAG_CLOUD
}

/// Whether the file's data isn't on this PC (reading it would download it).
fn is_online_only(r: &LocalRow) -> bool {
    r.attributes & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
}

fn bytes(size: i64) -> u64 {
    u64::try_from(size).unwrap_or(0)
}

/// A folder to process after this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Present on both sides: compare the children.
    Both { local: u128, remote: String, path: String },
    /// Only on this PC. With `inherit`, every descendant gets that category
    /// (e.g. everything inside a folder whose name OneDrive refuses).
    Local { id: u128, path: String, inherit: Option<Category> },
    /// Only in OneDrive (or online-only here), likewise.
    Remote { id: String, path: String, inherit: Option<Category> },
}

/// A same-size file pair whose content can be compared on request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRequest {
    /// The local row as scanned, so the verifier can re-check it's unchanged.
    pub local: LocalRow,
    /// The cloud copy's QuickXorHash.
    pub expected: [u8; 20],
    pub path: String,
}

/// The decisions for one folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderPlan {
    /// Categorised children, in name order.
    pub entries: Vec<Entry>,
    /// Same-size files that `--verify` may hash. Their entries are *not* in
    /// `entries`: the driver adds them once their outcome is known.
    pub verify: Vec<VerifyRequest>,
    pub next: Vec<Next>,
}

impl FolderPlan {
    fn add(&mut self, path: &str, category: Category, is_folder: bool, size: i64) {
        self.entries.push(Entry { path: path.to_owned(), category, is_folder, bytes: bytes(size) });
    }
}

fn join(parent: &str, name: &str) -> String {
    format!("{parent}/{name}")
}

/// Categorises the children of the folder at `path` ("" for the root).
///
/// `local` and `remote` are the stored children on each side (either may be
/// empty for a one-sided folder). `inherit`, if set, overrides everything:
/// the folder itself got that category, so its whole subtree does too.
pub fn plan_folder(
    path: &str,
    inherit: Option<Category>,
    local: Vec<LocalRow>,
    remote: Vec<(RemoteRow, bool)>,
) -> FolderPlan {
    let mut plan = FolderPlan::default();

    // Pair children by case-folded name. BTreeMap gives a stable, readable
    // order for examples and tests.
    #[derive(Default)]
    struct Pair {
        local: Vec<LocalRow>,
        remote: Vec<(RemoteRow, bool)>,
    }
    let mut pairs: BTreeMap<String, Pair> = BTreeMap::new();
    for l in local {
        pairs.entry(l.name.to_lowercase()).or_default().local.push(l);
    }
    // OneNote notebooks: the official client represents each as a
    // "<name>.url" shortcut file on disk.
    let mut notebook_shortcuts = Vec::new();
    for r in remote {
        if r.0.kind == Kind::Package {
            notebook_shortcuts.push(format!("{}.url", r.0.name.to_lowercase()));
        }
        pairs.entry(r.0.name.to_lowercase()).or_default().remote.push(r);
    }

    // Personal Vault: in the top folder, the OneDrive app represents the
    // vault on disk as a "<name>.lnk" shortcut while it's locked. The name is
    // localised, so it's recognised by that pairing: a cloud-only top-level
    // item with a local-only shortcut of the same name. The cloud item may be
    // a folder *or* stored as a file: on a first real run a
    // folder-only rule didn't match, which fits Graph sending the vault
    // without a `folder` facet, so the mirror keeps it as a file. Keeping
    // Graph's `specialFolder` facet in the mirror would make this exact.
    let mut vault_keys = Vec::new();
    if path.is_empty() {
        for (key, pair) in &pairs {
            let cloud_only = pair.local.is_empty()
                && matches!(pair.remote.as_slice(), [(r, _)] if matches!(r.kind, Kind::Folder | Kind::File));
            let shortcut = format!("{key}.lnk");
            let has_shortcut = pairs.get(&shortcut).is_some_and(|p| {
                p.remote.is_empty()
                    && matches!(p.local.as_slice(), [l] if matches!(l.kind, LocalKind::File | LocalKind::Link))
            });
            if cloud_only && has_shortcut {
                vault_keys.push(key.clone());
                vault_keys.push(shortcut);
            }
        }
    }

    for (key, pair) in pairs {
        if vault_keys.contains(&key) {
            for l in &pair.local {
                plan.add(&join(path, &l.name), Category::NotSynced(NotSynced::PersonalVault), false, l.size);
            }
            for (r, _) in &pair.remote {
                plan.add(&join(path, &r.name), Category::NotSynced(NotSynced::PersonalVault), r.kind != Kind::File, 0);
            }
            continue;
        }
        match (pair.local.len(), pair.remote.len()) {
            (0, _) => {
                for (r, excluded) in pair.remote {
                    remote_only(&mut plan, path, inherit, r, excluded);
                }
            }
            (1, 0) => {
                let shortcut = notebook_shortcuts.contains(&key);
                for l in pair.local {
                    local_only(&mut plan, path, inherit, l, shortcut);
                }
            }
            (1, 1) => {
                let (Some(l), Some((r, excluded))) = (pair.local.into_iter().next(), pair.remote.into_iter().next())
                else {
                    continue;
                };
                both(&mut plan, path, inherit, l, r, excluded);
            }
            // Case-only name clashes. Nothing below them is examined: which
            // local item corresponds to the cloud one is a question for the
            // user, and the report says so.
            _ => {
                for l in &pair.local {
                    let c = inherit.unwrap_or(Category::Conflict(Conflict::NameClash));
                    plan.add(&join(path, &l.name), c, l.kind != LocalKind::File, l.size);
                }
                if pair.local.is_empty() {
                    // Several cloud items with one folded name: Graph doesn't
                    // allow that, so this means a corrupt mirror. Count them
                    // rather than guess.
                    for (r, _) in &pair.remote {
                        let c = inherit.unwrap_or(Category::Conflict(Conflict::NameClash));
                        plan.add(&join(path, &r.name), c, r.kind != Kind::File, r.size);
                    }
                }
            }
        }
    }
    plan
}

fn remote_only(plan: &mut FolderPlan, parent: &str, inherit: Option<Category>, r: RemoteRow, excluded: bool) {
    let path = join(parent, &r.name);
    let is_folder = r.kind != Kind::File;
    let category = match (inherit, r.kind) {
        (Some(c), _) => c,
        _ if excluded => Category::Excluded,
        (None, Kind::Package) => Category::Package,
        (None, _) => Category::Download,
    };
    if r.kind == Kind::Folder && !excluded {
        plan.next.push(Next::Remote { id: r.id.clone(), path: path.clone(), inherit });
    }
    plan.add(&path, category, is_folder, r.size);
}

fn local_only(plan: &mut FolderPlan, parent: &str, inherit: Option<Category>, l: LocalRow, notebook_shortcut: bool) {
    let path = join(parent, &l.name);
    let is_folder = l.kind != LocalKind::File;
    let listed_folder = l.kind == LocalKind::Folder && l.skip == Skip::None;

    // Order matters: the first applicable reason is the one reported.
    let category = if let Some(c) = inherit {
        c
    } else if l.skip == Skip::Excluded {
        Category::Excluded
    } else if names::is_onedrive_marker(&l.name) {
        Category::NotSynced(NotSynced::OneDriveAppFile)
    } else if l.kind == LocalKind::Link {
        Category::Parked(Park::Link)
    } else if l.skip == Skip::Unreadable {
        Category::Parked(Park::Unreadable)
    } else if l.skip == Skip::Unpopulated || (l.kind == LocalKind::File && is_online_only(&l)) {
        Category::Parked(Park::OnlineOnlyNotInCloud)
    } else if l.name_lossy {
        Category::Parked(Park::UnusualName)
    } else if names::is_invalid(&l.name) {
        Category::Parked(Park::InvalidName)
    } else if notebook_shortcut && l.kind == LocalKind::File {
        Category::NotSynced(NotSynced::OneNoteShortcut)
    } else if let Some(n) = names::not_synced(&l.name).filter(|_| l.kind == LocalKind::File) {
        Category::NotSynced(n)
    } else if names::path_units(&path) > names::MAX_PATH_UNITS {
        Category::Parked(Park::PathTooLong)
    } else if l.kind == LocalKind::File && l.size > names::MAX_FILE_BYTES {
        Category::Parked(Park::TooLarge)
    } else {
        Category::Upload
    };
    if listed_folder {
        // Below a folder that can't be uploaded, nothing can be: pass the
        // reason down so the report counts the whole subtree under it.
        let inherit = match category {
            Category::Upload => None,
            c => Some(c),
        };
        plan.next.push(Next::Local { id: l.file_id, path: path.clone(), inherit });
    }
    plan.add(&path, category, is_folder, l.size);
}

fn both(plan: &mut FolderPlan, parent: &str, inherit: Option<Category>, l: LocalRow, r: RemoteRow, r_excluded: bool) {
    let path = join(parent, &l.name);
    if let Some(c) = inherit {
        // Only reachable if a caller passes `inherit` with both sides, which
        // the driver never does. Count the item without descending.
        plan.add(&path, c, l.kind != LocalKind::File, l.size);
        return;
    }
    let local_folder = matches!(l.kind, LocalKind::Folder | LocalKind::Root);
    let remote_folder = matches!(r.kind, Kind::Folder | Kind::Root);

    // An exclusion on either side wins. Both trees were built with the same
    // rules (the driver checks), so normally both sides agree; if only one
    // matched (e.g. a file named like an excluded folder on the other side),
    // the excluded side's contents aren't known, so it can't be compared.
    if l.skip == Skip::Excluded || r_excluded {
        plan.add(&path, Category::Excluded, local_folder || remote_folder, 0);
        return;
    }
    match (l.kind, r.kind) {
        (LocalKind::File, Kind::File) => {
            let category = if is_online_only(&l) {
                if l.size == r.size { Category::OnlineOnly } else { Category::OnlineOnlyChanged }
            } else if l.size != r.size {
                Category::Conflict(Conflict::SizeDiffers)
            } else if is_placeholder(&l) {
                Category::SameSize(Unchecked::Placeholder)
            } else if let Some(expected) = r.qxh {
                plan.verify.push(VerifyRequest { local: l, expected, path });
                return;
            } else {
                Category::SameSize(Unchecked::NoCloudHash)
            };
            plan.add(&path, category, false, r.size);
        }
        (LocalKind::Folder, Kind::Folder) => match l.skip {
            // (`Excluded` was handled above; listed here only to be exhaustive.)
            Skip::None | Skip::Excluded => {
                plan.add(&path, Category::FolderInBoth, true, 0);
                plan.next.push(Next::Both { local: l.file_id, remote: r.id, path });
            }
            // The local folder is an online-only placeholder: whatever the
            // cloud holds below it is online-only here too.
            Skip::Unpopulated => {
                plan.add(&path, Category::OnlineOnly, true, 0);
                plan.next.push(Next::Remote { id: r.id, path, inherit: Some(Category::OnlineOnly) });
            }
            Skip::Unreadable => plan.add(&path, Category::Parked(Park::Unreadable), true, 0),
        },
        // File vs folder, link vs anything, notebook vs anything. Neither
        // subtree is examined: they can't be paired up item by item.
        _ => plan.add(&path, Category::Conflict(Conflict::KindDiffers), local_folder || remote_folder, l.size),
    }
}

/// The category of a same-size pair once its content check is done.
pub fn after_verify(outcome: Verified) -> Category {
    match outcome {
        Verified::Match => Category::Identical,
        Verified::Mismatch => Category::Conflict(Conflict::ContentDiffers),
        Verified::Skipped(u) => Category::SameSize(u),
    }
}

/// The result of comparing one [`VerifyRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verified {
    Match,
    Mismatch,
    Skipped(Unchecked),
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONLINE: u32 = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    const CLOUD: u32 = 0x9000_101A;

    fn l(id: u128, name: &str, kind: LocalKind, size: i64) -> LocalRow {
        LocalRow {
            file_id: id,
            parent_id: Some(1),
            name: name.into(),
            name_lossy: false,
            kind,
            size,
            mtime: 0,
            attributes: 0,
            reparse_tag: 0,
            skip: Skip::None,
        }
    }

    fn r(id: &str, name: &str, kind: Kind, size: i64) -> (RemoteRow, bool) {
        let row = RemoteRow {
            id: id.into(),
            parent_id: Some("root".into()),
            name: name.into(),
            kind,
            size,
            etag: None,
            qxh: (kind == Kind::File).then_some([7; 20]),
            mtime_ms: None,
        };
        (row, false)
    }

    fn cats(p: &FolderPlan) -> Vec<(&str, Category)> {
        p.entries.iter().map(|e| (e.path.as_str(), e.category)).collect()
    }

    use Kind as R;
    use LocalKind as L;

    #[test]
    fn files_match_case_insensitively_and_by_size() {
        let p = plan_folder(
            "",
            None,
            vec![l(2, "Same.txt", L::File, 5), l(3, "big.bin", L::File, 9), l(4, "new.txt", L::File, 1)],
            vec![r("a", "same.TXT", R::File, 5), r("b", "big.bin", R::File, 8), r("c", "cloud.txt", R::File, 3)],
        );
        assert_eq!(
            cats(&p),
            [
                ("/big.bin", Category::Conflict(Conflict::SizeDiffers)),
                ("/cloud.txt", Category::Download),
                ("/new.txt", Category::Upload),
            ]
        );
        // The same-size pair waits for the content check, under the local name.
        assert_eq!(p.verify.len(), 1);
        assert_eq!((p.verify[0].path.as_str(), p.verify[0].expected), ("/Same.txt", [7; 20]));
        assert!(p.next.is_empty());
    }

    #[test]
    fn placeholders_are_never_queued_for_reading() {
        let online = LocalRow { attributes: ONLINE, reparse_tag: CLOUD, ..l(2, "a", L::File, 5) };
        let hydrated = LocalRow { reparse_tag: CLOUD, ..l(3, "b", L::File, 5) };
        let lonely = LocalRow { attributes: ONLINE, reparse_tag: CLOUD, ..l(4, "c", L::File, 5) };
        let stale = LocalRow { attributes: ONLINE, reparse_tag: CLOUD, ..l(5, "e", L::File, 5) };
        let p = plan_folder(
            "/d",
            None,
            vec![online, hydrated, lonely, stale],
            vec![r("a", "a", R::File, 5), r("b", "b", R::File, 5), r("e", "e", R::File, 9)],
        );
        assert!(p.verify.is_empty(), "no placeholder may be read");
        assert_eq!(
            cats(&p),
            [
                ("/d/a", Category::OnlineOnly),
                ("/d/b", Category::SameSize(Unchecked::Placeholder)),
                ("/d/c", Category::Parked(Park::OnlineOnlyNotInCloud)),
                ("/d/e", Category::OnlineOnlyChanged),
            ]
        );
    }

    #[test]
    fn folders_recurse_on_the_right_sides() {
        let lazy = LocalRow { skip: Skip::Unpopulated, ..l(3, "lazy", L::Folder, 0) };
        let p = plan_folder(
            "",
            None,
            vec![l(2, "docs", L::Folder, 0), lazy, l(4, "mine", L::Folder, 0)],
            vec![r("d", "Docs", R::Folder, 0), r("z", "lazy", R::Folder, 0), r("c", "cloud", R::Folder, 0)],
        );
        assert_eq!(
            p.next,
            [
                Next::Remote { id: "c".into(), path: "/cloud".into(), inherit: None },
                Next::Both { local: 2, remote: "d".into(), path: "/docs".into() },
                Next::Remote { id: "z".into(), path: "/lazy".into(), inherit: Some(Category::OnlineOnly) },
                Next::Local { id: 4, path: "/mine".into(), inherit: None },
            ]
        );
        assert_eq!(cats(&p)[2], ("/lazy", Category::OnlineOnly));
    }

    #[test]
    fn exclusions_kinds_links_and_packages() {
        let junk = LocalRow { skip: Skip::Excluded, ..l(2, "node_modules", L::Folder, 0) };
        let (mut cloud_junk, _) = r("x", "target", R::Folder, 0);
        cloud_junk.qxh = None;
        let p = plan_folder(
            "",
            None,
            vec![junk, l(3, "thing", L::File, 1), l(4, "link", L::Link, 0), l(5, "Notes.url", L::File, 1)],
            vec![(cloud_junk, true), r("t", "thing", R::Folder, 0), r("n", "Notes", R::Package, 0)],
        );
        assert_eq!(
            cats(&p),
            [
                ("/link", Category::Parked(Park::Link)),
                ("/node_modules", Category::Excluded),
                ("/Notes", Category::Package),
                ("/Notes.url", Category::NotSynced(NotSynced::OneNoteShortcut)),
                ("/target", Category::Excluded),
                ("/thing", Category::Conflict(Conflict::KindDiffers)),
            ]
        );
        assert!(p.next.is_empty(), "nothing excluded, linked or mismatched is descended into");
    }

    #[test]
    fn local_only_problems_are_parked_and_inherited() {
        let lossy = LocalRow { name_lossy: true, ..l(3, "bad\u{FFFD}", L::File, 1) };
        let deep_parent = "/".to_string() + &"d".repeat(395);
        let p = plan_folder(
            "",
            None,
            vec![l(2, "aux", L::Folder, 0), lossy, l(4, "desktop.ini", L::File, 1), l(5, "~$x.docx", L::File, 1)],
            vec![],
        );
        assert_eq!(
            cats(&p),
            [
                ("/aux", Category::Parked(Park::InvalidName)),
                ("/bad\u{FFFD}", Category::Parked(Park::UnusualName)),
                ("/desktop.ini", Category::NotSynced(NotSynced::SystemFile)),
                ("/~$x.docx", Category::NotSynced(NotSynced::OfficeLock)),
            ]
        );
        // Everything under a parked folder is parked for the same reason.
        assert_eq!(
            p.next,
            [Next::Local { id: 2, path: "/aux".into(), inherit: Some(Category::Parked(Park::InvalidName)) }]
        );
        let below =
            plan_folder("/aux", Some(Category::Parked(Park::InvalidName)), vec![l(9, "ok.txt", L::File, 4)], vec![]);
        assert_eq!(cats(&below), [("/aux/ok.txt", Category::Parked(Park::InvalidName))]);

        let p = plan_folder(&deep_parent, None, vec![l(6, "long.txt", L::File, 1), l(7, "x", L::File, 1)], vec![]);
        assert_eq!(
            cats(&p).iter().map(|(_, c)| *c).collect::<Vec<_>>(),
            [Category::Parked(Park::PathTooLong), Category::Upload],
            "397 units is fine, 404 isn't"
        );
        let huge = plan_folder("", None, vec![l(8, "huge.vhdx", L::File, names::MAX_FILE_BYTES + 1)], vec![]);
        assert_eq!(cats(&huge), [("/huge.vhdx", Category::Parked(Park::TooLarge))]);
    }

    /// Both seen on a real OneDrive folder, where they were
    /// first reported as an upload and a download.
    #[test]
    fn onedrive_app_files_and_the_personal_vault() {
        let marker = ".849C9593-D756-4E56-8D6E-42412F2A707B";
        let p = plan_folder(
            "",
            None,
            vec![l(2, marker, L::File, 0), l(3, "Personal Vault.lnk", L::File, 1), l(4, "Other.lnk", L::File, 1)],
            vec![r("v", "Personal Vault", R::Folder, 0)],
        );
        assert_eq!(
            cats(&p),
            [
                (&*format!("/{marker}"), Category::NotSynced(NotSynced::OneDriveAppFile)),
                ("/Other.lnk", Category::Upload),
                ("/Personal Vault", Category::NotSynced(NotSynced::PersonalVault)),
                ("/Personal Vault.lnk", Category::NotSynced(NotSynced::PersonalVault)),
            ]
        );
        assert!(p.next.is_empty(), "the vault isn't descended into");
        // As seen for real: the vault mirrored as a file-like item.
        let p = plan_folder(
            "",
            None,
            vec![l(3, "Personal Vault.lnk", L::File, 1)],
            vec![(RemoteRow { qxh: None, ..r("v", "Personal Vault", R::File, 0).0 }, false)],
        );
        assert!(cats(&p).iter().all(|(_, c)| *c == Category::NotSynced(NotSynced::PersonalVault)), "{:?}", cats(&p));
        // Only in the top folder; and the marker is recognised anywhere.
        let deeper = plan_folder(
            "/a",
            None,
            vec![l(3, "Personal Vault.lnk", L::File, 1), l(2, marker, L::File, 0)],
            vec![r("v", "Personal Vault", R::Folder, 0)],
        );
        assert_eq!(
            cats(&deeper).iter().map(|(_, c)| *c).collect::<Vec<_>>(),
            [Category::NotSynced(NotSynced::OneDriveAppFile), Category::Download, Category::Upload]
        );
    }

    #[test]
    fn case_clashes_are_conflicts() {
        let p = plan_folder(
            "",
            None,
            vec![l(2, "Readme", L::File, 1), l(3, "README", L::File, 1)],
            vec![r("a", "readme", R::File, 1)],
        );
        assert_eq!(cats(&p).len(), 2);
        assert!(cats(&p).iter().all(|(_, c)| *c == Category::Conflict(Conflict::NameClash)));
        assert!(p.verify.is_empty() && p.next.is_empty());
    }

    #[test]
    fn verify_outcomes() {
        assert_eq!(after_verify(Verified::Match), Category::Identical);
        assert_eq!(after_verify(Verified::Mismatch), Category::Conflict(Conflict::ContentDiffers));
        assert_eq!(
            after_verify(Verified::Skipped(Unchecked::ChangedSinceScan)),
            Category::SameSize(Unchecked::ChangedSinceScan)
        );
    }
}
