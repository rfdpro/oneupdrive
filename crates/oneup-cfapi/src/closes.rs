//! Which programs recently closed which placeholders, from Windows' close
//! notifications (`CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION`, which
//! names the process). The change journal doesn't say who wrote a file, and
//! Restart Manager only finds a writer that still has the file open, so the
//! program tracker (`oneup_app::apps`) asks here for one that closed it
//! already: `cmd /c echo >>`, or a save that takes milliseconds.
//!
//! Windows sends these only for placeholders opened with data or delete
//! access, so a new file that isn't one yet has no record here.
//!
//! In memory, for this process only (the agent runs both), bounded: at most
//! [`MAX`] files and [`PER_FILE`] closes each, each for [`FOR`].

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a close is remembered: the tracker looks within seconds.
pub const FOR: Duration = Duration::from_secs(60);
/// Files remembered at most; the oldest go first.
pub const MAX: usize = 8192;
/// Closes remembered per file, the latest.
pub const PER_FILE: usize = 4;

/// A program closed a placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Close {
    pub at: Instant,
    pub pid: u32,
    /// Its executable's file name (`WINWORD.EXE`).
    pub exe: String,
}

static CLOSES: Mutex<Option<HashMap<u128, Vec<Close>>>> = Mutex::new(None);

/// The program `pid` (executable path or name `image`) closed the file with
/// this ID (an NTFS file reference, as the Local tree stores it).
pub fn note(file_id: u128, pid: u32, image: &str) {
    let exe = image.rsplit(['\\', '/']).next().unwrap_or_default();
    if exe.is_empty() {
        return;
    }
    let mut g = CLOSES.lock().unwrap_or_else(|p| p.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    let now = Instant::now();
    if m.len() >= MAX && !m.contains_key(&file_id) {
        m.retain(|_, c| c.last().is_some_and(|c| now.duration_since(c.at) < FOR));
        if m.len() >= MAX
            && let Some(oldest) = m.iter().min_by_key(|(_, c)| c.last().map(|c| c.at)).map(|(id, _)| *id)
        {
            m.remove(&oldest);
        }
    }
    let list = m.entry(file_id).or_default();
    // A program closing many handles in a row is one close.
    if let Some(last) = list.last_mut()
        && last.pid == pid
    {
        last.at = now;
        return;
    }
    if list.len() >= PER_FILE {
        list.remove(0);
    }
    list.push(Close { at: now, pid, exe: exe.to_string() });
}

/// The programs that closed the file at `since` or later, oldest first.
pub fn since(file_id: u128, since: Instant) -> Vec<Close> {
    let g = CLOSES.lock().unwrap_or_else(|p| p.into_inner());
    g.as_ref()
        .and_then(|m| m.get(&file_id))
        .map(|c| c.iter().filter(|c| c.at >= since && c.at.elapsed() < FOR).cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::{Duration, Instant};

    #[test]
    fn closes_are_kept_per_file_latest_first_and_bounded() {
        let start = Instant::now() - Duration::from_millis(1);
        let id = u128::MAX - 7;
        super::note(id, 10, r"C:\Windows\System32\cmd.exe");
        super::note(id, 10, r"C:\Windows\System32\cmd.exe");
        super::note(id, 11, "WINWORD.EXE");
        super::note(id, 12, "");
        let got: Vec<String> = super::since(id, start).into_iter().map(|c| c.exe).collect();
        assert_eq!(got, ["cmd.exe", "WINWORD.EXE"]);
        assert!(super::since(id, Instant::now() + Duration::from_secs(1)).is_empty());
        for pid in 0..10 {
            super::note(id, pid + 100, "a.exe");
        }
        assert_eq!(super::since(id, start).len(), super::PER_FILE);
        for i in 0..(super::MAX as u128 + 10) {
            super::note(i, 1, "b.exe");
        }
        let n = super::CLOSES.lock().unwrap().as_ref().map_or(0, |m| m.len());
        assert!(n <= super::MAX, "{n}");
    }
}
