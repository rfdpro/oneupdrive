//! Work that blocks a thread, run off the executor's task.
//!
//! A round's operations share one task, so anything that blocks it (reading
//! a file, hashing one, an effect that checks a file's content before it
//! acts) holds up every operation under way, including transfers that a
//! stall of a minute makes give up. Such work runs on a blocking thread
//! ([`Jobs::run`]) while the task goes on with the rest.
//!
//! A round never ends while any of it still runs: an effect left running
//! after a halt would change the folder behind the next round's back (the
//! settling of write-ahead records at its start included). After a halt the
//! round waits for it ([`Jobs::settled`]), which leaves things as a crash
//! right after that effect would; dropping a round under way waits too
//! ([`Settle`]).

use std::io::Read;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use futures_util::future::BoxFuture;
use oneup_hash::QuickXorHash;

use crate::cloud::{Source, read_full};

/// Blocking work the executor started and that hasn't returned yet.
#[derive(Default)]
pub(crate) struct Jobs {
    running: Mutex<usize>,
    idle: Condvar,
    finished: tokio::sync::Notify,
}

impl Jobs {
    fn running(&self) -> MutexGuard<'_, usize> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on a blocking thread, counted until it returns (or is
    /// dropped unrun, if the runtime is shutting down). The work goes on
    /// if the returned future is dropped: wait for it with
    /// [`Jobs::settled`].
    pub fn run<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> tokio::task::JoinHandle<T> {
        *self.running() += 1;
        let counted = Counted(Arc::clone(self));
        tokio::task::spawn_blocking(move || {
            let _counted = counted;
            f()
        })
    }

    /// Waits until no work started by [`Jobs::run`] still runs.
    pub async fn settled(&self) {
        loop {
            let finished = self.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            if *self.running() == 0 {
                return;
            }
            finished.await;
        }
    }

    /// [`Jobs::settled`], blocking the thread: for a round dropped under
    /// way, which can't wait otherwise.
    fn settled_blocking(&self) {
        let mut running = self.running();
        while *running > 0 {
            running = self.idle.wait(running).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// One job's place in [`Jobs`]' count, given back when it's dropped.
struct Counted(Arc<Jobs>);

impl Drop for Counted {
    fn drop(&mut self) {
        let mut running = self.0.running();
        *running -= 1;
        if *running == 0 {
            self.0.idle.notify_all();
            self.0.finished.notify_waiters();
        }
    }
}

/// Waits, when dropped, until the blocking work [`Jobs`] counts is done.
/// A round holds one so that, ended any way, nothing it started still
/// runs; one that ends normally waits with [`Jobs::settled`] first, so this
/// finds nothing left.
///
/// Dropped under way, it blocks the dropping thread, which may be the only
/// thread of a current-thread runtime. That can't deadlock: the work runs
/// on the blocking pool's own threads and never waits for the runtime (no
/// job awaits anything, takes the store or calls back into the task), and
/// work the pool drops unrun gives its count back as it's dropped.
/// (`a_round_dropped_while_it_recycles_a_file_waits_for_the_recycling`.)
pub(crate) struct Settle<'a>(pub &'a Jobs);

impl Drop for Settle<'_> {
    fn drop(&mut self) {
        self.0.settled_blocking();
    }
}

/// What a read that was lost (its thread panicked, or the runtime is
/// shutting down) leaves: the source went with it.
fn lost() -> std::io::Error {
    std::io::Error::other("a file read off the executor's task was lost")
}

/// A file an upload reads, a block at a time on a blocking thread
/// ([`Source::fill`]), hashed as it's read. Through `Read` it's read in
/// place (see [`Source`]), hashed the same way.
///
/// The source's own checks travel with it: reads of an
/// [`oneup_effects::UploadSource`] fail with
/// [`oneup_effects::ChangedMeanwhile`] once the file was written, on
/// whatever thread they run, and the error comes back as it was.
pub(crate) struct OffTask<R> {
    /// `None` while a read is under way, and after one was lost.
    src: Option<R>,
    /// What was read so far.
    pub hash: QuickXorHash,
    jobs: Arc<Jobs>,
}

impl<R: Read + Send + 'static> OffTask<R> {
    pub fn new(src: R, jobs: &Arc<Jobs>) -> Self {
        Self { src: Some(src), hash: QuickXorHash::new(), jobs: Arc::clone(jobs) }
    }

    /// The source, unless a read lost it.
    pub fn get(&self) -> Option<&R> {
        self.src.as_ref()
    }

    /// Runs `f` on the source on a blocking thread.
    pub async fn with<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut R) -> T + Send + 'static,
    ) -> std::io::Result<T> {
        let mut src = self.src.take().ok_or_else(lost)?;
        let (src, out) = self
            .jobs
            .run(move || {
                let out = f(&mut src);
                (src, out)
            })
            .await
            .map_err(|_| lost())?;
        self.src = Some(src);
        Ok(out)
    }
}

impl<R: Read> Read for OffTask<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.src.as_mut().ok_or_else(lost)?.read(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }
}

impl<R: Read + Send + 'static> Source for OffTask<R> {
    fn fill(&mut self, mut buf: Vec<u8>, want: usize) -> BoxFuture<'_, (Vec<u8>, std::io::Result<usize>)> {
        Box::pin(async move {
            let mut hash = std::mem::take(&mut self.hash);
            let done = self
                .with(move |src| {
                    let read = read_full(src, &mut buf[..want]);
                    if let Ok(n) = read {
                        hash.update(&buf[..n]);
                    }
                    (buf, hash, read)
                })
                .await;
            match done {
                Ok((buf, hash, read)) => {
                    self.hash = hash;
                    (buf, read)
                }
                Err(e) => (Vec::new(), Err(e)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_read_off_the_task_reads_and_hashes_like_one_in_place() {
        let jobs = Arc::new(Jobs::default());
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut off = OffTask::new(std::io::Cursor::new(data.clone()), &jobs);
        let mut got = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.resize(70_000, 0);
            let (b, read) = off.fill(std::mem::take(&mut buf), 65_536).await;
            buf = b;
            let n = read.unwrap();
            got.extend_from_slice(&buf[..n]);
            if n < 65_536 {
                break;
            }
        }
        assert_eq!(got, data);
        let mut whole = QuickXorHash::new();
        whole.update(&data);
        assert_eq!((off.hash.finalize(), off.hash.len()), (whole.finalize(), whole.len()));
        jobs.settled().await;
        assert_eq!(*jobs.running(), 0);
    }

    #[tokio::test]
    async fn a_read_error_comes_back_as_it_was() {
        struct Fails;
        impl Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no"))
            }
        }
        let jobs = Arc::new(Jobs::default());
        let mut off = OffTask::new(Fails, &jobs);
        let (_, read) = off.fill(vec![0; 10], 10).await;
        assert_eq!(read.map_err(|e| e.kind()), Err(std::io::ErrorKind::PermissionDenied));
        assert!(off.get().is_some(), "the source stays for what comes next");
    }

    #[tokio::test]
    async fn settling_waits_for_work_whose_future_was_dropped() {
        let jobs = Arc::new(Jobs::default());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&done);
        drop(jobs.run(move || {
            let _ = rx.recv();
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        let settled = jobs.settled();
        tokio::pin!(settled);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), &mut settled).await.is_err());
        tx.send(()).unwrap();
        settled.await;
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
    }
}
