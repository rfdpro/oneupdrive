//! What the reconciler sees: the three trees, as plain values.
//!
//! These types are deliberately independent of the store's row types. The
//! reconciler must run unchanged over the SQLite store (production) and over
//! an in-memory world (simulation), so it reads everything through [`Trees`].

/// QuickXorHash of a file's content.
pub type Hash = [u8; 20];

/// A Synced item's identity: our own surrogate key, stable across renames,
/// moves and atomic saves on either side.
pub type Sid = u64;

/// The Synced tree's root, linking the local sync folder and the drive root.
pub const ROOT: Sid = 0;

/// A local NTFS/ReFS file ID (always 128-bit; see AGENTS.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u128);

/// A OneDrive drive item ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Folder,
}

/// One Synced row: what both sides last agreed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub sid: Sid,
    /// Agreed parent; `None` only for [`ROOT`].
    pub parent: Option<Sid>,
    /// Agreed name, exactly (case included).
    pub name: String,
    pub kind: NodeKind,
    /// Agreed content (files only; 0 / `None` for folders).
    pub size: i64,
    pub hash: Option<Hash>,
    /// The local item this row is linked to. `None` once the executor has
    /// *detached* it (e.g. after renaming a conflicting local copy away):
    /// that counts as deletion evidence for the local side.
    pub local: Option<LocalId>,
    /// The local last-write time (FILETIME) when this row was agreed. A
    /// different value later means "changed locally" unless a hash proves
    /// otherwise.
    pub local_mtime: i64,
    /// The cloud item this row is linked to; `None` once detached.
    pub remote: Option<RemoteId>,
    /// The cloud eTag when this row was agreed. Used to detect changes only
    /// when the cloud doesn't report a hash.
    pub remote_etag: String,
}

/// A local item as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalNode {
    pub id: LocalId,
    /// `None` only for the sync folder itself.
    pub parent: Option<LocalId>,
    pub name: String,
    pub kind: NodeKind,
    pub size: i64,
    pub mtime: i64,
    /// The content hash, if one was computed for exactly this
    /// (id, size, mtime). The reconciler asks for one with
    /// [`crate::Op::Hash`] when a decision depends on it.
    pub hash: Option<Hash>,
}

/// A cloud item as it is now (from the Remote tree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNode {
    pub id: RemoteId,
    /// `None` only for the drive root.
    pub parent: Option<RemoteId>,
    pub name: String,
    pub kind: NodeKind,
    pub size: i64,
    pub hash: Option<Hash>,
    pub etag: String,
}

/// What one side says about an item it is asked about by ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen<T> {
    Present(T),
    /// Gone, with explicit evidence: a USN delete record, a delta `deleted`
    /// facet (directly or on an ancestor), or our own executor removing it.
    Deleted,
    /// Gone without evidence (e.g. not found after a rescan or a mirror
    /// rebuild). Never grounds for deleting anything (`data-safety.md` inv. 3).
    Missing,
}

impl<T> Seen<T> {
    pub fn present(&self) -> Option<&T> {
        match self {
            Seen::Present(t) => Some(t),
            _ => None,
        }
    }
}

/// Read access to the three trees. Implementations must be side-effect free:
/// the reconciler's output may depend only on what these return.
///
/// Children lists contain only items that are synced or *could* be (the
/// driver filters excluded and parked items before the reconciler sees
/// them), in any order: the reconciler sorts them. An item filtered out of
/// the children lists must also read as [`Seen::Missing`] when asked for by
/// ID, so that a Synced item that became unsyncable is held rather than
/// mistaken for a change.
pub trait Trees {
    type Error;

    fn base(&self, sid: Sid) -> Result<Option<Base>, Self::Error>;
    /// Synced rows whose agreed parent is `sid`.
    fn base_children(&self, sid: Sid) -> Result<Vec<Base>, Self::Error>;
    fn base_by_local(&self, id: LocalId) -> Result<Option<Base>, Self::Error>;
    fn base_by_remote(&self, id: &RemoteId) -> Result<Option<Base>, Self::Error>;

    fn local(&self, id: LocalId) -> Result<Seen<LocalNode>, Self::Error>;
    fn remote(&self, id: &RemoteId) -> Result<Seen<RemoteNode>, Self::Error>;
    fn local_children(&self, id: LocalId) -> Result<Vec<LocalNode>, Self::Error>;
    fn remote_children(&self, id: &RemoteId) -> Result<Vec<RemoteNode>, Self::Error>;

