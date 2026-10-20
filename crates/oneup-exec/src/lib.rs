//! The executor: carries out one round's gate-approved operations and
//! records each outcome in the state store.
//!
//! * **Side by side where independent.** An operation waits for every
//!   earlier one it depends on (the same item, a folder it works in that
//!   another changes, a name another frees or takes), so dependent
//!   operations keep the planner's order. Independent ones run
//!   concurrently, up to the memory profile's limits; moves, renames and
//!   removals of folders run alone (`claims_in` in `exec.rs`, `schedule.rs`).
//! * **Disk work off the task.** The operations under way share one task.
//!   Whatever reads a file (an upload's fragments, hashing, checking an old
//!   copy before it's dropped) runs on a blocking thread (`blocking.rs`),
//!   and writes to the store wait for another program's write without
//!   blocking the task (`Db` in `exec.rs`), so a slow disk or a busy
//!   database doesn't stall the transfers under way.
//! * **New folders' contents in the same round.** Once an operation that
//!   made a folder is recorded, the caller's planner and gate plan and
//!   review what goes into it, and that runs after it in the same round
//!   (`Executor::run_following`), so a new tree doesn't take a round per
//!   level.
//! * Local changes go through [`oneup_effects::LocalFs`], cloud changes
//!   through a [`Cloud`] (Graph in production, a fake in tests). Neither is
//!   reachable without an [`oneup_gate::Approved`] token for that operation.
//! * Bookkeeping operations (`Pair`, `Record`, `Forget`) re-check their
//!   preconditions against the store, which the observer and the mirror
//!   keep current. They touch no user content.
//! * After an effect, the executor writes the rows it changed itself
//!   (Local tree, Remote tree, Synced tree, hashes) in one transaction, so
//!   the next round plans from the new state without waiting for the change
//!   journal or delta. When those report the same change later, applying it
//!   again changes nothing.
//! * The user's answers for held items ([`resolve`]) only change what the
//!   planner reads; what they lead to runs as any planned operation does.
//! * **Crash safety.** A crash between an effect and its record must not
//!   leave anything that looks like a deletion without evidence (which would
//!   be held forever) or, worse, evidence without a deletion. Most effects
//!   recover by themselves: the next round finds both sides agreeing and
//!   pairs or records them. Three remove an item from the tree in a way the
//!   change journal sees only as a move out (into the version store), so
//!   their record is written **ahead**: recycling and replacing note
//!   deletion evidence first, and a conflict copy detaches its Synced row
//!   first. The same transaction saves which record is pending (in `meta`,
//!   `exec.pending.<local file ID>`, one per item), and the final record
//!   clears it. That transaction commits durably (`synchronous = FULL`,
//!   where the rest commit at NORMAL): a power cut can roll back the
//!   newest commits, and must not take a record whose effect is done. One left over (the operation failed, or the process died)
//!   is settled right after the operation, before it releases its claims,
//!   or at the start of the next run: if the folder shows the effect didn't
//!   happen, the record is undone. `tests/sync.rs` crashes a round at every
//!   such point, and between the steps of effects that take several
//!   (feature `crash-test`), and restarts it.
//! * A failed precondition **skips** the operation; the next round re-plans
//!   it from fresh state. A failed identity check of the sync folder, or a
//!   database error, **halts** the round; operations still under way are
//!   dropped where they wait, as a crash would leave them, once what they
//!   handed to a blocking thread has run to its end.

#[cfg(windows)]
mod blocking;
pub mod cloud;
#[cfg(windows)]
mod exec;
pub mod graph;
#[cfg(windows)]
pub mod hydrate;
#[cfg(windows)]
pub mod placement;
pub mod resolve;
#[cfg(windows)]
mod schedule;
pub mod timing;

pub use cloud::{Cloud, CloudError, Meta, MetaDone, Source, VersionMark};
#[cfg(all(windows, feature = "crash-test"))]
pub use exec::{BlockingHook, CRASHED};
#[cfg(windows)]
pub use exec::{Executor, FOLLOW_MAX, Follow, Report, Skip, link_roots};
pub use graph::{GraphCloud, UploadMemory};
#[cfg(windows)]
pub use schedule::Parallel;
pub use timing::Timings;
