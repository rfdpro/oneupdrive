//! What `oneup-tray` shows: the icon's picture and tooltip, its menu, and
//! which changes deserve a notification. Kept free of Windows calls so it
//! can be tested; the executable (`bin/oneup-tray.rs`) wires it to
//! `oneup_sys::tray` and the agent's control pipe.

use std::collections::HashSet;

use oneup_ipc::{Activity, Problem, Status};
use oneup_sys::tray::{Glyph, Item, Kind};

use crate::l10n::{Locale, current};
use crate::{t, t_in, t_key};

/// What the tray knows of the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    /// Not asked yet.
    Connecting,
    NotRunning,
    /// Not running: an error stopped it (`oneup_app::problem`).
    Stopped(Box<Problem>),
    /// Running, but the pipe failed (the message says how).
    Unreachable(String),
    Running(Box<Status>),
}

/// The icon's look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    UpToDate,
    Syncing,
    Paused,
    /// Waiting for the user (a safety stop, a version-store decision).
    Attention,
    /// Retrying after a problem.
    Problem,
    /// The agent isn't running.
    Off,
}

pub fn look(v: &View) -> Look {
    if matches!(v, View::Stopped(_)) {
        return Look::Problem;
    }
    let Some(s) = running(v) else { return Look::Off };
    if !s.safety_stops.is_empty() || s.overflow.as_ref().is_some_and(|o| o.waiting) {
        return Look::Attention;
    }
    if s.problem.is_some() {
        return Look::Problem;
    }
    match s.activity {
        Activity::Starting | Activity::Syncing => Look::Syncing,
        Activity::Paused | Activity::Stopping => Look::Paused,
        Activity::WaitingForYou => Look::Attention,
        Activity::Retrying => Look::Problem,
        Activity::Idle if s.paused.local || s.paused.remote || s.auto_pause.is_some() => Look::Paused,
        Activity::Idle => Look::UpToDate,
    }
}

fn running(v: &View) -> Option<&Status> {
    match v {
        View::Running(s) => Some(s.as_ref()),
        _ => None,
    }
}

/// One line saying how things are (the tooltip's).
pub fn summary(v: &View) -> String {
    headline(v, true)
}

/// [`summary`]; `failures`: with the count of items that can't sync (the
/// menu lists them on a line of their own instead).
fn headline(v: &View, failures: bool) -> String {
    let s = match v {
        View::Connecting => return t!("Connecting…"),
        View::NotRunning => return t!("Not running"),
        View::Stopped(p) => return t!("Stopped: {what}", what = crate::problem::shown(p, current()).0),
        View::Unreachable(e) => return t!("Can't connect: {why}", why = e),
        View::Running(s) => s,
    };
    let waiting = s.safety_stops.len() + usize::from(s.overflow.as_ref().is_some_and(|o| o.waiting));
    if waiting > 0 {
        return t!("{n, plural, one {Needs your review} other {# need your review}}", n = waiting);
    }
    if let Some(why) = &s.auto_pause {
        // The agent says why in English (`autopause`); shown in the reader's language.
        return t!("Paused: {why}", why = crate::autopause::said(why, s.auto_pause_hours.as_deref(), current()));
    }
    if let Some(p) = working(s) {
        let percent = p.items_done * 100 / p.items_total.max(1);
        let (done, total) = (p.items_done, p.items_total);
        return if p.round > 1 {
            t!(
                "Syncing {done} of {total} items ({percent}%) (pass {pass})",
                done = done,
                total = total,
                percent = percent,
                pass = p.round
            )
        } else {
            t!("Syncing {done} of {total} items ({percent}%)", done = done, total = total, percent = percent)
        };
    }
    match s.activity {
        Activity::Starting => t!("Checking all folders…"),
        Activity::Syncing => t!("Syncing…"),
        Activity::Paused => t!("Paused"),
        Activity::Stopping => t!("Stopping…"),
        Activity::WaitingForYou => t!("Needs your review"),
        Activity::Retrying => match &s.problem {
            Some(p) => t!("Problem: {what}", what = crate::problem::shown(p, current()).0),
            None => t!("Retrying soon"),
        },
        // OneDrive full: up to date otherwise, but uploads wait.
        Activity::Idle if s.problem.is_some() => {
            t!(
                "Problem: {what}",
                what = s.problem.as_ref().map(|p| crate::problem::shown(p, current()).0).unwrap_or_default()
            )
        }
        Activity::Idle => match (s.paused.local, s.paused.remote) {
            (true, false) => t!("Paused: changes to this PC"),
            (false, true) => t!("Paused: changes to OneDrive"),
            _ => match &s.unsynced {
                Some(u) if failures => t!("Up to date · {items} failed", items = unsynced_count(u, current())),
                _ => t!("Up to date"),
            },
        },
    }
}

/// Problems that usually clear by themselves (the connection, OneDrive's
/// service) are notified only once this many tries in a row met them
/// (about 15 minutes, with the agent's growing pauses); the others at once.
const PATIENT_ATTEMPTS: u32 = 4;

/// The notification for a problem: what it is and what to do.
/// `stopped`: the agent ended with it (else it's being retried).
fn problem_notice(stopped: bool, p: &Problem, loc: &Locale) -> Notice {
    let (what, _, action) = crate::problem::shown(p, loc);
    Notice {
        title: if stopped { t_in!(loc, "OneUpDrive stopped") } else { t_in!(loc, "Sync problem") },
        text: t_in!(loc, "{problem} {action}", problem = what, action = action),
        kind: Kind::Error,
    }
}

/// `12 items`, `1,000 items or more`, in `loc`'s language.
pub(crate) fn unsynced_count(u: &oneup_ipc::Unsynced, loc: &Locale) -> String {
    let n = u.count as u64;
    if u.more {
        t_in!(loc, "{n, plural, one {# item} other {# items}} or more", n = n)
    } else {
        t_in!(loc, "{n, plural, one {# item} other {# items}}", n = n)
    }
}