    /// Whether `f` holds for any of [`Self::base_children`], stopping at the
    /// first that does. The defaults read the whole list; a backend that can
    /// read children one at a time overrides these, because a deleted
    /// folder's emptiness is checked every round its parent is planned and
    /// the folder may hold hundreds of thousands of items.
    fn any_base_child(
        &self,
        sid: Sid,
        f: &mut dyn FnMut(&Base) -> Result<bool, Self::Error>,
    ) -> Result<bool, Self::Error> {
        for b in self.base_children(sid)? {
            if f(&b)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// [`Self::any_base_child`] for [`Self::local_children`].
    fn any_local_child(
        &self,
        id: LocalId,
        f: &mut dyn FnMut(&LocalNode) -> Result<bool, Self::Error>,
    ) -> Result<bool, Self::Error> {
        for n in self.local_children(id)? {
            if f(&n)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// [`Self::any_base_child`] for [`Self::remote_children`].
    fn any_remote_child(
        &self,
        id: &RemoteId,
        f: &mut dyn FnMut(&RemoteNode) -> Result<bool, Self::Error>,
    ) -> Result<bool, Self::Error> {
        for n in self.remote_children(id)? {
            if f(&n)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether Synced folder `sid` has any child rows.
    fn has_base_children(&self, sid: Sid) -> Result<bool, Self::Error> {
        self.any_base_child(sid, &mut |_| Ok(true))
    }
    /// Whether local folder `id` has any children.
    fn has_local_children(&self, id: LocalId) -> Result<bool, Self::Error> {
        self.any_local_child(id, &mut |_| Ok(true))
    }
    /// Whether cloud folder `id` has any children.
    fn has_remote_children(&self, id: &RemoteId) -> Result<bool, Self::Error> {
        self.any_remote_child(id, &mut |_| Ok(true))
    }
}

/// Names are compared the way Windows and OneDrive do: case-insensitively,
/// one character at a time, each through its simple upper case, as NTFS's
/// upcase table maps one character to one. A character whose upper case is
/// more than one character (`ß` → `SS`) stays as it is. So "ΟΔΟΣ" and
/// "οδος" are one name (whole-string lower-casing makes a word-final `Σ`
/// into `ς`), while the Kelvin sign and `k`, or `İ` and `i`, are two. Every
/// comparison of names or paths that stands for Windows' or OneDrive's
/// should use this one, so no two parts disagree about whether names are
/// the same.
/// (Unicode normalisation is not folded; see `planner.md`.)
pub fn fold(name: &str) -> String {
    if name.is_ascii() {
        return name.to_ascii_uppercase();
    }
    name.chars().map(fold_char).collect()
}

/// [`fold`] for one character.
pub fn fold_char(c: char) -> char {
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// A Synced row's local side as it is now. A detached row is "deleted":
/// the executor itself removed the link, which is evidence.
pub fn local_of<T: Trees + ?Sized>(t: &T, b: &Base) -> Result<Seen<LocalNode>, T::Error> {
    match b.local {
        Some(id) => t.local(id),
        None => Ok(Seen::Deleted),
    }
}

/// A Synced row's cloud side as it is now (see [`local_of`]).
pub fn remote_of<T: Trees + ?Sized>(t: &T, b: &Base) -> Result<Seen<RemoteNode>, T::Error> {
    match &b.remote {
        Some(id) => t.remote(id),
        None => Ok(Seen::Deleted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fold_as_windows_compares_them() {
        let same =
            [("ΟΔΟΣ.txt", "οδοσ.txt"), ("ΟΔΟΣ.txt", "οδος.txt"), ("Report.TXT", "report.txt"), ("Ärger", "ärger")];
        for (a, b) in same {
            assert_eq!(fold(a), fold(b), "{a} and {b}");
        }
        let different = [
            ("\u{212A}it", "kit"),     // Kelvin sign
            ("\u{2126}mega", "ωmega"), // Ohm sign
            ("\u{2126}mega", "Ωmega"),
            ("İ", "i"),
            ("İ", "I"),
            ("straße", "STRASSE"),
            ("straße", "strasse"),
        ];
        for (a, b) in different {
            assert_ne!(fold(a), fold(b), "{a} and {b}");
        }
        assert_eq!(fold("ß"), "ß");
    }

    #[test]
    fn folding_maps_each_character_to_one_and_is_stable() {
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            let f = fold_char(c);
            assert_eq!(fold_char(f), f, "{c:?}");
            assert_eq!(fold(c.encode_utf8(&mut [0; 4])), f.to_string(), "{c:?}");
        }
    }
}
