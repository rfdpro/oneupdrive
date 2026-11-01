//! Hydration: serving `FETCH_DATA` for OneUpDrive's placeholders
//! (`docs/architecture/files-on-demand.md` § Hydration, `data-safety.md`
//! invariant 13). For each request, on a tokio task (never on Windows'
//! callback thread):
//!
//! 1. the identity must parse, match the placeholder's size, and name the
//!    Remote tree's current version of the item; otherwise the request is
//!    refused and the agent woken, so a round updates the placeholder;
//! 2. the content is downloaded into staging in the data folder (outside
//!    the sync folder: nothing is written there but by transfers);
//! 3. its size and QuickXorHash must equal the identity's;
//! 4. it is transferred from staging, with progress, and the request
//!    completed; the staging file is removed.
//!
//! Anything else fails the request: the reader gets an error, and the
//! placeholder stays dehydrated.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use oneup_cfapi::identity::Identity;
use oneup_cfapi::provider::{self, Cancel, Fetch, Provider};
use oneup_store::{ActivityEntry, ActivityOutcome, AppFile, AppUse, Store, UseKind};
use oneup_sync::{RemoteId, RemotePre};

use crate::cloud::{Cloud, Hashing};

/// Transfer chunks: a multiple of 4 KiB, as Cloud Files requires.
const CHUNK: usize = 1 << 20;

/// What a refused request reports to the caller (for tests and logs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    BadIdentity,
    NotCurrent,
    Download,
    Mismatch,
}

pub struct Hydrator<C> {
    /// Serves programs opening a file: the user's own action.
    cloud: C,
    /// Serves OneUpDrive's own requests (files set to always stay on this
    /// device), on the data budget; `None`: as `cloud`.
    ours: Option<C>,
    /// A connection of its own to the state database (the agent's is busy
    /// with rounds); read for the Remote tree, written for the activity log.
    store: Mutex<Store>,
    /// In the data folder, never in the sync folder.
    staging: PathBuf,
    /// The sync folder as Windows names it in callbacks (`\Users\…`,
    /// without the drive), to log paths relative to it.
    root: String,
    rt: tokio::runtime::Handle,
    /// Called when a placeholder turned out to be out of date: wakes the
    /// agent so a round updates it.
    on_stale: Box<dyn Fn() + Send + Sync>,
    running: Mutex<HashMap<i64, tokio::task::AbortHandle>>,
    seq: AtomicU64,
    /// Refusals so far (for tests).
    pub refused: Mutex<Vec<Refusal>>,
}