/// The round being carried out, when that's what the agent is doing (not
/// when it waits for the user or is paused).
fn working(s: &Status) -> Option<&oneup_ipc::Progress> {
    let busy = matches!(s.activity, Activity::Starting | Activity::Syncing) && s.auto_pause.is_none();
    s.progress.as_ref().filter(|p| busy && p.items_total > 0)
}

/// A time left, roughly: "about 25 min", "about 2 h 10 min".
pub(crate) fn time_left(ms: u64) -> String {
    time_left_in(ms, current())
}

/// [`time_left`] in `loc`'s language.
pub(crate) fn time_left_in(ms: u64, loc: &Locale) -> String {
    let min = ms.div_ceil(60_000);
    match min {
        0 | 1 => t_in!(loc, "less than a minute"),
        m if m < 60 => t_in!(loc, "about {m} min", m = m),
        m if m % 60 == 0 || m >= 600 => t_in!(loc, "about {h} h", h = (m + 30) / 60),
        m => t_in!(loc, "about {h} h {m} min", h = m / 60, m = m % 60),
    }
}

/// The second line of the tooltip while a round runs: the bytes moved, if
/// it transfers anything, and the time left, once it can be told.
fn progress_detail(p: &oneup_ipc::Progress) -> Option<String> {
    let size = crate::versions::size;
    let mut parts = Vec::new();
    if p.bytes_total > 0 {
        parts.push(t!("{done} of {total}", done = size(p.bytes_done), total = size(p.bytes_total)));
    }
    match p.eta_ms() {
        Some(ms) => parts.push(t!("{time} left", time = time_left(ms))),
        None if p.items_done < p.items_total => parts.push(t!("estimating time left")),
        None => {}
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A round's progress on one line (`oneup status`).
pub fn progress_line(p: &oneup_ipc::Progress) -> String {
    let percent = p.items_done * 100 / p.items_total.max(1);
    let head = t!(
        "round {round}: {done} of {total} items ({percent}%)",
        round = p.round,
        done = p.items_done,
        total = p.items_total,
        percent = percent
    );
    match progress_detail(p) {
        Some(d) => format!("{head}, {d}"),
        None => head,
    }
}

/// The tooltip (Windows shows at most 127 characters).
pub fn tip(v: &View) -> String {
    let mut t = format!("OneUpDrive: {}", summary(v));
    if let View::Running(s) = v
        && let Some(d) = working(s).and_then(progress_detail)
    {
        t = format!("{t}\n{d}");
    }
    if t.chars().count() <= 127 { t } else { format!("{}…", t.chars().take(126).collect::<String>()) }
}

/// The menu's commands.
pub mod id {
    pub const OPEN_FOLDER: u32 = 2;
    pub const SYNC_NOW: u32 = 3;
    /// Pause for [`super::PAUSES`]`[i]`: `PAUSE + i`.
    pub const PAUSE: u32 = 10;
    pub const RESUME: u32 = 20;
    /// Review safety stop `i`: `REVIEW_STOP + i`.
    pub const REVIEW_STOP: u32 = 30;
    pub const REVIEW_OVERFLOW: u32 = 40;
    pub const REVIEW_PRUNE: u32 = 45;
    pub const LIST_PRUNE: u32 = 46;
    pub const LIST_UNSYNCED: u32 = 47;
    /// Allow [`crate::netusage::ALLOW_MB`] more on a used-up data budget.
    pub const ALLOW_DATA: u32 = 48;
    pub const ACTIVITY: u32 = 50;
    pub const PROBLEM: u32 = 51;
    pub const START: u32 = 60;
    pub const STOP: u32 = 61;
    pub const CLOSE: u32 = 70;
    pub const SETTINGS: u32 = 80;
    pub const SEARCH: u32 = 81;
    pub const UPDATE: u32 = 90;
    /// The status line: opens the settings window, where it's told at length.
    pub const STATUS: u32 = 1;
    /// A line that only tells.
    pub const NOTE: u32 = 4;
}

/// The pause choices: label, minutes (`None`: until resumed).
/// (`t_key!`: translated where they're shown.)
pub const PAUSES: &[(&str, Option<u64>)] = &[
    (t_key!("30 minutes"), Some(30)),
    (t_key!("2 hours"), Some(120)),
    (t_key!("8 hours"), Some(480)),
    (t_key!("Until I resume"), None),
];

/// The menu's pictures: symbols from Windows' icon font.
mod glyph {
    use oneup_sys::tray::Glyph;

    pub const FOLDER: Glyph = Glyph::Symbol('\u{E838}');
    pub const SYNC: Glyph = Glyph::Symbol('\u{E895}');
    pub const PAUSE: Glyph = Glyph::Symbol('\u{E769}');
    pub const PLAY: Glyph = Glyph::Symbol('\u{E768}');
    pub const HISTORY: Glyph = Glyph::Symbol('\u{E81C}');
    pub const SETTINGS: Glyph = Glyph::Symbol('\u{E713}');
    pub const SEARCH: Glyph = Glyph::Symbol('\u{E721}');
    pub const POWER: Glyph = Glyph::Symbol('\u{E7E8}');
    pub const CLOSE: Glyph = Glyph::Symbol('\u{E711}');
    pub const WARNING: Glyph = Glyph::Symbol('\u{E7BA}');
    pub const ERROR: Glyph = Glyph::Symbol('\u{E783}');
    pub const INFO: Glyph = Glyph::Symbol('\u{E946}');
    pub const CLOUD: Glyph = Glyph::Symbol('\u{E753}');
    /// Something new to do, like the update ready to install.
    pub const NEW: Glyph = Glyph::Dot([0, 120, 212]);
}

/// The status dot's colour for a look.
pub fn dot(look: Look) -> [u8; 3] {
    match look {
        Look::UpToDate => [16, 185, 92],
        Look::Syncing => [0, 120, 212],
        Look::Attention => [242, 169, 0],
        Look::Problem => [209, 52, 56],
        Look::Paused | Look::Off => [138, 138, 138],
    }
}

fn action(id: u32, label: impl Into<String>, glyph: Glyph) -> Item {
    Item::Action { id, label: label.into(), enabled: true, glyph: Some(glyph) }
}

/// A line that only tells (greyed).
fn note(label: impl Into<String>) -> Item {
    Item::Action { id: id::NOTE, label: label.into(), enabled: false, glyph: None }
}

/// The data budget in use, when it's used up.
fn budget_reached(s: &Status) -> Option<&oneup_ipc::DataUse> {
    s.network.as_ref()?.data.as_ref().filter(|d| d.reached)
}

/// The notification for a new safety stop, in `loc`'s language.
pub(crate) fn stop_notice(st: &oneup_ipc::SafetyStop, loc: &Locale) -> Notice {
    if st.kind == "content" {
        return Notice {
            title: t_in!(loc, "Files may be encrypted"),
            text: t_in!(
                loc,
                "{n} changed files look encrypted, possibly by ransomware. They weren't uploaded. Check them before continuing.",
                n = st.count
            ),
            kind: Kind::Warning,
        };
    }
    if st.kind == "removal" {
        return Notice {
            title: t_in!(loc, "Approve removing duplicates"),
            text: t_in!(loc, "The duplicates you chose are removed once you approve."),
            kind: Kind::Info,
        };
    }
    let n = st.items();
    Notice {
        title: t_in!(loc, "Review needed"),
        text: if st.side == "local" {
            t_in!(
                loc,
                "{n} items on this PC would be deleted, overwritten or moved. Nothing changes until you review.",
                n = n
            )
        } else {
            t_in!(
                loc,
                "{n} items in OneDrive would be deleted, overwritten or moved. Nothing changes until you review.",
                n = n
            )
        },
        kind: Kind::Warning,
    }
}

/// The menu for what the tray knows now; `update`: the version of an
/// update ready to install. In groups, like Windows' own: how things are,
/// what waits for you, the everyday commands, the rest, then quitting.
pub fn menu(v: &View, update: Option<&str>) -> Vec<Item> {
    // A command, not a greyed note: a greyed line's dot loses its colour.
    let mut items = vec![action(id::STATUS, headline(v, false), Glyph::Dot(dot(look(v))))];
    if let View::Running(s) = v
        && let Some(d) = working(s).and_then(progress_detail)
    {
        items.push(note(d));
    }
    items.push(Item::Separator);
    match v {
        View::Running(s) => {
            let waiting = items.len();
            for (i, st) in s.safety_stops.iter().enumerate() {
                let label = if st.kind == "content" {
                    t!("Possibly encrypted: {n} files…", n = st.count)
                } else if st.kind == "removal" {
                    t!("Review removing duplicates…")
                } else if st.side == "local" {
                    t!("Review {n} changes on this PC…", n = st.items())
                } else {
                    t!("Review {n} changes in OneDrive…", n = st.items())
                };
                items.push(action(id::REVIEW_STOP + i as u32, label, glyph::WARNING));
            }
            if let Some(o) = &s.overflow
                && o.waiting
            {
                let label = t!("Review {n} removed files…", n = o.count);
                items.push(action(id::REVIEW_OVERFLOW, label, glyph::WARNING));
            }
            if let Some(o) = &s.prune_offer {
                let sub = vec![
                    Item::Action { id: id::REVIEW_PRUNE, label: t!("Remove or keep…"), enabled: true, glyph: None },
                    Item::Action { id: id::LIST_PRUNE, label: t!("Show list"), enabled: true, glyph: None },
                ];
                items.push(Item::Submenu {
                    label: t!("Excluded, in OneDrive: {n}", n = o.items),
                    items: sub,
                    glyph: Some(glyph::CLOUD),
                });
            }
            if s.problem.is_some() {
                items.push(action(id::PROBLEM, t!("Error details…"), glyph::ERROR));
            }
            if let Some(u) = &s.unsynced {
                let label = t!("Sync failure: {items}", items = unsynced_count(u, current()));
                items.push(action(id::LIST_UNSYNCED, label, glyph::INFO));
            }
            if budget_reached(s).is_some() {
                let label = t!("Allow {mb} MB more", mb = crate::netusage::ALLOW_MB);
                items.push(action(id::ALLOW_DATA, label, glyph::WARNING));
            }
            if items.len() > waiting {
                items.push(Item::Separator);
            }
            items.push(action(id::OPEN_FOLDER, t!("Open folder"), glyph::FOLDER));
            items.push(action(id::SYNC_NOW, t!("Sync now"), glyph::SYNC));
            if s.paused.local || s.paused.remote {
                items.push(action(id::RESUME, t!("Resume"), glyph::PLAY));
            }
            if !(s.paused.local && s.paused.remote) {
                let choices = PAUSES
                    .iter()
                    .enumerate()
                    .map(|(i, (label, _))| Item::Action {
                        id: id::PAUSE + i as u32,
                        label: crate::l10n::message(label, &[]),
                        enabled: true,
                        glyph: None,
                    })
                    .collect();
                items.push(Item::Submenu { label: t!("Pause"), items: choices, glyph: Some(glyph::PAUSE) });
            }
        }
        View::NotRunning => items.push(action(id::START, t!("Start OneUpDrive"), glyph::PLAY)),
        View::Stopped(_) => {
            items.push(action(id::PROBLEM, t!("Error details…"), glyph::ERROR));
            items.push(Item::Separator);
            items.push(action(id::START, t!("Start OneUpDrive"), glyph::PLAY));
        }
        View::Connecting | View::Unreachable(_) => {}
    }
    if !matches!(items.last(), Some(Item::Separator)) {
        items.push(Item::Separator);
    }
    items.push(action(id::ACTIVITY, t!("Recent activity"), glyph::HISTORY));
    items.push(action(id::SEARCH, t!("Search"), glyph::SEARCH));
    items.push(action(id::SETTINGS, t!("Settings"), glyph::SETTINGS));
    items.push(Item::Separator);
    if let Some(version) = update {
        items.push(action(id::UPDATE, t!("Install update {version}", version = version), glyph::NEW));
    }
    if matches!(v, View::Running(_)) {
        items.push(action(id::STOP, t!("Stop OneUpDrive"), glyph::POWER));
    }
    items.push(action(id::CLOSE, t!("Hide icon"), glyph::CLOSE));
    items
}

/// A notification to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub text: String,
    pub kind: Kind,
}

/// Decides what to notify about: each safety stop, version-store batch and
/// offer to remove excluded items from OneDrive once, a problem once per
/// spell (see [`PATIENT_ATTEMPTS`]), and the agent ending when nobody asked
/// it to, with why if an error stopped it. Pauses the agent makes by itself
/// (a game, battery saver) only change the icon: they come and go too often
/// to interrupt for.
#[derive(Debug, Default)]
pub struct Notices {
    shown: HashSet<String>,
    was_running: bool,
    /// The agent was asked to stop (from the menu, or seen stopping).
    pub stop_expected: bool,
}

impl Notices {
    pub fn update(&mut self, v: &View) -> Vec<Notice> {
        let mut out = Vec::new();
        match v {
            View::Running(s) => {
                self.was_running = true;
                // Stopping as asked (from the menu, `oneup stop`, signing out).
                self.stop_expected |= s.activity == Activity::Stopping;
                for st in &s.safety_stops {
                    if self.shown.insert(st.code.clone()) {
                        out.push(stop_notice(st, current()));
                    }
                }
                if let Some(o) = &s.prune_offer
                    && self.shown.insert(o.code.clone())
                {
                    out.push(Notice {
                        title: t!("Excluded items in OneDrive"),
                        text: t!(
                            "{n} excluded items are still in OneDrive. Click to remove or keep them.",
                            n = o.items
                        ),
                        kind: Kind::Info,
                    });
                }
                if let Some(o) = &s.overflow
                    && o.waiting
                    && self.shown.insert(o.code.clone())
                {
                    out.push(Notice {
                        title: t!("Review needed"),
                        text: t!(
                            "{n} files deleted in OneDrive don't fit in the version store. They stay here until you decide.",
                            n = o.count
                        ),
                        kind: Kind::Warning,
                    });
                }
                if let Some(d) = budget_reached(s)
                    && self.shown.insert(format!("budget:{}:{}", d.month, d.budget))
                {
                    let size = crate::versions::size;
                    out.push(Notice {
                        title: t!("Data budget reached"),
                        text: t!(
                            "Sync paused: {used} of {budget} used this month on this network. Allow {mb} MB more from this icon's menu.",
                            used = size(d.up + d.down),
                            budget = size(d.budget),
                            mb = crate::netusage::ALLOW_MB
                        ),
                        kind: Kind::Warning,
                    });
                }
                if let Some(p) = &s.problem {
                    let patient = matches!(p.kind.as_str(), "offline" | "onedrive" | "other");
                    if (!patient || p.attempts >= PATIENT_ATTEMPTS)
                        && self.shown.insert(format!("problem:{}:{}", p.kind, p.since_ms))
                    {
                        out.push(problem_notice(false, p, current()));
                    }
                }
            }
            View::NotRunning | View::Stopped(_) => {
                if self.was_running && !self.stop_expected {
                    out.push(match v {
                        View::Stopped(p) => problem_notice(true, p, current()),
                        _ => Notice {
                            title: t!("OneUpDrive stopped"),
                            text: t!("Files aren't syncing. Start it from this icon's menu."),
                            kind: Kind::Error,
                        },
                    });
                }
                self.was_running = false;
                self.stop_expected = false;
            }
            View::Connecting | View::Unreachable(_) => {}
        }
        out
    }
}

/// The program's icon file (`.ico`): the up-to-date cloud at the sizes
/// Windows asks for, each a 32-bit bitmap with its transparency mask.
/// `packaging/oneupdrive.ico` is this, kept in step by a test (the build
/// script embeds that file in every executable).
pub fn ico() -> Vec<u8> {
    const SIZES: [u32; 6] = [16, 24, 32, 48, 64, 256];
    let images: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&n| {
            let rgba = render(Look::UpToDate, n);
            // BITMAPINFOHEADER; the height counts the colour and mask halves.
            let mut b = Vec::new();
            for v in [40u32, n, 2 * n, 0x0020_0001, 0, 0, 0, 0, 0, 0] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.truncate(12);
            b.extend_from_slice(&1u16.to_le_bytes()); // planes
            b.extend_from_slice(&32u16.to_le_bytes()); // bits a pixel
            for v in [0u32, 0, 0, 0, 0, 0] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            // Bottom-up rows, BGRA.
            for y in (0..n).rev() {
                for x in 0..n {
                    let i = ((y * n + x) * 4) as usize;
                    b.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            // The AND mask (all visible: the alpha decides), rows padded to 32 bits.
            let row = n.div_ceil(32) * 4;
            b.extend(std::iter::repeat_n(0u8, (row * n) as usize));
            (n, b)
        })
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (n, b) in &images {
        let side = if *n >= 256 { 0 } else { *n as u8 };
        out.extend_from_slice(&[side, side, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += b.len() as u32;
    }
    for (_, b) in images {
        out.extend_from_slice(&b);
    }
    out
}

/// The mark: a cloud lifted one step above a chevron-shaped base ("one
/// up"), cut along the chevron with a transparent gap between the two.
/// Coordinates are in the unit square; `None` outside the mark.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Piece {
    Top,
    Base,
}

fn mark(x: f32, y: f32) -> Option<(Piece, f32)> {
    const CX: f32 = 0.52;
    const CY: f32 = 0.52;
    const SLOPE: f32 = 0.40;
    const GAP: f32 = 0.07;
    const LIFT: f32 = 0.055;
    let circle = |cx: f32, cy: f32, r: f32, x: f32, y: f32| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
    let cloud = |x: f32, y: f32| {
        // A rounded base under three lobes.
        let (x0, y0, x1, y1, r) = (0.11, 0.55, 0.93, 0.77, 0.11);
        let base = x >= x0 && x <= x1 && y >= y0 && y <= y1 && {
            // Not `clamp`: the base is 2r tall, so its range is one point
            // that rounding can turn inside out.
            let (cx, cy) = (x.max(x0 + r).min(x1 - r), y.max(y0 + r).min(y1 - r));
            (x - cx).powi(2) + (y - cy).powi(2) <= r * r
        };
        base || circle(0.30, 0.58, 0.19, x, y) || circle(0.55, 0.45, 0.27, x, y) || circle(0.78, 0.62, 0.15, x, y)
    };
    let line = |x: f32| CY + (x - CX).abs() * SLOPE;
    // Centred in the square.
    let y = y - 0.04;
    if cloud(x, y) && y > line(x) + GAP / 2.0 {
        // Shade from the cut down.
        return Some((Piece::Base, ((y - 0.45) / 0.35).clamp(0.0, 1.0)));
    }
    let y = y + LIFT;
    if cloud(x, y) && y < line(x) - GAP / 2.0 {
        return Some((Piece::Top, ((y - 0.18) / 0.5).clamp(0.0, 1.0)));
    }
    None
}

/// RGBA pixels (`size`×`size`) for a look: the mark in its colours when
/// syncing normally and grey when paused or off, with a badge for what
/// needs noticing.
pub fn render(look: Look, size: u32) -> Vec<u8> {
    type Rgba = [f32; 4];
    const SKY: Rgba = [63.0, 182.0, 255.0, 255.0];
    const AZURE: Rgba = [47.0, 134.0, 246.0, 255.0];
    const INDIGO: Rgba = [45.0, 63.0, 216.0, 255.0];
    const GREY_LIGHT: Rgba = [160.0, 160.0, 160.0, 255.0];
    const GREY: Rgba = [130.0, 130.0, 130.0, 255.0];
    const GREY_DARK: Rgba = [100.0, 100.0, 100.0, 255.0];
    const WHITE: Rgba = [255.0, 255.0, 255.0, 255.0];
    const AMBER: Rgba = [242.0, 169.0, 0.0, 255.0];
    const RED: Rgba = [209.0, 52.0, 56.0, 255.0];
    const DARK_BLUE: Rgba = [11.0, 79.0, 156.0, 255.0];
    let mix = |a: Rgba, b: Rgba, t: f32| -> Rgba { std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t) };
    let circle = |cx: f32, cy: f32, r: f32| move |x: f32, y: f32| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32| move |x: f32, y: f32| x >= x0 && x <= x1 && y >= y0 && y <= y1;
    let grey = matches!(look, Look::Paused | Look::Off);
    // Faded: the agent isn't running.
    let alpha = if look == Look::Off { 140.0 } else { 255.0 };
    let (bx, by) = (0.72, 0.72);
    let badge: Option<Rgba> = match look {
        Look::UpToDate | Look::Off => None,
        Look::Syncing | Look::Paused => Some(DARK_BLUE),
        Look::Attention => Some(AMBER),
        Look::Problem => Some(RED),
    };
    let glyph = |x: f32, y: f32| match look {
        Look::Syncing => circle(bx, by, 0.17)(x, y) && !circle(bx, by, 0.07)(x, y),
        Look::Paused => {
            rect(bx - 0.11, by - 0.12, bx - 0.03, by + 0.12)(x, y)
                || rect(bx + 0.03, by - 0.12, bx + 0.11, by + 0.12)(x, y)
        }
        Look::Attention | Look::Problem => {
            rect(bx - 0.045, by - 0.15, bx + 0.045, by + 0.04)(x, y) || circle(bx, by + 0.11, 0.045)(x, y)
        }
        Look::UpToDate | Look::Off => false,
    };
    let sample = |x: f32, y: f32| -> Option<Rgba> {
        if let Some(b) = badge {
            if glyph(x, y) {
                return Some(WHITE);
            }
            if circle(bx, by, 0.23)(x, y) {
                return Some(b);
            }
            if circle(bx, by, 0.29)(x, y) {
                return Some(WHITE);
            }
        }
        let (piece, t) = mark(x, y)?;
        let mut c = match (piece, grey) {
            (Piece::Top, false) => mix(SKY, AZURE, t),
            (Piece::Base, false) => mix(AZURE, INDIGO, t),
            (Piece::Top, true) => mix(GREY_LIGHT, GREY, t),
            (Piece::Base, true) => mix(GREY, GREY_DARK, t),
        };
        c[3] = alpha;
        Some(c)
    };
    // 4×4 samples a pixel, for smooth edges.
    const S: u32 = 4;
    let n = size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..S {
                for sx in 0..S {
                    let x = (px as f32 + (sx as f32 + 0.5) / S as f32) / n;
                    let y = (py as f32 + (sy as f32 + 0.5) / S as f32) / n;
                    if let Some(c) = sample(x, y) {
                        // Colour premultiplied by alpha, for averaging.
                        let a = c[3] / 255.0;
                        for i in 0..3 {
                            acc[i] += c[i] * a;
                        }
                        acc[3] += c[3];
                    }
                }
            }
            let k = (S * S) as f32;
            let alpha = acc[3] / k;
            for (i, a) in acc.iter().enumerate() {
                let v = if i == 3 {
                    alpha
                } else if alpha > 0.0 {
                    // Straight (not premultiplied) colour.
                    a / k * 255.0 / alpha
                } else {
                    0.0
                };
                out.push(v.round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    out
}

/// A name for the single-instance lock of `data_dir`'s tray.
/// The event raised when `[search]` changed (`oneup search`, the settings
/// window), so the icon takes up a new hotkey (`oneup_sys::signal`).
pub fn settings_signal(data_dir: &std::path::Path) -> String {
    instance_name(data_dir).replace("-tray-", "-search-settings-")
}

/// Tells `data_dir`'s icon, if it's showing, that `[search]` changed.
pub fn settings_changed(data_dir: &std::path::Path) {
    let _ = oneup_sys::signal::raise(&settings_signal(data_dir));
}

pub fn instance_name(data_dir: &std::path::Path) -> String {
    // FNV-1a over the path folded as Windows compares names: the same
    // folder, however it's typed. (Only processes of one version need to
    // agree on it.)
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in oneup_sync::fold(data_dir.to_string_lossy().trim_end_matches('\\')).bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("OneUpDrive-tray-{h:016x}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use oneup_ipc::{PauseState, SafetyStop};

    fn status(activity: Activity) -> Status {
        Status {
            version: "t".into(),
            rev: 1,
            local: "L".into(),
            remote: "/".into(),
            activity,
            paused: PauseState::default(),
            push: true,
            checked_ms: None,
            last: None,
            safety_stops: Vec::new(),
            overflow: None,
            problem: None,
            auto_pause: None,
            auto_pause_hours: None,
            prune_offer: None,
            progress: None,
            unsynced: None,
            retrying: None,
            databases: Vec::new(),
            databases_more: 0,
            network: None,
        }
    }

    #[test]
    fn the_tooltip_shows_how_far_a_round_is_and_the_time_left() {
        let mut s = status(Activity::Starting);
        s.progress = Some(oneup_ipc::Progress {
            round: 3,
            items_done: 40_000,
            items_total: 59_218,
            bytes_done: 1 << 30,
            bytes_total: 3 << 30,
            elapsed_ms: 10 * 60_000,
        });
        // Two thirds of the items are done but a third of the bytes: the
        // bytes say 20 more minutes, the slower estimate.
        assert_eq!(
            tip(&up(s.clone())),
            "OneUpDrive: Syncing 40,000 of 59,218 items (67%) (pass 3)\n1.0 GiB of 3.0 GiB · about 20 min left"
        );
        assert!(tip(&up(s.clone())).chars().count() <= 127);
        // Just started: no estimate yet; nothing transferred: no bytes.
        s.progress = Some(oneup_ipc::Progress {
            round: 1,
            items_done: 3,
            items_total: 400,
            bytes_done: 0,
            bytes_total: 0,
            elapsed_ms: 2_000,
        });
        assert_eq!(tip(&up(s.clone())), "OneUpDrive: Syncing 3 of 400 items (0%)\nestimating time left");
        // Paused meanwhile: the pause is what's shown.
        s.activity = Activity::Paused;
        assert_eq!(tip(&up(s)), "OneUpDrive: Paused");
        assert_eq!(time_left(90 * 60_000), "about 1 h 30 min");
        assert_eq!(time_left(30_000), "less than a minute");
        assert_eq!(crate::l10n::number(1_234_567u64), "1,234,567");
    }

    /// `packaging/oneupdrive.ico` must be what [`ico`] draws. To redraw it:
    /// `ONEUP_WRITE_ICON=1 cargo test -p oneup --lib the_icon_file`.
    #[test]
    fn the_icon_file_is_the_drawn_icon() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/oneupdrive.ico");
        if std::env::var_os("ONEUP_WRITE_ICON").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, ico()).unwrap();
        }
        assert!(std::fs::read(&path).unwrap() == ico(), "redraw packaging/oneupdrive.ico (see the test's docs)");
    }

    fn menu_of(v: &View) -> Vec<Item> {
        menu(v, None)
    }

    fn up(s: Status) -> View {
        View::Running(Box::new(s))
    }

    fn stop(code: &str) -> SafetyStop {
        SafetyStop {
            kind: "mass".into(),
            side: "local".into(),
            count: 40,
            at_least: false,
            recent: 0,
            synced_items: 100,
            code: code.into(),
        }
    }

    fn ids(items: &[Item]) -> Vec<u32> {
        items
            .iter()
            .flat_map(|i| match i {
                Item::Action { id, .. } => vec![*id],
                Item::Submenu { items, .. } => ids(items),
                Item::Separator => vec![],
            })
            .collect()
    }

    #[test]
    fn the_look_says_what_needs_attention_first() {
        assert_eq!(look(&View::NotRunning), Look::Off);
        assert_eq!(look(&up(status(Activity::Idle))), Look::UpToDate);
        assert_eq!(look(&up(status(Activity::Syncing))), Look::Syncing);
        let mut s = status(Activity::Idle);
        s.auto_pause = Some("on a metered connection".into());
        assert_eq!(look(&up(s.clone())), Look::Paused);
        assert_eq!(summary(&up(s.clone())), "Paused: on a metered connection");
        s.safety_stops.push(stop("a"));
        assert_eq!(look(&up(s)), Look::Attention);
    }

    #[test]
    fn the_menu_offers_what_applies() {
        let m = ids(&menu_of(&View::NotRunning));
        assert!(m.contains(&id::START) && !m.contains(&id::STOP));
        let mut s = status(Activity::Idle);
        let m = ids(&menu_of(&up(s.clone())));
        assert!(m.contains(&id::STOP) && m.contains(&id::PAUSE) && !m.contains(&id::RESUME));
        s.paused = PauseState { local: true, remote: true, until_ms: None };
        s.safety_stops = vec![stop("a"), stop("b")];
        let m = ids(&menu_of(&up(s)));
        assert!(m.contains(&id::RESUME) && !m.contains(&id::PAUSE));
        assert!(m.contains(&id::REVIEW_STOP) && m.contains(&(id::REVIEW_STOP + 1)));
        assert_eq!(*m.last().unwrap(), id::CLOSE);
    }

    /// Groups are set apart by one line each: none first, last or doubled,
    /// whatever applies. The status leads, with its coloured dot.
    #[test]
    fn the_menu_is_grouped_cleanly() {
        let mut busy = status(Activity::Syncing);
        busy.progress = Some(oneup_ipc::Progress {
            round: 1,
            items_done: 5,
            items_total: 10,
            bytes_done: 0,
            bytes_total: 0,
            elapsed_ms: 0,
        });
        busy.safety_stops = vec![stop("a")];
        busy.problem = Some(problem("offline", 1));
        for v in
            [View::Connecting, View::NotRunning, View::Unreachable("x".into()), up(status(Activity::Idle)), up(busy)]
        {
            for update in [None, Some("1.2.3")] {
                let m = menu(&v, update);
                assert!(!matches!(m.first(), Some(Item::Separator)) && !matches!(m.last(), Some(Item::Separator)));
                assert!(!m.windows(2).any(|w| matches!(w, [Item::Separator, Item::Separator])), "{v:?}: {m:#?}");
                let Some(Item::Action { id: id::STATUS, glyph: Some(Glyph::Dot(c)), .. }) = m.first() else {
                    panic!("no status first: {m:#?}");
                };
                assert_eq!(*c, dot(look(&v)));
            }
        }
    }

    fn problem(kind: &str, attempts: u32) -> Problem {
        Problem {
            kind: kind.into(),
            title: "Sync records need repair".into(),
            detail: "d".into(),
            action: "Install the latest update.".into(),
            technical: "mirror inconsistency: synced: 1 rows whose parent is missing or not a folder".into(),
            since_ms: 7,
            attempts,
            retry_ms: Some(60_000),
        }
    }

    /// The menu names the problem in a few words, never the technical
    /// text; Error details has the rest.
    #[test]
    fn a_problem_is_named_plainly_and_detailed_on_request() {
        let mut s = status(Activity::Retrying);
        s.problem = Some(problem("records", 1));
        let v = up(s);
        assert_eq!(summary(&v), "Problem: Sync records need repair");
        assert_eq!(look(&v), Look::Problem);
        assert!(ids(&menu_of(&v)).contains(&id::PROBLEM));
        // Stopped by one: said, with a way to start again.
        let stopped = View::Stopped(Box::new(problem("folder", 1)));
        assert_eq!(summary(&stopped), "Stopped: Sync records need repair");
        assert_eq!(look(&stopped), Look::Problem);
        let m = ids(&menu_of(&stopped));
        assert!(m.contains(&id::PROBLEM) && m.contains(&id::START));
        // OneDrive full: the rest is up to date, but that's what to know.
        let mut s = status(Activity::Idle);
        s.problem = Some(crate::problem::cloud_full(0, None, 0));
        assert_eq!(summary(&up(s)), "Problem: OneDrive is full");
    }

    /// Problems that need the user are notified at once, once a spell;
    /// those that usually clear by themselves only once they've lasted.
    #[test]
    fn a_problem_notice_is_one_message_in_the_readers_language() {
        let de = r#"{
            "Sync problem": "Synchronisierungsproblem",
            "OneUpDrive stopped": "OneUpDrive angehalten",
            "{problem} {action}": "{problem} Bitte: {action}",
            "This PC's disk is full": "Der Datenträger ist voll",
            "Free up space on the disk, or turn on Files On-Demand (Settings > Files) so files download only when opened.": "Schaffen Sie Platz."
        }"#;
        let de =
            Locale::with_catalogue("de", de, "de-DE", crate::l10n::Numbers::new(",", ".", "3;0")).expect("catalogue");
        let e = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::StorageFull));
        let p = crate::problem::describe(&e, None, 0, Some(1));
        assert_eq!(p.kind, "disk-full");
        let n = problem_notice(false, &p, &de);
        assert_eq!(n.title, "Synchronisierungsproblem");
        assert_eq!(n.text, "Der Datenträger ist voll Bitte: Schaffen Sie Platz.");
        assert_eq!(problem_notice(true, &p, &de).title, "OneUpDrive angehalten");
        let en = problem_notice(true, &p, &Locale::english());
        assert_eq!(en.title, "OneUpDrive stopped");
        assert_eq!(
            en.text,
            "This PC's disk is full Free up space on the disk, or turn on Files On-Demand (Settings > Files) so files download only when opened."
        );
    }

    #[test]
    fn problems_are_notified_when_worth_it() {
        let mut n = Notices::default();
        let with = |p: Problem| up(Status { problem: Some(p), ..status(Activity::Retrying) });
        let notes = n.update(&with(problem("records", 1)));
        assert_eq!(notes.len(), 1);
        assert!(notes[0].text.contains("Install the latest update"), "says what to do: {:?}", notes[0]);
        assert!(n.update(&with(problem("records", 2))).is_empty(), "once a spell");
        assert!(n.update(&with(problem("offline", 1))).is_empty(), "may clear by itself");
        assert_eq!(n.update(&with(problem("offline", PATIENT_ATTEMPTS))).len(), 1);
        // Stopped by an error: why, in the notification.
        let stopped = n.update(&View::Stopped(Box::new(problem("folder", 1))));
        assert_eq!(stopped[0].title, "OneUpDrive stopped");
        assert!(stopped[0].text.starts_with("Sync records need repair"));
    }

    /// Up to date, but some items can't sync: said in the summary, listed
    /// from the menu, and not notified (nothing changes until the user acts).
    #[test]
    fn what_cant_sync_is_said_and_listed() {
        let mut s = status(Activity::Idle);
        s.unsynced = Some(oneup_ipc::Unsynced { count: 1_000, more: true });
        let v = up(s);
        assert_eq!(summary(&v), "Up to date · 1,000 items or more failed");
        assert_eq!(look(&v), Look::UpToDate);
        assert!(ids(&menu_of(&v)).contains(&id::LIST_UNSYNCED));
        assert!(Notices::default().update(&v).is_empty());
    }

    /// A used-up data budget is said once (again once more is allowed and
    /// used up too) and can be answered from the menu.
    #[test]
    fn a_reached_data_budget_is_notified_and_answered_from_the_menu() {
        let mut s = status(Activity::Idle);
        let data = oneup_ipc::DataUse { month: "2026-10".into(), up: 3, down: 7, budget: 10, reached: true };
        s.network = Some(oneup_ipc::NetStatus { data: Some(data.clone()), ..oneup_ipc::NetStatus::default() });
        s.auto_pause = Some("data budget reached".into());
        let mut n = Notices::default();
        let first = n.update(&up(s.clone()));
        assert!(first.iter().any(|x| x.title == "Data budget reached"), "{first:?}");
        assert!(n.update(&up(s.clone())).is_empty(), "once");
        assert!(ids(&menu_of(&up(s.clone()))).contains(&id::ALLOW_DATA));
        let more = oneup_ipc::DataUse { budget: 20, ..data };
        s.network = Some(oneup_ipc::NetStatus { data: Some(more), ..oneup_ipc::NetStatus::default() });
        assert_eq!(n.update(&up(s.clone())).len(), 1, "used up again");
        s.network = None;
        assert!(!ids(&menu_of(&up(s))).contains(&id::ALLOW_DATA));
    }

    #[test]
    fn each_thing_is_notified_once() {
        let mut n = Notices::default();
        let mut s = status(Activity::Idle);
        assert!(n.update(&up(s.clone())).is_empty());
        s.safety_stops.push(stop("a"));
        assert_eq!(n.update(&up(s.clone())).len(), 1);
        assert!(n.update(&up(s.clone())).is_empty());
        s.prune_offer = Some(oneup_ipc::PruneOffer { entries: 1, items: 9, bytes: 0, code: "p".into() });
        assert_eq!(n.update(&up(s.clone()))[0].kind, Kind::Info);
        assert!(n.update(&up(s.clone())).is_empty());
        assert!(ids(&menu_of(&up(s.clone()))).contains(&id::REVIEW_PRUNE));
        s.auto_pause = Some("battery saver is on".into());
        assert!(n.update(&up(s.clone())).is_empty());
        // Stopping unasked is notified; asked for, it isn't.
        assert_eq!(n.update(&View::NotRunning)[0].kind, Kind::Error);
        assert!(n.update(&View::NotRunning).is_empty());
        n.update(&up(s.clone()));
        n.stop_expected = true;
        assert!(n.update(&View::NotRunning).is_empty());
        // Seen stopping first (`oneup stop`): expected too.
        n.update(&up(s.clone()));
        n.update(&up(Status { activity: Activity::Stopping, ..s }));
        assert!(n.update(&View::NotRunning).is_empty());
    }

    #[test]
    fn icons_have_a_transparent_corner_and_a_solid_mark() {
        for look in [Look::UpToDate, Look::Syncing, Look::Paused, Look::Attention, Look::Problem, Look::Off] {
            for size in [16, 20, 24, 32] {
                let px = render(look, size);
                assert_eq!(px.len(), (size * size * 4) as usize);
                assert_eq!(px[3], 0, "top-left is transparent");
                // The middle of the lifted cloud is opaque (faded when off).
                let mid = ((size * 3 / 8) * size + size / 2) as usize * 4;
                assert_eq!(px[mid + 3], if look == Look::Off { 140 } else { 255 }, "{look:?} {size}");
            }
        }
        assert_ne!(render(Look::UpToDate, 16), render(Look::Paused, 16));
    }

    #[test]
    fn instance_names_ignore_case_and_a_trailing_backslash() {
        let a = instance_name(std::path::Path::new(r"C:\Users\x\AppData\Local\OneUpDrive-test"));
        assert_eq!(a, instance_name(std::path::Path::new(r"c:\users\X\appdata\local\oneupdrive-test\")));
        assert_ne!(a, instance_name(std::path::Path::new(r"C:\Users\x\AppData\Local\OneUpDrive")));
        // Beyond ASCII too, as Windows compares names: a word-final sigma
        // is a sigma; the Kelvin sign isn't k.
        let greek = instance_name(std::path::Path::new(r"D:\Data\ΟΔΟΣ"));
        assert_eq!(greek, instance_name(std::path::Path::new(r"d:\data\οδος\")));
        assert_eq!(greek, instance_name(std::path::Path::new(r"D:\DATA\οδοσ")));
        assert_ne!(
            instance_name(std::path::Path::new("D:\\\u{212A}it")),
            instance_name(std::path::Path::new(r"D:\kit"))
        );
    }

    #[test]
    fn long_tips_are_cut() {
        let mut s = status(Activity::Retrying);
        s.problem = Some(Problem { title: "x".repeat(500), ..problem("other", 1) });
        assert!(tip(&up(s)).chars().count() <= 127);
    }
}
