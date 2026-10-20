//! The sharing audit (`oneup shared`, the settings window's **Shared**
//! page): what the account shares, who can open each item and how, and
//! what others share with it.
//!
//! * **What may be shared** comes from the mirror, which keeps Graph's
//!   `shared` facet as delta delivers it (`oneup_store::shared`): no request.
//! * **Who with** is read on demand, a bounded page at a time
//!   ([`list_page`]: candidates by ID, ten per Graph round trip): each link
//!   or grant, with a code over what's shown. Graph puts the facet on what
//!   inherits a folder's sharing too; only a permission without
//!   `inheritedFrom` says an item is shared on its own. An item with none
//!   (the owner's and inherited ones only) isn't listed and loses its mark,
//!   so it's read once, and again only after it changes.
//! * **Stop sharing** and **Set expiry** act on one permission as it was
//!   listed (its code), through the gate (`oneup_gate::sharing`) and the
//!   effects layer (`oneup_effects::sharing`). The activity log gets the
//!   attempt before the request, settled after; it holds the kind and
//!   access of the permission and a summary of any failure, never the link,
//!   the people or anything from OneDrive's answer. Neither change touches
//!   the item's content.

use std::path::Path;

use anyhow::{Context, Result, bail};
use oneup_effects::sharing::{self as effect, Done, SharingApi};
use oneup_gate::sharing::{Change, Grant, Request, review_permission};
use oneup_graph::{ItemSharing, Permission};
use oneup_store::{ActivityEntry, ActivityOutcome, IntendedChange, Store};
use serde_json::{Value, json};

use crate::{t, t_key};

/// Candidates read from the store at a time (Graph reads ten per round trip).
const CHUNK: usize = 20;
/// A page of the audit: at most this many items listed...
pub const PAGE_ITEMS: usize = 50;
/// ...or this many candidates checked, whichever comes first.
pub const PAGE_CHECKS: usize = 500;
/// At most this many items shared with the account are listed.
pub const WITH_ME_MAX: usize = 2000;

/// One way an item is shared, as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub id: String,
    /// What acting on it must still match ([`Grant::code`]), 16 hex digits.
    pub code: String,
    /// "Anyone with the link", "Specific people", and so on.
    pub kind: String,
    /// "Can view", "Can edit".
    pub access: &'static str,
    pub who: Vec<String>,
    pub expires_ms: Option<i64>,
    pub password: bool,
    /// Anyone who has the link can open the item, without signing in.
    pub anyone: bool,
}

/// A shared item, as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    pub id: String,
    /// Where it is in OneDrive (`/Photos/Trip`).
    pub path: String,
    pub folder: bool,
    /// When it was shared, if OneDrive said.
    pub shared_ms: Option<i64>,
    pub links: Vec<Link>,
    /// Reading its sharing failed (a summary, never OneDrive's answer).
    pub error: Option<String>,
}

/// One page of the audit ([`list_page`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub items: Vec<Shared>,
    /// Where the next page starts; `None`: this was the last.
    pub next: Option<String>,
    /// Candidates read from OneDrive for this page.
    pub checked: usize,
    /// Of those, the ones shared on their own with nobody: marks dropped.
    pub unmarked: usize,
}

/// How a link or grant is described.
fn kind(p: &Permission) -> String {
    match (p.link_type.is_some(), p.link_scope.as_deref()) {
        (true, Some("anonymous")) => t_key!("Anyone with the link"),
        (true, Some("organization")) => t_key!("People in the organization"),
        (true, Some("existingAccess")) => t_key!("People with access"),
        (true, Some("users")) | (false, _) => t_key!("Specific people"),
        (true, _) => t_key!("Link"),
    }
    .into()
}

fn link(item: &str, s: &ItemSharing, p: &Permission) -> Link {
    Link {
        id: p.id.clone(),
        code: format!("{:016x}", effect::grant(item, s, p).code().0),
        kind: kind(p),
        access: if p.link_type.as_deref() == Some("embed") {
            t_key!("Can view (embedded)")
        } else if p.can_edit() {
            t_key!("Can edit")
        } else {
            t_key!("Can view")
        },
        who: p.granted_to.clone(),
        expires_ms: p.expires_ms,
        password: p.has_password,
        anyone: p.link_type.is_some() && p.is_anyone(),
    }
}

/// The item's own permissions: not the owner's, not inherited.
fn own(p: &Permission) -> bool {
    !p.is_owner() && p.inherited_from.is_none()
}

/// One page of what the account shares, with who can open each item (see
/// the module docs), starting after mark `after`: at most `want` items
/// listed and `budget` marks read (each read from the store is cut to what
/// remains of both). Memory and requests are bounded by those, however
/// many items are marked. Marks nothing will list (for an item the mirror
/// doesn't hold) are dropped on the way.
pub async fn list_page<A: SharingApi>(
    store: &mut Store,
    api: &A,
    after: Option<&str>,
    want: usize,
    budget: usize,
) -> Result<Page> {
    let mut out = Page::default();
    let mut cursor = after.map(str::to_string);
    loop {
        let room = CHUNK.min(want.saturating_sub(out.items.len())).min(budget.saturating_sub(out.checked));
        if room == 0 {
            out.next = cursor;
            return Ok(out);
        }
        let page = store.read(|o| o.shared_page(cursor.as_deref(), room))?;
        let Some(last) = page.last.clone() else {
            out.next = None;
            return Ok(out);
        };
        cursor = Some(last);
        out.checked += page.read;
        let mut unmark: Vec<(String, i64)> = page.unheld.clone();
        if !page.rows.is_empty() {
            let ids: Vec<&str> = page.rows.iter().map(|t| t.item.id.as_str()).collect();
            let read = api.sharing(&ids).await.map_err(|e| {
                anyhow::anyhow!(t!("Couldn't read who items are shared with: {why}", why = graph_said(&e)))
            })?;
            for (t, r) in page.rows.iter().zip(read) {
                let id = &t.item.id;
                let listed = |links: Vec<Link>, error: Option<String>| -> Result<Shared> {
                    let path = store.read(|o| o.path_of(id))?.unwrap_or_else(|| format!("/{}", t.item.name));
                    let folder = t.item.kind != oneup_store::Kind::File;
                    Ok(Shared { id: id.clone(), path, folder, shared_ms: t.at_ms, links, error })
                };
                match r {
                    // Gone from OneDrive: the mirror hears of it by itself.
                    Ok(None) => {}
                    Ok(Some(s)) => {
                        let links: Vec<Link> =
                            s.permissions.iter().filter(|p| own(p)).map(|p| link(id, &s, p)).collect();
                        if links.is_empty() {
                            unmark.push((id.clone(), t.rev));
                        } else {
                            out.items.push(listed(links, None)?);
                        }
                    }
                    Err(e) => out.items.push(listed(Vec::new(), Some(graph_said(&e)))?),
                }
            }
        }
        if !unmark.is_empty() {
            // Only the mark read: one the mirror wrote meanwhile (shared
            // again) stays for the next look.
            out.unmarked += store.write(|o| {
                let mut n = 0;
                for (id, rev) in &unmark {
                    n += usize::from(o.unmark_shared_read(id, *rev)?);
                }
                Ok(n)
            })?;
        }
    }
}