impl<C: Cloud + 'static> Hydrator<C> {
    pub fn new(
        cloud: C,
        store: Store,
        staging: PathBuf,
        sync_root: &std::path::Path,
        rt: tokio::runtime::Handle,
        on_stale: Box<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Arc<Self>> {
        Self::with_ours(cloud, None, store, staging, sync_root, rt, on_stale)
    }

    /// [`Hydrator::new`], serving OneUpDrive's own requests through `ours`
    /// (the agent's: on the data budget), and only programs opening files
    /// through `cloud`. Which one is decided from who asked, before
    /// anything is downloaded.
    #[allow(clippy::too_many_arguments)]
    pub fn with_ours(
        cloud: C,
        ours: Option<C>,
        store: Store,
        staging: PathBuf,
        sync_root: &std::path::Path,
        rt: tokio::runtime::Handle,
        on_stale: Box<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Arc<Self>> {
        std::fs::create_dir_all(&staging)?;
        // Leftovers of an interrupted run are ours (only our extension).
        for e in std::fs::read_dir(&staging)?.flatten() {
            if e.path().extension().is_some_and(|x| x == "hydrating") {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Ok(Arc::new(Self {
            cloud,
            ours,
            store: Mutex::new(store),
            staging,
            root: volume_relative(sync_root),
            rt,
            on_stale,
            running: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            refused: Mutex::new(Vec::new()),
        }))
    }

    async fn serve(self: &Arc<Self>, fetch: Fetch) {
        let key = fetch_key(&fetch);
        let path = fetch.path.clone();
        // The content arrives by writes the program tracker would otherwise
        // count as a program's (`oneup_cfapi::ours`).
        oneup_cfapi::ours::note(u128::from(fetch.file_id as u64));
        match self.hydrate(&fetch).await {
            Ok(n) => {
                let who = fetch.process.as_ref().map(|p| image_name(&p.image)).unwrap_or_default();
                let file_id = fetch.file_id;
                // Asked for by OneUpDrive itself (a file set to always keep on
                // this device): no program used it.
                let ours = fetch.process.as_ref().is_some_and(|p| p.pid == std::process::id());
                fetch.complete();
                if ours {
                    self.log(ActivityOutcome::Done, &path, format!("{n} bytes, kept on this device"));
                } else {
                    self.log(ActivityOutcome::Done, &path, format!("{n} bytes, opened by {who}"));
                    self.note_use(file_id, who, n, &path);
                }
            }
            Err(why) => {
                tracing::debug!(?why, "a hydration was refused");
                if why == Refusal::NotCurrent {
                    (self.on_stale)();
                }
                self.refused.lock().unwrap_or_else(|p| p.into_inner()).push(why);
                fetch.fail(provider::NOT_IN_SYNC);
                self.log(ActivityOutcome::Failed, &path, format!("{why:?}"));
            }
        }
        self.running.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
    }

    async fn hydrate(&self, fetch: &Fetch) -> Result<u64, Refusal> {
        let id = Identity::decode(&fetch.identity).ok_or(Refusal::BadIdentity)?;
        if id.size != fetch.file_size {
            return Err(Refusal::NotCurrent);
        }
        let current = {
            let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
            store.read(|o| o.get_item(&id.item)).map_err(|_| Refusal::NotCurrent)?
        };
        let current = current.ok_or(Refusal::NotCurrent)?;
        if current.etag.as_deref() != Some(id.etag.as_str()) || current.size != id.size || current.qxh != Some(id.hash)
        {
            return Err(Refusal::NotCurrent);
        }

        let staged =
            Staged(self.staging.join(format!("{}-{}.hydrating", std::process::id(), self.seq.fetch_add(1, Relaxed))));
        self.download_and_transfer(fetch, &id, &staged.0).await
    }

    async fn download_and_transfer(&self, fetch: &Fetch, id: &Identity, path: &PathBuf) -> Result<u64, Refusal> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|_| Refusal::Download)?;
        let pre = RemotePre { id: RemoteId(id.item.clone()), etag: id.etag.clone() };
        let cloud = self.cloud_for(asked_by_us(fetch.process.as_ref(), std::process::id()));
        let (len, hash) = {
            let mut w = Hashing::new(&mut file);
            let received = cloud.download(&pre, &mut w).await.map_err(|_| Refusal::Download)?;
            // The identity's hash is the cloud's (see `cloud`).
            (w.hash.len(), received.unwrap_or_else(|| w.hash.finalize()))
        };
        if i64::try_from(len).ok() != Some(id.size) || hash != id.hash {
            return Err(Refusal::Mismatch);
        }
        file.flush().map_err(|_| Refusal::Download)?;
        file.seek(SeekFrom::Start(0)).map_err(|_| Refusal::Download)?;
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0i64;
        while offset < id.size {
            let n = file.read(&mut buf).map_err(|_| Refusal::Download)?;
            if n == 0 {
                return Err(Refusal::Mismatch);
            }
            fetch.transfer(offset, &buf[..n]).map_err(|_| Refusal::Download)?;
            offset += n as i64;
            fetch.progress(id.size, offset);
        }
        Ok(len)
    }

    /// Remembers that a program used this file (at `path`) now, for the
    /// space policies, and counts it for that program (`app_use`, and the
    /// file in `app_file`).
    fn note_use(&self, file_id: i64, app: String, bytes: u64, path: &str) {
        let now = now_ms();
        // NTFS's 64-bit file reference, as the Local tree stores it (128-bit).
        let id = u128::from(file_id as u64);
        let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
        let file = AppFile { at_ms: now, app: app.clone(), kind: UseKind::Opened, path: self.relative(path), bytes };
        let use_ = AppUse { hour: now / 3_600_000, app, kind: UseKind::Opened, files: 1, bytes };
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = store.write(|o| {
            o.local_used_set(id, now)?;
            o.app_use_add(&[use_])?;
            o.app_files_add(&[file])
        }) {
            tracing::debug!(error = %e, "a file's use wasn't recorded");
        }
    }

    /// A request's path as the log keeps it: relative to the synced
    /// folder, `/`-separated.
    fn relative(&self, path: &str) -> String {
        let rel = path.strip_prefix(self.root.as_str()).unwrap_or(path);
        format!("/{}", rel.trim_start_matches(['\\', '/']).replace('\\', "/"))
    }

    fn log(&self, outcome: ActivityOutcome, path: &str, detail: String) {
        let path = self.relative(path);
        let e = ActivityEntry { seq: 0, at_ms: now_ms(), outcome, action: "open".into(), path, detail };
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = store.write(|o| o.activity_add(&[e])) {
            tracing::debug!(error = %e, "a hydration wasn't logged");
        }
    }
}

impl<C> Hydrator<C> {
    /// The cloud a request is served through: OneUpDrive's own (a file set
    /// to always stay here) on the data budget, a program's opening not.
    fn cloud_for(&self, ours: bool) -> &C {
        match (&self.ours, ours) {
            (Some(c), true) => c,
            _ => &self.cloud,
        }
    }
}

/// Whether a request comes from this process (`me`): OneUpDrive itself
/// downloading a file set to always stay on this device, not a program
/// opening it. Unknown requesters count as programs.
fn asked_by_us(process: Option<&provider::Process>, me: u32) -> bool {
    process.is_some_and(|p| p.pid == me)
}

/// The wrapper Cloud Files calls (an `Arc` so tasks can keep it).
pub struct Serve<C>(pub Arc<Hydrator<C>>);

impl<C: Cloud + 'static> Provider for Serve<C> {
    fn fetch_data(&self, fetch: Fetch) {
        let me = Arc::clone(&self.0);
        let key = fetch_key(&fetch);
        let task = self.0.rt.spawn(async move { me.serve(fetch).await });
        self.0.running.lock().unwrap_or_else(|p| p.into_inner()).insert(key, task.abort_handle());
    }

    fn cancel_fetch(&self, cancel: Cancel) {
        // The aborted task drops its `Fetch`, which fails the request.
        if let Some(t) = self.0.running.lock().unwrap_or_else(|p| p.into_inner()).remove(&cancel.transfer_key) {
            t.abort();
        }
    }
}

