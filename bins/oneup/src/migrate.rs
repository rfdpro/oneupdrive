//! `oneup migrate check`: the read-only preflight for moving over from the
//! OneDrive app, printed (the work is `oneup_app::moveover`, which the
//! setup window's Move over path uses too). `oneup migrate finish-unlink`:
//! getting the app's stalled unlinking finished.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use oneup_app::moveover::{self, Check, Step};
use oneup_store::Store;
use serde_json::json;

use crate::{Cli, MemoryProfile, MirrorArgs, ProfileArg, ScanArgs};

pub fn check(data_dir: &std::path::Path, store: &Store, as_json: bool) -> Result<()> {
    let Some(c) = moveover::check(data_dir, store)? else {
        if as_json {
            println!("null");
        } else {
            println!("No OneDrive app folder found for this Windows account: nothing to move over.");
        }
        return Ok(());
    };
    if as_json {
        println!("{}", c.to_json());
        return Ok(());
    }
    print(&c);
    Ok(())
}

/// " 3 hours ago", from a time in ms since 1970.
fn since(at_ms: Option<i64>) -> String {
    let Some(at) = at_ms else { return String::new() };
    let mins = (oneup_app::sync::now_ms() - at) / 60_000;
    match mins {
        i64::MIN..=1 => String::new(),
        2..=119 => format!(" {mins} minutes ago"),
        _ => format!(" {} hours ago", mins / 60),
    }
}

/// `oneup migrate finish-unlink`: runs `steps` in order until the app has
/// released its folder (see `oneup_app::moveover` § a stalled unlinking).
/// Once released (by the app or by OneUpDrive), it also releases what the
/// release left behind in folders some program had open.
pub async fn finish_unlink(
    cli: &Cli,
    steps: &[Step],
    yes: bool,
    wait_minutes: u64,
    as_json: bool,
    profile: MemoryProfile,
) -> Result<()> {
    let dir = crate::data_dir(cli)?;
    let wait = Duration::from_secs(wait_minutes.max(1) * 60);
    let done = |released: bool, by: Option<&str>, message: String| {
        if as_json {
            println!("{}", json!({ "released": released, "by": by, "message": message }));
        } else {
            println!("{message}");
        }
        Ok(())
    };
    let Some(app) = moveover::app_in(&dir) else {
        return done(true, None, "No OneDrive app folder on this PC: nothing to finish.".into());
    };
    let mut steps = steps.to_vec();
    if app.linked.is_none() {
        // Released already: only what the release may have left behind.
        say("Looking through the folder for anything the unlinking left behind (names only)…");
        rescan(cli, &app.folder, profile).await?;
        let left = left_behind(cli, &dir, profile)?;
        if left == 0 {
            return done(
                true,
                None,
                format!("The OneDrive app no longer holds {}, and nothing was left behind.", app.folder.display()),
            );
        }
        say(&format!(
            "The unlinking left {left} placeholders behind (Windows skips any folder a program has open meanwhile)."
        ));
        steps = vec![Step::Release];
    } else if !app.stalled() {
        bail!(
            "the OneDrive app still has its account for {}: unlink it in the app first (its icon in the corner of the \
             taskbar > Settings > Account > Unlink this PC), and run this only if that gets stuck",
            app.folder.display()
        );
    }
    for &step in &steps {
        let app = moveover::app_in(&dir).context("the OneDrive app's folder is gone")?;
        match step {
            Step::Restart | Step::Reset if app.linked.is_none() => {
                return finished_by_app(cli, &dir, &app.folder, step, yes, as_json, profile).await;
            }
            Step::Restart => {
                say(&format!(
                    "Restarting the OneDrive app, then waiting up to {wait_minutes} minutes for it to finish…"
                ));
                moveover::restart_app(&app)?;
                let folder = app.folder.clone();
                if tokio::task::spawn_blocking(move || moveover::wait_released(&folder, wait)).await? {
                    return finished_by_app(cli, &dir, &app.folder, step, yes, as_json, profile).await;
                }
            }
            Step::Reset if app.other_accounts => {
                say(
                    "Not resetting the OneDrive app: a work or school account is set up in it too, and would be unlinked.",
                );
            }
            Step::Reset => {
                say(&format!(
                    "Resetting the OneDrive app (its own reset), then waiting up to {wait_minutes} minutes for it to finish…"
                ));
                moveover::reset_app(&app)?;
                let folder = app.folder.clone();
                if tokio::task::spawn_blocking(move || moveover::wait_released(&folder, wait)).await? {
                    return finished_by_app(cli, &dir, &app.folder, step, yes, as_json, profile).await;
                }
            }
            Step::Release => return release_all(cli, &dir, &app.folder, yes, as_json, profile).await,
        }
    }
    done(false, None, "The OneDrive app hasn't finished unlinking yet.".into())
}

