//! `oneup find` (search the drive by name, `oneup_app::search`) and
//! `oneup search` (its settings, config.toml `[search]`).

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use oneup_app::search::{self, Found, Item, MatchMode};
use oneup_app::t;

#[derive(Debug, Args)]
pub struct Options {
    /// Text to match in a name, in any case
    #[arg(required = true, num_args = 1..)]
    text: Vec<String>,
    /// List only files or only folders
    #[arg(long, value_enum)]
    kind: Option<Kind>,
    /// Require every word in the name, in any order
    #[arg(long, conflicts_with_all = ["regex", "onedrive"])]
    words: bool,
    /// Match names with a case-insensitive regular expression
    #[arg(long, conflicts_with = "onedrive")]
    regex: bool,
    /// How many to list at most
    #[arg(long, default_value_t = search::DEFAULT_LIMIT)]
    limit: usize,
    /// Also search inside files with OneDrive (sends the search text to Microsoft)
    #[arg(long)]
    onedrive: bool,
    /// Search indexed file names on this PC
    #[arg(long, conflicts_with_all = ["regex", "onedrive", "everywhere", "network_shares"])]
    this_pc: bool,
    /// Search indexed names on mapped network shares
    #[arg(long, conflicts_with_all = ["regex", "onedrive", "everywhere"])]
    network_shares: bool,
    /// Search all enabled indexes and OneDrive shared folders
    #[arg(long, conflicts_with_all = ["regex", "onedrive"])]
    everywhere: bool,
    /// Print the results as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Kind {
    Files,
    Folders,
}

/// Lists what's found: placement, then the path (a folder ends in `/`).
fn print(found: &Found) {
    for i in &found.items {
        println!("{:<13} {}", place(i.placement), shown(i));
    }
}

/// Where an item is, in the user's language (`--json` keeps the names).
fn place(p: search::Placement) -> String {
    use search::Placement as P;
    match p {
        P::Here => t!("here"),
        P::OnlineOnly => t!("online-only"),
        P::NotHereYet => t!("not here yet"),
        P::Excluded => t!("excluded"),
        P::NotSynced => t!("not synced"),
        P::InOneDrive => t!("in OneDrive"),
    }
}

fn shown(i: &Item) -> String {
    if i.folder { format!("{}/", i.path.trim_end_matches('/')) } else { i.path.clone() }
}

pub async fn run(dir: &Path, client_id: Option<&str>, args: &Options) -> Result<()> {
    let text = args.text.join(" ");
    let folder = args.kind.map(|kind| matches!(kind, Kind::Folders));
    if args.this_pc || args.network_shares || args.everywhere {
        let result = if args.everywhere {
            search::drives::everywhere(dir, &text, args.words, folder, args.limit, |_| {})?
        } else if args.network_shares {
            search::shares::find(dir, &text, args.words, folder, args.limit)?
        } else {
            search::drives::find(dir, &text, args.words, folder, args.limit)?
        };
        if args.json {
            println!("{result}");
        } else {
            for item in result["items"].as_array().into_iter().flatten() {
                if let Some(path) = item["local"].as_str().or_else(|| item["path"].as_str()) {
                    println!("{path}");
                }
            }
            if result["ready"] == 0 {
                println!("{}", t!("No drive or network share index is ready."));
            } else if result["more"] == true {
                println!("{}", t!("More found: add to the search to narrow it, or use --limit."));
            }
        }
        return Ok(());
    }
    let mode = if args.regex {
        MatchMode::Regex
    } else if args.words {
        MatchMode::Words
    } else {
        MatchMode::Text
    };
    let query = search::name_query(&text, mode, true)?;
    let limit = args.limit.clamp(1, 10_000);
    let mut told = false;
    let store = if args.regex {
        oneup_app::setup::open_store(dir, 1024)?
    } else {
        search::open(dir, |n| {
            if !args.json {
                if !told {
                    eprintln!("{}", t!("Building the search index (first search only)..."));
                    told = true;
                }
                eprint!("\r{}", t!("{n, plural, one {# item} other {# items}}", n = n));
                let _ = std::io::stderr().flush();
            }
        })?
    };
    if told {
        eprintln!();
    }
    let found = search::find_matching(dir, &store, &query, folder, limit)?;
    drop(store);
    let inside = if args.onedrive {
        let graph = search::graph(dir, client_id)?;
        let mut results = search::find_in_onedrive(dir, &graph, &text, limit).await?;
        if let Some(folder) = folder {
            results.items.retain(|item| item.folder == folder);
        }
        Some(results)
    } else {
        None
    };
    if args.json {
        let mut v = search::to_json(&found);
        if let Some(f) = &inside {
            v["onedrive"] = search::to_json(f);
        }
        println!("{v}");
        return Ok(());
    }
    if found.too_many {
        println!("{}", oneup_app::l10n::message(search::TOO_MANY, &[]));
    } else if found.items.is_empty() {
        println!("{}", t!("Nothing found."));
    }
    print(&found);
    if found.more && !found.too_many {
        println!("{}", t!("More found: add to the search to narrow it, or use --limit."));
    }
    if let Some(f) = &inside {
        println!("\n{}", t!("Inside files (OneDrive's search):"));
        if f.items.is_empty() {
            println!("{}", t!("Nothing found."));
        }
        print(f);
    }
    println!("{}", oneup_app::l10n::message(search::LIMIT_NOTE, &[]));
    Ok(())
}

