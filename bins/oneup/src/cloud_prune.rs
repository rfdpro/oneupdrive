//! `oneup cloud prune`: lists the OneDrive items the rules exclude and,
//! with `--yes`, removes them (to OneDrive's recycle bin) once they're all
//! on this PC; the copies here stay. `--details PATH` lists what's in one of
//! them, a page at a time. See `docs/architecture/rules.md` § Cloud cleanup.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use oneup_app::config::{self, CloudCopies};
use oneup_app::prune::{self, Candidate, Scope};
use oneup_app::setup::{self, DirLock};
use oneup_app::sync;
use oneup_graph::Graph;
use oneup_store::Store;

/// Entries per page of `--details`.
const PAGE: usize = 100;

pub async fn run(
    data_dir: &Path,
    store: Store,
    graph: Arc<Graph>,
    yes: bool,
    details: Option<&str>,
    page: usize,
) -> Result<()> {
    // Not here yet: brought here by syncing, unless OneDrive's copies are
    // left alone (`keep`).
    let brings = config::load_rules(data_dir, &store)?.cloud != CloudCopies::Keep;
    if let Some(path) = details {
        return show_details(&store, &graph, data_dir, path, page.max(1), brings).await;
    }
    if !yes {
        let (rules, _) = oneup_app::rulefiles::load(data_dir, &store, &mut Default::default())?;
        let all = prune::candidates(&store, &rules, Scope::All, brings)?;
        let kept = all.len();
        let found = prune::without_kept(&store, all)?;
        let kept = kept - found.len();
        let found = prune::counted(&graph, found).await?;
        print_list(&found);
        if kept > 0 {
            println!("({kept} more you chose to keep in OneDrive aren't listed.)");
        }
        if !found.is_empty() {
            println!(
                "Nothing was changed. `oneup cloud prune --yes` brings these here (if they aren't all here yet), then"
            );
            println!("removes them from OneDrive (into its recycle bin). `--details PATH` lists what's in a folder.");
        }
        return Ok(());
    }

    // Removing changes OneDrive: nothing else may sync this data folder meanwhile.
    setup::require_test_account(data_dir)?;
    let Ok(_lock) = DirLock::acquire(data_dir) else {
        return accept_offer(data_dir).await;
    };
    let pruning = oneup_app::rulefiles::load(data_dir, &store, &mut Default::default())?.0.pruning();
    let (_, engine) = sync::resume(&store, Arc::clone(&graph), pruning, data_dir)?;
    let store = engine.refresh(store).await?;
    let (rules, _) = oneup_app::rulefiles::load(data_dir, &store, &mut Default::default())?;
    let found = prune::without_kept(&store, prune::candidates(&store, &rules, Scope::All, brings)?)?;
    if found.is_empty() {
        println!("Nothing in OneDrive is excluded by your rules.");
        return Ok(());
    }
    let (ready, waiting): (Vec<Candidate>, Vec<Candidate>) = found.into_iter().partition(Candidate::ready);
    let ready = prune::counted(&graph, ready).await?;
    print_list(&ready);
    let (_, report) = engine.prune(store, &ready).await?;
    println!(
        "Removed {} from OneDrive (its recycle bin keeps them, usually for 30 days); {} skipped, {} failed.",
        report.done.len(),
        report.skipped.len(),
        report.failed.len()
    );
    if !waiting.is_empty() {
        println!(
            "{} more aren't all on this PC yet, so they stay in OneDrive for now. With OneUpDrive running, \
             `oneup cloud prune --yes` has it bring them here first.",
            waiting.len()
        );
    }
    Ok(())
}

/// `oneup cloud prune --keep`: the excluded items stay in OneDrive. They're
/// brought here and never sent (`cloud_copies = "pull"`, set if it was
/// `keep`), and not offered for removal again ([`prune::KEY_KEPT`]). Changes
/// nothing in OneDrive.
pub async fn keep(data_dir: &Path) -> Result<()> {
    let store = setup::open_store(data_dir, 1024)?;
    let mut settings = config::load_rules(data_dir, &store)?;
    drop(store);
    match settings.cloud {
        CloudCopies::Prune => bail!(
            "your settings remove excluded items from OneDrive by themselves (`cloud_copies = \"prune\"`); \
             `oneup rules --cloud pull` keeps them instead"
        ),
        CloudCopies::Keep => {
            settings.cloud = CloudCopies::Pull;
            config::save_rules(data_dir, &settings)?;
            println!("Excluded items in OneDrive are now brought here and never sent (`cloud_copies = \"pull\"`).");
        }
        CloudCopies::Pull => {}
    }
    let Ok(_lock) = DirLock::acquire(data_dir) else {
        // The agent is running: it records the answer.
        let Some(status) = crate::agent::current(data_dir).await? else {
            bail!("the data folder is busy (a sync is running); try again when it's done");
        };
        let Some(offer) = status.prune_offer else {
            println!("The background agent isn't asking about excluded items in OneDrive; nothing to decide.");
            return Ok(());
        };
        let code = u64::from_str_radix(&offer.code, 16).map_err(|_| anyhow::anyhow!("unexpected code from oneupd"))?;
        return crate::agent::answer(data_dir, code, oneup_ipc::Answer::KeepInCloud).await;
    };
    let mut store = setup::open_store(data_dir, 1024)?;
    let (rules, _) = oneup_app::rulefiles::load(data_dir, &store, &mut Default::default())?;
    let found = prune::without_kept(&store, prune::candidates(&store, &rules, Scope::All, true)?)?;
    let n = found.len();
    prune::keep(&mut store, found.into_iter().map(|c| c.id))?;
    println!("{n} excluded items in OneDrive stay there; they won't be offered for removal again.");
    Ok(())
}

