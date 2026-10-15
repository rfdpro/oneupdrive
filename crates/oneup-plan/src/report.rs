//! What the observe-mode planner found: every item falls in exactly one
//! [`Category`], and the [`Report`] keeps per-category totals plus a few
//! example paths. Memory is bounded: examples are capped and nothing else
//! grows with the tree.

use std::collections::BTreeMap;

/// Why a file present on both sides with the same size wasn't compared by
/// content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unchecked {
    /// Content checks weren't requested (`--verify`).
    NotRequested,
    /// The local file is a cloud placeholder; reading it could download it.
    Placeholder,
    /// OneDrive didn't report a QuickXorHash for the cloud copy.
    NoCloudHash,
    /// The local file changed after the scan (or while it was read).
    ChangedSinceScan,
    /// The local file couldn't be read (access denied, locked).
    Unreadable,
}

/// How the two sides disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Conflict {
    /// Same path, different sizes.
    SizeDiffers,
    /// Same path and size, but the content hashes differ.
    ContentDiffers,
    /// A file on one side and a folder (or link, or OneNote notebook) on the other.
    KindDiffers,
    /// Several local names differ only in letter case (possible in
    /// case-sensitive folders). OneDrive can hold only one of them.
    NameClash,
}

/// Local items the official client deliberately doesn't sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NotSynced {
    /// `desktop.ini`.
    SystemFile,
    /// Office's `~$` owner files, present while a document is open.
    OfficeLock,
    /// `.tmp` files.
    Temporary,
    /// The `.url` shortcut the official client creates for a OneNote
    /// notebook; the notebook itself lives in OneDrive.
    OneNoteShortcut,
    /// The OneDrive app's own hidden system file (a dot followed by a GUID,
    /// `.849C9593-…`). Local bookkeeping, never uploaded.
    OneDriveAppFile,
    /// The Personal Vault folder in OneDrive and the `.lnk` shortcut the
    /// OneDrive app keeps for it on disk. The vault needs its own unlock
    /// flow, which OneUpDrive doesn't support yet.
    PersonalVault,
}

/// Why a local-only item couldn't be uploaded. A sync would leave these alone
/// and report them ("parked") instead of retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Park {
    /// A name OneDrive refuses (see [`crate::names::is_invalid`]).
    InvalidName,
    /// Longer than OneDrive's 400-character path limit.
    PathTooLong,
    /// Larger than OneDrive's 250 GB file limit.
    TooLarge,
    /// The name isn't valid Unicode, so it can't be represented in OneDrive.
    UnusualName,
    /// A symbolic link, junction or mount point. Never followed.
    Link,
    /// A folder that couldn't be read (access denied).
    Unreadable,
    /// An online-only placeholder whose data isn't on this PC, with no
    /// counterpart in this OneDrive. Its data may be in another account, or
    /// the item may be pending deletion; either way it can't be uploaded.
    OnlineOnlyNotInCloud,
}

/// The category of one item (file or folder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// Same path and size on both sides, and the same content hash.
    Identical,
    /// Same path and size on both sides; content not compared.
    SameSize(Unchecked),
    /// A folder present on both sides (its contents are categorised separately).
    FolderInBoth,
    /// Online-only on this PC (a placeholder, or inside an online-only
    /// folder) and in OneDrive with the same size: nothing to transfer.
    OnlineOnly,
    /// Online-only on this PC, and in OneDrive with another size: OneDrive
    /// has a newer version than the placeholder shows. Nothing of it is on
    /// this PC, so nothing local can be lost.
    OnlineOnlyChanged,
    /// Only in OneDrive: a sync would download it (or create a placeholder).
    Download,
    /// Only on this PC: a sync would upload it.
    Upload,
    /// Both sides have it, differently. A sync would keep both copies.
    Conflict(Conflict),
    /// Matched an exclusion rule. Contents not counted.
    Excluded,
    /// A OneNote notebook in OneDrive; notebooks sync through OneNote itself.
    Package,
    /// Local only, but OneDrive never syncs these anyway.
    NotSynced(NotSynced),
    /// Local only and can't be uploaded.
    Parked(Park),
}

