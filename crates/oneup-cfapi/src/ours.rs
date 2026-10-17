//! Files OneUpDrive itself just wrote: downloads it placed, placeholders it
//! filled or freed. The change journal doesn't say who wrote a file, so the
//! program tracker (`oneup_app::apps`) asks Windows who has it open, and
//! found nobody for OneUpDrive's own writes: 4,084 "not identified"
//! changes (204 GiB) in a week were measured to be almost all ours. The
//! writers note the file here; the tracker skips it.
//!
//! In memory, for this process only (the agent runs both), bounded: at
//! most [`MAX`] files, each for [`FOR`].

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::recent::Recent;

/// How long a write stays noted: the tracker looks within seconds.
pub const FOR: Duration = Duration::from_secs(120);
/// Files noted at most; the oldest go first.
pub const MAX: usize = 8192;

static NOTED: Mutex<Option<Recent<()>>> = Mutex::new(None);

/// OneUpDrive is writing the file with this ID (an NTFS file reference,
/// as the Local tree stores it).
pub fn note(file_id: u128) {
    let mut g = NOTED.lock().unwrap_or_else(|p| p.into_inner());
    g.get_or_insert_with(|| Recent::new(MAX, FOR)).touch(file_id, Instant::now());
}

/// Whether OneUpDrive wrote the file recently.
pub fn wrote(file_id: u128) -> bool {
    let g = NOTED.lock().unwrap_or_else(|p| p.into_inner());
    g.as_ref().and_then(|m| m.get(file_id)).is_some_and(|(t, _)| t.elapsed() < FOR)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn noted_files_are_ours_and_the_record_stays_bounded() {
        assert!(!super::wrote(u128::MAX));
        super::note(u128::MAX);
        assert!(super::wrote(u128::MAX));
        for i in 0..(super::MAX as u128 + 10) {
            super::note(i);
        }
        let n = super::NOTED.lock().unwrap().as_ref().map_or(0, |m| m.len());
        assert!(n <= super::MAX, "{n}");
    }
}
