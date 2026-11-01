//! The agent's event feed (`docs/architecture/hooks.md`): what happened,
//! as events for `oneup watch`, scripts (the control pipe's `events`
//! call) and hooks (`crate::hooks`).
//!
//! Item events come from the activity log's entries for a round (the same
//! lines, so the feed says nothing the log doesn't); the others from the
//! agent's control state (`crate::control`): pauses, problems, safety
//! stops. Kept in memory only, the latest [`KEEP`]: a reader that falls
//! further behind is told it missed some. Hooks get every event as it's
//! added, so they miss none.
//!
//! Events never hold file contents, tokens or approval codes; links in a
//! detail (an error that quotes one) are left out.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use oneup_ipc::{Event, EventKind, Events};
use oneup_store::{ActivityEntry, ActivityOutcome};

/// Events kept for readers.
pub const KEEP: usize = 4096;
/// A page must fit in one reply line (`oneup_ipc::MAX_LINE`), with room.
const PAGE_BYTES: usize = 48 * 1024;
/// An event's detail, at most (characters).
const MAX_DETAIL: usize = 300;

/// Something that hears of every event as it's added (the hooks).
pub type Sink = Arc<dyn Fn(&[Event]) + Send + Sync>;

/// The feed.
pub struct Feed {
    ring: Mutex<Ring>,
    newest: tokio::sync::watch::Sender<u64>,
    sinks: Mutex<Vec<Sink>>,
}

#[derive(Default)]
struct Ring {
    items: VecDeque<Event>,
    last: u64,
}

impl Default for Feed {
    fn default() -> Self {
        Self::new()
    }
}

/// An event before it has its number and time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct New {
    pub kind: EventKind,
    pub path: String,
    pub action: String,
    pub detail: String,
}

impl New {
    pub fn about_sync(kind: EventKind, detail: impl Into<String>) -> Self {
        Self { kind, path: String::new(), action: String::new(), detail: detail.into() }
    }
}

/// `detail` without links (an error may quote a download link, which
/// works as a credential for a while), and short.
fn clean(detail: &str) -> String {
    // Control characters (NUL included) become spaces and `"` becomes `'`,
    // so a detail can't break a log line, a JSON consumer or an environment
    // block; any word with a link is left out.
    let plain: String = detail
        .chars()
        .map(|c| {
            if c.is_control() {
                ' '
            } else if c == '"' {
                '\''
            } else {
                c
            }
        })
        .collect();
    let mut out = String::with_capacity(plain.len().min(MAX_DETAIL + 8));
    for (i, word) in plain.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let lower = word.to_ascii_lowercase();
        if lower.contains("http://") || lower.contains("https://") {
            out.push_str("(link)");
        } else {
            out.push_str(word);
        }
    }
    // Cut by characters, never bytes, so a multi-byte character at the limit
    // can't panic.
    if out.chars().count() > MAX_DETAIL {
        out = out.chars().take(MAX_DETAIL).collect::<String>() + "…";
    }
    out
}

impl Feed {
    pub fn new() -> Self {
        Self { ring: Mutex::default(), newest: tokio::sync::watch::Sender::new(0), sinks: Mutex::default() }
    }

    /// The newest event's number, as it changes.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.newest.subscribe()
    }

    /// `sink` hears of every event added from now on.
    pub fn add_sink(&self, sink: Sink) {
        self.sinks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(sink);
    }

    /// Adds events, numbered in order, now. Sinks hear of them before this
    /// returns; they only queue, so this never waits on a hook.
    pub fn push(&self, new: Vec<New>) {
        if new.is_empty() {
            return;
        }
        let at_ms = crate::control::now_ms();
        let added: Vec<Event> = {
            let mut r = self.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let first = r.last + 1;
            let added: Vec<Event> = new
                .into_iter()
                .zip(first..)
                .map(|(n, seq)| Event {
                    seq,
                    at_ms,
                    kind: n.kind,
                    path: n.path,
                    action: n.action,
                    detail: clean(&n.detail),
                })
                .collect();
            r.last += added.len() as u64;
            // Only the latest are kept; a big round adds its newest.
            let skip = added.len().saturating_sub(KEEP);
            r.items.extend(added[skip..].iter().cloned());
            while r.items.len() > KEEP {
                r.items.pop_front();
            }
            added
        };
        let last = added.last().map_or(0, |e| e.seq);
        let sinks = self.sinks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        for s in sinks {
            s(&added);
        }
        self.newest.send_replace(last);
    }

    /// The events after `since`, as many as fit in a reply (the reader asks
    /// again from `next`); `None`: none, only where the feed is.
    pub fn page(&self, since: Option<u64>) -> Events {
        let r = self.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(since) = since else { return Events { next: r.last, missed: false, items: Vec::new() } };
        // A number from before a restart is past the end: start over.
        let restarted = since > r.last;
        let since = if restarted { 0 } else { since };
        let oldest = r.items.front().map_or(r.last + 1, |e| e.seq);
        let missed = restarted || since + 1 < oldest;
        let mut items = Vec::new();
        let mut bytes = 0;
        for e in r.items.iter().filter(|e| e.seq > since) {
            bytes += 96 + e.path.len() + e.action.len() + e.detail.len() * 2;
            if bytes > PAGE_BYTES && !items.is_empty() {
                break;
            }
            items.push(e.clone());
        }
        let next = items.last().map_or(if restarted { r.last } else { since.max(oldest.saturating_sub(1)) }, |e| e.seq);
        Events { next, missed, items }
    }
}

/// The destination of a move or rename, from its activity-log line
/// (`-> /new/path; note`, or `-> new name` for a rename in place).
fn moved_to(path: &str, detail: &str) -> Option<String> {
    let to = detail.strip_prefix("-> ")?;
    let to = to.split("; ").next().unwrap_or(to);
    if to.starts_with('/') {
        return Some(to.to_string());
    }
    let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
    Some(format!("{parent}/{to}"))
}

