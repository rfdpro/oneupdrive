//! `oneup unsynced resolve`: the user's answers for what can't sync
//! (`oneup_app::resolve`), which the settings window's buttons run too.

use std::path::Path;

use anyhow::{Context, Result, bail};
use oneup_app::resolve::{self, Choice, Request};
use oneup_app::unsynced::{self, Item, Why};
use oneup_store::Store;

/// What was asked on the command line.
pub struct Ask<'a> {
    pub paths: &'a [String],
    pub all: bool,
    /// With `all`: how many items the caller saw (refused if the list changed).
    pub expect: Option<usize>,
    pub choice: Option<Choice>,
    pub name: Option<&'a str>,
    pub yes: bool,
}

/// Whether `path` is inside folder `top` (both record paths).
fn below(path: &str, top: &str) -> bool {
    let (p, t) = (path.as_bytes(), top.trim_end_matches('/').as_bytes());
    p.len() > t.len() && p[..t.len()].eq_ignore_ascii_case(t) && p[t.len()] == b'/'
}

pub async fn resolve(dir: &Path, mut store: Store, a: Ask<'_>) -> Result<()> {
    let pair = oneup_app::sync::pairing(&store)?;
    let backup = oneup_app::flow::get(&store)? == oneup_app::flow::Flow::Backup;
    let record = unsynced::load(&store)?;
    let mut items: Vec<&Item> = if a.all {
        let Some(c) = a.choice.filter(|c| c.in_bulk()) else {
            bail!("--all needs --delete-in-onedrive, --keep-in-onedrive, --restore, --delete-here or --upload");
        };
        record.items.iter().filter(|i| resolve::choices(i.why, backup).contains(&c)).collect()
    } else {
        if a.paths.is_empty() {
            bail!("name the items as `oneup unsynced` lists them, or use --all");
        }
        let mut v = Vec::new();
        for p in a.paths {
            let rel = crate::activity::normalize(&crate::activity::within(&pair.local, p)?);
            let i = record.items.iter().find(|i| i.path.eq_ignore_ascii_case(&rel));
            v.push(i.with_context(|| format!("{rel} isn't in the list (`oneup unsynced`)"))?);
        }
        v
    };
    if let Some(n) = a.expect
        && n != items.len()
    {
        bail!("{} items fit now, not {n}: the list changed", items.len());
    }
    let Some(choice) = a.choice else {
        for i in &items {
            show_choices(&store, i, backup)?;
        }
        return Ok(());
    };
    if items.is_empty() {
        println!("Nothing to resolve.");
        return Ok(());
    }
    if choice == Choice::Rename && items.len() > 1 {
        bail!("rename one item at a time");
    }
    for i in &items {
        if !resolve::choices(i.why, backup).contains(&choice) {
            bail!("{}: {} doesn't fit an item that's {}", i.path, choice.label(), i.why.title().to_lowercase());
        }
    }
    if choice == Choice::Exclude {
        for i in &items {
            let folder = pair.local.join(i.path.trim_start_matches('/')).is_dir();
            println!("{}", oneup_app::filetree::set_sync(dir, &i.path, folder, false)?);
        }
        crate::agent::nudge(dir).await;
        return Ok(());
    }
    // A folder answers for what's in it.
    items.sort_by(|x, y| x.path.cmp(&y.path));
    let mut tops: Vec<&Item> = Vec::new();
    for i in items {
        if !tops.iter().any(|t| below(&i.path, &t.path)) {
            tops.push(i);
        }
    }
    let mut asked = Vec::new();
    let mut count = 0;
    for i in tops {
        match store.read(|o| resolve::check(o, i, choice, a.name, backup, &|_| false))? {
            Ok(c) => {
                count += c.items();
                asked.push(Request { path: i.path.clone(), choice, name: a.name.map(|n| n.trim().to_string()) });
            }
            Err(why) => println!("Skipped {}: {why}.", i.path),
        }
    }
    if asked.is_empty() {
        bail!("nothing to do");
    }
    let what = if asked.len() == 1 && count <= 1 { asked[0].path.clone() } else { format!("{count} items") };
    if choice.deletes() && !a.yes {
        let to = if choice == Choice::DeleteHere {
            "on this PC, into its kept copies (.oneup\\versions)"
        } else {
            "in OneDrive, into its recycle bin"
        };
        println!("Would delete {to}:");
        for r in &asked {
            println!("  {}", r.path);
        }
        println!("Add --yes to delete {what}.");
        return Ok(());
    }
    resolve::ask(&mut store, &asked)?;
    let done = match choice {
        Choice::DeleteInOnedrive => format!("delete {what} in OneDrive"),
        Choice::KeepInOnedrive => format!("keep {what} in OneDrive"),
        Choice::Restore => format!("restore {what}"),
        Choice::DeleteHere => format!("delete {what} on this PC"),
        Choice::Upload => format!("upload {what}"),
        Choice::Rename => format!("rename {what} to {}", a.name.unwrap_or_default().trim()),
        Choice::Exclude => unreachable!("handled above"),
    };
    println!("Queued: {done}.");
    if !crate::agent::nudge(dir).await {
        println!("OneUpDrive isn't running; it does this when it starts (`oneup start`).");
    }
    Ok(())
}

/// What can be done about `i`, as options to type.
fn show_choices(store: &Store, i: &Item, backup: bool) -> Result<()> {
    println!("{}  ({})", i.path, i.why.label());
    let choices = resolve::choices(i.why, backup);
    for c in choices {
        let flag = if *c == Choice::Rename { "--rename NAME".to_string() } else { format!("--{}", c.code()) };
        println!("  {flag:<22}{}", c.label());
    }
    if i.why == Why::NotEmpty {
        let (side, kids) = store.read(|o| resolve::inside(o, &i.path))?;
        let side = if side == "local" { "on this PC" } else { "in OneDrive" };
        println!("  Inside, {side}:");
        for k in kids.iter().take(50) {
            let tag = if k.excluded { "  (excluded)" } else { "" };
            println!("    {}{}{tag}", k.name, if k.folder { "\\" } else { "" });
        }
        if kids.len() > 50 {
            println!("    and {} more", kids.len() - 50);
        }
    } else if choices.is_empty() {
        println!("  Change it in File Explorer or on OneDrive's website.");
    }
    Ok(())
}
