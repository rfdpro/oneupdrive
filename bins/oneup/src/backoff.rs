//! Failed operations wait before they're tried again (the agent's; `oneup
//! sync` tries everything at once): a minute after the first failure, then
//! twice as long after each further one, at most an hour
//! (`oneup_store::retry`, kept in the database across restarts).
//!
//! A waiting item's folder stays marked, so it isn't forgotten, but a round
//! leaves its operation out until its time (`crate::sync`), and other items
//! in the folder go as usual. The wait is for the operation as planned
//! ([`version`]): once the item changes on either side (a new local stamp, a
//! new cloud eTag), the new plan goes at once. Success ends it.

use oneup_exec::Report;
use oneup_store::{Ops, Retry, RetryItem};
use oneup_sync::Op;

/// The item `op` acts on, whose failures make it wait. `None`: bookkeeping
/// (pairing, records), which waits for nothing.
pub fn item(op: &Op) -> Option<RetryItem<'_>> {
    Some(match op {
        Op::Upload { local, .. }
        | Op::Hash(local)
        | Op::Dehydrate { local, .. }
        | Op::MkdirRemote { local, .. }
        | Op::MoveLocal { local, .. }
        | Op::RenameLocal { local, .. }
        | Op::ConflictCopy { local, .. }
        | Op::RecycleLocal { local, .. }
        | Op::RemoveCopy { remote: None, local: Some(local), .. } => RetryItem::Local(local.id),
        Op::RemoveCopy { remote: Some(remote), .. } => RetryItem::Remote(&remote.id),
        Op::RemoveCopy { remote: None, local: None, .. } => return None,
        Op::Download { remote, .. }
        | Op::MkdirLocal { remote, .. }
        | Op::MoveRemote { remote, .. }
        | Op::RenameRemote { remote, .. }
        | Op::DeleteRemote { remote, .. }
        | Op::PruneRemote { remote, .. } => RetryItem::Remote(&remote.id),
        Op::Rewind { step, .. } => match &**step {
            oneup_sync::rewind::Step::SetAside { local } | oneup_sync::rewind::Step::Discard { local } => {
                RetryItem::Local(local.id)
            }
            oneup_sync::rewind::Step::Restore { id, .. } => RetryItem::Remote(id),
            oneup_sync::rewind::Step::Reset { remote, .. }
            | oneup_sync::rewind::Step::Remove { remote, .. }
            | oneup_sync::rewind::Step::RevertTo { remote, .. } => RetryItem::Remote(&remote.id),
            oneup_sync::rewind::Step::Forget | oneup_sync::rewind::Step::RestoreKept { .. } => return None,
        },
        Op::Pair { .. } | Op::Record(_) | Op::Forget(_) | Op::Hold { .. } => return None,
    })
}

