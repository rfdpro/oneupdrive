//! OneUpDrive's three-tree reconciler.
//!
//! Given the **Local** tree (what's on disk), the **Remote** tree (what's in
//! OneDrive) and the **Synced** tree (what both last agreed on), it proposes
//! the operations that bring the two sides together, without ever guessing
//! direction from timestamps and without ever losing a change. The rules and
//! their reasons are in `docs/architecture/sync-engine.md`.
//!
//! The crate is **pure**: it reads the trees through [`Trees`] and returns
//! [`Op`]s. It has no I/O and no clock, so it runs unchanged under the
//! deterministic simulation in `tests/sim.rs`. Carrying the operations out
//! safely (re-checking preconditions, `If-Match`, the safety gate, the
//! intent journal) is the executor's job, in later steps.
//!
//! A sync **round** is: plan → execute what still applies → plan again.
//! Operations that depend on others (a child needs its new parent; a name
//! must be vacated first) simply happen in a later round. Rounds stop when
//! a plan contains no actions ([`Op::is_action`]).

mod merge;
pub mod model;
pub mod op;
mod resolve;

pub use merge::{conflict_candidate, conflict_name};
pub use model::{
    Base, Hash, LocalId, LocalNode, NodeKind, ROOT, RemoteId, RemoteNode, Seen, Sid, Trees, fold, fold_char,
};
pub use op::{Dir, HoldReason, Link, LocalPre, Op, RemotePre, Vacates};

/// Plans one round over the given Synced folders (each folder's *children*
/// are decided). A full pass passes every Synced folder ([`all_folders`]);
/// steady state passes only folders touched by changes, at both their old
/// and new locations.
///
/// If the sync folder or the drive root isn't there, nothing is planned and
/// the root is held: a missing root means an unmounted disk or a replaced
/// folder, never "everything was deleted" (`data-safety.md` invariant 4).
pub fn plan<T: Trees + ?Sized>(t: &T, folders: impl IntoIterator<Item = Sid>) -> Result<Vec<Op>, T::Error> {
    plan_flow(t, folders, Flow::TwoWay)
}

/// Which way changes travel in a folder pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Flow {
    /// Both ways (the default): what changes on either side reaches the other.
    #[default]
    TwoWay,
    /// One way, this PC → OneDrive: a backup. New and changed files and
    /// folders, renames and moves made here reach the cloud; **deletions
    /// here never do**, and nothing in the cloud ever changes this PC. When
    /// both sides changed a file, this PC's version wins and the cloud's
    /// stays in OneDrive's version history. Something the cloud deleted is
    /// uploaded again.
    Backup,
}

/// [`plan`] for a pair whose changes travel as `flow` says. A backup plan
/// never holds an operation that changes this PC (checked last, whatever the
/// rules above produced: such an operation becomes a [`HoldReason::OneWay`]
/// hold), nor one that deletes from the cloud.
pub fn plan_flow<T: Trees + ?Sized>(
    t: &T,
    folders: impl IntoIterator<Item = Sid>,
    flow: Flow,
) -> Result<Vec<Op>, T::Error> {
    let Some(root) = t.base(ROOT)? else { return Ok(Vec::new()) };
    let local_ok = matches!(model::local_of(t, &root)?, Seen::Present(_));
    let remote_ok = matches!(model::remote_of(t, &root)?, Seen::Present(_));
    if !local_ok || !remote_ok {
        return Ok(vec![Op::Hold {
            sid: Some(ROOT),
            local: root.local,
            remote: root.remote,
            reason: HoldReason::MissingWithoutEvidence,
        }]);
    }
    let mut ops = Vec::new();
    for sid in folders {
        ops.extend(merge::plan_folder(t, sid, flow)?);
    }
    let ops = resolve::resolve(t, ops, flow == Flow::Backup)?;
    if flow == Flow::TwoWay {
        return Ok(ops);
    }
    Ok(ops
        .into_iter()
        .filter(|op| !matches!(op, Op::DeleteRemote { .. }))
        .map(|op| {
            if !op.changes_local() {
                return op;
            }
            let (sid, local, remote) = op.subjects();
            Op::Hold { sid, local, remote, reason: HoldReason::OneWay }
        })
        .collect())
}

/// Every Synced folder, root first (breadth-first). For full passes; its
/// memory is one level of the Synced tree's folders at a time plus the result.
pub fn all_folders<T: Trees + ?Sized>(t: &T) -> Result<Vec<Sid>, T::Error> {
    let mut out = vec![ROOT];
    let mut i = 0;
    while i < out.len() {
        // Sorted for the same reason as in `merge::plan_folder`.
        let mut kids: Vec<Sid> =
            t.base_children(out[i])?.into_iter().filter(|b| b.kind == NodeKind::Folder).map(|b| b.sid).collect();
        kids.sort_unstable();
        out.extend(kids);
        i += 1;
    }
    Ok(out)
}