/// A download's staging file, removed when dropped: also when a cancelled
/// fetch aborts its task mid-download, so repeated cancels of big files
/// don't fill the disk until the next start.
struct Staged(PathBuf);

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn fetch_key(f: &Fetch) -> i64 {
    f.transfer_key()
}

/// `\\?\C:\Users\x` → `\Users\x` (how Cloud Files names paths).
fn volume_relative(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let s = match s.get(1..2) {
        Some(":") => &s[2..],
        _ => s,
    };
    s.trim_end_matches('\\').to_string()
}

fn image_name(image: &str) -> String {
    image.rsplit(['\\', '/']).next().unwrap_or(image).to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Who asked decides the cloud, before anything is downloaded: this
    /// process (a file set to stay here) the budgeted one, a program
    /// opening the file the other.
    #[test]
    fn our_own_requests_go_on_the_budget_and_programs_opening_files_dont() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let h = Hydrator {
            cloud: "opened",
            ours: Some("budgeted"),
            store: Mutex::new(Store::open_in_memory().unwrap()),
            staging: PathBuf::new(),
            root: String::new(),
            rt: rt.handle().clone(),
            on_stale: Box::new(|| {}),
            running: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            refused: Mutex::new(Vec::new()),
        };
        let me = std::process::id();
        let proc = |pid| provider::Process { pid, image: "x.exe".into() };
        assert_eq!(*h.cloud_for(asked_by_us(Some(&proc(me)), me)), "budgeted");
        assert_eq!(*h.cloud_for(asked_by_us(Some(&proc(me + 1)), me)), "opened");
        assert_eq!(*h.cloud_for(asked_by_us(None, me)), "opened");
        let one = Hydrator { ours: None, ..h };
        assert_eq!(*one.cloud_for(true), "opened");
    }

    /// A cancel aborts the hydration task mid-download: its staging file,
    /// still open and part-written, goes with it.
    #[test]
    fn an_aborted_download_leaves_no_staging_file() {
        let dir = std::env::temp_dir().join(format!("oneup-exec-staged-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("1-0.hydrating");
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (started, written) = std::sync::mpsc::channel();
        let staged = Staged(path.clone());
        let task = rt.spawn(async move {
            let mut file = std::fs::File::create_new(&staged.0).unwrap();
            file.write_all(b"part of a download").unwrap();
            started.send(()).unwrap();
            std::future::pending::<()>().await;
            drop((file, staged));
        });
        rt.block_on(async {
            while written.try_recv().is_err() {
                tokio::task::yield_now().await;
            }
        });
        assert!(path.exists());
        task.abort();
        let _ = rt.block_on(task);
        assert!(!path.exists(), "removed with the aborted task");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
