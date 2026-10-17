//! The safety gate (`data-safety.md` invariants 8 and 10, § Code structure).
//!
//! Every operation the reconciler plans passes through [`review`] before
//! anything is executed. The gate is **pure** (no I/O, no clock), so its
//! decisions are deterministic and testable like the planner's.
//!
//! It approves an operation by wrapping it in an [`Approved`] token. The
//! token's constructor is private to this crate, and the effects layer only
//! accepts tokens, so nothing else in the program can run a change to user
//! content without passing review. The token proves *that* the gate
//! approved the operation, not that the operation is still valid: the
//! effects layer still re-checks every precondition immediately before
//! acting (invariant 5).
//!
//! What the gate withholds, per round:
//! * **Panic** (`oneup panic`): every operation that changes either side.
//!   Bookkeeping (recording what already agrees, hashing) continues.
//! * **Paused direction**: every operation that changes that side.
//! * **Circuit breaker**: if a round would delete, overwrite or move more
//!   items on one side than the policy allows, *all* such operations on that
//!   side are withheld and reported with a preview, until the user approves
//!   that exact batch (identified by a [`Fingerprint`]). Creating new items
//!   and the other side's operations continue, because each operation stands
//!   on its own preconditions.
//! * **Content guard**: if a round would send more files whose new content
//!   doesn't look like their type (the caller judges, with
//!   [`content::looks_wrong`]: a `.docx` that isn't a zip, a `.txt` that
//!   isn't text, as when ransomware encrypts files where they are) than the
//!   policy allows, those uploads are withheld until the user approves that
//!   batch, like a circuit breaker. Counted with the recent window too.
//! * **Malformed** operations, as a defence in depth: a file operation
//!   without a size/time stamp could not be re-checked, so it never runs.
//! * The planner's own **holds** pass through, so they can be reported.
//! * **Version-store overflow**:
//!   if the files a round would move into the local version store (files
//!   deleted in OneDrive; with [`Policy::keep_replaced`], also the old copies
//!   of replaced files) add up to more than the store has room for, they
//!   form one batch. By default ([`OverflowPolicy::Hold`]) the batch is
//!   withheld until the user decides once for all of it ([`Decision`]): keep
//!   them anyway, or discard them (see [`OldCopy::DropIfSynced`]). With
//!   [`OverflowPolicy::Keep`] they are kept and the user is told. Either way
//!   it isn't a stop: everything else in the round goes ahead.
//!
//! The token also says what happens to a local file's old content when the
//! operation removes or replaces it ([`OldCopy`]). A file replaced by a newer
//! cloud version is not kept by default: a replacement is only ever planned
//! when the file is unchanged since the last sync (both sides changed means a
//! conflict, which keeps both), so the old content is the version OneDrive
//! already has in its version history (`data-safety.md` invariant 1, exception).
//!
//! One decision stands apart from the rounds: [`release::review_release`],
//! for finishing another program's stalled unlinking (it releases a whole
//! folder, not one item), with its own token.

use oneup_sync::{LocalId, Op, Sid};

pub mod content;
pub mod release;

/// The side an operation changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    /// This PC (downloads, local moves and renames, local recycling).
    Local,
    /// OneDrive (uploads, cloud moves, cloud deletions).
    Remote,
}

impl Side {
    /// Which side `op` changes, if any. Bookkeeping changes neither.
    pub fn changed_by(op: &Op) -> Option<Side> {
        match op {
            Op::Pair { .. } | Op::Record(_) | Op::Forget(_) | Op::Hash(_) | Op::Hold { .. } => None,
            Op::Download { .. }
            | Op::MkdirLocal { .. }
            | Op::MoveLocal { .. }
            | Op::RenameLocal { .. }
            | Op::ConflictCopy { .. }
            | Op::RecycleLocal { .. }
            | Op::Dehydrate { .. } => Some(Side::Local),
            Op::Upload { .. }
            | Op::MkdirRemote { .. }
            | Op::MoveRemote { .. }
            | Op::RenameRemote { .. }
            | Op::DeleteRemote { .. }
            | Op::PruneRemote { .. } => Some(Side::Remote),
        }
    }
}

