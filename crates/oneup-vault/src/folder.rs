//! A vault in a local folder: creating one, opening it, listing, reading,
//! and adding new files and folders. This is what the `oneup vault` tools
//! use, to check compatibility with Cryptomator's apps; syncing vaults is
//! designed separately (`docs/architecture/vaults.md`).
//!
//! It never changes or removes anything that exists: new files and folders
//! are created only under names that aren't taken (`create_new`), and a
//! file it fails to finish is removed, as it was its own. It refuses to
//! read online-only files (Files On-Demand placeholders), which would make
//! another sync app download them: callers must make placeholders visible
//! first (`oneup_cfapi::expose_placeholders`).

use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::key::{self, Masterkey};
use crate::names::{self, Stored};
use crate::{Error, Result, content};

/// A folder in the vault, by ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    pub id: String,
}

impl Dir {
    pub fn root() -> Dir {
        Dir { id: names::ROOT_DIR_ID.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// With its content size.
    File {
        size: u64,
    },
    Dir(Dir),
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// The encrypted file (a file's contents; for others, its folder).
    stored: PathBuf,
}

/// A listing: the entries, and what couldn't be read (for a check).
#[derive(Debug, Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub problems: Vec<String>,
}

/// An open vault.
pub struct Folder {
    root: PathBuf,
    key: Masterkey,
    config: Config,
}

/// Longest `dir.c9r` / `name.c9s` read (they hold an ID or a name).
const SMALL_FILE: u64 = 64 * 1024;

/// Refuses Files On-Demand placeholders and other files whose contents
/// aren't on this PC.
fn local_only(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const OFFLINE: u32 = 0x1000;
        const RECALL_ON_OPEN: u32 = 0x4_0000;
        const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
        let attrs = fs::symlink_metadata(path)?.file_attributes();
        if attrs & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
            return Err(Error::Unsupported(format!(
                "{} is online-only; make it available on this PC first",
                path.display()
            )));
        }
    }
    Ok(())
}

fn read_small(path: &Path) -> Result<Vec<u8>> {
    local_only(path)?;
    let f = fs::File::open(path)?;
    let mut out = Vec::new();
    f.take(SMALL_FILE + 1).read_to_end(&mut out)?;
    if out.len() as u64 > SMALL_FILE {
        return Err(Error::Malformed(format!("{} is too big", path.display())));
    }
    Ok(out)
}

/// A folder ID read from `dir.c9r`: at most 36 ASCII characters.
fn read_dir_id(path: &Path) -> Result<String> {
    let bytes = read_small(path)?;
    match String::from_utf8(bytes) {
        Ok(s) if !s.is_empty() && s.len() <= 36 && s.is_ascii() => Ok(s),
        _ => Err(Error::Malformed(format!("{} doesn't hold a folder ID", path.display()))),
    }
}

impl Folder {
    /// Creates a vault in `root`, which must be empty or not exist yet.
    pub fn create(root: &Path, password: &str) -> Result<Folder> {
        Self::create_with(root, password, |k, pw| k.to_file(pw))
    }

    /// As [`Folder::create`], with the masterkey file made by `file` (tests
    /// use cheap key derivation).
    pub fn create_with(
        root: &Path,
        password: &str,
        file: impl Fn(&Masterkey, &str) -> Result<Vec<u8>>,
    ) -> Result<Folder> {
        if root.exists() && fs::read_dir(root)?.next().is_some() {
            return Err(Error::Unsupported(format!("{} isn't empty", root.display())));
        }
        fs::create_dir_all(root)?;
        let key = Masterkey::generate()?;
        let config = Config::new()?;
        write_new(&root.join(key::FILE), &file(&key, password)?)?;
        write_new(&root.join(config::FILE), config.sign(&key)?.as_bytes())?;
        fs::create_dir_all(root.join(names::dir_path(&key, names::ROOT_DIR_ID)?))?;
        Ok(Folder { root: root.to_path_buf(), key, config })
    }