fn say(t: &str) {
    println!("{t}");
}

fn result(as_json: bool, released: bool, by: Option<&str>, message: String) -> Result<()> {
    if as_json {
        println!("{}", json!({ "released": released, "by": by, "message": message }));
    } else {
        println!("{message}");
    }
    Ok(())
}

/// The app released the folder after `step`: then only what it may have
/// left behind is OneUpDrive's to release.
async fn finished_by_app(
    cli: &Cli,
    dir: &std::path::Path,
    folder: &std::path::Path,
    step: Step,
    yes: bool,
    as_json: bool,
    profile: MemoryProfile,
) -> Result<()> {
    say("The OneDrive app released the folder. Looking for anything it left behind (names only)…");
    rescan(cli, folder, profile).await?;
    if left_behind(cli, dir, profile)? > 0 {
        return release_all(cli, dir, folder, yes, as_json, profile).await;
    }
    let after = if step == Step::Reset {
        " If it opens asking you to sign in, close it: OneUpDrive syncs this folder from now on."
    } else {
        ""
    };
    result(as_json, true, Some(step.name()), format!("The OneDrive app finished unlinking.{after}"))
}

async fn rescan(cli: &Cli, folder: &std::path::Path, profile: MemoryProfile) -> Result<()> {
    crate::scan(
        cli,
        &ScanArgs { path: folder.to_path_buf(), exclude: None, profile: ProfileArg { memory_profile: profile } },
    )
    .await
}

async fn relist(cli: &Cli, profile: MemoryProfile) -> Result<()> {
    crate::mirror(
        cli,
        &MirrorArgs {
            depth: 2,
            max_depth: 6,
            split_size_mib: 256,
            max_concurrency: 16,
            exclude: None,
            rebuild: false,
            no_hints: false,
            profile: ProfileArg { memory_profile: profile },
        },
    )
    .await
}

/// Placeholders the last scan found with no sync app holding the folder.
fn left_behind(cli: &Cli, dir: &std::path::Path, profile: MemoryProfile) -> Result<i64> {
    let store = crate::open_store(cli, profile)?;
    Ok(moveover::check(dir, &store)?.and_then(|c| c.scanned).map_or(0, |s| s.left_behind))
}

/// OneUpDrive releases the folder (step 3), if something still holds it,
/// then rescans: whatever Windows' pass skipped (any folder a program has
/// open) is cleared item by item, which open programs don't block.
async fn release_all(
    cli: &Cli,
    dir: &std::path::Path,
    folder: &std::path::Path,
    yes: bool,
    as_json: bool,
    profile: MemoryProfile,
) -> Result<()> {
    if !yes {
        if as_json {
            bail!("OneUpDrive finishing the unlinking itself needs --yes");
        }
        println!(
            "OneUpDrive will now finish the unlinking itself. First it stops the OneDrive app, looks through {} again\n\
             and brings its OneDrive listing up to date; it goes ahead only if every online-only file is in OneDrive.\n\
             Windows then removes those online-only files from this PC, as unlinking does; files on this PC stay.\n\
             Type yes to go ahead:",
            folder.display()
        );
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "yes" {
            return result(as_json, false, None, "Nothing was changed.".into());
        }
    }
    if let Some(app) = moveover::app_in(dir) {
        say("Stopping the OneDrive app…");
        moveover::stop_app(&app)?;
    }
    say("Looking through the folder again (names only: opens and downloads nothing)…");
    rescan(cli, folder, profile).await?;
    let mut said = Vec::new();
    if oneup_cfapi::provider::registration(folder).is_some() {
        say("Bringing the OneDrive listing up to date…");
        relist(cli, profile).await?;
        let r = release_once(cli, dir, profile, oneup_app::sync::now_ms())?;
        said.push(format!(
            "Windows removed the online-only files ({} files and {} folders checked to be in OneDrive{})",
            r.online_only.0,
            r.online_only.1,
            moveover::left_out(r.excluded_folders)
        ));
        say("Checking that nothing was left behind (names only)…");
        rescan(cli, folder, profile).await?;
    }
    let mut left = left_behind(cli, dir, profile)?;
    if left > 0 {
        say(&format!(
            "{left} placeholders are left behind, in folders some program has open (Windows skips those). Clearing them \
             one by one, which open programs don't block…"
        ));
        say("Bringing the OneDrive listing up to date…");
        relist(cli, profile).await?;
        let listed = oneup_app::sync::now_ms();
        let mut store = crate::open_store(cli, profile)?;
        let c = moveover::clear_left_behind(dir, &mut store, listed, true)?;
        drop(store);
        said.push(format!(
            "{} online-only files removed (in OneDrive), {} files and {} folders made ordinary with their content",
            c.removed, c.files, c.folders
        ));
        say("Checking that nothing was left behind (names only)…");
        rescan(cli, folder, profile).await?;
        left = left_behind(cli, dir, profile)?;
    }
    let what = if said.is_empty() { String::new() } else { format!(" {}.", said.join("; ")) };
    if left > 0 {
        return result(
            as_json,
            false,
            Some("release"),
            format!(
                "OneUpDrive finished most of the unlinking, but {left} placeholders are still left (in use by a program, \
                 or changed meanwhile). Run `oneup migrate finish-unlink` again to clear them.{what}"
            ),
        );
    }
    result(
        as_json,
        true,
        Some("release"),
        format!(
            "OneUpDrive finished unlinking the OneDrive app: no placeholders are left.{what} The online-only files come \
             back as OneUpDrive's when the folder is paired."
        ),
    )
}

