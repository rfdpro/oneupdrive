use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
#[cfg(windows)]
use bytes::Bytes;
use iroh::{EndpointId, SecretKey};
#[cfg(windows)]
use iroh_blobs::api::TempTag;
use iroh_blobs::{Hash, store::fs::FsStore};
use tempfile::{NamedTempFile, TempPath};
#[cfg(windows)]
use tokio::sync::mpsc;
#[cfg(windows)]
use tokio_stream::wrappers::ReceiverStream;
use zeroize::Zeroizing;

use crate::ticket::Ticket;

#[cfg(windows)]
const CHUNK_BYTES: usize = 128 * 1024;
const SESSION_PREFIX: &str = "receive-";
const SESSION_SUFFIX_LEN: usize = 20;

pub fn scratch_root(root: &Path) -> Result<PathBuf> {
    let root = std::path::absolute(root)?;
    #[cfg(windows)]
    {
        oneup_cfapi::expose_placeholders()?;
        for directory in root.ancestors().skip(1) {
            plain_directory(directory)?;
        }
        outside_cloud(root.parent().context("No transfer data folder.")?)?;
    }
    create_directory(&root)?;
    outside_cloud(&root)?;
    Ok(root)
}

fn plain_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(metadata.is_dir() && !metadata.file_type().is_symlink(), "Invalid transfer directory.");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        ensure!(metadata.file_attributes() & oneup_ntfs::dirent::NOT_PLAIN == 0, "Invalid transfer directory.");
    }
    Ok(())
}