    /// Opens the vault in `root` with `password`.
    pub fn open(root: &Path, password: &str) -> Result<Folder> {
        let token = String::from_utf8(read_small(&root.join(config::FILE))?)
            .map_err(|_| Error::Malformed(format!("{} isn't text", config::FILE)))?;
        let key_file = config::key_file(&token)?;
        let key = Masterkey::from_file(&read_small(&root.join(&key_file))?, password)?;
        let config = Config::verify(&token, &key)?;
        Ok(Folder { root: root.to_path_buf(), key, config })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn storage(&self, dir: &Dir) -> Result<PathBuf> {
        Ok(self.root.join(names::dir_path(&self.key, &dir.id)?))
    }

    /// What's in `dir`.
    pub fn list(&self, dir: &Dir) -> Result<Listing> {
        let at = self.storage(dir)?;
        local_only(&at)?;
        let mut out = Listing::default();
        for e in fs::read_dir(&at)? {
            let e = e?;
            let stored_name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            let entry = if stored_name.ends_with(names::SHORT_EXT) {
                self.short_entry(dir, &path)
            } else if stored_name.ends_with(names::EXT) && stored_name != names::DIR_ID_BACKUP {
                self.entry(dir, &stored_name, &path, &path, e.file_type()?.is_file())
            } else {
                continue;
            };
            match entry {
                Ok(Some(x)) => out.entries.push(x),
                Ok(None) => {}
                Err(err) => out.problems.push(format!("{stored_name}: {err}")),
            }
        }
        out.entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn short_entry(&self, dir: &Dir, path: &Path) -> Result<Option<Entry>> {
        let name = String::from_utf8(read_small(&path.join(names::NAME_FILE))?)
            .map_err(|_| Error::Malformed("name.c9s isn't text".into()))?;
        let contents = path.join(names::CONTENTS_FILE);
        let is_file = contents.is_file();
        self.entry(dir, name.trim(), path, if is_file { &contents } else { path }, is_file)
    }

    /// `node`: the entry's file or folder; `data`: a file's contents.
    fn entry(&self, dir: &Dir, encrypted: &str, node: &Path, data: &Path, is_file: bool) -> Result<Option<Entry>> {
        let name = names::decrypt_name(&self.key, encrypted, &dir.id)?;
        let kind = if is_file {
            let size = fs::symlink_metadata(data)?.len();
            Kind::File {
                size: content::content_size(size).ok_or_else(|| Error::Malformed("impossible file size".into()))?,
            }
        } else if node.join(names::DIR_FILE).is_file() {
            Kind::Dir(Dir { id: read_dir_id(&node.join(names::DIR_FILE))? })
        } else if node.join(names::SYMLINK_FILE).is_file() {
            Kind::Symlink
        } else {
            // A node type from a later version: left alone.
            return Ok(None);
        };
        Ok(Some(Entry { name, kind, stored: data.to_path_buf() }))
    }

    /// The entry at `path` (`/`-separated, from the vault's top).
    pub fn find(&self, path: &str) -> Result<Option<Entry>> {
        let mut dir = Dir::root();
        let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Ok(Some(Entry { name: String::new(), kind: Kind::Dir(Dir::root()), stored: self.storage(&dir)? }));
        }
        for (i, part) in parts.iter().enumerate() {
            let Some(e) = self.lookup(&dir, part)? else { return Ok(None) };
            if i + 1 == parts.len() {
                return Ok(Some(e));
            }
            match e.kind {
                Kind::Dir(d) => dir = d,
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    /// The entry called `name` in `dir`, straight from its encrypted name.
    fn lookup(&self, dir: &Dir, name: &str) -> Result<Option<Entry>> {
        let stored = Stored::of(names::encrypt_name(&self.key, name, &dir.id)?, self.config.shortening_threshold);
        let node = self.storage(dir)?.join(stored.stored_name());
        if !node.exists() {
            return Ok(None);
        }
        match &stored {
            Stored::Full(n) => self.entry(dir, n, &node, &node, node.is_file()),
            Stored::Short { .. } => self.short_entry(dir, &node),
        }
    }

    /// Decrypts a file's contents to `out`.
    pub fn read(&self, e: &Entry, out: impl Write) -> Result<u64> {
        if !matches!(e.kind, Kind::File { .. }) {
            return Err(Error::Unsupported(format!("{} isn't a file", e.name)));
        }
        local_only(&e.stored)?;
        content::decrypt(&self.key, BufReader::new(fs::File::open(&e.stored)?), out)
    }

    /// Where a new entry `name` in `dir` goes; an error if it's taken.
    fn new_node(&self, dir: &Dir, name: &str) -> Result<(Stored, PathBuf)> {
        if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
            return Err(Error::Unsupported(format!("{name:?} isn't a valid name")));
        }
        let stored = Stored::of(names::encrypt_name(&self.key, name, &dir.id)?, self.config.shortening_threshold);
        let node = self.storage(dir)?.join(stored.stored_name());
        if node.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{name} already exists in the vault"),
            )));
        }
        Ok((stored, node))
    }

    /// Adds a new file `name` to `dir` with the contents of `input`.
    pub fn create_file(&self, dir: &Dir, name: &str, input: impl Read) -> Result<u64> {
        let (stored, node) = self.new_node(dir, name)?;
        let target = match &stored {
            Stored::Full(_) => node.clone(),
            Stored::Short { name, .. } => {
                fs::create_dir(&node)?;
                write_new(&node.join(names::NAME_FILE), name.as_bytes())?;
                node.join(names::CONTENTS_FILE)
            }
        };
        let f = fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
        let mut w = BufWriter::new(f);
        let written = content::encrypt(&self.key, input, &mut w).and_then(|n| {
            w.into_inner().map_err(|e| Error::Io(e.into_error()))?.sync_all()?;
            Ok(n)
        });
        if written.is_err() {
            // Our own half-written file, created just now.
            let _ = fs::remove_file(&target);
        }
        written
    }

    /// Adds a new, empty folder `name` to `dir`.
    pub fn create_dir(&self, dir: &Dir, name: &str) -> Result<Dir> {
        let (stored, node) = self.new_node(dir, name)?;
        let new = Dir { id: crate::new_uuid()? };
        // Its storage first: a dir.c9r never points to nothing.
        fs::create_dir_all(self.storage(&new)?)?;
        fs::create_dir(&node)?;
        if let Stored::Short { name, .. } = &stored {
            write_new(&node.join(names::NAME_FILE), name.as_bytes())?;
        }
        write_new(&node.join(names::DIR_FILE), new.id.as_bytes())?;
        Ok(new)
    }
}