/// Operations that remove, replace or relocate existing synced content:
/// what the circuit breakers count. Renames that only clear a name
/// (`RenameLocal`, `RenameRemote`) or keep both copies (`ConflictCopy`)
/// protect data and don't count.
pub fn is_mass_sensitive(op: &Op) -> bool {
    matches!(
        op,
        Op::RecycleLocal { .. }
            | Op::DeleteRemote { .. }
            | Op::PruneRemote { .. }
            | Op::Download { replace: Some(_), .. }
            | Op::Upload { replace: Some(_), .. }
            | Op::MoveLocal { .. }
            | Op::MoveRemote { .. }
    )
}

/// Whether a plan for the contents of folders a round has just made may
/// run in that round (`oneup_exec::Executor::run_following`): only if
/// nothing in it is [`is_mass_sensitive`]. A new folder's contents are new
/// items; a plan that removes, replaces or moves anything is left whole to
/// the next round, where the circuit breakers, the content guard and the
/// version store's room weigh it with everything else.
pub fn may_follow(ops: &[Op]) -> bool {
    !ops.iter().any(is_mass_sensitive)
}

/// How many items `op` counts as for the circuit breakers: a pruned folder
/// counts as everything in it.
pub fn weight(op: &Op) -> usize {
    match op {
        Op::PruneRemote { items, .. } => usize::try_from(*items).unwrap_or(usize::MAX).max(1),
        _ => 1,
    }
}

/// What the gate does with a version-store overflow batch until the user
/// decides (see the module docs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Withhold the batch: the files stay untouched on this PC.
    #[default]
    Hold,
    /// Move them into the store anyway (it stays over its limit until they
    /// are old enough to trim) and tell the user.
    Keep,
}

/// The user's safety settings. Defaults are conservative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// A side trips when a round has more than this many deletions,
    /// overwrites and moves on it...
    pub max_count: usize,
    /// ...or more than `min_count` of them that are also more than
    /// `max_percent` of all synced items. The minimum keeps small folders
    /// usable (deleting 3 of 10 files is ordinary, not a mass event).
    pub min_count: usize,
    pub max_percent: u32,
    /// Changes to this PC are paused (no downloads, local moves, recycling).
    pub pause_local: bool,
    /// Changes to OneDrive are paused (no uploads, cloud moves, deletions).
    pub pause_remote: bool,
    /// Every change to either side is stopped (`oneup panic`).
    pub panic: bool,
    /// Keep the old copy of a file replaced by a newer cloud version in the
    /// version store, even though OneDrive's version history has it.
    pub keep_replaced: bool,
    pub overflow: OverflowPolicy,
    /// The content guard trips when more than this many files would be
    /// sent with content that doesn't look like their type; 0 turns it off.
    pub max_suspicious: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_count: 200,
            min_count: 20,
            max_percent: 10,
            max_suspicious: 20,
            pause_local: false,
            pause_remote: false,
            panic: false,
            keep_replaced: false,
            overflow: OverflowPolicy::Hold,
        }
    }
}

/// Identifies one batch of mass-sensitive operations on one side, so an
/// approval applies to exactly the batch the user saw. Any difference (one
/// more file, another precondition) yields another fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub u64);

/// The user approved a tripped breaker's batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Approval {
    pub side: Side,
    pub fingerprint: Fingerprint,
}

/// The user's answer for a version-store overflow batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Move the files into the store anyway.
    Keep,
    /// Remove them without a local copy, where OneDrive provably has their
    /// content ([`OldCopy::DropIfSynced`]).
    Discard,
}

/// The user decided about the overflow batch with this fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub fingerprint: Fingerprint,
    pub choice: Choice,
}

/// What happens to a local file's old content when an approved operation
/// removes or replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OldCopy {
    /// Moved into the local version store.
    Keep,
    /// Not kept, **if** it is provably the content OneDrive has: under the
    /// effects layer's exclusive handle, its QuickXorHash must equal the
    /// last synced version's. Otherwise (no recorded hash, or different
    /// content) it is kept like [`OldCopy::Keep`].
    DropIfSynced,
}

/// A version-store overflow batch (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overflow {
    /// Files in the batch.
    pub count: usize,
    /// Their total size.
    pub bytes: u64,
    /// What the store had room for.
    pub room: u64,
    pub fingerprint: Fingerprint,
    pub outcome: OverflowOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowOutcome {
    /// Withheld until the user decides.
    Held,
    /// Kept, as the policy says ([`OverflowPolicy::Keep`]).
    KeptByPolicy,
    /// Approved as the user decided.
    Decided(Choice),
}

/// Why an operation was not approved this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    Panic,
    Paused(Side),
    Breaker(Side),
    /// Can't be re-checked before running (see the module docs).
    Malformed,
    /// The planner held it (reported, never acted on).
    Held,
    /// Part of a version-store overflow batch awaiting the user's decision.
    Overflow,
}