/// `op` as planned, its preconditions (the local stamp, the cloud eTag)
/// included: any other plan for the item is another version, which
/// doesn't wait.
pub fn version(op: &Op) -> String {
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(format!("{op:?}").as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Until when `op` waits at `now_ms` (Unix ms), if it does.
pub fn until(o: &Ops<'_>, op: &Op, now_ms: i64) -> oneup_store::Result<Option<i64>> {
    let Some(item) = item(op) else { return Ok(None) };
    Ok(o.retry_get(item)?.filter(|r| r.holds(&version(op), now_ms)).map(|r| r.until_ms))
}

/// What a round's failures came to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    /// The failures that now wait.
    pub waiting: usize,
    /// When the first of them may go (Unix ms).
    pub first_ms: Option<i64>,
}

/// Records what a round did at `now_ms`: its failures wait, what was done
/// is through with its failures, and records of items not tried for a long
/// while are dropped.
pub fn record(o: &Ops<'_>, report: &Report, now_ms: i64) -> oneup_store::Result<Recorded> {
    let mut out = Recorded::default();
    // Most rounds have nothing on record and nothing failed: no writes.
    if !o.retry_any()? && report.failed.is_empty() {
        return Ok(out);
    }
    for op in &report.done {
        if let Some(item) = item(op) {
            o.retry_clear(item)?;
        }
    }
    for (op, error) in &report.failed {
        if let Some(item) = item(op) {
            let r = o.retry_failed(item, &version(op), error, now_ms)?;
            out.waiting += 1;
            out.first_ms = Some(out.first_ms.map_or(r.until_ms, |f| f.min(r.until_ms)));
        }
    }
    o.retry_expire(now_ms)?;
    Ok(out)
}

/// How `oneup explain` tells an item's wait, at `now_ms`.
pub fn describe(r: &Retry, now_ms: i64) -> String {
    let times = if r.failures == 1 { "once".to_string() } else { format!("{} times in a row", r.failures) };
    let when = match (r.until_ms - now_ms + 59_999) / 60_000 {
        m if m <= 0 => "at the next round".to_string(),
        m if m < 60 => format!("in {m} min"),
        m => format!("in {} h {} min", m / 60, m % 60),
    };
    format!("failed {times}; tried again {when} ({})", r.error)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_store::Store;
    use oneup_sync::{Link, LocalId, LocalPre, RemoteId, RemotePre};

    fn upload(stamp: (i64, i64), etag: &str) -> Op {
        Op::Upload {
            local: LocalPre { id: LocalId(4), stamp: Some(stamp) },
            to: RemoteId("d".into()),
            parent: 1,
            name: "a.txt".into(),
            replace: Some(RemotePre { id: RemoteId("r".into()), etag: etag.into() }),
            link: Link::Existing(2),
        }
    }

    fn failed(op: &Op) -> Report {
        Report { failed: vec![(op.clone(), "HTTP 500".into())], ..Report::default() }
    }

    #[test]
    fn a_failure_waits_and_a_change_on_either_side_ends_the_wait() {
        let mut s = Store::open_in_memory().unwrap();
        let op = upload((7, 9), "e1");
        let r = s.write(|o| record(o, &failed(&op), 0)).unwrap();
        assert_eq!(r, Recorded { waiting: 1, first_ms: Some(60_000) });
        let wait = |s: &Store, op: &Op, at| s.read(|o| until(o, op, at)).unwrap();
        assert_eq!(wait(&s, &op, 1_000), Some(60_000), "the same upload waits");
        assert_eq!(wait(&s, &op, 60_000), None, "until its time");
        assert_eq!(wait(&s, &upload((8, 9), "e1"), 1_000), None, "the file changed here");
        assert_eq!(wait(&s, &upload((7, 9), "e2"), 1_000), None, "it changed in OneDrive");
        // Failing again doubles the wait; success ends it.
        let r = s.write(|o| record(o, &failed(&op), 60_000)).unwrap();
        assert_eq!(r.first_ms, Some(180_000));
        let done = Report { done: vec![upload((8, 9), "e1")], ..Report::default() };
        s.write(|o| record(o, &done, 70_000)).unwrap();
        assert_eq!(wait(&s, &op, 70_000), None);
        assert!(!s.read(|o| o.retry_any()).unwrap());
    }

    #[test]
    fn bookkeeping_never_waits_and_other_items_are_unaffected() {
        let mut s = Store::open_in_memory().unwrap();
        let r = s.write(|o| record(o, &failed(&Op::Forget(3)), 0)).unwrap();
        assert_eq!(r, Recorded::default());
        s.write(|o| record(o, &failed(&upload((7, 9), "e1")), 0)).unwrap();
        let download = Op::Download {
            remote: RemotePre { id: RemoteId("x".into()), etag: "e".into() },
            to: LocalId(1),
            parent: 1,
            name: "b.txt".into(),
            replace: None,
            link: Link::New,
        };
        assert_eq!(s.read(|o| until(o, &download, 1)).unwrap(), None);
    }

    #[test]
    fn the_wait_is_told_plainly() {
        let r = |failures, until_ms| Retry { version: String::new(), failures, until_ms, error: "HTTP 500".into() };
        assert_eq!(describe(&r(1, 60_000), 0), "failed once; tried again in 1 min (HTTP 500)");
        assert_eq!(describe(&r(3, 4_000_000), 0), "failed 3 times in a row; tried again in 1 h 7 min (HTTP 500)");
        assert_eq!(describe(&r(2, 0), 5), "failed 2 times in a row; tried again at the next round (HTTP 500)");
    }
}