impl Category {
    /// Short label for reports.
    pub fn label(self) -> String {
        match self {
            Category::Identical => "identical (content verified)".into(),
            Category::SameSize(u) => format!(
                "same name and size, content not compared ({})",
                match u {
                    Unchecked::NotRequested => "ordinary files, not read",
                    Unchecked::Placeholder => "cloud placeholder, deliberately not read",
                    Unchecked::NoCloudHash => "no hash in OneDrive",
                    Unchecked::ChangedSinceScan => "changed since the scan",
                    Unchecked::Unreadable => "couldn't be read",
                }
            ),
            Category::FolderInBoth => "folders on both sides".into(),
            Category::OnlineOnly => "online-only here, in OneDrive (nothing to transfer)".into(),
            Category::OnlineOnlyChanged => "online-only here, a newer version in OneDrive".into(),
            Category::Download => "only in OneDrive (would download)".into(),
            Category::Upload => "only on this PC (would upload)".into(),
            Category::Conflict(c) => format!(
                "different on each side ({}; both copies would be kept)",
                match c {
                    Conflict::SizeDiffers => "size differs",
                    Conflict::ContentDiffers => "content differs",
                    Conflict::KindDiffers => "file vs folder",
                    Conflict::NameClash => "names differ only in letter case",
                }
            ),
            Category::Excluded => "excluded by your rules".into(),
            Category::Package => "OneNote notebooks (stay in OneDrive)".into(),
            Category::NotSynced(n) => format!(
                "not synced by OneDrive anyway ({})",
                match n {
                    NotSynced::SystemFile => "desktop.ini",
                    NotSynced::OfficeLock => "Office lock files",
                    NotSynced::Temporary => ".tmp files",
                    NotSynced::OneNoteShortcut => "OneNote shortcuts",
                    NotSynced::OneDriveAppFile => "the OneDrive app's own files",
                    NotSynced::PersonalVault => "Personal Vault, not supported yet",
                }
            ),
            Category::Parked(p) => format!(
                "can't be uploaded ({})",
                match p {
                    Park::InvalidName => "name not allowed in OneDrive",
                    Park::PathTooLong => "path longer than 400 characters",
                    Park::TooLarge => "larger than 250 GB",
                    Park::UnusualName => "name isn't valid Unicode",
                    Park::Link => "link or junction",
                    Park::Unreadable => "folder couldn't be read",
                    Park::OnlineOnlyNotInCloud => "online-only, but not in this OneDrive",
                }
            ),
        }
    }
}

/// Totals for one category.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub files: u64,
    pub folders: u64,
    /// Sum of file sizes (folders count 0).
    pub bytes: u64,
}

/// One categorised item, as produced by [`crate::plan_folder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path from the root ("/a/b.txt"), using the local name when both
    /// sides have the item.
    pub path: String,
    pub category: Category,
    pub is_folder: bool,
    pub bytes: u64,
}

/// The planner's findings.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub tallies: BTreeMap<Category, Tally>,
    /// Up to `max_examples` paths per category, in walk order.
    pub examples: BTreeMap<Category, Vec<String>>,
    pub max_examples: usize,
    /// Files whose content was hashed (`--verify`), and their total size.
    pub hashed_files: u64,
    pub hashed_bytes: u64,
}

impl Report {
    pub fn new(max_examples: usize) -> Self {
        Self { max_examples, ..Self::default() }
    }

    pub fn add(&mut self, e: Entry) {
        let t = self.tallies.entry(e.category).or_default();
        if e.is_folder {
            t.folders += 1;
        } else {
            t.files += 1;
            t.bytes += e.bytes;
        }
        let ex = self.examples.entry(e.category).or_default();
        if ex.len() < self.max_examples {
            ex.push(e.path);
        }
    }

    pub fn tally(&self, c: Category) -> Tally {
        self.tallies.get(&c).copied().unwrap_or_default()
    }

    /// Sum over all categories matching `pred`.
    pub fn sum(&self, pred: impl Fn(Category) -> bool) -> Tally {
        self.tallies.iter().filter(|(c, _)| pred(**c)).fold(Tally::default(), |a, (_, t)| Tally {
            files: a.files + t.files,
            folders: a.folders + t.folders,
            bytes: a.bytes + t.bytes,
        })
    }
}