/// Mass-sensitive operations already carried out recently, per side.
///
/// A one-shot `oneup sync` sees a mass deletion in one round. The background
/// agent wakes on every change, so the same deletion, done slowly (a script,
/// a misbehaving app, ransomware), could arrive as many small rounds that
/// each stay under the limits. The agent therefore passes what it carried
/// out in its recent window, and the breakers judge the round *plus* that.
/// Keeping the window is the caller's job; the gate stays pure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recent {
    pub local: usize,
    pub remote: usize,
    /// Files sent with content that didn't look like their type.
    pub suspicious: usize,
}

impl Recent {
    pub fn on(&self, side: Side) -> usize {
        match side {
            Side::Local => self.local,
            Side::Remote => self.remote,
        }
    }

    pub fn plus(self, o: Recent) -> Recent {
        Recent {
            local: self.local + o.local,
            remote: self.remote + o.remote,
            suspicious: self.suspicious + o.suspicious,
        }
    }
}

/// What tripped a breaker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BreakerKind {
    /// Too many deletions, overwrites or moves.
    #[default]
    Mass,
    /// Too many files whose new content doesn't look like their type
    /// (the content guard).
    Suspicious,
}

/// A tripped circuit breaker, for the user to review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breaker {
    pub kind: BreakerKind,
    pub side: Side,
    /// Mass-sensitive operations on that side this round.
    pub count: usize,
    /// Of the limit, how much earlier rounds in the window had used (see
    /// [`Recent`]); 0 for a one-shot run.
    pub recent: usize,
    pub synced_items: u64,
    pub fingerprint: Fingerprint,
}

/// An operation the gate approved. Only this crate can create one (the
/// private field), and the effects layer accepts nothing else.
#[derive(Debug, PartialEq, Eq)]
pub struct Approved {
    op: Op,
    old: OldCopy,
    _seal: Seal,
}

#[derive(Debug, PartialEq, Eq)]
struct Seal;

impl Approved {
    pub fn op(&self) -> &Op {
        &self.op
    }

    /// What happens to the old content of a file this operation removes or
    /// replaces (meaningless for other operations).
    pub fn old_copy(&self) -> OldCopy {
        self.old
    }
}

/// The gate's decision for one round.
#[derive(Debug, Default)]
pub struct Review {
    /// In the planner's order (it matters: e.g. a folder before its contents).
    pub approved: Vec<Approved>,
    pub withheld: Vec<(Op, Withheld)>,
    pub breakers: Vec<Breaker>,
    pub overflow: Option<Overflow>,
    /// Bytes the approved operations will move into the version store (the
    /// files whose old copy is kept), so the caller can make room first.
    pub store_bytes: u64,
}

/// Everything a review needs besides the policy and the plan.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context<'a> {
    /// The size of the Synced tree (the base for percentages).
    pub synced_items: u64,
    /// See [`Recent`].
    pub recent: Recent,
    /// The user's approvals of tripped breakers.
    pub approvals: &'a [Approval],
    /// The user's decisions on overflow batches.
    pub decisions: &'a [Decision],
    /// Bytes the version store can take this round without going over its
    /// limit; `None`: no limit. The caller works it out (the effects
    /// layer's `Trim::room`), and only needs to when [`store_bytes`] of the
    /// plan is more than 0.
    pub room: Option<u64>,
    /// Local files whose content the caller judged not to look like their
    /// type ([`content::looks_wrong`]), **sorted**. Uploads of these count
    /// for the content guard.
    pub suspicious: &'a [LocalId],
}

/// Whether `op` sends a file the caller judged suspicious.
pub fn is_suspicious(op: &Op, suspicious: &[LocalId]) -> bool {
    matches!(op, Op::Upload { local, .. } if suspicious.binary_search(&local.id).is_ok())
}

/// Reviews one round's plan with no history, decisions or store limit.
/// `synced_items` is the size of the Synced tree (the base for
/// percentages); `approvals` are the user's approvals of tripped batches.
pub fn review(policy: &Policy, synced_items: u64, approvals: &[Approval], ops: Vec<Op>) -> Review {
    review_in(policy, &Context { synced_items, approvals, ..Context::default() }, ops)
}

