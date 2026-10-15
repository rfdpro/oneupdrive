//! OneUpDrive's first-run planner, in **observe mode**: it compares the
//! Local tree (a folder scan) with the Remote tree (the OneDrive mirror) and
//! reports what a sync *would* do. It never changes anything.
//!
//! * [`plan_folder`]: the pure decision for one folder's children. No I/O;
//!   every rule is unit-tested.
//! * [`names`]: OneDrive's naming, path-length and size limits.
//! * [`observe`]: walks both trees from the state database and builds a
//!   [`Report`]. With a [`ContentCheck`] it also compares the contents of
//!   same-size files by QuickXorHash.
//!
//! First-run semantics. With no Synced tree there is no record of what was
//! in sync before, so the planner can't tell "deleted here" from "new there".
//! It therefore never proposes a deletion: an item on one side only is an
//! upload or a download, and a disagreement is a conflict in which both
//! copies are kept.

pub mod folder;
pub mod names;
pub mod observe;
pub mod park;
pub mod report;

pub use folder::{FolderPlan, Next, Verified, VerifyRequest, after_verify, plan_folder};
pub use observe::{Basis, ContentCheck, Error, Progress, basis, observe};
pub use park::Parked;
pub use report::{Category, Conflict, Entry, NotSynced, Park, Report, Tally, Unchecked};
