//! `oneup duplicates`: files with the same content, hashing files only on
//! this PC on request, and removing the copies you choose
//! (`oneup_app::duplicates`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use anyhow::{Result, bail};
use oneup_app::duplicates::{self, Page};
use oneup_app::versions::size;
use oneup_store::Store;

/// At most this much is hashed in one `--hash-local` run.
const HASH_MAX_BYTES: u64 = 20 << 30;

/// `1830` → `1,830`.
fn count(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

fn bytes(n: i64) -> String {
    size(u64::try_from(n).unwrap_or(0))
}

/// Lists a page of groups.
pub fn list(dir: &Path, store: &Store, json: bool, limit: usize, after: Option<&str>) -> Result<()> {
    let after = after.map(duplicates::parse_cursor).transpose()?;
    let page = duplicates::list(dir, store, after, limit.clamp(1, 1000))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&duplicates::page_json(&page))?);
        return Ok(());
    }
    show(&page);
    if let Some(w) = store.read(duplicates::queued)? {
        println!(
            "Removing {} copies waits for your approval (`oneup status`); `oneup duplicates cancel` drops it.",
            w.copies.len()
        );
    }
    Ok(())
}

fn show(page: &Page) {
    let s = &page.summary;
    if s.groups == 0 {
        println!("No duplicates found (files of 1 MiB or more). {}", duplicates::EXCLUDED_NOTE);
        return;
    }
    println!("Duplicates: {} in {} files ({} groups).", bytes(s.bytes), count(s.files), count(s.groups));
    println!("Files of 1 MiB or more. {}", duplicates::EXCLUDED_NOTE);
    for g in &page.groups {
        println!();
        let note = if g.labels.is_empty() { String::new() } else { format!("  [{}]", labels(&g.labels)) };
        println!("{} copies of {}, {} to save{note}", g.group.copies, bytes(g.group.size), bytes(g.group.saved()));
        for c in &g.copies {
            let labels = if c.labels.is_empty() { String::new() } else { format!("  [{}]", labels(&c.labels)) };
            println!("  {}  ({}){labels}", c.path, c.place.label());
        }
        let more = g.group.copies - g.copies.len() as i64;
        if more > 0 {
            println!("  and {more} more");
        }
    }
    if let Some(next) = &page.next {
        println!();
        println!("More: oneup duplicates --after {next}");
    }
}

/// `--hash-local`: at background priority, until done, the limits, a
/// pause or Ctrl+C.
pub async fn hash_local(dir: &Path, mut store: Store, max_files: usize) -> Result<()> {
    if let Err(e) = oneup_sys::priority::set_background(true) {
        tracing::debug!(error = %e, "background priority not set");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            flag.store(true, Relaxed);
        }
    });
    println!("Hashing files on this PC that aren't in OneDrive (Ctrl+C stops; a later run continues)…");
    let d = dir.to_path_buf();
    let done = tokio::task::spawn_blocking(move || {
        let stopped = || stop.load(Relaxed) || oneup_app::control::paused_in(&d, oneup_app::control::now_ms());
        duplicates::hash_local(&mut store, max_files.max(1), HASH_MAX_BYTES, &stopped)
    })
    .await??;
    let unreadable = if done.unreadable > 0 {
        format!(" {} couldn't be read (open elsewhere, or changed).", done.unreadable)
    } else {
        String::new()
    };
    println!("Hashed {} files ({}).{unreadable}", count(done.files as i64), size(done.bytes));
    if done.full {
        println!("No more room for hashes of files only here; remove some duplicates first.");
    } else if done.finished {
        println!("Every such file has been looked at. `oneup duplicates` lists what's the same.");
    } else if done.stopped {
        println!("Stopped (sync is paused, or you pressed Ctrl+C). Run it again to continue.");
    } else {
        println!("More to hash: run it again to continue.");
    }
    Ok(())
}

/// What `remove` was asked.
pub struct Remove<'a> {
    pub paths: &'a [String],
    pub yes: bool,
    pub expect: Option<&'a str>,
}

/// `remove`: the preview, or with `--yes --expect CODE` the queued choice.
pub async fn remove(dir: &Path, mut store: Store, a: Remove<'_>) -> Result<()> {
    if a.paths.is_empty() {
        bail!("name the copies to remove as `oneup duplicates` lists them");
    }
    let mut chosen = Vec::new();
    for p in a.paths {
        match duplicates::find(dir, &store, p)? {
            Some((_, c)) => chosen.push(c),
            None => bail!("{p} isn't a copy `oneup duplicates` lists"),
        }
    }
    if !a.yes {
        let p = duplicates::prepare(dir, &mut store, &chosen)?;
        if p.lines.is_empty() {
            println!("Nothing to remove.");
        } else {
            println!("Would remove {} copies ({}):", p.lines.len(), bytes(p.bytes));
            for l in &p.lines {
                println!("  {}  → {}", l.path, l.to);
            }
        }
        for (path, why) in &p.left {
            println!("Left: {path} ({why})");
        }
        if !p.lines.is_empty() {
            println!("Add --yes --expect {} to remove exactly these. OneUpDrive then asks you to approve it.", p.code);
        }
        return Ok(());
    }
    let Some(code) = a.expect else { bail!("--yes needs --expect CODE, from the preview (run it without --yes)") };
    let p = duplicates::ask(dir, &mut store, &chosen, code)?;
    println!(
        "Queued: remove {} copies ({}). Approve it with `oneup approve {}` once `oneup status` shows it.",
        p.lines.len(),
        bytes(p.bytes),
        p.code
    );
    if !crate::agent::nudge(dir).await {
        println!("OneUpDrive isn't running; it asks when it starts (`oneup start`).");
    }
    Ok(())
}

/// `cancel`.
pub async fn cancel(dir: &Path, mut store: Store) -> Result<()> {
    if duplicates::cancel(&mut store)? {
        println!("Dropped the removal waiting for approval.");
        crate::agent::nudge(dir).await;
    } else {
        println!("No removal waits.");
    }
    Ok(())
}

fn labels(l: &[duplicates::Label]) -> String {
    l.iter().map(duplicates::Label::english).collect::<Vec<_>>().join("; ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_have_thousands_separators() {
        assert_eq!(super::count(1830), "1,830");
        assert_eq!(super::count(999), "999");
        assert_eq!(super::count(1_000_000), "1,000,000");
    }
}