/// Reviews one round's plan. For a caller that runs many small rounds, the
/// breakers count this round's batch plus `cx.recent` (see [`Recent`]). A
/// round with no mass-sensitive operations on a side never trips that side,
/// however much came before: there's nothing in it left to stop.
pub fn review_in(policy: &Policy, cx: &Context<'_>, ops: Vec<Op>) -> Review {
    let mut out = Review::default();
    let mut breakers = Vec::new();
    for side in [Side::Local, Side::Remote] {
        let batch: Vec<&Op> =
            ops.iter().filter(|o| Side::changed_by(o) == Some(side) && is_mass_sensitive(o)).collect();
        if batch.is_empty() {
            continue;
        }
        let weighed: usize = batch.iter().map(|o| weight(o)).sum();
        let n = weighed + cx.recent.on(side);
        let too_many = n > policy.max_count
            || (n > policy.min_count
                && (n as u128) * 100 > u128::from(policy.max_percent) * u128::from(cx.synced_items));
        if too_many {
            let fingerprint = fingerprint(b"breaker", &batch);
            if !cx.approvals.contains(&Approval { side, fingerprint }) {
                breakers.push(Breaker {
                    kind: BreakerKind::Mass,
                    side,
                    count: weighed,
                    recent: cx.recent.on(side),
                    synced_items: cx.synced_items,
                    fingerprint,
                });
            }
        }
    }

    // The content guard: uploads of files that don't look like their type.
    if policy.max_suspicious > 0 && !cx.suspicious.is_empty() {
        let batch: Vec<&Op> = ops.iter().filter(|o| is_suspicious(o, cx.suspicious)).collect();
        if !batch.is_empty() && batch.len() + cx.recent.suspicious > policy.max_suspicious {
            let fingerprint = fingerprint(b"suspicious", &batch);
            if !cx.approvals.contains(&Approval { side: Side::Remote, fingerprint }) {
                breakers.push(Breaker {
                    kind: BreakerKind::Suspicious,
                    side: Side::Remote,
                    count: batch.len(),
                    recent: cx.recent.suspicious,
                    synced_items: cx.synced_items,
                    fingerprint,
                });
            }
        }
    }

    // The overflow batch, only when local changes can run at all this round
    // (otherwise there is nothing to decide yet).
    let local_blocked = policy.panic || policy.pause_local || breakers.iter().any(|b| b.side == Side::Local);
    let mut held_overflow = false;
    let mut discard = false;
    if !local_blocked && let Some(room) = cx.room {
        let batch: Vec<&Op> = ops.iter().filter(|o| store_size(policy, o).is_some()).collect();
        let bytes: u64 = batch.iter().filter_map(|o| store_size(policy, o)).sum();
        if bytes > room {
            let fingerprint = fingerprint(b"overflow", &batch);
            let decided = cx.decisions.iter().find(|d| d.fingerprint == fingerprint).map(|d| d.choice);
            let outcome = match (decided, policy.overflow) {
                (Some(c), _) => OverflowOutcome::Decided(c),
                (None, OverflowPolicy::Keep) => OverflowOutcome::KeptByPolicy,
                (None, OverflowPolicy::Hold) => OverflowOutcome::Held,
            };
            held_overflow = outcome == OverflowOutcome::Held;
            discard = outcome == OverflowOutcome::Decided(Choice::Discard);
            out.overflow = Some(Overflow { count: batch.len(), bytes, room, fingerprint, outcome });
        }
    }

    for op in ops {
        let side = Side::changed_by(&op);
        let in_batch = store_size(policy, &op).is_some();
        let why = if matches!(op, Op::Hold { .. }) {
            Some(Withheld::Held)
        } else if malformed(&op) {
            Some(Withheld::Malformed)
        } else if side.is_some() && policy.panic {
            Some(Withheld::Panic)
        } else if side == Some(Side::Local) && policy.pause_local {
            Some(Withheld::Paused(Side::Local))
        } else if side == Some(Side::Remote) && policy.pause_remote {
            Some(Withheld::Paused(Side::Remote))
        } else if is_mass_sensitive(&op) && breakers.iter().any(|b| b.kind == BreakerKind::Mass && Some(b.side) == side)
        {
            side.map(Withheld::Breaker)
        } else if is_suspicious(&op, cx.suspicious) && breakers.iter().any(|b| b.kind == BreakerKind::Suspicious) {
            Some(Withheld::Breaker(Side::Remote))
        } else if held_overflow && in_batch {
            Some(Withheld::Overflow)
        } else {
            None
        };
        match why {
            Some(w) => out.withheld.push((op, w)),
            None => {
                let old = match &op {
                    Op::Download { replace: Some(_), .. } if !policy.keep_replaced => OldCopy::DropIfSynced,
                    _ if discard && in_batch => OldCopy::DropIfSynced,
                    _ => OldCopy::Keep,
                };
                if old == OldCopy::Keep {
                    out.store_bytes += store_size(policy, &op).unwrap_or(0);
                }
                out.approved.push(Approved { op, old, _seal: Seal });
            }
        }
    }
    out.breakers = breakers;
    out
}