pub fn page_json(p: &Page) -> Value {
    json!({
        "items": p.items.iter().map(|s| json!({
            "id": s.id,
            "path": s.path,
            "name": s.path.rsplit('/').next().unwrap_or_default(),
            "folder": s.folder,
            "shared_ms": s.shared_ms,
            "error": s.error,
            "links": s.links.iter().map(|k| json!({
                "id": k.id,
                "code": k.code,
                "kind": k.kind,
                "access": k.access,
                "who": k.who,
                "expires_ms": k.expires_ms,
                "password": k.password,
                "anyone": k.anyone,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "next": p.next,
        "checked": p.checked,
        "unmarked": p.unmarked,
    })
}

/// The item a command names: its path in OneDrive as listed (`/Photos`),
/// or its ID.
pub fn find_item(store: &Store, item: &str) -> Result<(String, String)> {
    if !item.starts_with('/') {
        let path = store.read(|o| o.path_of(item))?.unwrap_or_else(|| item.to_string());
        return Ok((item.to_string(), path));
    }
    let root =
        store.read(|o| o.remote_root())?.with_context(|| t!("OneDrive isn't listed in this data folder yet."))?;
    let mut id = root.id;
    for part in item.split('/').filter(|p| !p.is_empty()) {
        // OneDrive can hold names that differ only in case side by side:
        // the exact one if there is one, never a guess between others.
        let found = store.read(|o| o.remote_named(&id, part))?;
        let exact: Vec<&oneup_store::RemoteRow> = found.iter().filter(|r| r.name == part).collect();
        id = match (exact.as_slice(), found.as_slice()) {
            ([one], _) => one.id.clone(),
            ([], [one]) => one.id.clone(),
            ([], []) => bail!(t!("{item} isn't in OneDrive's listing.", item = item)),
            _ => bail!(t!(
                "{item} could be more than one item (names that differ only in case); name it by its ID, as oneup shared --json shows it.",
                item = item
            )),
        };
    }
    Ok((id, item.to_string()))
}

/// The "end of day" a date names: 23:59:59 on `date` (exactly
/// `YYYY-MM-DD`) in the local time `offset_at` gives for that moment, as
/// Unix milliseconds.
pub fn end_of_day(date: &str, offset_at: impl Fn(time::PrimitiveDateTime) -> time::UtcOffset) -> Result<i64> {
    let bad = || anyhow::anyhow!(t!("Expected a date like 2026-11-30, not {date}.", date = date));
    let parts: Vec<&str> = date.trim().split('-').collect();
    let [y, m, d] = parts.as_slice() else { return Err(bad()) };
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(y, 4) || !digits(m, 2) || !digits(d, 2) {
        return Err(bad());
    }
    let (y, m, d): (i32, u8, u8) = (y.parse()?, m.parse()?, d.parse()?);
    let month = time::Month::try_from(m).map_err(|_| bad())?;
    let day = time::Date::from_calendar_date(y, month, d).map_err(|_| bad())?;
    let local = day.with_hms(23, 59, 59).map_err(|_| bad())?;
    let at = local.assume_offset(offset_at(local));
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).map_err(|_| bad())
}

/// The PC's time zone offset at local time `local`: the one in force on
/// that date, not today's (summer time).
pub fn local_offset_on(local: time::PrimitiveDateTime) -> time::UtcOffset {
    let utc = time::UtcOffset::UTC;
    let first = time::UtcOffset::local_offset_at(local.assume_utc()).unwrap_or(utc);
    time::UtcOffset::local_offset_at(local.assume_offset(first)).unwrap_or(first)
}

/// The PC's time zone offset now (UTC if it can't be told).
fn local_offset() -> time::UtcOffset {
    time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC)
}

/// A time for messages: `2026-11-30 23:59`.
fn when(at_ms: i64, offset: time::UtcOffset) -> String {
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000)
        .ok()
        .and_then(|t| t.to_offset(offset).format(fmt).ok())
        .unwrap_or_else(|| at_ms.to_string())
}

/// A Graph failure, in the person's language: what
/// [`oneup_graph::Error::summary`] keeps (status and OneDrive's error code,
/// never its answer or a URL).
pub fn graph_said(e: &oneup_graph::Error) -> String {
    use oneup_graph::Error as G;
    match e {
        G::Status { status, .. } => {
            let code = e.code().filter(|c| {
                !c.is_empty() && c.len() <= 64 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            });
            match code {
                Some(c) => t!("OneDrive refused it ({status}, {code}).", status = *status, code = c),
                None => t!("OneDrive refused it ({status}).", status = *status),
            }
        }
        G::Http(_) | G::Stalled(..) => t!("Network error."),
        G::Exhausted(n) => t!("Gave up after {n} attempts.", n = *n),
        G::Auth(_) => t!("Sign-in failed."),
        G::NotSignedIn => t!("Not signed in."),
        G::Json(_) => t!("Unexpected answer from OneDrive."),
        G::Invalid(_) | G::Sys(_) | G::Io(_) | G::Local(_) | G::Push(_) => t!("Error on this PC."),
    }
}

/// Why the gate won't make a change, in the person's language.
fn refusal_said(r: &oneup_gate::sharing::Refusal) -> String {
    use oneup_gate::sharing::Refusal as R;
    match r {
        R::Gone => t!("The item or the permission is gone from OneDrive."),
        R::Changed => t!("It changed in OneDrive since it was listed; look again."),
        R::Inherited => t!("It's inherited from a folder above; change it on that folder."),
        R::Owner => t!("That's the owner's own access."),
        R::Root => t!("OneDrive doesn't change the sharing of the drive's top folder."),
        R::InThePast => t!("The expiry date has passed."),
        R::Experimental => t!(
            "Setting an expiry is experimental: turn on experimental options first (oneup settings --experimental on)."
        ),
        R::NotConfirmed => t!("Not confirmed."),
    }
}

/// What went wrong with a change, in the person's language.
fn effect_said(e: &effect::Error) -> String {
    match e {
        effect::Error::Changed => t!("It changed in OneDrive meanwhile; look again."),
        effect::Error::Gone => t!("The item or the permission is gone from OneDrive."),
        effect::Error::NotApplied => t!("OneDrive didn't make the change."),
        effect::Error::Unconfirmed => t!("OneDrive didn't confirm the change; oneup shared checks it again."),
        effect::Error::Graph(g) => graph_said(g),
    }
}

/// What a permission is, for messages: kind and access, in the person's
/// language (never who, nor the link itself).
fn shown_to_user(g: &Link) -> String {
    t!("{kind} ({access})", kind = crate::l10n::message(&g.kind, &[]), access = crate::l10n::message(g.access, &[]))
}

/// What a permission is, for the activity log, which stays English (never
/// who, nor the link itself).
fn describe(g: &Link) -> String {
    format!("{}, {}", g.kind, g.access.to_lowercase())
}

/// What one `oneup shared stop|expire` asks.
pub struct Ask<'a> {
    /// The item as listed (its path in OneDrive) or its ID.
    pub item: &'a str,
    pub permission: &'a str,
    /// The permission's code as listed: refused if it changed since.
    pub expect: Option<u64>,
    pub change: Change,
    pub yes: bool,
    /// Experimental options are on.
    pub experimental: bool,
    pub now_ms: i64,
}

/// What [`change`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Without `yes`: what it would do, and the code to confirm it with.
    Would {
        what: String,
        code: String,
    },
    Done(String),
}

/// Held from the moment a sharing change is put on record until its outcome
/// is: a second process can't make a change meanwhile, and only a holder
/// may settle an unsettled one ([`reconcile`]), which therefore can't be
/// in flight elsewhere. The lock is a file in the data folder opened with
/// no sharing; Windows releases it when the process ends, however it ends.
pub struct SharingLock {
    _file: std::fs::File,
}

impl SharingLock {
    /// The lock, or `None` while another process holds it.
    pub fn try_acquire(dir: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("sharing.lock");
        let mut open = std::fs::OpenOptions::new();
        open.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut open, 0);
        match open.open(&path) {
            Ok(file) => Ok(Some(Self { _file: file })),
            // ERROR_SHARING_VIOLATION
            Err(e) if e.raw_os_error() == Some(32) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("opening {}", path.display())),
        }
    }
}

