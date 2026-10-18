//! `oneup unsynced resolve`: the user's answers for what can't sync
//! (`oneup_app::resolve`), which the settings window's buttons run too.

use std::path::Path;

use anyhow::{Result, bail};
use oneup_app::resolve::{self, Choice};
use oneup_app::unsynced::{self, Item, Why};
use oneup_store::Store;

/// What was asked on the command line.
pub struct Ask<'a> {
    pub paths: &'a [String],
    pub all: bool,
    /// The code of what the caller was shown (`resolve::Prepared::code`):
    /// refused if the choice concerns anything else now.
    pub expect: Option<&'a str>,
    pub choice: Option<Choice>,
    pub name: Option<&'a str>,
    pub yes: bool,
}

pub async fn resolve(dir: &Path, mut store: Store, a: Ask<'_>) -> Result<()> {
    let pair = oneup_app::sync::pairing(&store)?;
    let backup = oneup_app::flow::get(&store)? == oneup_app::flow::Flow::Backup;
    let record = unsynced::load(&store)?;
    if !a.all && a.paths.is_empty() {
        bail!("name the items as `oneup unsynced` lists them, or use --all");
    }
    let paths: Vec<String> = a
        .paths
        .iter()
        .map(|p| Ok(crate::activity::normalize(&crate::activity::within(&pair.local, p)?)))
        .collect::<Result<_>>()?;
    let items = resolve::select(&record, a.choice, (!a.all).then_some(paths.as_slice()), backup)?;
    let Some(choice) = a.choice else {
        for i in &items {
            show_choices(&store, i, backup)?;
        }
        return Ok(());
    };
    if items.is_empty() && a.expect.is_none() {
        println!("Nothing to resolve.");
        return Ok(());
    }
    if choice == Choice::Rename && items.len() > 1 {
        bail!("rename one item at a time");
    }
    for i in &items {
        if !resolve::choices(i.why, backup).contains(&choice) {
            bail!("{}: {} isn't offered for \"{}\"", i.path, choice.label(), i.why.title());
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
    let p = resolve::prepare(&store, &items, choice, a.name, backup)?;
    if let Some(code) = a.expect
        && code != p.code
    {
        bail!("what this concerns changed since it was shown; look again");
    }
    if p.asked.is_empty() {
        bail!("{}", p.refused.join("\n"));
    }
    for r in &p.refused {
        println!("Skipped {r}.");
    }
    let what = if p.asked.len() == 1 && p.items <= 1 { p.asked[0].path.clone() } else { format!("{} items", p.items) };
    if choice.deletes() && !a.yes {
        let to = if choice == Choice::DeleteHere {
            "on this PC, into its kept copies (.oneup\\versions)"
        } else {
            "in OneDrive, into its recycle bin"
        };
        println!("Would delete {to}:");
        for r in &p.asked {
            println!("  {}", r.path);
        }
        println!("Add --yes to delete {what} (or --yes --expect {} to delete exactly these).", p.code);
        return Ok(());
    }
    resolve::ask(&mut store, &p.asked)?;
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
    const SHOWN: usize = 50;
    println!("{}  ({})", i.path, i.why.label());
    let choices = resolve::choices(i.why, backup);
    for c in choices {
        let flag = if *c == Choice::Rename { "--rename NAME".to_string() } else { format!("--{}", c.code()) };
        println!("  {flag:<22}{}", c.label());
    }
    if i.why == Why::NotEmpty {
        let c = store.read(|o| resolve::inside(o, &i.path, SHOWN))?;
        let side = if c.side == "local" { "on this PC" } else { "in OneDrive" };
        println!("  Inside, {side}:");
        for k in &c.items {
            let tag = if k.excluded { "  (excluded)" } else { "" };
            println!("    {}{}{tag}", k.name, if k.folder { "\\" } else { "" });
        }
        if c.total > c.items.len() {
            println!("    and {} more", c.total - c.items.len());
        }
    } else if choices.is_empty() {
        println!("  Change it in File Explorer or on OneDrive's website.");
    }
    Ok(())
}