/// The bytes `op` would move into the version store if its old copy is
/// kept, or `None` if it moves none: files recycled locally (deleted in
/// OneDrive) and, with [`Policy::keep_replaced`], replaced files. Folders
/// are only recycled once empty, so they take no room and aren't counted.
pub fn store_size(policy: &Policy, op: &Op) -> Option<u64> {
    match op {
        Op::RecycleLocal { local, .. } => local.stamp.map(|(size, _)| size.max(0) as u64),
        Op::Download { replace: Some(local), .. } if policy.keep_replaced => {
            local.stamp.map(|(size, _)| size.max(0) as u64)
        }
        _ => None,
    }
}

/// The most bytes a plan would move into the version store (see
/// [`store_size`]). While it's 0, the caller needn't work out
/// [`Context::room`].
pub fn store_bytes(policy: &Policy, ops: &[Op]) -> u64 {
    ops.iter().filter_map(|o| store_size(policy, o)).sum()
}

/// Operations whose local precondition can't be re-checked as required:
/// every operation that reads, replaces, moves or recycles a local *file*
/// must carry its (size, last-write time) stamp. Folders carry none; their
/// kind is checked on the handle instead.
fn malformed(op: &Op) -> bool {
    match op {
        // Content goes from or to a file: the stamp is required.
        Op::Upload { local, .. } | Op::Hash(local) => local.stamp.is_none(),
        Op::Download { replace: Some(local), .. } => local.stamp.is_none(),
        // Folders are created or empty when they move; anything goes.
        _ => false,
    }
}