fn create_directory(path: &Path) -> Result<()> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            plain_directory(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                ensure!(fs::metadata(path)?.permissions().mode() & 0o077 == 0, "Transfer directory must be private.");
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn outside_cloud(_path: &Path) -> Result<()> {
    #[cfg(windows)]
    ensure!(oneup_cfapi::sync_root_containing(_path).is_none(), "Transfer storage is in a cloud folder.");
    Ok(())
}

pub fn prepare(root: &Path) -> Result<(String, EndpointId)> {
    let dir = tempfile::Builder::new().prefix(SESSION_PREFIX).rand_bytes(SESSION_SUFFIX_LEN).tempdir_in(root)?;
    let key = SecretKey::generate();
    let bytes = Zeroizing::new(key.to_bytes());
    #[cfg(windows)]
    let bytes = oneup_sys::dpapi::protect(bytes.as_ref())?;
    write_new(dir.path(), "recipient-key", bytes.as_ref())?;
    let id = dir.path().file_name().and_then(|id| id.to_str()).context("Invalid session directory.")?.to_owned();
    let _ = dir.keep();
    Ok((id, key.public()))
}

pub struct Session {
    path: PathBuf,
    _lock: File,
}

pub fn set_current(root: &Path, id: &str) -> Result<()> {
    write_owned(root, "current-receive", id.as_bytes())
}

pub fn current(root: &Path) -> Result<Option<String>> {
    match fs::read_to_string(root.join("current-receive")) {
        Ok(id) => Ok(Some(id)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl Session {
    pub fn open(root: &Path, id: &str) -> Result<Self> {
        ensure!(
            id.strip_prefix(SESSION_PREFIX)
                .is_some_and(|s| s.len() == SESSION_SUFFIX_LEN && s.bytes().all(|c| c.is_ascii_alphanumeric())),
            "Invalid session ID."
        );
        let path = root.join(id);
        plain_directory(&path).context("Receive session unavailable.")?;
        outside_cloud(&path)?;
        let lock = OpenOptions::new().write(true).create(true).truncate(false).open(path.join("session.lock"))?;
        lock.try_lock().context("Receive session is already in use.")?;
        Ok(Self { path, _lock: lock })
    }

    pub fn key(&self) -> Result<SecretKey> {
        let raw = Zeroizing::new(fs::read(self.path.join("recipient-key"))?);
        #[cfg(windows)]
        let raw = oneup_sys::dpapi::unprotect(&raw)?;
        Ok(SecretKey::from_bytes(raw.as_slice().try_into().context("Invalid recipient key.")?))
    }

    pub fn saved_ticket(&self) -> Result<Option<Ticket>> {
        match fs::read_to_string(self.path.join("ticket")) {
            Ok(text) => Ok(Some(text.parse().map_err(|_| anyhow::anyhow!("Invalid saved ticket."))?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn ticket(&self, prompt: impl FnOnce() -> Result<Ticket>) -> Result<Ticket> {
        if let Some(ticket) = self.saved_ticket()? {
            return Ok(ticket);
        }
        let ticket = prompt()?;
        write_new(&self.path, "ticket", ticket.encode()?.as_bytes())?;
        Ok(ticket)
    }

    pub fn blob_dir(&self) -> Result<PathBuf> {
        let dir = self.path.join("blobs");
        create_directory(&dir)?;
        Ok(dir)
    }

    pub fn destination(&self) -> Result<Option<PathBuf>> {
        match fs::read(self.path.join("destination")) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_destination(&self, path: &Path) -> Result<()> {
        self.check_destination(path)?;
        write_owned(&self.path, "destination", &serde_json::to_vec(path)?)
    }

    pub fn output(&self) -> Result<PathBuf> {
        Ok(self.destination()?.unwrap_or_else(|| self.path.join("received")))
    }

    pub fn saved(&self, ticket: &Ticket) -> Result<Option<PathBuf>> {
        crate::ticket::file_name(&ticket.name)?;
        let path = self.output()?.join(&ticket.name);
        let published = match fs::read_to_string(self.path.join("published")) {
            Ok(name) => name == ticket.name,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        Ok((published && path.try_exists()?).then_some(path))
    }

    pub async fn publish(&self, store: &FsStore, ticket: &Ticket) -> Result<PathBuf> {
        crate::ticket::file_name(&ticket.name)?;
        let output = self.output()?;
        if output == self.path.join("received") {
            create_directory(&output)?;
            outside_cloud(&output)?;
        } else {
            self.check_destination(&output)?;
        }
        let temp = NamedTempFile::new_in(&output)?.into_temp_path();
        let hash = ticket.blob.hash();
        let size = store.blobs().export(hash, &temp).await?;
        ensure!(size == ticket.size, "File size differs from the ticket.");
        let dest = output.join(&ticket.name);
        let saved = tokio::task::spawn_blocking(move || publish_verified(temp, hash, dest)).await??;
        write_owned(&self.path, "published", ticket.name.as_bytes())?;
        Ok(saved)
    }
    fn check_destination(&self, path: &Path) -> Result<()> {
        ensure!(path.is_absolute(), "Choose an existing folder on this PC.");
        #[cfg(windows)]
        {
            oneup_cfapi::expose_placeholders()?;
            for directory in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
                plain_directory(directory).context("Choose an ordinary folder on this PC.")?;
            }
        }
        #[cfg(not(windows))]
        plain_directory(path).context("Choose an ordinary folder on this PC.")?;
        let scratch = self.path.parent().context("No transfer data folder.")?;
        ensure!(
            !path.canonicalize()?.starts_with(scratch.canonicalize()?),
            "Choose a folder outside transfer storage."
        );
        outside_cloud(path)
    }
}

fn publish_verified(temp: TempPath, hash: Hash, dest: PathBuf) -> Result<PathBuf> {
    let file = OpenOptions::new().read(true).write(true).open(&temp)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&file)?;
    ensure!(hasher.finalize().as_bytes() == hash.as_bytes(), "File verification failed.");
    file.sync_all()?;
    drop(file);

    mark_download(&temp)?;
    temp.persist_noclobber(&dest).map_err(|e| {
        if e.error.kind() == io::ErrorKind::AlreadyExists {
            anyhow::anyhow!("A file with this name already exists. Choose another folder.")
        } else {
            anyhow::Error::new(e).context("Could not save the verified file.")
        }
    })?;
    Ok(dest)
}

#[cfg(windows)]
fn mark_download(path: &Path) -> Result<()> {
    let mut zone = path.as_os_str().to_owned();
    zone.push(":Zone.Identifier");
    let mut mark = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(zone)
        .context("Could not mark the file as an Internet download.")?;
    mark.write_all(b"[ZoneTransfer]\r\nZoneId=3\r\n")?;
    mark.sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
fn mark_download(_path: &Path) -> Result<()> {
    Ok(())
}

fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut temp = NamedTempFile::new_in(dir)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(dir.join(name))?;
    Ok(())
}

fn write_owned(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut temp = NamedTempFile::new_in(dir)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join(name))?;
    Ok(())
}

#[cfg(windows)]
pub async fn import(store: &FsStore, source: PathBuf) -> Result<(TempTag, u64)> {
    let (send, recv) = mpsc::channel(2);
    let task = tokio::task::spawn_blocking(move || read_source(&source, send));
    let imported = store.blobs().add_stream(ReceiverStream::new(recv)).await.temp_tag().await;
    // A completed import alone cannot prove the file remained unchanged. No offer
    // starts until the producer checks the final stamp and oplock.
    let size = task.await??;
    Ok((imported?, size))
}

#[cfg(windows)]
fn read_source(path: &Path, send: mpsc::Sender<io::Result<Bytes>>) -> Result<u64> {
    use std::io::Read as _;
    use std::os::windows::fs::MetadataExt as _;

    use oneup_ntfs::{dirent, win};

    oneup_cfapi::expose_placeholders()?;
    let path = std::path::absolute(path)?;
    ensure!(oneup_cfapi::other_sync_root(&path).is_none(), "Choose a file outside other providers' sync folders.");
    let meta = fs::symlink_metadata(&path)?;
    ensure!(meta.is_file() && meta.file_attributes() & dirent::NOT_PLAIN == 0, "Choose an ordinary local file.");
    let volume = win::Volume::open_containing(&path).context("Could not open the source volume.")?;
    let mut file = volume.open_watched_path(&path).context("Could not open the source file.")?;
    let stamp = |file: &win::WatchedFile| {
        file.with_handle(|h| -> Result<_> {
            let (attrs, tag) = win::attribute_tag(h)?;
            ensure!(
                attrs & (dirent::NOT_PLAIN | dirent::FILE_ATTRIBUTE_DIRECTORY) == 0 && tag == 0,
                "Choose an ordinary local file."
            );
            ensure!(
                oneup_cfapi::other_sync_root(&win::final_path(h)?).is_none(),
                "Choose a file outside other providers' sync folders."
            );
            let meta = h.as_file().metadata()?;
            Ok((meta.len(), meta.last_write_time()))
        })
        .context("Source file changed.")?
    };
    let before = stamp(&file)?;
    let mut count = 0;
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        count += len as u64;
        if send.blocking_send(Ok(Bytes::copy_from_slice(&buffer[..len]))).is_err() {
            return Ok(count); // Cancelled or the blob store stopped receiving.
        }
    }
    ensure!(!file.broken() && before.0 == count && before == stamp(&file)?, "Source file changed.");
    Ok(count)
}
