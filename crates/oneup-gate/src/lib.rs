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
//!   side are withheld and reported with a preview (a stop's **list**), until
//!   the user approves that list (identified by a [`Fingerprint`]). Creating
//!   new items and the other side's operations continue, because each
//!   operation stands on its own preconditions. An approved list stays
//!   approved item by item ([`ItemKey`], [`Context::approved`]): later
//!   rounds run whatever of it they plan, and anything not on it still needs
//!   a stop of its own. A list also names the folders its deletions will
//!   empty ([`Context::pending`]), so one approval covers a whole tree.
//! * **Content guard**: if a round would send more files whose new content
//!   doesn't look like their type (the caller judges, with
//!   [`content::looks_wrong`]: a `.docx` that isn't a zip, a `.txt` that
//!   isn't text, as when ransomware encrypts files where they are) than the
//!   policy allows, those uploads are withheld until the user approves that
//!   batch, like a circuit breaker. Counted with the recent window too.
//! * **Removal** of duplicate copies the user chose (`oneup_exec::dupes`):
//!   the operations that remove them (every [`Op::RemoveCopy`]) are always
//!   a stop's list of their own
//!   ([`BreakerKind::Removal`]), whatever the limits: one copy is as much a
//!   stop as a thousand. They're keyed and approved item by item like any
//!   list, but never counted in, nor released by, a mass breaker's list, and
//!   they don't hold the rest of the round back. What they remove here
//!   always goes to the version store.
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
//! folder, not one item), with its own token. So does
//! [`sharing::review_permission`], for removing a sharing link or setting
//! its expiry (it changes who can open an item, never its content).

use oneup_sync::{LocalId, Op, Sid};

pub mod content;
pub mod release;
pub mod sharing;

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
            | Op::RemoveCopy { remote: None, .. }
            | Op::Dehydrate { .. } => Some(Side::Local),
            // A synced copy goes from both sides: OneDrive's first
            // ([`changes`] says this PC too).
            Op::RemoveCopy { remote: Some(_), .. } => Some(Side::Remote),
            Op::Upload { .. }
            | Op::MkdirRemote { .. }
            | Op::MoveRemote { .. }
            | Op::RenameRemote { .. }
            | Op::DeleteRemote { .. }
            | Op::PruneRemote { .. } => Some(Side::Remote),
        }
    }
}

/// Whether `op` changes `side`: the side [`Side::changed_by`] names, and
/// this PC too for a duplicate removed from both sides.
pub fn changes(op: &Op, side: Side) -> bool {
    Side::changed_by(op) == Some(side) || (side == Side::Local && matches!(op, Op::RemoveCopy { local: Some(_), .. }))
}

