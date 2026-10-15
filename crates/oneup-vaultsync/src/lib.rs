//! Syncing a Cryptomator vault that lives in OneDrive as an ordinary,
//! decrypted folder on this PC (`docs/architecture/vaults.md` § Vault
//! pairs). The sync engine itself is unchanged: a vault is a folder pair
//! whose cloud side is seen through the vault.
//!
//! * [`project`]: the **decrypted view**. From the vault's encrypted items
//!   in the cloud listing (the main data folder's Remote tree), it works out
//!   the decrypted tree: names decrypted with their folder's ID, sizes
//!   converted, folders found through their `dir.c9r`. That view is the
//!   vault pair's Remote tree. Hashes stay those of the encrypted files (the
//!   cloud's hash domain, `oneup_exec::cloud`). A decrypted item is removed
//!   only when its encrypted item is gone from the listing (a delta
//!   deletion, or our own), never because it couldn't be placed: no
//!   deletion is ever inferred (`data-safety.md` invariant 3).
//! * [`cloud::VaultCloud`]: a [`oneup_exec::Cloud`] that encrypts uploads
//!   and names, decrypts downloads (each chunk authenticated before it's
//!   passed on), creates folders the way Cryptomator does, and records the
//!   encrypted items it changes in the cloud listing at once, so the
//!   decrypted view never lags behind its own changes.
//!
//! The decrypted items' IDs are the encrypted items' that get moved and
//! deleted: a file's `.c9r` (or a long name's `.c9s` folder), a folder's
//! `.c9r` folder; the vault's own folder is the root.

use std::collections::HashMap;
use std::sync::Arc;

use oneup_vault::key::Masterkey;

pub mod cloud;
pub mod project;

pub use cloud::VaultCloud;
pub use project::{Projection, Source, apply, project};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] oneup_store::Error),
    #[error(transparent)]
    Vault(#[from] oneup_vault::Error),
    #[error("the vault's folder isn't in the cloud listing")]
    NoVault,
    #[error("reading the vault: {0}")]
    Fetch(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// An unlocked vault in OneDrive.
#[derive(Clone)]
pub struct Vault {
    pub key: Arc<Masterkey>,
    /// Names longer than this (encrypted) are stored shortened.
    pub threshold: usize,
    /// The cloud ID of the vault's folder (the one with `vault.cryptomator`).
    pub root: String,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("root", &self.root).finish_non_exhaustive()
    }
}

/// What a decrypted item is, in terms of the encrypted ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// The vault's own folder.
    Root,
    Dir,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    /// The decrypted parent (`None` for the root).
    pub parent: Option<String>,
    /// Stored under a shortened name (`….c9s`).
    pub long: bool,
    /// A file: the encrypted contents' item (itself, or `contents.c9r` of a
    /// long name).
    pub content: Option<String>,
    /// A folder: its ID, and the cloud folder holding its entries (`None`
    /// until that exists).
    pub dir_id: Option<String>,
    pub storage: Option<String>,
}

/// The decrypted items, by ID.
pub type Nodes = HashMap<String, Node>;
