//! What `oneup-tray` shows: the icon's picture and tooltip, its menu, and
//! which changes deserve a notification. Kept free of Windows calls so it
//! can be tested; the executable (`bin/oneup-tray.rs`) wires it to
//! `oneup_sys::tray` and the agent's control pipe.

use std::collections::HashSet;

use oneup_ipc::{Activity, Problem, Status};
use oneup_sys::tray::{Glyph, Item, Kind};

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
        View::Connecting => return "Connecting…".into(),
        View::NotRunning => return "Not running".into(),
        View::Stopped(p) => return format!("Stopped: {}", p.title),
        View::Unreachable(e) => return format!("Can't connect: {e}"),
        View::Running(s) => s,
    };
    let waiting = s.safety_stops.len() + usize::from(s.overflow.as_ref().is_some_and(|o| o.waiting));
    if waiting > 0 {
        return if waiting == 1 { "Needs your review".into() } else { format!("{waiting} need your review") };
    }
    if let Some(why) = &s.auto_pause {
        return format!("Paused: {why}");
    }
    if let Some(p) = working(s) {
        let pass = if p.round > 1 { format!(" (pass {})", p.round) } else { String::new() };
        let percent = p.items_done * 100 / p.items_total.max(1);
        return format!("Syncing {} of {} items ({percent}%){pass}", count(p.items_done), count(p.items_total));
    }
    match s.activity {
        Activity::Starting => "Checking all folders…".into(),
        Activity::Syncing => "Syncing…".into(),
        Activity::Paused => "Paused".into(),
        Activity::Stopping => "Stopping…".into(),
        Activity::WaitingForYou => "Needs your review".into(),
        Activity::Retrying => match &s.problem {
            Some(p) => format!("Problem: {}", p.title),
            None => "Retrying soon".into(),
        },
        // OneDrive full: up to date otherwise, but uploads wait.
        Activity::Idle if s.problem.is_some() => {
            format!("Problem: {}", s.problem.as_ref().map_or("", |p| p.title.as_str()))
        }
        Activity::Idle => match (s.paused.local, s.paused.remote) {
            (true, false) => "Paused: changes to this PC".into(),
            (false, true) => "Paused: changes to OneDrive".into(),
            _ => match &s.unsynced {
                Some(u) if failures => format!("Up to date · {} failed", unsynced_count(u)),
                _ => "Up to date".into(),
            },
        },
    }
}

/// Problems that usually clear by themselves (the connection, OneDrive's
/// service) are notified only once this many tries in a row met them
/// (about 15 minutes, with the agent's growing pauses); the others at once.
const PATIENT_ATTEMPTS: u32 = 4;

/// The notification for a problem: what it is and what to do.
fn problem_notice(title: &str, p: &Problem) -> Notice {
    Notice { title: title.into(), text: format!("{} {}", p.title, p.action), kind: Kind::Error }
}

/// `12 items`, `1,000 items or more`.
fn unsynced_count(u: &oneup_ipc::Unsynced) -> String {
    let n = if u.count == 1 { "1 item".to_string() } else { format!("{} items", count(u.count as u64)) };
    if u.more { format!("{n} or more") } else { n }
}

/// The round being carried out, when that's what the agent is doing (not
/// when it waits for the user or is paused).
fn working(s: &Status) -> Option<&oneup_ipc::Progress> {
    let busy = matches!(s.activity, Activity::Starting | Activity::Syncing) && s.auto_pause.is_none();
    s.progress.as_ref().filter(|p| busy && p.items_total > 0)
}

/// `12345` as `12,345`.
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A time left, roughly: "about 25 min", "about 2 h 10 min".
fn time_left(ms: u64) -> String {
    let min = ms.div_ceil(60_000);
    match min {
        0 | 1 => "less than a minute".into(),
        m if m < 60 => format!("about {m} min"),
        m if m % 60 == 0 || m >= 600 => format!("about {} h", (m + 30) / 60),
        m => format!("about {} h {} min", m / 60, m % 60),
    }
}