/// Stops sharing one permission of an item, or sets its expiry: checked
/// by the gate against what was listed (`a.expect`, required to go ahead),
/// carried out by the effects layer, and recorded before the request (the
/// activity entry and a write-ahead intent, in one transaction) and after
/// (both settled in one transaction). An outcome that can't be read back
/// now stays on record for [`reconcile`]. Refusals and failures are
/// errors, logged too.
pub async fn change<A: SharingApi>(store: &mut Store, api: &A, _held: &SharingLock, a: &Ask<'_>) -> Result<Outcome> {
    let (id, path) = find_item(store, a.item)?;
    let read = match api.sharing(&[id.as_str()]).await {
        Ok(r) => r.into_iter().next().transpose().map(Option::flatten),
        Err(e) => Err(e),
    }
    .map_err(|e| anyhow::anyhow!(t!("Couldn't read who it's shared with: {why}", why = graph_said(&e))))?;
    let found = read.as_ref().and_then(|s| s.permissions.iter().find(|p| p.id == a.permission).map(|p| (s, p)));
    let now: Option<Grant> = found.map(|(s, p)| effect::grant(&id, s, p));
    let shown = found.map(|(s, p)| link(&id, s, p));
    let what = match (a.change, &shown) {
        (Change::Stop, Some(l)) => t!("Stop sharing {path}: {what}", path = &path, what = shown_to_user(l)),
        (Change::Expire(at), Some(l)) => {
            t!(
                "Set expiry of {path} ({what}) to {when}",
                path = &path,
                what = shown_to_user(l),
                when = when(at, local_offset())
            )
        }
        (_, None) => t!("{path}: no permission {id}", path = &path, id = a.permission),
    };
    if !a.yes {
        let code = now.as_ref().map(|g| format!("{:016x}", g.code().0)).unwrap_or_default();
        return Ok(Outcome::Would { what, code });
    }
    // Exactly what was shown, never what happens to be there now.
    let Some(expect) = a.expect else {
        bail!(t!("Give the code shown without --yes: --expect CODE"));
    };
    let shown_as = shown.as_ref().map(describe).unwrap_or_default();
    let (action, detail) = match a.change {
        Change::Stop => ("stop sharing", shown_as.clone()),
        Change::Expire(at) => (
            "set expiry",
            format!("{shown_as}{}until {}", if shown.is_some() { "; " } else { "" }, when(at, local_offset())),
        ),
    };
    let with = |why: &str| if why.is_empty() { detail.clone() } else { format!("{detail}; {why}") };
    let entry = |outcome: ActivityOutcome, why: &str| ActivityEntry {
        seq: 0,
        at_ms: a.now_ms,
        outcome,
        action: action.into(),
        path: path.clone(),
        detail: with(why),
    };
    let req = Request {
        seen: oneup_gate::Fingerprint(expect),
        change: a.change,
        confirmed: true,
        experimental: a.experimental,
        now_ms: a.now_ms,
    };
    let token = match review_permission(&req, now.as_ref()) {
        Ok(t) => t,
        Err(why) => {
            store.write(|o| o.activity_add(&[entry(ActivityOutcome::Skipped, &why.to_string())]))?;
            bail!(refusal_said(&why));
        }
    };
    // On record, with what's asked, before OneDrive is asked.
    let intended = match a.change {
        Change::Stop => IntendedChange::Stop,
        Change::Expire(at) => IntendedChange::Expire(at),
    };
    // Durably (`synchronous = FULL`): the record survives a power cut that
    // follows the request.
    let seq = store.write_durable(|o| {
        o.intent_begin(&entry(ActivityOutcome::Waiting, "asked OneDrive"), &id, a.permission, intended, &detail)
    })?;
    let (outcome, why, reply) = match effect::apply(api, token).await {
        Ok(Done::Stopped) => {
            let what = shown.as_ref().map(shown_to_user).unwrap_or_default();
            (ActivityOutcome::Done, String::new(), Ok(t!("Stopped sharing {path}: {what}.", path = &path, what = what)))
        }
        Ok(Done::Expires(at)) => {
            let what = shown.as_ref().map(shown_to_user).unwrap_or_default();
            let msg =
                t!("{path}: {what} now expires {when}.", path = &path, what = what, when = when(at, local_offset()));
            (ActivityOutcome::Done, String::new(), Ok(msg))
        }
        // Asked, and what OneDrive has now couldn't be read: the intent
        // stays, for `reconcile` to settle.
        Err(e @ effect::Error::Unconfirmed) => {
            bail!(effect_said(&e))
        }
        Err(e @ (effect::Error::Changed | effect::Error::Gone)) => {
            (ActivityOutcome::Skipped, e.summary(), Err(effect_said(&e)))
        }
        Err(e) => (ActivityOutcome::Failed, e.summary(), Err(effect_said(&e))),
    };
    if !store.write(|o| o.intent_settle(seq, outcome, &with(&why)))? {
        let said = match &reply {
            Ok(msg) => msg.clone(),
            Err(why) => why.clone(),
        };
        bail!(t!(
            "{said} Its outcome couldn't be recorded (its activity entry is missing); the change stays on record.",
            said = said
        ));
    }
    match reply {
        Ok(msg) => Ok(Outcome::Done(msg)),
        Err(why) => bail!("{why}"),
    }
}

/// What [`reconcile`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settled {
    /// Changes whose outcome is now recorded.
    pub settled: usize,
    /// Changes judged, whose activity entry is missing: they stay on record.
    pub unrecorded: usize,
}

/// Settles the sharing changes asked of OneDrive whose outcome isn't on
/// record (the process ended, or recording or reading back failed, after
/// the request): each is judged from what OneDrive has now
/// ([`effect::judge`]), never assumed. One that can't be read now stays for
/// next time. Needs the lock, so none of them is still in flight.
pub async fn reconcile<A: SharingApi>(store: &mut Store, api: &A, _held: &SharingLock) -> Result<Settled> {
    let mut out = Settled::default();
    for i in store.read(|o| o.intents())? {
        let change = match i.change {
            IntendedChange::Stop => Change::Stop,
            IntendedChange::Expire(at) => Change::Expire(at),
        };
        let Ok(now) = effect::seen(api, &i.item, &i.permission).await else { continue };
        let (outcome, why) = match effect::judge(change, now) {
            effect::Judged::Made(_) => (ActivityOutcome::Done, "confirmed afterwards"),
            effect::Judged::NotMade => (ActivityOutcome::Skipped, "not made; look again"),
            effect::Judged::Gone => (ActivityOutcome::Skipped, "the item or the permission is gone from OneDrive"),
        };
        if store.write(|o| o.intent_settle(i.seq, outcome, &format!("{}; {why}", i.detail)))? {
            out.settled += 1;
        } else {
            out.unrecorded += 1;
        }
    }
    Ok(out)
}

/// What a `oneup shared` command is about to do ([`begin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Stop sharing or set an expiry: needs the lock.
    Change,
    /// List, scan, or list what's shared with the account.
    Look,
}

/// What [`begin`] did: the lock for a change, and what it settled.
pub struct Begun {
    /// Held for [`Mode::Change`]; `None` for [`Mode::Look`].
    pub lock: Option<SharingLock>,
    pub settled: Settled,
}

/// The first step of every `oneup shared` command and of the window's
/// Shared page, before its own work: changes left unsettled are settled
/// ([`reconcile`]). A change takes the lock (refused while another holds
/// it); a look settles only if the lock is free (a change under way
/// elsewhere settles its own) and lets it go again.
pub async fn begin<A: SharingApi>(dir: &Path, store: &mut Store, api: &A, mode: Mode) -> Result<Begun> {
    let lock = SharingLock::try_acquire(dir)?;
    let lock = match (mode, lock) {
        (Mode::Change, None) => {
            bail!(t!("Another change to sharing is under way in this data folder; try again in a moment."))
        }
        (_, None) => return Ok(Begun { lock: None, settled: Settled::default() }),
        (_, Some(l)) => l,
    };
    let settled = reconcile(store, api, &lock).await?;
    Ok(Begun { lock: (mode == Mode::Change).then_some(lock), settled })
}

/// Marks shared items from one page of a whole-drive listing
/// (`oneup shared --scan`), only those the mirror holds (marks for others
/// would never be listed); returns how many it marked. Only marks: the
/// agent's own updates may have run meanwhile, and a mark too many costs
/// one read in the audit, which then drops it.
pub fn mark_from_page(store: &mut Store, page: &oneup_graph::Page) -> Result<usize> {
    let shared: Vec<(&str, Option<i64>)> = page
        .value
        .iter()
        .filter(|it| !it.is_deleted())
        .filter_map(|it| Some((it.id.as_str(), it.sharing()?)))
        .collect();
    store
        .write(|o| {
            let mut n = 0;
            for (id, at) in &shared {
                n += usize::from(o.mark_shared_if_held(id, *at)?);
            }
            Ok(n)
        })
        .map_err(Into::into)
}

// Lists the whole drive once, with only what tells shared items apart, to
/// mark items shared before the mirror kept the `shared` facet. One page
/// in memory at a time.
pub async fn scan(store: &mut Store, graph: &oneup_graph::Graph) -> Result<usize> {
    let mut url = graph.shared_scan_url();
    let mut marked = 0;
    loop {
        let page = graph
            .page(&url)
            .await
            .map_err(|e| anyhow::anyhow!(t!("Couldn't list OneDrive: {why}", why = graph_said(&e))))?;
        marked += mark_from_page(store, &page)?;
        match page.next_link {
            Some(n) => url = n,
            None => return Ok(marked),
        }
    }
}

