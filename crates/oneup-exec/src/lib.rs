//! The executor: carries out one round's gate-approved operations, in the
//! planner's order, and records each outcome in the state store.
//!
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
//! * **Crash safety.** A crash between an effect and its record must not
//!   leave anything that looks like a deletion without evidence (which would
//!   be held forever) or, worse, evidence without a deletion. Most effects
//!   recover by themselves: the next round finds both sides agreeing and
//!   pairs or records them. Three remove an item from the tree in a way the
//!   change journal sees only as a move out (into the version store), so
//!   their record is written **ahead**: recycling and replacing note
//!   deletion evidence first, and a conflict copy detaches its Synced row
//!   first. The same transaction saves which record is pending (in `meta`,
//!   `exec.pending`), and the final record clears it. One left over (the
//!   operation failed, or the process died) is settled after the operation
//!   or at the start of the next run: if the folder shows the effect didn't
//!   happen, the record is undone. `tests/sync.rs` crashes a round at every
//!   such point (feature `crash-test`) and restarts it.
//! * A failed precondition **skips** the operation; the next round re-plans
//!   it from fresh state. A failed identity check of the sync folder, or a
//!   database error, **halts** the round.

pub mod cloud;
#[cfg(windows)]
mod exec;
pub mod graph;
#[cfg(windows)]
pub mod hydrate;
#[cfg(windows)]
pub mod placement;

pub use cloud::{Cloud, CloudError, Meta, MetaDone};
#[cfg(all(windows, feature = "crash-test"))]
pub use exec::CRASHED;
#[cfg(windows)]
pub use exec::{Executor, Report, Skip, link_roots};
pub use graph::{GraphCloud, UploadMemory};