/// FNV-1a over the batch's operations, in plan order. Stable across runs of
/// the same build (unlike `std`'s randomly keyed hasher), which is all an
/// approval needs: it's given for a preview just shown, and a new plan from
/// changed state must not inherit it anyway. `kind` keeps a breaker's code
/// and an overflow batch's code apart, even for the same operations.
fn fingerprint(kind: &[u8], batch: &[&Op]) -> Fingerprint {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let ops = batch.iter().map(|op| format!("{op:?}").into_bytes());
    for part in std::iter::once(kind.to_vec()).chain(ops) {
        for b in part.into_iter().chain([0]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Fingerprint(h)
}

/// For reports: a sid the operation concerns, if any.
pub fn sid_of(op: &Op) -> Option<Sid> {
    match op {
        Op::Record(b) => Some(b.sid),
        Op::Forget(s) => Some(*s),
        Op::MoveRemote { sid, .. }
        | Op::MoveLocal { sid, .. }
        | Op::ConflictCopy { sid, .. }
        | Op::DeleteRemote { sid, .. }
        | Op::RecycleLocal { sid, .. } => Some(*sid),
        Op::Hold { sid, .. } => *sid,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneup_sync::{HoldReason, Link, LocalId, LocalPre, RemoteId, RemotePre};

    fn lpre(id: u128) -> LocalPre {
        LocalPre { id: LocalId(id), stamp: Some((1, 1)) }
    }

    fn rpre(id: u128) -> RemotePre {
        RemotePre { id: RemoteId(format!("r{id}")), etag: "e".into() }
    }

    fn recycle(i: u128) -> Op {
        Op::RecycleLocal { sid: i as Sid, local: lpre(i) }
    }

    fn delete_remote(i: u128) -> Op {
        Op::DeleteRemote { sid: i as Sid, remote: rpre(i) }
    }

    fn upload_over(i: u128) -> Op {
        Op::Upload {
            local: lpre(i),
            to: RemoteId("root".into()),
            parent: 0,
            name: format!("f{i}.docx"),
            replace: Some(rpre(i)),
            link: Link::Existing(i as Sid),
        }
    }

    #[test]
    fn a_new_folders_plan_follows_only_without_moves_replacements_or_removals() {
        let new_file = Op::Upload {
            local: LocalPre { id: LocalId(1), stamp: Some((1, 1)) },
            to: RemoteId("p".into()),
            parent: 1,
            name: "a".into(),
            replace: None,
            link: oneup_sync::Link::New,
        };
        assert!(may_follow(std::slice::from_ref(&new_file)));
        assert!(may_follow(&[]));
        let mv = Op::MoveRemote {
            sid: 2,
            remote: RemotePre { id: RemoteId("x".into()), etag: "e".into() },
            to: RemoteId("p".into()),
            parent: 1,
            name: "a".into(),
        };
        assert!(!may_follow(&[new_file, mv]), "one is enough to leave the whole plan");
    }

    #[test]
    fn the_content_guard_holds_uploads_that_dont_look_like_their_type() {
        let ops: Vec<Op> = (1..=30).map(upload_over).collect();
        let bad: Vec<LocalId> = (1..=25).map(LocalId).collect();
        let cx = Context { synced_items: 100_000, suspicious: &bad, ..Context::default() };
        let r = review_in(&Policy::default(), &cx, ops.clone());
        assert_eq!(r.breakers.len(), 1);
        let b = &r.breakers[0];
        assert_eq!((b.kind, b.side, b.count), (BreakerKind::Suspicious, Side::Remote, 25));
        // Only the suspicious ones are held; the 5 others are ordinary edits.
        assert_eq!(r.withheld.len(), 25);
        assert_eq!(r.approved.len(), 5);

        // Approving exactly that batch lets it through.
        let ok = [Approval { side: Side::Remote, fingerprint: b.fingerprint }];
        let r = review_in(&Policy::default(), &Context { approvals: &ok, ..cx }, ops.clone());
        assert!(r.breakers.is_empty());
        assert_eq!(r.approved.len(), 30);

        // A few at a time still trip it, counted with the recent window.
        let few: Vec<LocalId> = (1..=5).map(LocalId).collect();
        let cx = Context { synced_items: 100_000, suspicious: &few, ..Context::default() };
        assert!(review_in(&Policy::default(), &cx, ops.clone()).breakers.is_empty());
        let recent = Recent { suspicious: 18, ..Recent::default() };
        assert_eq!(review_in(&Policy::default(), &Context { recent, ..cx }, ops.clone()).breakers.len(), 1);
        // 0 turns it off.
        let off = Policy { max_suspicious: 0, ..Policy::default() };
        assert!(
            review_in(&off, &Context { synced_items: 100_000, suspicious: &bad, ..Context::default() }, ops)
                .breakers
                .is_empty()
        );
    }

    fn with_recent(synced_items: u64, recent: Recent, ops: Vec<Op>) -> Review {
        review_in(&Policy::default(), &Context { synced_items, recent, ..Context::default() }, ops)
    }

    /// A file of `size` bytes deleted in OneDrive, to be recycled here.
    fn recycle_sized(i: u128, size: i64) -> Op {
        Op::RecycleLocal { sid: i as Sid, local: LocalPre { id: LocalId(i), stamp: Some((size, 1)) } }
    }

    /// A file of `size` bytes replaced by a newer cloud version.
    fn replace_sized(i: u128, size: i64) -> Op {
        Op::Download {
            remote: rpre(i),
            to: LocalId(0),
            parent: 0,
            name: format!("f{i}"),
            replace: Some(LocalPre { id: LocalId(i), stamp: Some((size, 1)) }),
            link: Link::Existing(i as Sid),
        }
    }

    fn with_room(policy: &Policy, room: u64, decisions: &[Decision], ops: Vec<Op>) -> Review {
        let cx = Context { synced_items: 1000, room: Some(room), decisions, ..Context::default() };
        review_in(policy, &cx, ops)
    }

    fn upload_new(i: u128) -> Op {
        Op::Upload {
            local: lpre(i),
            to: RemoteId("root".into()),
            parent: 0,
            name: format!("f{i}"),
            replace: None,
            link: Link::New,
        }
    }

    #[test]
    fn small_rounds_pass_and_keep_their_order() {
        let ops = vec![upload_new(1), recycle(2), Op::Forget(3), delete_remote(4)];
        let r = review(&Policy::default(), 1000, &[], ops.clone());
        assert!(r.withheld.is_empty() && r.breakers.is_empty());
        assert_eq!(r.approved.iter().map(|a| a.op().clone()).collect::<Vec<_>>(), ops);
    }

    #[test]
    fn a_mass_deletion_trips_only_its_side_until_approved() {
        let mut ops: Vec<Op> = (0..30).map(recycle).collect();
        ops.push(upload_new(100));
        ops.push(delete_remote(101));
        // 30 of 100 synced items is over both the minimum and 10%.
        let r = review(&Policy::default(), 100, &[], ops.clone());
        assert_eq!(r.breakers.len(), 1);
        let b = &r.breakers[0];
        assert_eq!((b.side, b.count), (Side::Local, 30));
        assert_eq!(r.withheld.len(), 30);
        assert!(r.withheld.iter().all(|(_, w)| *w == Withheld::Breaker(Side::Local)));
        assert_eq!(r.approved.len(), 2, "new uploads and the cloud side continue");

        let approval = Approval { side: Side::Local, fingerprint: b.fingerprint };
        let r = review(&Policy::default(), 100, &[approval], ops.clone());
        assert!(r.breakers.is_empty() && r.withheld.is_empty());

        // The approval covers exactly that batch, not a bigger one.
        ops.push(recycle(31));
        let r = review(&Policy::default(), 100, &[approval], ops);
        assert_eq!(r.breakers.len(), 1);
    }

    #[test]
    fn thresholds_scale_with_the_tree() {
        let ops: Vec<Op> = (0..30).map(recycle).collect();
        // 30 of 10,000 is only 0.3%: an ordinary clean-up.
        assert!(review(&Policy::default(), 10_000, &[], ops).breakers.is_empty());
        // Below the minimum count, even 100% passes (a small folder).
        assert!(review(&Policy::default(), 5, &[], (0..5).map(recycle).collect()).breakers.is_empty());
        // Above the absolute maximum, any tree size trips.
        let big: Vec<Op> = (0..201).map(delete_remote).collect();
        assert_eq!(review(&Policy::default(), 10_000_000, &[], big).breakers.len(), 1);
    }

    #[test]
    fn a_slow_mass_deletion_trips_on_what_came_before() {
        let five: Vec<Op> = (0..5).map(recycle).collect();
        // Five of 100 alone is ordinary...
        assert!(review(&Policy::default(), 100, &[], five.clone()).breakers.is_empty());
        // ...but not after 20 more in the window: 25 of 100.
        let recent = Recent { local: 20, ..Recent::default() };
        let r = with_recent(100, recent, five.clone());
        assert_eq!(r.breakers.len(), 1);
        assert_eq!((r.breakers[0].count, r.breakers[0].recent), (5, 20));
        // The other side's history doesn't count against this side.
        let other = Recent { remote: 20, ..Recent::default() };
        assert!(with_recent(100, other, five).breakers.is_empty());
        // Nothing to stop, nothing trips, however much came before.
        let r = with_recent(100, recent, vec![upload_new(1)]);
        assert!(r.breakers.is_empty());
    }

    #[test]
    fn panic_and_pause_stop_changes_but_not_bookkeeping() {
        let ops = vec![upload_new(1), recycle(2), Op::Forget(3), Op::Hash(lpre(4))];
        let panic = Policy { panic: true, ..Policy::default() };
        let r = review(&panic, 10, &[], ops.clone());
        assert_eq!(r.approved.len(), 2);
        assert!(r.withheld.iter().all(|(_, w)| *w == Withheld::Panic));

        let paused = Policy { pause_remote: true, ..Policy::default() };
        let r = review(&paused, 10, &[], ops);
        assert_eq!(r.withheld, vec![(upload_new(1), Withheld::Paused(Side::Remote))]);
    }

    #[test]
    fn holds_and_malformed_operations_never_run() {
        let hold = Op::Hold { sid: Some(1), local: None, remote: None, reason: HoldReason::MissingWithoutEvidence };
        let unstamped = Op::Upload {
            local: LocalPre { id: LocalId(1), stamp: None },
            to: RemoteId("root".into()),
            parent: 0,
            name: "f".into(),
            replace: None,
            link: Link::New,
        };
        let r = review(&Policy::default(), 10, &[], vec![hold.clone(), unstamped.clone()]);
        assert!(r.approved.is_empty());
        assert_eq!(r.withheld, vec![(hold, Withheld::Held), (unstamped, Withheld::Malformed)]);
    }

    #[test]
    fn a_batch_over_the_room_is_held_as_one_until_decided() {
        let p = Policy::default();
        let ops = vec![recycle_sized(1, 600), recycle_sized(2, 500), upload_new(3), replace_sized(4, 900)];
        // 1,100 bytes of recycled files into 1,000 of room: the two files
        // wait together; the upload and the replacement go ahead.
        let r = with_room(&p, 1000, &[], ops.clone());
        let o = r.overflow.clone().expect("overflow reported");
        assert_eq!((o.count, o.bytes, o.room, o.outcome), (2, 1100, 1000, OverflowOutcome::Held));
        assert_eq!(r.withheld, vec![(ops[0].clone(), Withheld::Overflow), (ops[1].clone(), Withheld::Overflow)]);
        assert_eq!(r.approved.len(), 2);
        assert!(r.breakers.is_empty(), "an overflow is not a safety stop");
        // Exactly at the limit: nothing to decide.
        assert!(with_room(&p, 1100, &[], ops.clone()).overflow.is_none());
        // No limit: never.
        let cx = Context { synced_items: 1000, ..Context::default() };
        assert!(review_in(&p, &cx, ops.clone()).overflow.is_none());

        // Keep: approved, kept, and counted for making room.
        let keep = [Decision { fingerprint: o.fingerprint, choice: Choice::Keep }];
        let r = with_room(&p, 1000, &keep, ops.clone());
        assert!(r.withheld.is_empty());
        assert_eq!(r.overflow.unwrap().outcome, OverflowOutcome::Decided(Choice::Keep));
        assert_eq!(r.store_bytes, 1100);
        assert!(
            r.approved
                .iter()
                .filter(|a| matches!(a.op(), Op::RecycleLocal { .. }))
                .all(|a| a.old_copy() == OldCopy::Keep)
        );

        // Discard: approved, dropped where provably in OneDrive.
        let discard = [Decision { fingerprint: o.fingerprint, choice: Choice::Discard }];
        let r = with_room(&p, 1000, &discard, ops.clone());
        assert!(r.withheld.is_empty());
        assert_eq!(r.store_bytes, 0);
        assert!(
            r.approved
                .iter()
                .filter(|a| matches!(a.op(), Op::RecycleLocal { .. }))
                .all(|a| a.old_copy() == OldCopy::DropIfSynced)
        );

        // A decision covers exactly that batch.
        let mut more = ops.clone();
        more.push(recycle_sized(5, 1));
        let r = with_room(&p, 1000, &discard, more);
        assert_eq!(r.overflow.unwrap().outcome, OverflowOutcome::Held);
    }

    #[test]
    fn the_keep_policy_keeps_and_reports() {
        let p = Policy { overflow: OverflowPolicy::Keep, ..Policy::default() };
        let r = with_room(&p, 10, &[], vec![recycle_sized(1, 600)]);
        assert_eq!(r.overflow.unwrap().outcome, OverflowOutcome::KeptByPolicy);
        assert_eq!((r.approved.len(), r.store_bytes), (1, 600));
    }

    #[test]
    fn replaced_files_are_not_kept_unless_asked() {
        // Default: a replacement's old copy is dropped where provably in
        // OneDrive, and takes no room, so it never overflows.
        let r = with_room(&Policy::default(), 0, &[], vec![replace_sized(1, 900)]);
        assert!(r.overflow.is_none());
        assert_eq!(r.approved[0].old_copy(), OldCopy::DropIfSynced);
        assert_eq!(r.store_bytes, 0);
        // Asked to keep them: kept, counted, and part of the batch.
        let keep = Policy { keep_replaced: true, ..Policy::default() };
        let r = with_room(&keep, 0, &[], vec![replace_sized(1, 900), recycle_sized(2, 100)]);
        assert_eq!(r.overflow.as_ref().map(|o| (o.count, o.bytes)), Some((2, 1000)));
        assert_eq!(r.withheld.len(), 2);
        let r = with_room(&keep, 5000, &[], vec![replace_sized(1, 900)]);
        assert_eq!((r.approved[0].old_copy(), r.store_bytes), (OldCopy::Keep, 900));
    }

    #[test]
    fn overflow_waits_while_local_changes_cant_run() {
        let ops = vec![recycle_sized(1, 600)];
        let paused = Policy { pause_local: true, ..Policy::default() };
        let r = with_room(&paused, 10, &[], ops.clone());
        assert!(r.overflow.is_none());
        assert_eq!(r.withheld[0].1, Withheld::Paused(Side::Local));
        // A breaker approval code never doubles as an overflow decision.
        let o = with_room(&Policy::default(), 10, &[], ops.clone()).overflow.unwrap();
        let breaker_code = fingerprint(b"breaker", &ops.iter().collect::<Vec<_>>());
        assert_ne!(o.fingerprint, breaker_code);
    }
}