/// What others share with the account ([`oneup_graph::Graph::shared_with_me`]).
pub trait WithMe: Sync {
    fn shared_with_me(
        &self,
        limit: usize,
    ) -> impl std::future::Future<Output = oneup_graph::Result<(Vec<oneup_graph::SharedItem>, bool)>> + Send;
}

impl WithMe for oneup_graph::Graph {
    async fn shared_with_me(&self, limit: usize) -> oneup_graph::Result<(Vec<oneup_graph::SharedItem>, bool)> {
        oneup_graph::Graph::shared_with_me(self, limit).await
    }
}

/// What others share with the account (at most `limit`, never more than
/// [`WITH_ME_MAX`]), as JSON for the window and `--json`: `{items, more}`.
pub async fn with_me_json<A: WithMe>(api: &A, limit: usize) -> Result<Value> {
    let (found, more) = api.shared_with_me(limit.min(WITH_ME_MAX)).await.map_err(|e| match e.status() {
        Some(401 | 403) => anyhow::anyhow!(t!(
            "OneDrive didn't allow listing what's shared with you. That needs a broader permission: sign in again and allow reading files others share."
        )),
        _ => anyhow::anyhow!(graph_said(&e)),
    })?;
    let items: Vec<Value> = found
        .into_iter()
        .map(|f| {
            json!({ "name": f.name, "folder": f.folder, "size": f.size, "owner": f.owner,
                "shared_ms": f.shared_ms, "drive_id": f.drive_id, "id": f.id })
        })
        .collect();
    Ok(json!({ "items": items, "more": more }))
}

/// `oneup shared`'s text for one page of the audit, in the person's
/// language (`day` writes a time); nothing for an empty page.
pub fn page_text(p: &Page, day: &dyn Fn(Option<i64>) -> String) -> Vec<String> {
    let mut out = Vec::new();
    for i in &p.items {
        out.push(if i.folder {
            t!("{path}\\  shared {when}", path = &i.path, when = day(i.shared_ms))
        } else {
            t!("{path}  shared {when}", path = &i.path, when = day(i.shared_ms))
        });
        if let Some(e) = &i.error {
            out.push(format!("  {}", t!("Couldn't read its sharing: {why}", why = e)));
        }
        for k in &i.links {
            let kind = crate::l10n::message(&k.kind, &[]);
            let access = crate::l10n::message(k.access, &[]);
            let who = if k.who.is_empty() { kind } else { t!("{kind}: {who}", kind = kind, who = k.who.join(", ")) };
            let expires = match k.expires_ms {
                Some(t) => t!("expires {when}", when = day(Some(t))),
                None => t!("no expiry"),
            };
            let line = if k.password {
                t!(
                    "{id}  {who}, {access}, {expires}, password  (code {code})",
                    id = &k.id,
                    who = who,
                    access = access,
                    expires = expires,
                    code = &k.code
                )
            } else {
                t!(
                    "{id}  {who}, {access}, {expires}  (code {code})",
                    id = &k.id,
                    who = who,
                    access = access,
                    expires = expires,
                    code = &k.code
                )
            };
            out.push(format!("  {line}"));
        }
    }
    out
}

/// The line after a listing: how to stop sharing, or that nothing was found.
pub fn page_footer(any: bool) -> String {
    if any { t!("Stop sharing: oneup shared stop ITEM ID --expect CODE --yes") } else { t!("Nothing found.") }
}

/// `oneup shared`'s text: one page of the audit, as `--json` gives one, and
/// how to go on from where it stopped. One page bounds what one run costs:
/// after a whole shared folder is listed, every item in it is a candidate
/// until checked, and checking them all at once would be a request per ten.
pub async fn listing_text<A: SharingApi>(
    store: &mut Store,
    api: &A,
    after: Option<&str>,
    day: &dyn Fn(Option<i64>) -> String,
) -> Result<Vec<String>> {
    let p = list_page(store, api, after, PAGE_ITEMS, PAGE_CHECKS).await?;
    let mut out = page_text(&p, day);
    if !p.items.is_empty() || p.next.is_none() {
        out.push(page_footer(!p.items.is_empty()));
    }
    if let Some(next) = &p.next {
        out.push(if p.items.is_empty() {
            t!(
                "Nothing shared on its own among the {n, plural, one {# item} other {# items}} checked. More: oneup shared --after {next}",
                n = p.checked,
                next = next
            )
        } else {
            t!("More: oneup shared --after {next}", next = next)
        });
    }
    Ok(out)
}

/// `oneup shared --with-me`'s text, in the person's language.
pub fn with_me_text(found: &Value, day: &dyn Fn(Option<i64>) -> String) -> Vec<String> {
    let items = found["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return vec![t!("Nothing found.")];
    }
    let mut out: Vec<String> = items
        .iter()
        .map(|i| {
            let s = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            let owner = Some(s("owner")).filter(|o| !o.is_empty()).unwrap_or_else(|| "?".into());
            let when = day(i.get("shared_ms").and_then(Value::as_i64));
            if i.get("folder").and_then(Value::as_bool) == Some(true) {
                t!("{name}\\  from {owner}  shared {when}", name = s("name"), owner = owner, when = when)
            } else {
                t!("{name}  from {owner}  shared {when}", name = s("name"), owner = owner, when = when)
            }
        })
        .collect();
    if found["more"] == true {
        out.push(t!("(the first {n})", n = WITH_ME_MAX));
    }
    out
}

/// What [`begin`] settled, said to the person (nothing if nothing).
pub fn settled_text(s: Settled) -> Vec<String> {
    let mut out = Vec::new();
    if s.settled > 0 {
        out.push(t!(
            "Settled {n, plural, one {# earlier change} other {# earlier changes}} to sharing; see `oneup activity`.",
            n = s.settled
        ));
    }
    if s.unrecorded > 0 {
        out.push(t!(
            "{n, plural, one {# earlier change} other {# earlier changes}} to sharing couldn't be recorded (activity entry missing).",
            n = s.unrecorded
        ));
    }
    out
}

/// What a change would do, and how to go ahead with exactly that.
pub fn would_text(what: &str, code: &str) -> String {
    t!("{what}.\nTo go ahead: --expect {code} --yes", what = what, code = code)
}

/// After `--scan`.
pub fn scanned_text(n: usize) -> String {
    t!("Listed OneDrive: {n, plural, one {# shared item} other {# shared items}} marked.", n = n)
}

/// The data folder's Graph client (its own account, the whole drive).
pub fn graph(dir: &Path) -> Result<oneup_graph::Graph> {
    let http = crate::setup::http()?;
    let auth = crate::setup::auth(dir, None, &http)?;
    if !auth.is_signed_in()? {
        bail!(t!("Sign in first."));
    }
    Ok(oneup_graph::Graph::new(http, auth, oneup_graph::Aimd::new(2, 4)))
}

/// A tab of the window's Shared page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    ByYou,
    WithMe,
}

/// What a tab of the window's Shared page shows, as JSON: one
/// [`list_page`] of what the account shares (`after`: the previous page's
/// `next`), or what others share with it. Opening either tab (no `after`)
/// first settles what an earlier change left unsettled ([`begin`]), so the
/// tab the page reopens on never leaves one waiting. `room`: how many more
/// items the page can hold; no answer has more.
pub async fn window_tab<A: SharingApi + WithMe>(
    dir: &Path,
    store: &mut Store,
    api: &A,
    tab: Tab,
    after: Option<&str>,
    room: usize,
) -> Result<Value> {
    if after.is_none() {
        begin(dir, store, api, Mode::Look).await?;
    }
    match tab {
        Tab::ByYou => Ok(page_json(&list_page(store, api, after, PAGE_ITEMS.min(room), PAGE_CHECKS).await?)),
        Tab::WithMe => with_me_json(api, room).await,
    }
}