/// Operations that remove, replace or relocate existing synced content:
/// what the circuit breakers count. Renames that only clear a name
/// (`RenameLocal`, `RenameRemote`) or keep both copies (`ConflictCopy`)
/// protect data and don't count.
pub fn is_mass_sensitive(op: &Op) -> bool {
    matches!(
        op,
        Op::RecycleLocal { .. }
            | Op::RemoveCopy { .. }
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
/// nothing in it is [`is_mass_sensitive`]. A new folder's contents are
/// mostly new items, but not always (a one-way pair joining two new
/// folders of the same name replaces OneDrive's differing files with this
/// PC's); a plan that removes, replaces or moves anything is left whole to
/// the next round, where the circuit breakers, the content guard and the
/// version store's room weigh it with everything else.
pub fn may_follow(ops: &[Op]) -> bool {
    !ops.iter().any(is_mass_sensitive)
}

/// Whether `op` removes an unknown number of items: a pruned folder never
/// listed in full, whose count is only a lower bound. It always trips its
/// side's breaker, and counts in the safety window as over the limit.
pub fn unknown_size(op: &Op) -> bool {
    matches!(op, Op::PruneRemote { exact: false, .. })
}

/// A count more than any limit: what the safety window records for a
/// removal of unknown size ([`unknown_size`]), whatever the limits are
/// then or later (raising them meanwhile doesn't let the next deletions
/// through). The window counts a record it can't read as this too.
pub const FULL: usize = 1_000_000_000;

/// Whether `op` is asked about whatever the counts: one of
/// [`unknown_size`] (it trips its side's breaker), or the removal of a
/// duplicate the user chose ([`is_removal`]: always a list of its own). The
/// one rule both [`review_in`] and [`may_stop`] use, so any operation that
/// must always be asked about is covered by both, and its list's keys
/// always hold the paths the list shows.
pub fn always_stops(op: &Op) -> bool {
    unknown_size(op) || is_removal(op)
}

/// Whether reviewing a round could raise a safety stop, or match an
/// approved list: whether the caller must read the paths the stops' lists
/// show ([`Context::paths`]) before reviewing. Never false where
/// [`review_in`] could stop: an operation that [`always_stops`], counts
/// over the smaller limit ([`weight`], with `pending` and the window), a
/// file the content guard judged, or a list approved before.
pub fn may_stop(
    policy: &Policy,
    ops: &[Op],
    pending: &[Op],
    recent: Recent,
    any_approved: bool,
    any_suspicious: bool,
) -> bool {
    let mass = ops.iter().chain(pending).filter(|o| is_mass_sensitive(o));
    let (mut n, mut always) = (0usize, false);
    for o in mass {
        n = n.saturating_add(weight(o));
        always |= always_stops(o);
    }
    let recent = recent.local.max(recent.remote);
    any_approved
        || any_suspicious
        || always
        || recent >= FULL
        || n.saturating_add(recent) > policy.min_count.min(policy.max_count)
}

/// How many items `op` counts as for the circuit breakers: a pruned folder
/// counts as everything in it (for one of [`unknown_size`], as many as
/// are known).
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

/// Identifies one stop's list, so an approval applies to exactly the list
/// the user saw: a hash of its items' [`ItemKey`]s, in any order. One more
/// item, or another item, yields another fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub u64);

/// One item of a stop's list: which kind of stop, the operation, the item
/// it acts on (its IDs) and where it goes, the path the list shows for it
/// ([`Context::paths`]), and the precondition it runs under, except an
/// upload's content stamp. The path matters for deletions: they name only
/// IDs, and the same ID under another folder (one above it moved
/// meanwhile) isn't what the user approved. A file that keeps changing
/// while the user reads the list (an app's settings file rewritten every few
/// seconds) would otherwise change the list's code between `status` and
/// `approve`; an upload over a file keeps the old version in OneDrive's
/// version history, so its stamp isn't what the user approves. Deletions and
/// local replacements keep theirs: approving them approves removing exactly
/// that content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemKey(pub u128);

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

    /// Both added up (never wrapping: a count at its limit stays there).
    pub fn plus(self, o: Recent) -> Recent {
        Recent {
            local: self.local.saturating_add(o.local),
            remote: self.remote.saturating_add(o.remote),
            suspicious: self.suspicious.saturating_add(o.suspicious),
        }
    }

    /// Adds `op` as carried out: its weight ([`weight`]) on the side it
    /// changes if it's mass-sensitive, and one suspicious upload if it's in
    /// `suspicious`. Every count the safety window keeps goes through this,
    /// so a pruned folder counts as everything in it, and one of unknown
    /// size ([`unknown_size`]) as [`FULL`].
    pub fn add_op(&mut self, op: &Op, suspicious: &[LocalId]) {
        if is_mass_sensitive(op) {
            let n = if unknown_size(op) { weight(op).max(FULL) } else { weight(op) };
            // A duplicate removed from both sides counts on each.
            if changes(op, Side::Local) {
                self.local = self.local.saturating_add(n);
            }
            if changes(op, Side::Remote) {
                self.remote = self.remote.saturating_add(n);
            }
        }
        if is_suspicious(op, suspicious) {
            self.suspicious = self.suspicious.saturating_add(1);
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
    /// Duplicate copies the user chose to remove (see the module docs):
    /// always a stop, whatever the limits.
    Removal,
}

/// A tripped circuit breaker, for the user to review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breaker {
    pub kind: BreakerKind,
    pub side: Side,
    /// Items on its list: the round's operations on that side that no
    /// approved list covers, and the folder deletions to follow ([`later`]).
    pub count: usize,
    /// Of the limit, how much was used already: what earlier rounds in the
    /// window carried out (see [`Recent`]; 0 for a one-shot run), and this
    /// round's operations on lists approved before.
    pub recent: usize,
    pub synced_items: u64,
    pub fingerprint: Fingerprint,
    /// `count` is only a lower bound: the list removes a folder never
    /// listed in full ([`unknown_size`]).
    pub at_least: bool,
    /// Every item on the list, sorted: what approving it approves.
    pub items: Vec<ItemKey>,
    /// Folders on the list that are deleted in later rounds, once the
    /// deletions on it have emptied them ([`Context::pending`]).
    pub later: Vec<Op>,
    /// The Synced items on the list, sorted. The caller marks their folders
    /// for planning, so every round plans the whole list (a full pass's stop
    /// would otherwise come back only with the next full pass).
    pub sids: Vec<Sid>,
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
    /// Lists over the limits that the user approved by their code
    /// ([`Context::approvals`]): they run, and the caller keeps their items
    /// approved for the rounds that follow (the folders they empty).
    pub answered: Vec<Breaker>,
    /// For each of [`breakers`](Self::breakers), the operations on its list
    /// this round, as places in [`withheld`](Self::withheld): what its list
    /// shows, exactly the operations its items key (with
    /// [`Breaker::later`]).
    pub lists: Vec<Vec<usize>>,
    /// The round's operations on lists the user approved (before, or by
    /// code now), withheld or not, with the keys they matched: the caller
    /// keeps those keys approved while such an operation waits (a file that
    /// keeps changing, a pause), and lets them go once it ran.
    pub listed: Vec<(Op, Vec<ItemKey>)>,
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
    /// The user's approvals of tripped breakers, by code: a list over the
    /// limits with that fingerprint runs whole ([`Review::answered`]).
    pub approvals: &'a [Approval],
    /// Items on lists the user approved earlier ([`Breaker::items`]),
    /// **sorted**. The caller keeps them from the round that showed or
    /// answered the list. They run whenever a round plans them; the rest of
    /// a round's batch is judged on its own, counted on top of them.
    pub approved: &'a [ItemKey],
    /// Folder deletions still to come: folders the round's deletions empty,
    /// each deleted with evidence on its side, which the planner deletes
    /// once empty (`oneup_sync::pending_deletes`). A tripped list names them
    /// and counts them with the round's batch, so one answer covers every
    /// level of an emptied tree.
    pub pending: &'a [Op],
    /// The path a stop's list shows for each operation of the plan, by its
    /// place in the plan (the caller reads them; the gate stays pure).
    /// Part of each item's key ([`ItemKey`]). May be empty when nothing in
    /// the plan can trip a stop and no list is approved; an operation
    /// without one is keyed with an empty path.
    pub paths: &'a [String],
    /// The same for [`pending`](Self::pending).
    pub pending_paths: &'a [String],
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

/// Whether `op` removes a duplicate copy the user chose to remove: an
/// [`Op::RemoveCopy`], only ever made for that.
pub fn is_removal(op: &Op) -> bool {
    matches!(op, Op::RemoveCopy { .. })
}

/// The code of a removal list made of `items` (each operation with the path
/// its list shows), as its stop shows it: a preview gives the same code for
/// the same operations at the same paths.
pub fn removal_fingerprint(items: &[(&Op, &str)]) -> Fingerprint {
    let mut keys: Vec<ItemKey> = items.iter().map(|(o, p)| item_key(BreakerKind::Removal, o, p)).collect();
    keys.sort_unstable();
    keys.dedup();
    fingerprint(b"removal", &keys)
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
    // Each tripped list's operations, by their place in `ops`.
    let mut lists: Vec<Vec<usize>> = Vec::new();
    // Each operation's keys on lists approved before or answered now.
    let mut on_lists: Vec<Vec<ItemKey>> = vec![Vec::new(); ops.len()];
    let path = |i: usize| cx.paths.get(i).map_or("", String::as_str);
    let items: Vec<Item<'_>> = ops.iter().enumerate().map(|(i, o)| (i, o, path(i))).collect();
    let pending: Vec<Item<'_>> = cx
        .pending
        .iter()
        .enumerate()
        .map(|(i, o)| (i, o, cx.pending_paths.get(i).map_or("", String::as_str)))
        .collect();
    let listed = |kind, (_, op, path): Item<'_>| is_listed(kind, op, path, cx.approved);
    let removal = is_removal;
    for (i, op, p) in items.iter().copied() {
        for kind in [BreakerKind::Mass, BreakerKind::Suspicious, BreakerKind::Removal] {
            let relevant = match kind {
                BreakerKind::Mass => is_mass_sensitive(op) && !removal(op),
                BreakerKind::Suspicious => is_suspicious(op, cx.suspicious),
                BreakerKind::Removal => removal(op),
            };
            if relevant && let Some(k) = listed_key(kind, op, p, cx.approved) {
                on_lists[i].push(k);
            }
        }
    }
    for side in [Side::Local, Side::Remote] {
        // A removal the user chose has a list of its own: a mass list's
        // approval never covers it.
        let on_side = |(_, o, _): &Item<'_>| Side::changed_by(o) == Some(side) && is_mass_sensitive(o) && !removal(o);
        let batch: Vec<Item<'_>> = items.iter().copied().filter(on_side).collect();
        if batch.is_empty() {
            continue;
        }
        let later: Vec<Item<'_>> =
            pending.iter().copied().filter(on_side).filter(|i| !listed(BreakerKind::Mass, *i)).collect();
        // The whole tree is judged when its contents go, its folders
        // included, rather than a level at a time.
        let weighed = weigh(&batch).saturating_add(later.len());
        let n = weighed.saturating_add(cx.recent.on(side));
        // A folder of unknown size may hold any number: always asked, and
        // so is anything after one in the window (or after a window that
        // couldn't be read), whatever the limits are set to.
        let too_many = batch.iter().any(|(_, o, _)| always_stops(o))
            || cx.recent.on(side) >= FULL
            || n > policy.max_count
            || (n > policy.min_count
                && (n as u128) * 100 > u128::from(policy.max_percent) * u128::from(cx.synced_items));
        let (on_list, rest): (Vec<Item<'_>>, Vec<Item<'_>>) =
            batch.into_iter().partition(|i| listed(BreakerKind::Mass, *i));
        // Everything over the limit is on lists the user approved.
        if !too_many || rest.is_empty() {
            continue;
        }
        let b = breaker(BreakerKind::Mass, side, cx, &rest, later, weigh(&on_list));
        if cx.approvals.contains(&Approval { side, fingerprint: b.fingerprint }) {
            for (i, o, p) in &rest {
                on_lists[*i].push(item_key(BreakerKind::Mass, o, p));
            }
            out.answered.push(b);
        } else {
            lists.push(rest.iter().map(|(i, _, _)| *i).collect());
            breakers.push(b);
        }
    }

    // The content guard: uploads of files that don't look like their type.
    if policy.max_suspicious > 0 && !cx.suspicious.is_empty() {
        let batch: Vec<Item<'_>> = items.iter().copied().filter(|(_, o, _)| is_suspicious(o, cx.suspicious)).collect();
        if !batch.is_empty() && batch.len().saturating_add(cx.recent.suspicious) > policy.max_suspicious {
            let (on_list, rest): (Vec<Item<'_>>, Vec<Item<'_>>) =
                batch.into_iter().partition(|i| listed(BreakerKind::Suspicious, *i));
            if !rest.is_empty() {
                let b = breaker(BreakerKind::Suspicious, Side::Remote, cx, &rest, Vec::new(), on_list.len());
                if cx.approvals.contains(&Approval { side: Side::Remote, fingerprint: b.fingerprint }) {
                    for (i, o, p) in &rest {
                        on_lists[*i].push(item_key(BreakerKind::Suspicious, o, p));
                    }
                    out.answered.push(b);
                } else {
                    lists.push(rest.iter().map(|(i, _, _)| *i).collect());
                    breakers.push(b);
                }
            }
        }
    }
    // Removing chosen duplicates: always a list of its own, whatever the
    // limits, until approved item by item like any other. One list for both
    // sides, shown on OneDrive's if it removes anything there.
    let batch: Vec<Item<'_>> = items.iter().copied().filter(|(_, o, _)| removal(o)).collect();
    let (on_list, rest): (Vec<Item<'_>>, Vec<Item<'_>>) =
        batch.into_iter().partition(|i| listed(BreakerKind::Removal, *i));
    if !rest.is_empty() {
        let remote = rest.iter().any(|(_, o, _)| Side::changed_by(o) == Some(Side::Remote));
        let side = if remote { Side::Remote } else { Side::Local };
        let b = breaker(BreakerKind::Removal, side, cx, &rest, Vec::new(), weigh(&on_list));
        if cx.approvals.contains(&Approval { side, fingerprint: b.fingerprint }) {
            for (i, o, p) in &rest {
                on_lists[*i].push(item_key(BreakerKind::Removal, o, p));
            }
            out.answered.push(b);
        } else {
            lists.push(rest.iter().map(|(i, _, _)| *i).collect());
            breakers.push(b);
        }
    }

    // Which operations each tripped list holds: they're all withheld (a
    // tripped list's operations never run), placed below.
    let mut in_list: Vec<Vec<usize>> = vec![Vec::new(); ops.len()];
    for (l, list) in lists.iter().enumerate() {
        for i in list {
            in_list[*i].push(l);
        }
    }
    let mut held_lists: Vec<Vec<usize>> = vec![Vec::new(); lists.len()];
    drop(items);

    // The overflow batch, only when local changes can run at all this round
    // (otherwise there is nothing to decide yet). A removal waiting for its
    // approval holds nothing else back.
    let local_blocked = policy.panic
        || policy.pause_local
        || breakers.iter().any(|b| b.side == Side::Local && b.kind != BreakerKind::Removal);
    let mut held_overflow = false;
    let mut discard = false;
    if !local_blocked && let Some(room) = cx.room {
        // What a removal takes from this PC is kept whatever the room: the
        // user approved exactly that.
        let batch: Vec<&Op> = ops.iter().filter(|o| store_size(policy, o).is_some() && !removal(o)).collect();
        let bytes: u64 = batch.iter().filter_map(|o| store_size(policy, o)).sum();
        if bytes > room {
            let fingerprint = overflow_fingerprint(&batch);
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

    for (i, (op, keys)) in ops.into_iter().zip(on_lists).enumerate() {
        let side = Side::changed_by(&op);
        let lists_of = std::mem::take(&mut in_list[i]);
        let chosen = removal(&op);
        let in_batch = store_size(policy, &op).is_some() && !chosen;
        let why = if matches!(op, Op::Hold { .. }) {
            Some(Withheld::Held)
        } else if malformed(&op) {
            Some(Withheld::Malformed)
        } else if side.is_some() && policy.panic {
            Some(Withheld::Panic)
        } else if changes(&op, Side::Local) && policy.pause_local {
            Some(Withheld::Paused(Side::Local))
        } else if changes(&op, Side::Remote) && policy.pause_remote {
            Some(Withheld::Paused(Side::Remote))
        } else if let Some(&l) = lists_of.first() {
            Some(Withheld::Breaker(breakers[l].side))
        } else if held_overflow && in_batch {
            Some(Withheld::Overflow)
        } else {
            None
        };
        if !keys.is_empty() {
            out.listed.push((op.clone(), keys));
        }
        match why {
            Some(w) => {
                for l in lists_of {
                    held_lists[l].push(out.withheld.len());
                }
                out.withheld.push((op, w));
            }
            None => {
                let old = match &op {
                    _ if chosen => OldCopy::Keep,
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
    out.lists = held_lists;
    out
}

/// The bytes `op` would move into the version store if its old copy is
/// kept, or `None` if it moves none: files recycled locally (deleted in
/// OneDrive) and, with [`Policy::keep_replaced`], replaced files. Folders
/// are only recycled once empty, so they take no room and aren't counted.
pub fn store_size(policy: &Policy, op: &Op) -> Option<u64> {
    match op {
        Op::RecycleLocal { local, .. } | Op::RemoveCopy { local: Some(local), .. } => {
            local.stamp.map(|(size, _)| size.max(0) as u64)
        }
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
        Op::Upload { local, .. } | Op::Hash(local) | Op::RemoveCopy { local: Some(local), .. } => local.stamp.is_none(),
        // Nothing to remove.
        Op::RemoveCopy { local: None, remote: None, .. } => true,
        Op::Download { replace: Some(local), .. } => local.stamp.is_none(),
        // Folders are created or empty when they move; anything goes.
        _ => false,
    }
}

/// An operation, by its place in the plan, and the path its stop's list
/// shows for it.
type Item<'a> = (usize, &'a Op, &'a str);

/// The weight of `items` for the breakers ([`weight`]), never wrapping.
fn weigh(items: &[Item<'_>]) -> usize {
    items.iter().fold(0usize, |n, (_, o, _)| n.saturating_add(weight(o)))
}

/// A tripped (or answered) list: the round's operations `rest` that no
/// approved list covers, and the folder deletions `later` to follow.
/// `approved`: how many of the round's operations on that side were on
/// approved lists.
fn breaker(
    kind: BreakerKind,
    side: Side,
    cx: &Context<'_>,
    rest: &[Item<'_>],
    later: Vec<Item<'_>>,
    approved: usize,
) -> Breaker {
    let mut items: Vec<ItemKey> = rest.iter().map(|(_, o, p)| item_key(kind, o, p)).collect();
    // A folder's eTag changes with anything inside it, so a folder still to
    // be emptied is listed without it (see `is_listed`).
    items.extend(later.iter().map(|(_, o, p)| folder_key(kind, o, p)));
    items.sort_unstable();
    items.dedup();
    let mut sids: Vec<Sid> = rest.iter().chain(&later).filter_map(|(_, o, _)| listed_sid(o)).collect();
    sids.sort_unstable();
    sids.dedup();
    let tag: &[u8] = match kind {
        BreakerKind::Mass => b"breaker",
        BreakerKind::Suspicious => b"suspicious",
        BreakerKind::Removal => b"removal",
    };
    Breaker {
        kind,
        side,
        count: weigh(rest).saturating_add(later.len()),
        recent: match kind {
            BreakerKind::Mass => cx.recent.on(side),
            BreakerKind::Suspicious => cx.recent.suspicious,
            // No limit to have used up: only what approved lists ran.
            BreakerKind::Removal => 0,
        }
        .saturating_add(approved),
        synced_items: cx.synced_items,
        fingerprint: fingerprint(tag, &items),
        at_least: rest.iter().any(|(_, o, _)| unknown_size(o)),
        items,
        later: later.into_iter().map(|(_, o, _)| o.clone()).collect(),
        sids,
    }
}

/// The Synced item a listed operation concerns, whose folder plans it.
fn listed_sid(op: &Op) -> Option<Sid> {
    match op {
        // Its Synced rows, deepest first: the item itself is the last.
        Op::PruneRemote { forget, .. } => forget.last().copied(),
        _ => op.subjects().0,
    }
}

/// `op`'s key on a list of `kind`, shown there at `path` ([`ItemKey`]).
pub fn item_key(kind: BreakerKind, op: &Op, path: &str) -> ItemKey {
    match op {
        Op::Upload { local, to, parent, name, replace, link } => {
            let unstamped = Op::Upload {
                local: oneup_sync::LocalPre { id: local.id, stamp: None },
                to: to.clone(),
                parent: *parent,
                name: name.clone(),
                replace: replace.clone(),
                link: *link,
            };
            key_of(kind, &unstamped, path)
        }
        _ => key_of(kind, op, path),
    }
}

/// The key of a folder deletion still to come: without the cloud folder's
/// eTag, which changes as its contents go. A local folder carries no stamp.
fn folder_key(kind: BreakerKind, op: &Op, path: &str) -> ItemKey {
    match op {
        Op::DeleteRemote { sid, remote } => key_of(
            kind,
            &Op::DeleteRemote {
                sid: *sid,
                remote: oneup_sync::RemotePre { id: remote.id.clone(), etag: String::new() },
            },
            path,
        ),
        _ => item_key(kind, op, path),
    }
}

/// Whether `op` is on a list of `kind` the user approved (`approved` sorted).
/// A cloud deletion matches by its key, or as a folder named on the list to
/// be emptied first ([`folder_key`]); folder keys only ever come from such
/// folders, so a file's deletion matches with its eTag only.
fn is_listed(kind: BreakerKind, op: &Op, path: &str, approved: &[ItemKey]) -> bool {
    listed_key(kind, op, path, approved).is_some()
}

/// The key by which `op` is on an approved list of `kind`, if it is.
fn listed_key(kind: BreakerKind, op: &Op, path: &str, approved: &[ItemKey]) -> Option<ItemKey> {
    if approved.is_empty() {
        return None;
    }
    let has = |k: &ItemKey| approved.binary_search(k).is_ok();
    let key = item_key(kind, op, path);
    if has(&key) {
        return Some(key);
    }
    let folder = folder_key(kind, op, path);
    (matches!(op, Op::DeleteRemote { .. }) && has(&folder)).then_some(folder)
}

/// SHA-256 over the kind, the path and the operation as debug-printed, cut to 128
/// bits: an approval runs whatever matches a key, so keys must not collide,
/// even for names chosen to. Stable across runs of the same build, which is
/// all an approval needs: it's given for a list just shown.
fn key_of(kind: BreakerKind, op: &Op, path: &str) -> ItemKey {
    use sha2::Digest as _;
    let tag: &[u8] = match kind {
        BreakerKind::Mass => b"mass",
        BreakerKind::Suspicious => b"suspicious",
        BreakerKind::Removal => b"removal",
    };
    let mut h = sha2::Sha256::new();
    h.update(tag);
    h.update([0]);
    h.update(path.as_bytes());
    h.update([0]);
    h.update(format!("{op:?}").as_bytes());
    let d = h.finalize();
    let mut first = [0u8; 16];
    first.copy_from_slice(&d[..16]);
    ItemKey(u128::from_le_bytes(first))
}

/// FNV-1a over a list's item keys, **sorted**, so the code doesn't depend
/// on the order a round planned them in (a full pass and a round over
/// marked folders plan the same items in different orders). `kind` keeps a
/// breaker's code and an overflow batch's code apart, even for the same
/// operations.
fn fingerprint(kind: &[u8], items: &[ItemKey]) -> Fingerprint {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let keys = items.iter().flat_map(|k| k.0.to_le_bytes());
    for b in kind.iter().copied().chain([0]).chain(keys) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Fingerprint(h)
}

/// The overflow batch's code: over its operations' keys, like a list's.
fn overflow_fingerprint(batch: &[&Op]) -> Fingerprint {
    let mut items: Vec<ItemKey> = batch.iter().map(|o| key_of(BreakerKind::Mass, o, "")).collect();
    items.sort_unstable();
    fingerprint(b"overflow", &items)
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
        let breaker_code = fingerprint(b"breaker", &[item_key(BreakerKind::Mass, &ops[0], "")]);
        assert_ne!(o.fingerprint, breaker_code);
    }

    fn upload_stamped(i: u128, stamp: (i64, i64)) -> Op {
        let mut op = upload_over(i);
        if let Op::Upload { local, .. } = &mut op {
            local.stamp = Some(stamp);
        }
        op
    }

    /// A file rewritten every few seconds (an app's settings file) in a
    /// stop's list no longer changes its code between `status` and
    /// `approve`, nor does the order a round planned the list in. A
    /// deletion's or a local replacement's content still does.
    #[test]
    fn a_file_that_keeps_changing_keeps_the_stops_code() {
        let p = Policy::default();
        let code = |ops: Vec<Op>| review(&p, 100, &[], ops).breakers[0].fingerprint;
        let mut ops: Vec<Op> = (0..30).map(delete_remote).collect();
        ops.push(upload_stamped(100, (1, 1)));
        let before = code(ops.clone());
        ops[30] = upload_stamped(100, (2, 7));
        assert_eq!(code(ops.clone()), before, "rewritten meanwhile");
        ops.reverse();
        assert_eq!(code(ops.clone()), before, "planned in another order");
        // ...but it's still another upload if it goes elsewhere or over another version.
        if let Op::Upload { name, .. } = &mut ops[0] {
            *name = "other.docx".into();
        }
        assert_ne!(code(ops), before);

        let recycles: Vec<Op> = (0..30).map(recycle).collect();
        let before = code(recycles.clone());
        let mut changed = recycles.clone();
        changed[3] = recycle_sized(3, 99);
        assert_ne!(code(changed), before, "a deletion keeps its content stamp");
        let mut replaced = recycles.clone();
        replaced.push(replace_sized(50, 5));
        let before = code(replaced.clone());
        replaced[30] = replace_sized(50, 6);
        assert_ne!(code(replaced), before, "a local replacement keeps its content stamp");
    }

    /// Approving a stop approves its list item by item: a later round runs
    /// whatever of it it plans, in any order and in part; anything else is
    /// judged on its own, counted on top of the approved items, and needs a
    /// stop of its own.
    #[test]
    fn an_approved_list_runs_whole_and_anything_else_needs_its_own_stop() {
        let p = Policy::default();
        let ops: Vec<Op> = (0..30).map(recycle).collect();
        let b = review(&p, 100, &[], ops.clone()).breakers.remove(0);
        assert_eq!((b.count, b.items.len()), (30, 30));
        assert_eq!(b.sids, (0..30).collect::<Vec<Sid>>(), "their folders are marked for planning");
        let approved = b.items.clone();
        let cx = Context { synced_items: 100, approved: &approved, ..Context::default() };

        let mut again = ops.clone();
        again.reverse();
        let r = review_in(&p, &cx, again);
        assert!(r.breakers.is_empty() && r.withheld.is_empty());
        assert_eq!(r.approved.len(), 30);
        let r = review_in(&p, &cx, ops[..10].to_vec());
        assert!(r.breakers.is_empty() && r.withheld.is_empty(), "part of it");

        let mut more = ops.clone();
        more.extend((30..35).map(recycle));
        let r = review_in(&p, &cx, more.clone());
        assert_eq!(r.breakers.len(), 1);
        let b2 = &r.breakers[0];
        assert_eq!((b2.count, b2.recent, b2.items.len()), (5, 30, 5));
        assert_ne!(b2.fingerprint, b.fingerprint);
        assert_eq!(r.withheld.iter().map(|(op, _)| op.clone()).collect::<Vec<_>>(), more[30..].to_vec());
        let mut both = [approved.clone(), b2.items.clone()].concat();
        both.sort_unstable();
        let r = review_in(&p, &Context { approved: &both, ..cx }, more);
        assert!(r.breakers.is_empty() && r.withheld.is_empty());

        // A list's other kind isn't approved by it: approving deletions
        // doesn't let the content guard's uploads through.
        let uploads: Vec<Op> = (1..=25).map(upload_over).collect();
        let bad: Vec<LocalId> = (1..=25).map(LocalId).collect();
        let mass_keys: Vec<ItemKey> = {
            let mut k: Vec<ItemKey> = uploads.iter().map(|o| item_key(BreakerKind::Mass, o, "")).collect();
            k.sort_unstable();
            k
        };
        let cx = Context { synced_items: 100_000, suspicious: &bad, approved: &mass_keys, ..Context::default() };
        assert_eq!(review_in(&p, &cx, uploads).breakers[0].kind, BreakerKind::Suspicious);

        // By its code (`oneup sync --approve`), the list is answered and its
        // items returned, to stay approved in the rounds that follow.
        let r = review(&p, 100, &[Approval { side: Side::Local, fingerprint: b.fingerprint }], ops);
        assert!(r.breakers.is_empty() && r.withheld.is_empty());
        assert_eq!(r.answered.len(), 1);
        assert_eq!(r.answered[0].items, b.items);
    }

    /// The path shown on a stop's list is part of what's approved: a
    /// deletion names only IDs, so after a folder above it moved between
    /// `status` and `approve`, the same ID elsewhere needs a stop of its own.
    #[test]
    fn an_approval_doesnt_follow_an_item_to_another_path() {
        let p = Policy::default();
        let ops: Vec<Op> = (0..30).map(delete_remote).collect();
        let shown: Vec<String> = (0..30).map(|i| format!("/old/f{i}")).collect();
        let cx = Context { synced_items: 100, paths: &shown, ..Context::default() };
        let b = review_in(&p, &cx, ops.clone()).breakers.remove(0);
        let approved = b.items.clone();
        let cx = Context { synced_items: 100, paths: &shown, approved: &approved, ..Context::default() };
        assert!(review_in(&p, &cx, ops.clone()).breakers.is_empty(), "where it was shown");
        let moved: Vec<String> = (0..30).map(|i| format!("/critical/f{i}")).collect();
        let cx = Context { paths: &moved, ..cx };
        let r = review_in(&p, &cx, ops);
        assert_eq!((r.breakers.len(), r.withheld.len()), (1, 30), "the same IDs under another folder");
        assert_ne!(r.breakers[0].fingerprint, b.fingerprint);
        // The same for a folder still to be emptied.
        let pending = vec![folder_delete(100, "e1")];
        let (at, moved) = (vec!["/old/".to_string()], vec!["/critical/".to_string()]);
        let cx =
            Context { synced_items: 100, paths: &shown, pending: &pending, pending_paths: &at, ..Context::default() };
        let items = review_in(&p, &cx, (0..30).map(delete_remote).collect()).breakers.remove(0).items;
        let recent = Recent { remote: 30, ..Recent::default() };
        let later = |paths: &[String]| {
            let cx = Context { synced_items: 100, paths, approved: &items, recent, ..Context::default() };
            review_in(&p, &cx, vec![folder_delete(100, "e2")]).breakers.len()
        };
        assert_eq!(later(&at), 0);
        assert_eq!(later(&moved), 1);
    }

    /// With a mass stop and a content-guard stop standing together, and
    /// parts of each approved earlier, each stop's list (what the user
    /// reads) is exactly what its code approves: no operation another list
    /// holds, or one already approved under this kind.
    #[test]
    fn each_stop_lists_exactly_what_its_code_approves() {
        let p = Policy::default();
        let mut ops: Vec<Op> = (1..=25).map(upload_over).collect();
        ops.extend((100..130).map(delete_remote));
        let bad: Vec<LocalId> = (1..=25).map(LocalId).collect();
        // Uploads 1-12 approved by an earlier content-guard stop, 13-25 by
        // an earlier mass stop.
        let mut approved: Vec<ItemKey> = ops[..12].iter().map(|o| item_key(BreakerKind::Suspicious, o, "")).collect();
        approved.extend(ops[12..25].iter().map(|o| item_key(BreakerKind::Mass, o, "")));
        approved.sort_unstable();
        let cx = Context { synced_items: 300, suspicious: &bad, approved: &approved, ..Context::default() };
        let r = review_in(&p, &cx, ops.clone());
        assert_eq!(r.breakers.len(), 2);
        assert_eq!(r.lists.len(), 2);
        for (b, list) in r.breakers.iter().zip(&r.lists) {
            let mut shown: Vec<ItemKey> = list.iter().map(|&w| item_key(b.kind, &r.withheld[w].0, "")).collect();
            shown.sort_unstable();
            assert_eq!(shown, b.items, "{:?}", b.kind);
            assert_eq!(b.count, list.len());
        }
        let mass = r.breakers.iter().position(|b| b.kind == BreakerKind::Mass).unwrap();
        assert_eq!(r.lists[mass].len(), 30 + 12, "the deletions and the uploads not approved as mass changes");
        let guard = 1 - mass;
        assert_eq!(r.lists[guard].len(), 13, "the uploads not approved by the content guard");
        // Every withheld upload is on at least one list, each once.
        assert_eq!(r.withheld.len(), 55);
    }

    /// Counts never wrap: a window or a pruned folder at the limit of
    /// what fits still trips the stop, never passes it.
    #[test]
    fn counts_never_wrap() {
        let p = Policy::default();
        let recent = Recent { local: usize::MAX, remote: usize::MAX, suspicious: usize::MAX };
        assert_eq!(recent.plus(recent), recent);
        let cx = Context { synced_items: 10, recent, ..Context::default() };
        assert_eq!(review_in(&p, &cx, vec![delete_remote(1)]).breakers.len(), 1);
        let huge = Op::PruneRemote { remote: rpre(9), forget: vec![9], items: u64::MAX, exact: true, keep: Vec::new() };
        let r = review_in(&p, &cx, vec![huge.clone(), delete_remote(1)]);
        assert_eq!(r.breakers[0].count, usize::MAX);
        let mut done = Recent::default();
        done.add_op(&huge, &[]);
        done.add_op(&huge, &[]);
        assert_eq!(done.remote, usize::MAX);
        let mut few = Recent::default();
        let prune = |items, exact| Op::PruneRemote { remote: rpre(9), forget: vec![9], items, exact, keep: Vec::new() };
        few.add_op(&prune(1000, true), &[]);
        assert_eq!(few, Recent { remote: 1000, ..Recent::default() }, "a pruned folder counts as all it holds");
    }

    /// A pruned folder never listed in full (its count only a lower bound)
    /// always trips its side's breaker, however few items are known, is
    /// shown as "at least", and counts in the window as over the limit.
    #[test]
    fn a_folder_of_unknown_size_always_needs_approval() {
        let p = Policy::default();
        let some = Op::PruneRemote { remote: rpre(9), forget: vec![9], items: 10, exact: false, keep: Vec::new() };
        let r = review(&p, 10_000_000, &[], vec![some.clone()]);
        assert_eq!(r.breakers.len(), 1);
        let b = &r.breakers[0];
        assert_eq!((b.side, b.count, b.at_least), (Side::Remote, 10, true));
        assert_eq!(r.withheld.len(), 1);
        // Approved, it runs; and the window counts it as over the limit.
        let cx = Context { synced_items: 10_000_000, approved: &b.items, ..Context::default() };
        assert!(review_in(&p, &cx, vec![some.clone()]).breakers.is_empty());
        let mut done = Recent::default();
        done.add_op(&some, &[]);
        assert_eq!(done.remote, FULL);
        let r = review_in(
            &p,
            &Context { synced_items: 10_000_000, recent: done, ..Context::default() },
            vec![delete_remote(1)],
        );
        assert_eq!(r.breakers.len(), 1, "a deletion right after it is asked about");
        // The same folder, counted in full and small: no stop.
        let exact = Op::PruneRemote { remote: rpre(9), forget: vec![9], items: 10, exact: true, keep: Vec::new() };
        assert!(review(&p, 10_000_000, &[], vec![exact]).breakers.is_empty());
    }

    /// A full window (after a removal of unknown size, or one that couldn't
    /// be read) stops the next deletion even with limits set above
    /// [`FULL`], and [`may_stop`] agrees.
    #[test]
    fn a_full_window_stops_whatever_the_limits() {
        let p = Policy { max_count: FULL + 1, min_count: FULL + 1, max_percent: 100, ..Policy::default() };
        let synced = u64::try_from(FULL).unwrap_or(u64::MAX) * 4;
        let full = Recent { remote: FULL, ..Recent::default() };
        let cx = Context { synced_items: synced, recent: full, ..Context::default() };
        let r = review_in(&p, &cx, vec![delete_remote(1)]);
        assert_eq!(r.breakers.len(), 1, "one deletion after a full window is asked about");
        assert!(may_stop(&p, &[delete_remote(1)], &[], full, false, false));
        // Below it, the raised limits apply as usual.
        let some = Recent { remote: 10, ..Recent::default() };
        let cx = Context { synced_items: synced, recent: some, ..Context::default() };
        assert!(review_in(&p, &cx, vec![delete_remote(1)]).breakers.is_empty());
        assert!(!may_stop(&p, &[delete_remote(1)], &[], some, false, false));
    }

    /// The round's operations on approved lists are reported with the keys
    /// they matched, approved or held, so the caller can keep those keys
    /// while they wait.
    #[test]
    fn operations_on_approved_lists_are_reported_with_their_keys() {
        let p = Policy::default();
        let ops: Vec<Op> = (0..30).map(recycle).collect();
        let b = review(&p, 100, &[], ops.clone()).breakers.remove(0);
        let cx = Context { synced_items: 100, approved: &b.items, ..Context::default() };
        let paused = Policy { pause_local: true, ..p.clone() };
        let r = review_in(&paused, &cx, ops.clone());
        assert_eq!(r.listed.len(), 30, "held by the pause, still on the list");
        let mut keys: Vec<ItemKey> = r.listed.iter().flat_map(|(_, k)| k.clone()).collect();
        keys.sort_unstable();
        assert_eq!(keys, b.items);
        // Answered by its code: the same.
        let r = review(&p, 100, &[Approval { side: Side::Local, fingerprint: b.fingerprint }], ops);
        assert_eq!(r.listed.len(), 30);
        assert!(review(&p, 100, &[], vec![recycle(1)]).listed.is_empty(), "nothing approved, nothing listed");
    }

    fn prune(i: u128) -> Op {
        Op::PruneRemote { remote: rpre(i), forget: Vec::new(), items: 1, exact: true, keep: Vec::new() }
    }

    fn keep() -> oneup_sync::Keep {
        oneup_sync::Keep::Remote(rpre(99))
    }

    /// A duplicate only on this PC, chosen for removal.
    fn remove_local(i: u128, size: i64) -> Op {
        Op::RemoveCopy {
            sid: None,
            remote: None,
            local: Some(LocalPre { id: LocalId(i), stamp: Some((size, 1)) }),
            remote_dir: None,
            remote_up: Default::default(),
            local_dir: None,
            local_up: Default::default(),
            name: format!("f{i}"),
            hash: [1; 20],
            keep: keep(),
        }
    }

    /// A duplicate in OneDrive only.
    fn remove_cloud(i: u128) -> Op {
        Op::RemoveCopy {
            sid: None,
            remote: Some(rpre(i)),
            local: None,
            remote_dir: None,
            remote_up: Default::default(),
            local_dir: None,
            local_up: Default::default(),
            name: format!("f{i}"),
            hash: [1; 20],
            keep: keep(),
        }
    }

    /// A synced duplicate: both sides.
    fn remove_synced(i: u128, size: i64) -> Op {
        Op::RemoveCopy {
            sid: Some(i as Sid),
            remote: Some(rpre(i)),
            local: Some(LocalPre { id: LocalId(i), stamp: Some((size, 1)) }),
            remote_dir: None,
            remote_up: Default::default(),
            local_dir: None,
            local_up: Default::default(),
            name: format!("f{i}"),
            hash: [1; 20],
            keep: keep(),
        }
    }

    /// Removing duplicates the user chose always stops for their approval,
    /// even one copy, far under every limit; only those operations wait.
    /// The list's code is the preview's for the same operations at the
    /// same paths; approved, its items run (kept here) and count in the
    /// window by weight; anything more is a list of its own.
    #[test]
    fn a_chosen_removal_is_always_a_stop_of_its_own() {
        let p = Policy::default();
        let ops = vec![remove_cloud(1), remove_synced(2, 1), upload_new(3), delete_remote(4), remove_local(5, 70)];
        let paths: Vec<String> = ["/a.jpg", "/b.jpg", "", "/d.txt", "/e.jpg"].map(String::from).to_vec();
        let cx = Context { synced_items: 1_000_000, paths: &paths, ..Context::default() };
        let r = review_in(&p, &cx, ops.clone());
        assert_eq!(r.breakers.len(), 1);
        let b = r.breakers[0].clone();
        assert_eq!((b.kind, b.side, b.count, b.items.len()), (BreakerKind::Removal, Side::Remote, 3, 3));
        let held: Vec<&Op> = r.withheld.iter().map(|(o, _)| o).collect();
        assert_eq!(held, [&ops[0], &ops[1], &ops[4]]);
        assert_eq!(r.lists, [vec![0, 1, 2]], "its list is exactly what it holds");
        assert_eq!(r.approved.len(), 2, "the rest of the round goes on");
        let shown = [(&ops[0], "/a.jpg"), (&ops[1], "/b.jpg"), (&ops[4], "/e.jpg")];
        assert_eq!(removal_fingerprint(&shown), b.fingerprint, "the preview's code");
        assert_ne!(removal_fingerprint(&shown[..2]), b.fingerprint);

        // Nothing else is a removal, however many: a prune and a recycle
        // are ordinary.
        assert!(!is_removal(&prune(1)) && !is_removal(&recycle(2)));

        // Approved item by item: it runs, what goes here is kept and isn't
        // part of an overflow batch, and it's reported as listed.
        let approved = b.items.clone();
        let cx2 = Context { approved: &approved, room: Some(0), ..cx };
        let r = review_in(&p, &cx2, ops.clone());
        assert!(r.breakers.is_empty() && r.withheld.is_empty() && r.overflow.is_none());
        assert!(r.approved.iter().all(|a| a.old_copy() == OldCopy::Keep));
        assert_eq!(r.store_bytes, 1 + 70);
        assert_eq!(r.listed.len(), 3);
        let mut w = Recent::default();
        for a in &r.approved {
            w.add_op(a.op(), &[]);
        }
        assert_eq!((w.local, w.remote), (2, 3), "counted in the window by weight, a synced copy on both sides");
        // One more copy is a list of its own.
        let mut more = ops.clone();
        more.push(remove_local(6, 1));
        let r = review_in(&p, &cx2, more);
        assert_eq!((r.breakers.len(), r.breakers[0].count, r.breakers[0].recent), (1, 1, 3));
        // Approved by its code: answered, and its items reported for keeping.
        let ok = [Approval { side: Side::Remote, fingerprint: b.fingerprint }];
        let r = review_in(&p, &Context { approvals: &ok, ..cx }, ops.clone());
        assert!(r.breakers.is_empty());
        assert_eq!((r.answered.len(), r.listed.len()), (1, 3));
        // Paused: withheld as such, whatever was approved; a synced copy
        // waits while either side is paused.
        let paused = Policy { pause_local: true, ..Policy::default() };
        let r = review_in(&paused, &cx2, ops.clone());
        assert!(r.withheld.contains(&(ops[1].clone(), Withheld::Paused(Side::Local))));
        assert!(r.approved.iter().any(|a| a.op() == &ops[0]), "only in OneDrive: goes on");
    }

    /// A round holding only a removal of chosen duplicates, far below every
    /// limit, still reads the paths its stop's list shows: they're part of
    /// its keys, so the list is keyed as it's shown.
    #[test]
    fn a_removal_always_has_its_paths_read() {
        let p = Policy::default();
        let ops = vec![remove_local(1, 1)];
        assert!(always_stops(&ops[0]));
        assert!(may_stop(&p, &ops, &[], Recent::default(), false, false));
        assert!(!may_stop(&p, &[recycle(1)], &[], Recent::default(), false, false), "an ordinary one doesn't");
    }

    /// A removal's operations are never counted in, nor released by, a mass
    /// breaker's list: many chosen removals trip only their own stop, and a
    /// mass list's approval doesn't run them.
    #[test]
    fn chosen_removals_are_never_on_a_mass_list() {
        let ops: Vec<Op> = (0..300).map(|i| remove_local(i, 1)).collect();
        let r = review(&Policy::default(), 100, &[], ops.clone());
        assert_eq!(r.breakers.iter().map(|b| b.kind).collect::<Vec<_>>(), [BreakerKind::Removal]);
        // Keys of the same operations on a mass list don't approve them.
        let mass: Vec<ItemKey> = {
            let mut v: Vec<ItemKey> = ops.iter().map(|o| item_key(BreakerKind::Mass, o, "")).collect();
            v.sort_unstable();
            v
        };
        let r =
            review_in(&Policy::default(), &Context { synced_items: 100, approved: &mass, ..Context::default() }, ops);
        assert_eq!(r.breakers.len(), 1);
        assert!(r.approved.is_empty());
    }

    fn folder_delete(i: u128, etag: &str) -> Op {
        Op::DeleteRemote { sid: i as Sid, remote: RemotePre { id: RemoteId(format!("r{i}")), etag: etag.into() } }
    }

    /// Deleting a folder tree on this PC removes its files first and each
    /// level of folders once emptied, a round each. One stop lists all of
    /// it, so the levels that follow don't each trip a stop of their own on
    /// top of the window; a folder not on the list still does.
    #[test]
    fn one_stop_covers_a_folder_tree_emptied_level_by_level() {
        let p = Policy::default();
        let files: Vec<Op> = (0..18).map(delete_remote).collect();
        let pending = vec![folder_delete(102, "e1"), folder_delete(101, "e1"), folder_delete(100, "e1")];
        // 18 files alone are under the minimum; with their 3 folders, 21 of
        // 200 are over it and over 10%: the tree is judged as a whole.
        assert!(review(&p, 200, &[], files.clone()).breakers.is_empty());
        let cx = Context { synced_items: 200, pending: &pending, ..Context::default() };
        let r = review_in(&p, &cx, files.clone());
        let b = &r.breakers[0];
        assert_eq!((b.count, b.items.len()), (21, 21));
        assert_eq!(b.later, pending);
        assert_eq!(r.withheld.len(), 18, "folders still to come aren't this round's");

        let approved = b.items.clone();
        // The files ran (and more since): the window is over the minimum.
        let recent = Recent { remote: 30, ..Recent::default() };
        let cx = Context { synced_items: 200, approved: &approved, recent, ..Context::default() };
        // Each level, its eTag changed with its contents going.
        for f in [folder_delete(102, "e2"), folder_delete(101, "e3"), folder_delete(100, "e4")] {
            let r = review_in(&p, &cx, vec![f.clone()]);
            assert!(r.breakers.is_empty() && r.withheld.is_empty(), "{f:?}: {:?}", r.breakers);
        }
        let r = review_in(&p, &cx, vec![folder_delete(103, "e1")]);
        assert_eq!((r.breakers.len(), r.withheld.len()), (1, 1), "a folder not on the list");
        let changed = folder_delete(5, "edited since");
        assert_eq!(review_in(&p, &cx, vec![changed]).breakers.len(), 1, "a file is listed with its eTag");
        // Once approved, folders still to come aren't listed again.
        let cx = Context { synced_items: 200, approved: &approved, pending: &pending, ..Context::default() };
        let mut ops = files;
        ops.extend((50..60).map(delete_remote));
        let r = review_in(&p, &cx, ops);
        assert_eq!((r.breakers[0].count, r.breakers[0].recent), (10, 18));
        assert!(r.breakers[0].later.is_empty());
    }
}