/// One release through the gate, saying what Windows is doing meanwhile:
/// it removes the placeholders inside one call, many minutes on a big
/// folder.
fn release_once(cli: &Cli, dir: &std::path::Path, profile: MemoryProfile, listed: i64) -> Result<moveover::Released> {
    let mut store = crate::open_store(cli, profile)?;
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let ticker = {
        let busy = std::sync::Arc::clone(&busy);
        move || {
            let started = std::time::Instant::now();
            while busy.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::park_timeout(Duration::from_secs(60));
                if busy.load(std::sync::atomic::Ordering::Relaxed) {
                    println!("  … still removing ({} min so far)", started.elapsed().as_secs() / 60);
                }
            }
        }
    };
    let mut tick = None;
    let r = moveover::release(dir, &mut store, listed, true, |r| {
        say(&format!(
            "Everything checks out. Releasing the folder: Windows now removes its online-only files ({} checked to be in \
             OneDrive{}).\nOn a big folder this takes many minutes (about 17 for 234,000 files on one PC). Leave this \
             running.",
            r.online_only.0,
            moveover::left_out(r.excluded_folders)
        ));
        tick = Some(std::thread::spawn(ticker));
    });
    busy.store(false, std::sync::atomic::Ordering::Relaxed);
    if let Some(t) = tick {
        t.thread().unpark();
        let _ = t.join();
    }
    r
}