/// What sync did to items, as the activity log has it, as events.
pub fn from_activity(entries: &[ActivityEntry]) -> Vec<New> {
    const SYNCED: [&str; 9] =
        ["upload", "download", "create", "move", "rename", "delete", "recycle", "remove", "prune"];
    entries
        .iter()
        .filter(|e| !e.path.is_empty())
        .filter_map(|e| {
            let kind = match e.outcome {
                ActivityOutcome::Failed => EventKind::Failure,
                ActivityOutcome::Done if e.action == "keep both" => EventKind::Conflict,
                ActivityOutcome::Done if e.detail.contains(crate::sync::KEPT_THEIRS) => EventKind::Conflict,
                ActivityOutcome::Done if SYNCED.contains(&e.action.as_str()) => EventKind::Synced,
                _ => return None,
            };
            let path = match e.action.as_str() {
                "move" | "rename" if kind == EventKind::Synced => {
                    moved_to(&e.path, &e.detail).unwrap_or_else(|| e.path.clone())
                }
                _ => e.path.clone(),
            };
            Some(New { kind, path, action: e.action.clone(), detail: e.detail.clone() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn entry(outcome: ActivityOutcome, action: &str, path: &str, detail: &str) -> ActivityEntry {
        ActivityEntry { seq: 0, at_ms: 0, outcome, action: action.into(), path: path.into(), detail: detail.into() }
    }

    #[test]
    fn activity_lines_become_events_for_what_people_care_about() {
        use ActivityOutcome::*;
        let kept = format!("{}; that version is kept as a (2).txt", crate::sync::KEPT_THEIRS);
        let log = [
            entry(Done, "upload", "/a.txt", ""),
            entry(Done, "move", "/a.txt", "-> /docs/a.txt; in OneDrive"),
            entry(Done, "rename", "/docs/b.txt", "-> c.txt; freeing the name"),
            entry(Done, "keep both", "/d.txt", "-> d (PC).txt; changed on both sides"),
            entry(Done, "upload", "/e.txt", &kept),
            entry(Failed, "download", "/f.txt", "the disk is full"),
            entry(Skipped, "download", "/g.txt", "open in another program"),
            entry(Done, "in sync", "/h.txt", ""),
            entry(Waiting, "paused", "", "3 operations wait"),
            entry(Failed, "stopped", "", "halted"),
        ];
        let events = from_activity(&log);
        let got: Vec<(EventKind, &str)> = events.iter().map(|n| (n.kind, n.path.as_str())).collect();
        assert_eq!(
            got,
            [
                (EventKind::Synced, "/a.txt"),
                (EventKind::Synced, "/docs/a.txt"),
                (EventKind::Synced, "/docs/c.txt"),
                (EventKind::Conflict, "/d.txt"),
                (EventKind::Conflict, "/e.txt"),
                (EventKind::Failure, "/f.txt"),
            ]
        );
    }

    fn synced(n: usize) -> Vec<New> {
        (0..n)
            .map(|i| New {
                kind: EventKind::Synced,
                path: format!("/{i}.txt"),
                action: "upload".into(),
                detail: String::new(),
            })
            .collect()
    }

    #[test]
    fn readers_page_through_and_hear_what_they_missed() {
        let f = Feed::new();
        assert_eq!(f.page(None), Events { next: 0, missed: false, items: Vec::new() });
        f.push(synced(3));
        let p = f.page(Some(0));
        assert_eq!((p.next, p.items.len(), p.missed), (3, 3, false));
        assert_eq!(f.page(Some(3)).items.len(), 0);
        assert_eq!(f.page(Some(3)).next, 3);
        // A burst bigger than what's kept: the reader is told.
        f.push(synced(KEEP + 10));
        let p = f.page(Some(3));
        assert!(p.missed);
        assert_eq!(p.items[0].seq, 3 + 10 + 1);
        // Pages fit in a reply; the next one carries on.
        let size = serde_json::to_vec(&p).unwrap().len();
        assert!(size < 60 * 1024, "{size}");
        let q = f.page(Some(p.next));
        assert!(!q.missed && q.items[0].seq == p.next + 1);
        // A number from before a restart.
        let r = f.page(Some(1_000_000));
        assert!(r.missed && r.items[0].seq == f.page(None).next - KEEP as u64 + 1);
    }

    #[test]
    fn details_are_cleaned_and_cut_by_characters() {
        // A multi-byte character straddling the limit doesn't panic.
        let long = format!("{}€€€", "a".repeat(MAX_DETAIL - 1));
        let c = clean(&long);
        assert!(c.ends_with('…') && c.chars().count() == MAX_DETAIL + 1, "{c}");
        assert_eq!(clean("a\u{0}b\r\nc\"d\""), "a b  c'd'");
        assert_eq!(clean("see HTTPS://x.example/a?k=1 now"), "see (link) now");
    }

    #[test]
    fn sinks_hear_every_event_and_links_are_left_out() {
        let f = Feed::new();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let h = Arc::clone(&heard);
        f.add_sink(Arc::new(move |e: &[Event]| h.lock().unwrap().extend(e.iter().map(|e| e.seq))));
        f.push(synced(KEEP + 5));
        assert_eq!(heard.lock().unwrap().len(), KEEP + 5, "hooks miss nothing");
        f.push(vec![New::about_sync(EventKind::Problem, "failed: GET https://x.example/a?tempauth=SECRET timed out")]);
        let last = f.page(Some(KEEP as u64 + 5)).items.pop().unwrap();
        assert_eq!(last.detail, "failed: GET (link) timed out");
    }
}