fn write_new(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oneup-vault-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn cheap(k: &Masterkey, pw: &str) -> Result<Vec<u8>> {
        k.to_file_with(pw, 1024, 8)
    }

    #[test]
    fn a_new_vault_holds_files_and_folders_and_reopens() {
        let root = temp("roundtrip");
        let v = Folder::create_with(&root, "pw", cheap).unwrap();
        v.create_file(&Dir::root(), "hello.txt", &b"hello vault"[..]).unwrap();
        let sub = v.create_dir(&Dir::root(), "Sub folder").unwrap();
        let long = "l".repeat(180) + ".txt";
        v.create_file(&sub, &long, &vec![3u8; 70_000][..]).unwrap();
        let deep = v.create_dir(&sub, &"d".repeat(170)).unwrap();
        v.create_file(&deep, "x", &b""[..]).unwrap();
        drop(v);

        assert!(matches!(Folder::open(&root, "nope"), Err(Error::WrongPassword)));
        let v = Folder::open(&root, "pw").unwrap();
        let top = v.list(&Dir::root()).unwrap();
        assert!(top.problems.is_empty(), "{:?}", top.problems);
        let names: Vec<_> = top.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Sub folder", "hello.txt"]);
        let mut out = Vec::new();
        v.read(&v.find("/hello.txt").unwrap().unwrap(), &mut out).unwrap();
        assert_eq!(out, b"hello vault");
        let e = v.find(&format!("Sub folder/{long}")).unwrap().unwrap();
        assert_eq!(e.kind, Kind::File { size: 70_000 });
        let inside = v.list(&sub).unwrap();
        assert_eq!(inside.entries.len(), 2, "long names, shortened, list too");
        assert!(v.find(&format!("Sub folder/{}/x", "d".repeat(170))).unwrap().is_some());
        assert!(v.find("missing/x").unwrap().is_none());

        // Nothing is ever replaced.
        assert!(v.create_file(&Dir::root(), "hello.txt", &b"again"[..]).is_err());
        assert!(v.create_dir(&Dir::root(), "Sub folder").is_err());
        assert!(v.create_file(&Dir::root(), "../x", &b""[..]).is_err());
        let mut again = Vec::new();
        v.read(&v.find("hello.txt").unwrap().unwrap(), &mut again).unwrap();
        assert_eq!(again, b"hello vault");
        // And a vault isn't created over anything.
        assert!(Folder::create_with(&root, "pw", cheap).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_file_moved_to_another_folder_is_reported_not_trusted() {
        let root = temp("moved");
        let v = Folder::create_with(&root, "pw", cheap).unwrap();
        let sub = v.create_dir(&Dir::root(), "a").unwrap();
        v.create_file(&Dir::root(), "f", &b"1"[..]).unwrap();
        let from = v.find("f").unwrap().unwrap().stored;
        // Moved by someone else, at the storage level, into another folder.
        let to = v.storage(&sub).unwrap().join(from.file_name().unwrap());
        fs::rename(&from, &to).unwrap();
        let l = v.list(&sub).unwrap();
        assert!(l.entries.is_empty());
        assert_eq!(l.problems.len(), 1);
        fs::remove_dir_all(&root).unwrap();
    }
}