fn print(c: &Check) {
    let size = |b: i64| oneup_app::versions::size(u64::try_from(b).unwrap_or(0));
    let folder = &c.app.folder;
    println!("OneDrive app folder: {}", folder.display());
    match &c.app.linked {
        Some(by) if c.app.stalled() => println!(
            "  the OneDrive app started unlinking{} but hasn't finished: {by} still holds the folder.\n  \
             `oneup migrate finish-unlink` gets it finished (nothing is lost: it checks first).",
            since(c.app.unlinked_at_ms)
        ),
        Some(by) => println!(
            "  still managed by {by}: unlink it before moving over (in the OneDrive app: Settings > Account > Unlink this PC)."
        ),
        None => println!("  not managed by the OneDrive app any more."),
    }
    for (name, path, inside) in &c.app.windows_folders {
        if *inside {
            println!("  your {name} folder is inside it ({}): it moves over with it", path.display());
        }
    }
    let Some(s) = &c.scanned else {
        println!(
            "  not scanned in this data folder yet: `oneup scan \"{}\" --exclude dev` (read-only) shows what's there.",
            folder.display()
        );
        return;
    };
    println!("  last scan{}: {} files ({})", if s.finished { "" } else { " (unfinished)" }, s.files, size(s.bytes));
    println!(
        "  on this PC: {} files; online-only: {} files ({}): those stay in OneDrive and come back as OneUpDrive's online-only files",
        s.on_pc,
        s.online_only,
        size(s.online_only_bytes)
    );
    if s.excluded_folders > 0 {
        println!("  {} folders left out by your exclusion choice (never listed)", s.excluded_folders);
    }
    if s.pinned > 0 {
        println!("  {} files are set to always keep on this device", s.pinned);
    }
    if s.left_behind > 0 {
        println!(
            "  {} placeholders were left behind by an unlinking (Windows skips any folder a program has open meanwhile):\n  \
             `oneup migrate finish-unlink` releases them (nothing is lost: it checks first).",
            s.left_behind
        );
    }
    match &c.cloud {
        None => {}
        Some(Err(why)) => {
            println!();
            println!(
                "  Not yet checked against OneDrive: {why}. Run `oneup mirror` (read-only), then this check again."
            );
        }
        Some(Ok(cc)) => {
            println!();
            println!(
                "Before unlinking: Windows removes the OneDrive app's online-only files from this PC when it's unlinked"
            );
            println!("(their content is only in OneDrive). Checked against this data folder's OneDrive listing:");
            println!(
                "  {} online-only files and {} folders not yet opened: in OneDrive at the same path",
                cc.in_cloud.0, cc.in_cloud.1
            );
            if cc.newer > 0 {
                println!(
                    "  {} online-only files: OneDrive has a newer version (nothing of them is on this PC)",
                    cc.newer
                );
            }
            if cc.all_in_cloud() {
                println!("  Every online-only item is in OneDrive: unlinking loses nothing on this PC.");
            } else {
                println!(
                    "  {} online-only files and {} folders are NOT in this OneDrive at the same path:",
                    cc.missing.0, cc.missing.1
                );
                for p in &cc.examples {
                    println!("      {p}");
                }
                println!(
                    "  Don't unlink yet. Usually the listing is just older than a change made since: run `oneup mirror`
           and this check again. If they're still listed, look for them in OneDrive (online, including the recycle bin)
           and, to keep a copy here, set them to \"Always keep on this device\" in Explorer first."
                );
            }
        }
    }
    if let Some(k) = &c.carried {
        println!();
        println!("Kept for moving over (applied when this folder is paired with all of your OneDrive):");
        println!("  {} item(s) set to always keep on this device stay on this PC", k.pinned.len());
        if !k.file_kinds.is_empty() {
            println!(
                "  kinds of files the OneDrive app doesn't upload stay excluded (never uploaded): {}",
                k.file_kinds.join(", ")
            );
        }
        if let Some(first) = k.not_on_pc.first() {
            println!(
                "  {} OneDrive folder(s) that aren't on this PC (unticked in the app's Choose folders?) stay off this PC, paused, e.g. /{first}",
                k.not_on_pc.len()
            );
        }
    }
    println!();
    println!("Nothing in your folders or OneDrive was changed. Next, after unlinking the OneDrive app:");
    println!("  oneup sync \"{}\" --remote / --exclude dev --pair-only --on-demand", folder.display());
    println!("  oneup start");
    println!("(or the settings window's Set up page, which walks through all of this).");
}

/// `oneup migrate leftovers [--clear | --undo]`: the OneDrive app's
/// leftovers after moving over (`oneup_app::appleftovers`).
pub fn leftovers(dir: &std::path::Path, store: &Store, clear: bool, undo: bool, as_json: bool) -> Result<()> {
    use oneup_app::appleftovers as lo;
    if undo {
        let u = lo::undo_now(dir)?;
        if as_json {
            println!(
                "{}",
                json!({
                    "restored": u.as_ref().map(|u| u.restored.iter().map(|l| l.name()).collect::<Vec<_>>()),
                    "values": u.as_ref().map(|u| u.values),
                })
            );
        } else {
            match u {
                None => println!("Nothing to undo."),
                Some(u) => {
                    println!("Put back: {}.", u.restored.iter().map(|l| l.label()).collect::<Vec<_>>().join(", "))
                }
            }
        }
        return Ok(());
    }
    let local = oneup_app::sync::pairing(store)?.local;
    if !lo::is_app_folder(&local) {
        bail!("{} isn't the OneDrive app's folder: nothing was moved over", local.display());
    }
    if clear {
        let c = lo::clear_now(dir, &local)?;
        if as_json {
            println!("{}", c.to_json());
        } else if c.removed.is_empty() && c.changed.is_empty() {
            println!("Nothing found.");
        } else {
            println!("{}", c.summary());
        }
        return Ok(());
    }
    let found = lo::pending(&local)?;
    if as_json {
        println!("{}", json!({ "found": found.iter().map(|l| l.name()).collect::<Vec<_>>() }));
    } else if found.is_empty() {
        println!("Nothing found.");
    } else {
        for l in &found {
            println!("{}", l.label());
        }
        println!("Clear them with `oneup migrate leftovers --clear`.");
    }
    Ok(())
}