/// Shows the search settings after changing those given.
pub fn settings(
    dir: &Path,
    onedrive: Option<bool>,
    hotkey: Option<&str>,
    local_drives: Option<bool>,
    network_shares: Option<bool>,
    pause_index: Option<bool>,
    compact_index: bool,
) -> Result<()> {
    let mut s = oneup_app::config::load_search(dir)?;
    let before = s.clone();
    if let Some(on) = onedrive {
        s.onedrive = on;
    }
    if let Some(on) = local_drives {
        s.local_drives = on;
    }
    if let Some(on) = network_shares {
        s.network_shares = on;
    }
    if let Some(pause) = pause_index {
        s.index_paused = pause;
    }
    if let Some(h) = hotkey.map(str::trim) {
        s.hotkey = if h.eq_ignore_ascii_case("off") || h.is_empty() {
            String::new()
        } else {
            search::Hotkey::parse(h).context("--hotkey")?.to_string()
        };
    }
    if s != before {
        oneup_app::config::save_search(dir, &s)?;
        // The icon in the notification area holds the hotkey.
        oneup_app::tray::settings_changed(dir);
    }
    if local_drives.is_some() || network_shares.is_some() || pause_index.is_some() {
        search::drives::wake(dir)?;
    }
    if compact_index {
        let status = search::drives::compact(dir)?;
        if status["compacting"] == true {
            println!("{}", t!("Compacting the drive indexes after the current update."));
        }
    }
    println!(
        "{}",
        if !s.local_drives {
            t!("Local drive indexing: off")
        } else if s.index_paused {
            t!("Local drive indexing: paused")
        } else {
            t!("Local drive indexing: on")
        }
    );
    println!(
        "{}",
        if !s.network_shares {
            t!("Network share indexing: off")
        } else if s.index_paused {
            t!("Network share indexing: paused")
        } else {
            t!("Network share indexing: on")
        }
    );
    if s.local_drives || s.network_shares {
        let status = search::drives::status(dir)?;
        for drive in status["drives"].as_array().into_iter().flatten() {
            println!(
                "  {} {}",
                drive["drive"].as_str().unwrap_or_default(),
                oneup_app::l10n::message(drive["status"].as_str().unwrap_or_default(), &[])
            );
        }
        let bytes = status["index_bytes"].as_u64().unwrap_or(0);
        let free = status["free_bytes"].as_u64().unwrap_or(0);
        if bytes > 0 && free > 0 {
            let (size, frees) = (oneup_app::versions::size(bytes), oneup_app::versions::size(free));
            println!(
                "  {}",
                t!("Index size: {size} (--compact-index frees at least {frees})", size = size, frees = frees)
            );
        } else if bytes > 0 {
            println!("  {}", t!("Index size: {size}", size = oneup_app::versions::size(bytes)));
        }
    }
    println!(
        "{}",
        if s.onedrive {
            t!("Search inside files with OneDrive: on (sends the search text to Microsoft)")
        } else {
            t!("Search inside files with OneDrive: off")
        }
    );
    match s.hotkey.as_str() {
        "" => println!("{}", t!("Hotkey: none")),
        h => println!("{}", t!("Hotkey: {keys} (while OneUpDrive's icon is in the notification area)", keys = h)),
    }
    Ok(())
}