/// [`window_tab`] for data folder `dir`.
pub async fn window_tab_for(dir: &Path, tab: Tab, after: Option<&str>, room: usize) -> Result<Value> {
    let graph = graph(dir)?;
    let mut store = crate::setup::open_store(dir, 4 * 1024)?;
    window_tab(dir, &mut store, &graph, tab, after, room).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use oneup_effects::sharing::fake::FakeSharing;
    use oneup_store::{Kind, RemoteRow};

    use super::*;

    fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> RemoteRow {
        RemoteRow {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            name: name.into(),
            kind,
            size: 0,
            etag: None,
            qxh: None,
            mtime_ms: None,
        }
    }

    /// A mirror with a shared folder and what's in it (one file inheriting
    /// its sharing, two shared on their own, with the folder's time and
    /// with none), a shared file, and a file shared with nobody any more;
    /// OneDrive's sharing to match.
    fn setup() -> (Store, FakeSharing) {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&row("R", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("P", Some("R"), "Photos", Kind::Folder), false)?;
            o.upsert_item(&row("P1", Some("P"), "a.jpg", Kind::File), false)?;
            o.upsert_item(&row("P2", Some("P"), "b.jpg", Kind::File), false)?;
            o.upsert_item(&row("P3", Some("P"), "c.jpg", Kind::File), false)?;
            o.upsert_item(&row("D", Some("R"), "Doc.docx", Kind::File), false)?;
            o.upsert_item(&row("S", Some("R"), "Stale.txt", Kind::File), false)?;
            o.mark_shared("P", Some(1_000))?;
            o.mark_shared("P1", Some(1_000))?;
            o.mark_shared("P2", Some(1_000))?;
            o.mark_shared("P3", None)?;
            o.mark_shared("D", None)?;
            o.mark_shared("S", None)
        })
        .unwrap();
        let f = FakeSharing::default();
        let owner = FakeSharing::permission("o", None, &["owner"]);
        let mut people = FakeSharing::permission("g", None, &["read"]);
        people.granted_to = vec!["Sam".into()];
        f.add("P", vec![owner.clone(), FakeSharing::permission("l", Some(("view", "anonymous")), &["read"]), people]);
        let mut inherited = FakeSharing::permission("l", Some(("view", "anonymous")), &["read"]);
        inherited.inherited_from = Some("P".into());
        f.add("P1", vec![owner.clone(), inherited.clone()]);
        let own_link = FakeSharing::permission("x", Some(("edit", "anonymous")), &["write"]);
        f.add("P2", vec![owner.clone(), inherited.clone(), own_link.clone()]);
        f.add("P3", vec![owner.clone(), inherited, own_link]);
        f.add("D", vec![owner.clone(), FakeSharing::permission("e", Some(("edit", "users")), &["write"])]);
        f.add("S", vec![owner]);
        (s, f)
    }

    fn block_on<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    fn all(s: &mut Store, f: &FakeSharing) -> Page {
        block_on(list_page(s, f, None, usize::MAX, usize::MAX)).unwrap()
    }

    #[test]
    fn the_audit_lists_what_is_shared_on_its_own_and_how() {
        let (mut s, f) = setup();
        let l = all(&mut s, &f);
        let paths: Vec<&str> = l.items.iter().map(|i| i.path.as_str()).collect();
        // Shared on its own inside a shared folder, whether its time is the
        // folder's or unknown: listed. Only inheriting: not.
        assert_eq!(paths, ["/Doc.docx", "/Photos", "/Photos/b.jpg", "/Photos/c.jpg"]);
        let photos = &l.items[1];
        assert!(photos.folder && photos.shared_ms == Some(1_000));
        let kinds: Vec<(&str, &str)> = photos.links.iter().map(|k| (k.kind.as_str(), k.access)).collect();
        assert_eq!(kinds, [("Anyone with the link", "Can view"), ("Specific people", "Can view")]);
        assert_eq!(photos.links[1].who, ["Sam"]);
        assert_eq!(l.items[2].links.len(), 1, "the inherited link isn't the item's own");
        assert_eq!(l.items[0].links[0].access, "Can edit");
        // Inheriting only, or shared with nobody any more: marks dropped,
        // so they aren't read again.
        assert_eq!((l.checked, l.unmarked, l.next.as_deref()), (6, 2, None));
        assert!(!s.read(|o| o.is_marked_shared("P1")).unwrap() && !s.read(|o| o.is_marked_shared("S")).unwrap());
        assert_eq!(all(&mut s, &f).checked, 4);
        assert!(f.log().is_empty(), "listing changes nothing in OneDrive");
        let j = page_json(&l);
        assert_eq!(j["items"][1]["links"][0]["code"].as_str().unwrap().len(), 16);
    }

    /// A shared folder's contents are all marked; a page reads a bounded
    /// number of them and says where to go on.
    #[test]
    fn pages_are_bounded_and_go_on_where_they_stopped() {
        let mut s = Store::open_in_memory().unwrap();
        let f = FakeSharing::default();
        let mut inherited = FakeSharing::permission("l", Some(("view", "anonymous")), &["read"]);
        inherited.inherited_from = Some("F".into());
        s.write(|o| {
            o.upsert_item(&row("R", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("F", Some("R"), "Big", Kind::Folder), false)?;
            o.mark_shared("F", Some(1))?;
            for i in 0..95 {
                let id = format!("F{i:03}");
                o.upsert_item(&row(&id, Some("F"), &id, Kind::File), false)?;
                o.mark_shared(&id, Some(1))?;
            }
            Ok(())
        })
        .unwrap();
        f.add("F", vec![FakeSharing::permission("l", Some(("view", "anonymous")), &["read"])]);
        for i in 0..95 {
            f.add(&format!("F{i:03}"), vec![inherited.clone()]);
        }
        // Mostly inheriting candidates: the page stops at its budget.
        let p = block_on(list_page(&mut s, &f, None, 10, 40)).unwrap();
        assert_eq!(p.checked, 40);
        assert_eq!(p.items.len(), 1, "only the folder is shared on its own");
        let mut next = p.next.clone();
        let mut checked = p.checked;
        while let Some(after) = next {
            let p = block_on(list_page(&mut s, &f, Some(&after), 10, 40)).unwrap();
            assert!(p.checked <= 40 && p.items.is_empty());
            checked += p.checked;
            next = p.next;
        }
        assert_eq!(checked, 96, "every candidate read once");
        assert_eq!(s.read(|o| o.shared_count()).unwrap(), 1, "only the folder stays marked");
    }

    fn ask<'a>(item: &'a str, perm: &'a str, expect: Option<u64>, change: Change, yes: bool) -> Ask<'a> {
        Ask { item, permission: perm, expect, change, yes, experimental: false, now_ms: 5_000 }
    }

    /// The sharing lock, in a folder of its own, removed afterwards.
    struct Held {
        lock: Option<SharingLock>,
        dir: std::path::PathBuf,
    }

    impl Drop for Held {
        fn drop(&mut self) {
            self.lock.take();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn held(name: &str) -> Held {
        let dir = std::env::temp_dir().join(format!("oneup-sharing-lock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lock = SharingLock::try_acquire(&dir).unwrap().unwrap();
        Held { lock: Some(lock), dir }
    }

    impl std::ops::Deref for Held {
        type Target = SharingLock;
        fn deref(&self) -> &SharingLock {
            self.lock.as_ref().unwrap()
        }
    }

    /// The code shown for a change without --yes.
    fn code(s: &mut Store, f: &FakeSharing, h: &Held, item: &str, perm: &str, c: Change) -> u64 {
        match block_on(change(s, f, h, &ask(item, perm, None, c, false))).unwrap() {
            Outcome::Would { code, .. } => u64::from_str_radix(&code, 16).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stopping_a_link_as_listed_goes_through_and_is_logged() {
        let (mut s, f) = setup();
        let h = held("stop");
        let l = all(&mut s, &f);
        let code = u64::from_str_radix(&l.items[1].links[0].code, 16).unwrap();
        // Without --yes: only what it would do, and the same code.
        let would = block_on(change(&mut s, &f, &h, &ask("/Photos", "l", None, Change::Stop, false))).unwrap();
        assert_eq!(
            would,
            Outcome::Would {
                what: "Stop sharing /Photos: Anyone with the link (Can view)".into(),
                code: l.items[1].links[0].code.clone()
            }
        );
        assert!(f.log().is_empty());
        let done = block_on(change(&mut s, &f, &h, &ask("/Photos", "l", Some(code), Change::Stop, true))).unwrap();
        assert!(matches!(done, Outcome::Done(_)));
        assert_eq!(f.log(), ["delete P l"]);
        let log = s.read(|o| o.activity_recent(None, 5)).unwrap();
        assert_eq!(log.len(), 1, "the attempt's entry, settled in place");
        assert_eq!((log[0].action.as_str(), log[0].path.as_str()), ("stop sharing", "/Photos"));
        assert_eq!(log[0].outcome, ActivityOutcome::Done);
        assert!(!log[0].detail.contains("1drv.ms"), "never the link itself");
        assert!(s.read(|o| o.intents()).unwrap().is_empty());
    }

    /// Going ahead needs the code of what was shown: never whatever is
    /// there by then.
    #[test]
    fn going_ahead_needs_the_code_shown() {
        let (mut s, f) = setup();
        let h = held("expect");
        let r = block_on(change(&mut s, &f, &h, &ask("D", "e", None, Change::Stop, true)));
        assert!(r.unwrap_err().to_string().contains("--expect"));
        assert!(f.log().is_empty() && s.read(|o| o.activity_recent(None, 1)).unwrap().is_empty());
    }

    /// A path names one item: the exact name where names differ only in
    /// case, never a guess between others.
    #[test]
    fn a_path_names_one_item_or_none() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&row("R", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("U", Some("R"), "Doc", Kind::File), false)?;
            o.upsert_item(&row("L", Some("R"), "doc", Kind::File), false)?;
            o.upsert_item(&row("O", Some("R"), "Only", Kind::File), false)
        })
        .unwrap();
        assert_eq!(find_item(&s, "/Doc").unwrap().0, "U");
        assert_eq!(find_item(&s, "/doc").unwrap().0, "L");
        assert!(find_item(&s, "/DOC").unwrap_err().to_string().contains("more than one"));
        assert_eq!(find_item(&s, "/only").unwrap().0, "O", "one match whatever the case");
        assert!(find_item(&s, "/none").is_err());
    }

    /// The attempt is on record, committed, before OneDrive is asked: a
    /// crash during the request leaves it.
    #[test]
    fn the_attempt_is_logged_before_the_request() {
        let dir = std::env::temp_dir().join(format!("oneup-sharing-intent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        let mut s = Store::open(&db, 1024).unwrap();
        s.write(|o| {
            o.upsert_item(&row("R", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("D", Some("R"), "Doc.docx", Kind::File), false)?;
            o.mark_shared("D", None)
        })
        .unwrap();
        let f = FakeSharing::default();
        f.add("D", vec![FakeSharing::permission("e", Some(("edit", "users")), &["write"])]);
        let h = held("intent");
        let c = code(&mut s, &f, &h, "D", "e", Change::Stop);
        let durable_before = s.read(|o| o.durable_commits()).unwrap();
        let during = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), Vec::new(), 0)));
        let (db2, during2) = (db.clone(), std::sync::Arc::clone(&during));
        // Runs as the request reaches OneDrive: another connection reads the record.
        *f.before_write.lock().unwrap() = Some(Box::new(move |_| {
            let other = Store::open(&db2, 1024).unwrap();
            *during2.lock().unwrap() = (
                other.read(|o| o.activity_recent(None, 5)).unwrap(),
                other.read(|o| o.intents()).unwrap(),
                other.read(|o| o.durable_commits()).unwrap(),
            );
        }));
        let f = FakeSharing { refuse: Some((500, String::new())), ..f };
        assert!(block_on(change(&mut s, &f, &h, &ask("D", "e", Some(c), Change::Stop, true))).is_err());
        let (log, intents, durable) = during.lock().unwrap().clone();
        assert_eq!(durable, durable_before + 1, "the intent committed durably before the request");
        assert_eq!(log.len(), 1);
        assert_eq!((log[0].outcome, log[0].action.as_str()), (ActivityOutcome::Waiting, "stop sharing"));
        assert_eq!(intents.len(), 1);
        assert_eq!((intents[0].item.as_str(), intents[0].permission.as_str()), ("D", "e"));
        assert_eq!(intents[0].change, IntendedChange::Stop);
        let log = s.read(|o| o.activity_recent(None, 5)).unwrap();
        assert_eq!(log.len(), 1, "settled in place");
        assert_eq!(log[0].outcome, ActivityOutcome::Failed);
        assert!(log[0].detail.ends_with("OneDrive refused it (500)"), "{}", log[0].detail);
        assert!(s.read(|o| o.intents()).unwrap().is_empty());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Asked, and OneDrive can't be read back: the change stays on record,
    /// and the next run settles it from what OneDrive has then.
    #[test]
    fn an_unconfirmed_change_is_settled_by_the_next_run() {
        let (mut s, f) = setup();
        let h = held("unconfirmed");
        let c = code(&mut s, &f, &h, "D", "e", Change::Stop);
        let f = FakeSharing { fail_reads_after_write: true, ..f };
        let e = block_on(change(&mut s, &f, &h, &ask("D", "e", Some(c), Change::Stop, true))).unwrap_err();
        assert!(e.to_string().contains("checks it again"), "{e}");
        assert_eq!(f.log(), ["delete D e"]);
        let entry = &s.read(|o| o.activity_recent(None, 1)).unwrap()[0];
        assert_eq!(entry.outcome, ActivityOutcome::Waiting);
        assert_eq!(s.read(|o| o.intents()).unwrap().len(), 1);
        // Still unreadable: it stays.
        assert_eq!(block_on(reconcile(&mut s, &f, &h)).unwrap().settled, 0);
        assert_eq!(s.read(|o| o.intents()).unwrap().len(), 1);
        // Readable again (a new run): the permission is gone, the item there.
        let f = FakeSharing { fail_reads_after_write: false, ..f };
        assert_eq!(block_on(reconcile(&mut s, &f, &h)).unwrap().settled, 1);
        assert!(s.read(|o| o.intents()).unwrap().is_empty());
        let entry = &s.read(|o| o.activity_recent(None, 1)).unwrap()[0];
        assert_eq!(entry.outcome, ActivityOutcome::Done);
        assert!(entry.detail.ends_with("confirmed afterwards"), "{}", entry.detail);
    }

    /// A change left on record by a process that ended before asking, or
    /// whose item went: settled as what OneDrive has, never assumed done.
    #[test]
    fn a_change_left_on_record_is_judged_from_onedrive() {
        let (mut s, f) = setup();
        let h = held("reconcile");
        let entry = |at_ms| ActivityEntry {
            seq: 0,
            at_ms,
            outcome: ActivityOutcome::Waiting,
            action: "stop sharing".into(),
            path: "/x".into(),
            detail: "Specific people, can edit; asked OneDrive".into(),
        };
        let detail = "Specific people, can edit";
        let (not_made, gone, expired) = s
            .write_durable(|o| {
                Ok((
                    o.intent_begin(&entry(1), "D", "e", IntendedChange::Stop, detail)?,
                    o.intent_begin(&entry(2), "NOPE", "e", IntendedChange::Stop, detail)?,
                    o.intent_begin(&entry(3), "D", "e", IntendedChange::Expire(9_000), detail)?,
                ))
            })
            .unwrap();
        f.items.lock().unwrap().get_mut("D").unwrap().permissions[1].expires_ms = Some(9_000);
        assert_eq!(block_on(reconcile(&mut s, &f, &h)).unwrap().settled, 3);
        let by_seq = |seq: i64| {
            s.read(|o| o.activity_recent(Some("/x"), 10)).unwrap().into_iter().find(|e| e.seq == seq).unwrap()
        };
        assert_eq!(by_seq(not_made).outcome, ActivityOutcome::Skipped, "the permission is still there");
        assert_eq!(by_seq(gone).outcome, ActivityOutcome::Skipped, "the item is gone: can't be told");
        assert_eq!(by_seq(expired).outcome, ActivityOutcome::Done);
        assert!(f.log().is_empty(), "reconciling only reads");
        // While another process holds the lock, nothing is settled there.
        assert!(SharingLock::try_acquire(&h.dir).unwrap().is_none());
    }

    #[test]
    fn a_failure_is_logged_without_what_onedrive_said() {
        let (mut s, f) = setup();
        let h = held("pii");
        let c = code(&mut s, &f, &h, "D", "e", Change::Stop);
        let body = r#"{"error":{"code":"accessDenied","message":"ana@example.com via https://1drv.ms/x?authkey=!K"}}"#;
        let f = FakeSharing { refuse: Some((403, body.into())), ..f };
        let e = block_on(change(&mut s, &f, &h, &ask("D", "e", Some(c), Change::Stop, true))).unwrap_err().to_string();
        let log = s.read(|o| o.activity_recent(None, 1)).unwrap();
        for text in [e.as_str(), log[0].detail.as_str()] {
            assert!(!text.contains("example.com") && !text.contains("1drv.ms") && !text.contains("authkey"), "{text}");
        }
        assert!(log[0].detail.contains("accessDenied"));
        assert_eq!(log[0].outcome, ActivityOutcome::Failed);
    }

    /// A success whose answer was lost comes back as a refusal; it's
    /// logged as done once OneDrive is read back.
    #[test]
    fn a_lost_answer_is_logged_as_what_happened() {
        let (mut s, f) = setup();
        let h = held("lost");
        let c = code(&mut s, &f, &h, "D", "e", Change::Stop);
        let f = FakeSharing { lose_answer: true, ..f };
        let r = block_on(change(&mut s, &f, &h, &ask("D", "e", Some(c), Change::Stop, true)));
        assert!(matches!(r, Ok(Outcome::Done(_))));
        assert_eq!(s.read(|o| o.activity_recent(None, 1)).unwrap()[0].outcome, ActivityOutcome::Done);
    }

    #[test]
    fn a_link_changed_since_the_listing_is_left_alone() {
        let (mut s, f) = setup();
        let h = held("changed");
        let l = all(&mut s, &f);
        let c = u64::from_str_radix(&l.items[0].links[0].code, 16).unwrap();
        // Someone made the link view-only meanwhile.
        f.items.lock().unwrap().get_mut("D").unwrap().permissions[1].roles = vec!["read".into()];
        let r = block_on(change(&mut s, &f, &h, &ask("D", "e", Some(c), Change::Stop, true)));
        assert!(r.unwrap_err().to_string().contains("look again"));
        assert!(f.log().is_empty(), "nothing sent");
        let log = s.read(|o| o.activity_recent(None, 1)).unwrap();
        assert_eq!(log[0].outcome, ActivityOutcome::Skipped);
        // An inherited one can't be removed here, whatever was listed.
        let c = code(&mut s, &f, &h, "/Photos/a.jpg", "l", Change::Stop);
        let r = block_on(change(&mut s, &f, &h, &ask("/Photos/a.jpg", "l", Some(c), Change::Stop, true)));
        assert!(r.unwrap_err().to_string().contains("inherited"));
        assert!(f.log().is_empty());
    }

    #[test]
    fn setting_an_expiry_needs_experimental_options() {
        let (mut s, f) = setup();
        let h = held("expiry");
        let at = end_of_day("2030-01-31", |_| time::UtcOffset::UTC).unwrap();
        let c = code(&mut s, &f, &h, "D", "e", Change::Expire(at));
        let r = block_on(change(&mut s, &f, &h, &ask("D", "e", Some(c), Change::Expire(at), true)));
        assert!(r.unwrap_err().to_string().contains("experimental"));
        let a = Ask { experimental: true, ..ask("D", "e", Some(c), Change::Expire(at), true) };
        assert!(matches!(block_on(change(&mut s, &f, &h, &a)).unwrap(), Outcome::Done(_)));
        assert_eq!(f.log(), [format!("expire D e {at}")]);
    }

    #[test]
    fn dates_are_the_end_of_that_day_in_that_days_time_zone() {
        let utc = |_| time::UtcOffset::UTC;
        assert_eq!(end_of_day("1970-01-02", utc).unwrap(), (86_400 + 86_399) * 1000);
        // Summer time from 1 April: a date then uses its own offset, not
        // the one in force when it's typed.
        let summer = |local: time::PrimitiveDateTime| {
            let h = if local.month() >= time::Month::April { 2 } else { 1 };
            time::UtcOffset::from_hms(h, 0, 0).unwrap()
        };
        let base = |d: &str| end_of_day(d, utc).unwrap();
        assert_eq!(end_of_day("2030-03-30", summer).unwrap(), base("2030-03-30") - 3_600_000);
        assert_eq!(end_of_day("2030-04-02", summer).unwrap(), base("2030-04-02") - 7_200_000);
        for bad in ["2030-13-01", "2030-02-30", "tomorrow", "2030-01", "2030-01-01-extra", "2030-1-01", "+030-01-01"] {
            assert!(end_of_day(bad, utc).is_err(), "{bad}");
        }
    }

    /// Pages stop at exactly their limits, whatever the store's chunk size.
    #[test]
    fn pages_end_exactly_at_their_limits() {
        let mut s = Store::open_in_memory().unwrap();
        let f = FakeSharing::default();
        s.write(|o| {
            o.upsert_item(&row("R", None, "root", Kind::Root), false)?;
            for i in 0..45 {
                let id = format!("S{i:03}");
                o.upsert_item(&row(&id, Some("R"), &id, Kind::File), false)?;
                o.mark_shared(&id, None)?;
            }
            Ok(())
        })
        .unwrap();
        for i in 0..45 {
            f.add(&format!("S{i:03}"), vec![FakeSharing::permission("l", Some(("view", "anonymous")), &["read"])]);
        }
        for (want, budget, items, checked) in [(1, 500, 1, 1), (3, 500, 3, 3), (50, 7, 7, 7), (25, 21, 21, 21)] {
            let p = block_on(list_page(&mut s, &f, None, want, budget)).unwrap();
            assert_eq!((p.items.len(), p.checked), (items, checked), "want {want}, budget {budget}");
            assert!(p.next.is_some());
        }
        let p = block_on(list_page(&mut s, &f, None, 50, 500)).unwrap();
        assert_eq!((p.items.len(), p.checked, p.next), (45, 45, None));
    }

    /// Marks from the whole-drive scan only for items the mirror holds;
    /// one left without its item is dropped by the audit.
    #[test]
    fn a_scan_marks_shared_items_the_mirror_holds() {
        let mut s = Store::open_in_memory().unwrap();
        s.write(|o| {
            o.upsert_item(&row("R", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("A", Some("R"), "a", Kind::File), false)?;
            o.upsert_item(&row("B", Some("R"), "b", Kind::File), false)
        })
        .unwrap();
        let page: oneup_graph::Page = serde_json::from_str(
            r#"{"value":[{"id":"A","shared":{"scope":"users"}},{"id":"B"},{"id":"C","deleted":{}},
                {"id":"X","shared":{"scope":"anonymous"}}]}"#,
        )
        .unwrap();
        assert_eq!(mark_from_page(&mut s, &page).unwrap(), 1, "X isn't in the mirror (excluded, say)");
        assert!(s.read(|o| o.is_marked_shared("A")).unwrap());
        assert!(!s.read(|o| o.is_marked_shared("B")).unwrap() && !s.read(|o| o.is_marked_shared("X")).unwrap());
        // A mark an older build left for an item the mirror doesn't hold.
        s.write(|o| o.mark_shared("Z", None)).unwrap();
        let f = FakeSharing::default();
        f.add("A", vec![FakeSharing::permission("l", Some(("view", "anonymous")), &["read"])]);
        let p = all(&mut s, &f);
        assert_eq!((p.items.len(), p.unmarked), (1, 1));
        assert!(!s.read(|o| o.is_marked_shared("Z")).unwrap());
    }

    /// A change left on record, as a process that stopped mid-request
    /// leaves it.
    fn left_on_record(s: &mut Store) -> i64 {
        let entry = ActivityEntry {
            seq: 0,
            at_ms: 1,
            outcome: ActivityOutcome::Waiting,
            action: "stop sharing".into(),
            path: "/Doc.docx".into(),
            detail: "Specific people, can edit; asked OneDrive".into(),
        };
        s.write_durable(|o| o.intent_begin(&entry, "D", "e", IntendedChange::Stop, "Specific people, can edit"))
            .unwrap()
    }

    /// Every command settles what an earlier one left before its own work:
    /// a change (which keeps the lock) and a look (list, scan, shared with
    /// you), which settles only while no change is under way.
    #[test]
    fn every_command_settles_leftovers_first() {
        for mode in [Mode::Change, Mode::Look] {
            let (mut s, f) = setup();
            let dir = std::env::temp_dir().join(format!("oneup-sharing-begin-{}-{mode:?}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            left_on_record(&mut s);
            // OneDrive has the permission gone: the change took effect.
            f.items.lock().unwrap().get_mut("D").unwrap().permissions.retain(|p| p.id != "e");
            let begun = block_on(begin(&dir, &mut s, &f, mode)).unwrap();
            assert_eq!(begun.settled, Settled { settled: 1, unrecorded: 0 }, "{mode:?}");
            assert!(s.read(|o| o.intents()).unwrap().is_empty());
            assert_eq!(s.read(|o| o.activity_recent(None, 1)).unwrap()[0].outcome, ActivityOutcome::Done);
            assert_eq!(begun.lock.is_some(), mode == Mode::Change, "only a change keeps the lock");
            drop(begun);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Three items shared with the account, cut to the limit as Graph's is.
    impl WithMe for FakeSharing {
        async fn shared_with_me(&self, limit: usize) -> oneup_graph::Result<(Vec<oneup_graph::SharedItem>, bool)> {
            let all: Vec<oneup_graph::SharedItem> = (0..3)
                .map(|i| oneup_graph::SharedItem {
                    name: format!("s{i}"),
                    drive_id: "b".into(),
                    id: format!("B!{i}"),
                    folder: false,
                    size: None,
                    owner: None,
                    shared_ms: None,
                })
                .collect();
            let more = all.len() > limit;
            Ok((all.into_iter().take(limit).collect(), more))
        }
    }

    /// Each tab answers with no more than the page has room for.
    #[test]
    fn each_tab_answers_within_the_room_left() {
        let (mut s, f) = setup();
        let dir = std::env::temp_dir().join(format!("oneup-sharing-room-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let by_you = block_on(window_tab(&dir, &mut s, &f, Tab::ByYou, None, 1)).unwrap();
        assert_eq!(by_you["items"].as_array().unwrap().len(), 1);
        assert!(by_you["next"].is_string(), "more to come, when there's room");
        let with_me = block_on(window_tab(&dir, &mut s, &f, Tab::WithMe, None, 2)).unwrap();
        assert_eq!((with_me["items"].as_array().unwrap().len(), with_me["more"].as_bool()), (2, Some(true)));
        let none = block_on(window_tab(&dir, &mut s, &f, Tab::ByYou, None, 0)).unwrap();
        assert!(none["items"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The item is shared again while the audit reads it: the mark the
    /// mirror wrote meanwhile stays, though the audit read it as not shared.
    #[test]
    fn a_mark_written_meanwhile_isnt_removed() {
        struct SharedAgain<'a> {
            fake: FakeSharing,
            db: &'a std::path::Path,
        }
        impl SharingApi for SharedAgain<'_> {
            async fn sharing(
                &self,
                ids: &[&str],
            ) -> oneup_graph::Result<Vec<oneup_graph::Result<Option<oneup_graph::ItemSharing>>>> {
                // The mirror, in another process, marks it again as the audit asks.
                let mut other = Store::open(self.db, 1024).unwrap();
                other.write(|o| o.mark_shared("S", None)).unwrap();
                self.fake.sharing(ids).await
            }
            async fn delete_permission(&self, item: &str, perm: &str, etag: &str) -> oneup_graph::Result<()> {
                self.fake.delete_permission(item, perm, etag).await
            }
            async fn set_permission_expiry(
                &self,
                item: &str,
                perm: &str,
                etag: &str,
                ms: i64,
            ) -> oneup_graph::Result<oneup_graph::Permission> {
                self.fake.set_permission_expiry(item, perm, etag, ms).await
            }
        }
        let dir = std::env::temp_dir().join(format!("oneup-sharing-again-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        let mut s = Store::open(&db, 1024).unwrap();
        s.write(|o| {
            o.upsert_item(&row("R", None, "root", Kind::Root), false)?;
            o.upsert_item(&row("S", Some("R"), "s.txt", Kind::File), false)?;
            o.mark_shared("S", None)
        })
        .unwrap();
        let fake = FakeSharing::default();
        fake.add("S", vec![FakeSharing::permission("o", None, &["owner"])]);
        let api = SharedAgain { fake, db: &db };
        let p = block_on(list_page(&mut s, &api, None, 10, 10)).unwrap();
        assert_eq!((p.items.len(), p.unmarked), (0, 0));
        assert!(s.read(|o| o.is_marked_shared("S")).unwrap(), "the newer mark stays");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The window reopens on the tab last chosen: either one settles what
    /// an earlier change left unsettled before it shows anything; a further
    /// page of the list doesn't read the record again.
    #[test]
    fn either_tab_of_the_window_settles_leftovers_first() {
        for tab in [Tab::WithMe, Tab::ByYou] {
            let (mut s, f) = setup();
            let dir = std::env::temp_dir().join(format!("oneup-sharing-tab-{}-{tab:?}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            left_on_record(&mut s);
            f.items.lock().unwrap().get_mut("D").unwrap().permissions.retain(|p| p.id != "e");
            let shown = block_on(window_tab(&dir, &mut s, &f, tab, None, 500)).unwrap();
            assert!(shown.get("items").is_some(), "{tab:?}: {shown}");
            assert!(s.read(|o| o.intents()).unwrap().is_empty(), "{tab:?}");
            assert_eq!(s.read(|o| o.activity_recent(Some("/Doc.docx"), 1)).unwrap()[0].outcome, ActivityOutcome::Done);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// One run reads one page, however many candidates there are, and
    /// says how to go on.
    #[test]
    fn the_command_reads_one_page_and_says_how_to_go_on() {
        let mut s = Store::open_in_memory().unwrap();
        let f = FakeSharing::default();
        let mut inherited = FakeSharing::permission("l", Some(("view", "anonymous")), &["read"]);
        inherited.inherited_from = Some("F".into());
        let n = PAGE_CHECKS + 100;
        s.write(|o| {
            o.upsert_item(&row("R", None, "root", Kind::Root), false)?;
            for i in 0..n {
                let id = format!("F{i:04}");
                o.upsert_item(&row(&id, Some("R"), &id, Kind::File), false)?;
                o.mark_shared(&id, None)?;
            }
            Ok(())
        })
        .unwrap();
        for i in 0..n {
            f.add(&format!("F{i:04}"), vec![inherited.clone()]);
        }
        let day = |_: Option<i64>| String::new();
        let lines = block_on(listing_text(&mut s, &f, None, &day)).unwrap();
        // The first page dropped the marks it read; the rest are untouched.
        assert_eq!(s.read(|o| o.shared_count()).unwrap(), 100);
        let last = lines.last().unwrap();
        assert!(last.contains("--after F0499") && last.contains("500 items checked"), "{last}");
        let lines = block_on(listing_text(&mut s, &f, Some("F0499"), &day)).unwrap();
        assert_eq!(lines, ["Nothing found."]);
        assert_eq!(s.read(|o| o.shared_count()).unwrap(), 0);
    }

    /// Messages to the person come from the catalogue; nothing from
    /// OneDrive's answer reaches them.
    #[test]
    fn failures_are_said_in_the_persons_words() {
        let body = r#"{"error":{"code":"accessDenied","message":"ana@example.com https://1drv.ms/x"}}"#;
        let e = oneup_graph::Error::refused(403, "x", body.into());
        assert_eq!(graph_said(&e), "OneDrive refused it (403, accessDenied).");
        assert_eq!(effect_said(&effect::Error::Graph(e)), "OneDrive refused it (403, accessDenied).");
        assert!(refusal_said(&oneup_gate::sharing::Refusal::Inherited).contains("inherited"));
        assert_eq!(graph_said(&oneup_graph::Error::Exhausted(8)), "Gave up after 8 attempts.");
    }

    /// While a change is under way elsewhere: a look leaves its record
    /// alone; another change is refused.
    #[test]
    fn nothing_is_settled_under_a_change_in_flight() {
        let (mut s, f) = setup();
        let h = held("in-flight");
        left_on_record(&mut s);
        let begun = block_on(begin(&h.dir, &mut s, &f, Mode::Look)).unwrap();
        assert_eq!(begun.settled, Settled::default());
        assert_eq!(s.read(|o| o.intents()).unwrap().len(), 1);
        assert!(block_on(begin(&h.dir, &mut s, &f, Mode::Change)).is_err());
    }
}