/// With the agent running, it does the removing: this accepts what it
/// offers (by its code, so exactly the items it found).
async fn accept_offer(data_dir: &Path) -> Result<()> {
    let Some(status) = crate::agent::current(data_dir).await? else {
        bail!("the data folder is busy (a sync is running); try again when it's done");
    };
    let Some(offer) = status.prune_offer else {
        bail!(
            "the background agent doesn't offer to remove anything from OneDrive right now: it looks when it starts \
             and when the rules change, and with `cloud_copies = \"prune\"` it removes excluded items by itself"
        );
    };
    let code = u64::from_str_radix(&offer.code, 16).map_err(|_| anyhow::anyhow!("unexpected code from oneupd"))?;
    // OneDrive reports a folder's size some time after a change.
    let bytes = if offer.bytes > 0 { format!(", {}", oneup_app::versions::size(offer.bytes)) } else { String::new() };
    println!(
        "The background agent found {} items or more{bytes} excluded by your rules in OneDrive ({} at the top).",
        offer.items, offer.entries
    );
    crate::agent::answer(data_dir, code, oneup_ipc::Answer::Prune).await
}

/// `, 12 MiB`, or nothing for a folder OneDrive reports no size for (it
/// computes folder sizes later; 0 would be misleading).
fn shown(size: i64, folder: bool) -> String {
    if folder && size <= 0 {
        return String::new();
    }
    format!(", {}", oneup_app::versions::size(u64::try_from(size).unwrap_or(0)))
}

fn print_list(found: &[Candidate]) {
    if found.is_empty() {
        println!("Nothing in OneDrive is excluded by your rules.");
        return;
    }
    println!("In OneDrive and excluded by your rules:");
    let (mut items, mut bytes) = (0u64, 0i64);
    for c in found {
        let what = if c.folder {
            let n = if c.unlisted { format!("at least {} items", c.items) } else { format!("{} items", c.items) };
            format!("folder, {n}")
        } else {
            "file".into()
        };
        let here = match (c.fetch.len() as u64 + c.waiting, c.ready()) {
            (_, true) => String::new(),
            (n, false) if n > 0 => format!("; {n} not on this PC yet"),
            _ => "; not on this PC yet".into(),
        };
        println!("  {}  ({what}{}{here})", c.path, shown(c.size, c.folder));
        items += c.items.max(1);
        bytes += c.size.max(0);
    }
    println!(
        "{} entries, {} items or more, {} in all.",
        found.len(),
        items,
        oneup_app::versions::size(u64::try_from(bytes).unwrap_or(0))
    );
}

/// Lists what's inside the excluded folder at `path`, depth first, page
/// `page` of [`PAGE`] entries. Read-only.
async fn show_details(
    store: &Store,
    graph: &Graph,
    data_dir: &Path,
    path: &str,
    page: usize,
    brings: bool,
) -> Result<()> {
    let (rules, _) = oneup_app::rulefiles::load(data_dir, store, &mut Default::default())?;
    let found = prune::candidates(store, &rules, Scope::All, brings)?;
    let want = format!("/{}", path.trim_matches(['/', '\\']).replace('\\', "/"));
    let Some(c) = found.iter().find(|c| c.path.eq_ignore_ascii_case(&want)) else {
        bail!("{want} isn't one of the excluded items in OneDrive (`oneup cloud prune` lists them)");
    };
    if !c.folder {
        println!("{} is a file.", c.path);
        return Ok(());
    }
    let (skip, end) = ((page - 1) * PAGE, page * PAGE);
    let mut seen = 0usize;
    // (folder id, its path, the next page of its listing)
    let mut stack: Vec<(String, String, Option<String>)> = vec![(c.id.clone(), c.path.clone(), None)];
    let mut more = false;
    'walk: while let Some((id, at, next)) = stack.pop() {
        let p = graph.children_page(&id, next.as_deref()).await?;
        if let Some(n) = p.next_link {
            stack.push((id.clone(), at.clone(), Some(n)));
        }
        for item in p.value {
            let full = format!("{at}/{}", item.name.as_deref().unwrap_or("?"));
            if seen >= end {
                more = true;
                break 'walk;
            }
            if seen >= skip {
                let folder = item.folder.is_some();
                let size = shown(item.size.unwrap_or(0), folder);
                let size = size.strip_prefix(", ").map(|s| format!("  ({s})")).unwrap_or_default();
                println!("  {full}{}{size}", if folder { "/" } else { "" });
            }
            seen += 1;
            if item.folder.is_some() {
                stack.push((item.id, full, None));
            }
        }
    }
    if seen <= skip {
        println!("(no page {page}: {seen} entries in all)");
    } else if more {
        println!("(page {page}; `--page {}` for more)", page + 1);
    } else {
        println!("(page {page}, the last)");
    }
    Ok(())
}