/// The second line of the tooltip while a round runs: the bytes moved, if
/// it transfers anything, and the time left, once it can be told.
fn progress_detail(p: &oneup_ipc::Progress) -> Option<String> {
    let size = crate::versions::size;
    let mut parts = Vec::new();
    if p.bytes_total > 0 {
        parts.push(format!("{} of {}", size(p.bytes_done), size(p.bytes_total)));
    }
    match p.eta_ms() {
        Some(ms) => parts.push(format!("{} left", time_left(ms))),
        None if p.items_done < p.items_total => parts.push("estimating time left".into()),
        None => {}
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A round's progress on one line (`oneup status`).
pub fn progress_line(p: &oneup_ipc::Progress) -> String {
    let percent = p.items_done * 100 / p.items_total.max(1);
    let head = format!("round {}: {} of {} items ({percent}%)", p.round, count(p.items_done), count(p.items_total));
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
    pub const ACTIVITY: u32 = 50;
    pub const PROBLEM: u32 = 51;
    pub const START: u32 = 60;
    pub const STOP: u32 = 61;
    pub const CLOSE: u32 = 70;
    pub const SETTINGS: u32 = 80;
    pub const UPDATE: u32 = 90;
    /// The status line: opens the settings window, where it's told at length.
    pub const STATUS: u32 = 1;
    /// A line that only tells.
    pub const NOTE: u32 = 4;
}

/// The pause choices: label, minutes (`None`: until resumed).
pub const PAUSES: &[(&str, Option<u64>)] =
    &[("30 minutes", Some(30)), ("2 hours", Some(120)), ("8 hours", Some(480)), ("Until I resume", None)];

/// The menu's pictures: symbols from Windows' icon font.
mod glyph {
    use oneup_sys::tray::Glyph;

    pub const FOLDER: Glyph = Glyph::Symbol('\u{E838}');
    pub const SYNC: Glyph = Glyph::Symbol('\u{E895}');
    pub const PAUSE: Glyph = Glyph::Symbol('\u{E769}');
    pub const PLAY: Glyph = Glyph::Symbol('\u{E768}');
    pub const HISTORY: Glyph = Glyph::Symbol('\u{E81C}');
    pub const SETTINGS: Glyph = Glyph::Symbol('\u{E713}');
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

/// The notification for a new safety stop.
fn stop_notice(st: &oneup_ipc::SafetyStop) -> Notice {
    if st.kind == "content" {
        return Notice {
            title: "Files may be encrypted".into(),
            text: format!(
                "{} changed files look encrypted, possibly by ransomware. They weren't uploaded. Check them before continuing.",
                st.count
            ),
            kind: Kind::Warning,
        };
    }
    let side = if st.side == "local" { "on this PC" } else { "in OneDrive" };
    Notice {
        title: "Review needed".into(),
        text: format!(
            "{} items {side} would be deleted, overwritten or moved. Nothing changes until you review.",
            st.count
        ),
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
                    format!("Possibly encrypted: {} files…", st.count)
                } else {
                    let side = if st.side == "local" { "on this PC" } else { "in OneDrive" };
                    format!("Review {} changes {side}…", st.count)
                };
                items.push(action(id::REVIEW_STOP + i as u32, label, glyph::WARNING));
            }
            if let Some(o) = &s.overflow
                && o.waiting
            {
                let label = format!("Review {} removed files…", o.count);
                items.push(action(id::REVIEW_OVERFLOW, label, glyph::WARNING));
            }
            if let Some(o) = &s.prune_offer {
                let sub = vec![
                    Item::Action {
                        id: id::REVIEW_PRUNE, label: "Remove or keep…".into(), enabled: true, glyph: None
                    },
                    Item::Action { id: id::LIST_PRUNE, label: "Show list".into(), enabled: true, glyph: None },
                ];
                items.push(Item::Submenu {
                    label: format!("Excluded, in OneDrive: {}", o.items),
                    items: sub,
                    glyph: Some(glyph::CLOUD),
                });
            }
            if s.problem.is_some() {
                items.push(action(id::PROBLEM, "Error details…", glyph::ERROR));
            }
            if let Some(u) = &s.unsynced {
                let label = format!("Sync failure: {}", unsynced_count(u));
                items.push(action(id::LIST_UNSYNCED, label, glyph::INFO));
            }
            if items.len() > waiting {
                items.push(Item::Separator);
            }
            items.push(action(id::OPEN_FOLDER, "Open folder", glyph::FOLDER));
            items.push(action(id::SYNC_NOW, "Sync now", glyph::SYNC));
            if s.paused.local || s.paused.remote {
                items.push(action(id::RESUME, "Resume", glyph::PLAY));
            }
            if !(s.paused.local && s.paused.remote) {
                let choices = PAUSES
                    .iter()
                    .enumerate()
                    .map(|(i, (label, _))| Item::Action {
                        id: id::PAUSE + i as u32,
                        label: (*label).into(),
                        enabled: true,
                        glyph: None,
                    })
                    .collect();
                items.push(Item::Submenu { label: "Pause".into(), items: choices, glyph: Some(glyph::PAUSE) });
            }
        }
        View::NotRunning => items.push(action(id::START, "Start OneUpDrive", glyph::PLAY)),
        View::Stopped(_) => {
            items.push(action(id::PROBLEM, "Error details…", glyph::ERROR));
            items.push(Item::Separator);
            items.push(action(id::START, "Start OneUpDrive", glyph::PLAY));
        }
        View::Connecting | View::Unreachable(_) => {}
    }
    if !matches!(items.last(), Some(Item::Separator)) {
        items.push(Item::Separator);
    }
    items.push(action(id::ACTIVITY, "Recent activity", glyph::HISTORY));
    items.push(action(id::SETTINGS, "Settings", glyph::SETTINGS));
    items.push(Item::Separator);
    if let Some(version) = update {
        items.push(action(id::UPDATE, format!("Install update {version}"), glyph::NEW));
    }
    if matches!(v, View::Running(_)) {
        items.push(action(id::STOP, "Stop OneUpDrive", glyph::POWER));
    }
    items.push(action(id::CLOSE, "Hide icon", glyph::CLOSE));
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
                        out.push(stop_notice(st));
                    }
                }
                if let Some(o) = &s.prune_offer
                    && self.shown.insert(o.code.clone())
                {
                    out.push(Notice {
                        title: "Excluded items in OneDrive".into(),
                        text: format!(
                            "{} excluded items are still in OneDrive. Click to remove or keep them.",
                            o.items
                        ),
                        kind: Kind::Info,
                    });
                }
                if let Some(o) = &s.overflow
                    && o.waiting
                    && self.shown.insert(o.code.clone())
                {
                    out.push(Notice {
                        title: "Review needed".into(),
                        text: format!(
                            "{} files deleted in OneDrive don't fit in the version store. They stay here until you decide.",
                            o.count
                        ),
                        kind: Kind::Warning,
                    });
                }
                if let Some(p) = &s.problem {
                    let patient = matches!(p.kind.as_str(), "offline" | "onedrive" | "other");
                    if (!patient || p.attempts >= PATIENT_ATTEMPTS)
                        && self.shown.insert(format!("problem:{}:{}", p.kind, p.since_ms))
                    {
                        out.push(problem_notice("Sync problem", p));
                    }
                }
            }
            View::NotRunning | View::Stopped(_) => {
                if self.was_running && !self.stop_expected {
                    out.push(match v {
                        View::Stopped(p) => problem_notice("OneUpDrive stopped", p),
                        _ => Notice {
                            title: "OneUpDrive stopped".into(),
                            text: "Files aren't syncing. Start it from this icon's menu.".into(),
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
pub fn instance_name(data_dir: &std::path::Path) -> String {
    // FNV-1a over the lower-cased path: the same folder, however it's typed.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data_dir.to_string_lossy().to_lowercase().trim_end_matches('\\').bytes() {
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
            prune_offer: None,
            progress: None,
            unsynced: None,
            retrying: None,
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
        assert_eq!(count(1_234_567), "1,234,567");
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
    }

    #[test]
    fn long_tips_are_cut() {
        let mut s = status(Activity::Retrying);
        s.problem = Some(Problem { title: "x".repeat(500), ..problem("other", 1) });
        assert!(tip(&up(s)).chars().count() <= 127);
    }
}
