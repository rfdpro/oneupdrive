//! `oneup find` (search the drive by name, `oneup_app::search`) and
//! `oneup search` (its settings, config.toml `[search]`).

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use oneup_app::search::{self, Found, Item};
use oneup_app::t;

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

pub async fn run(
    dir: &Path,
    client_id: Option<&str>,
    text: &str,
    limit: usize,
    onedrive: bool,
    json: bool,
) -> Result<()> {
    let text = search::pattern(text)?;
    let limit = limit.clamp(1, 10_000);
    let mut told = false;
    let store = search::open(dir, |n| {
        if !json {
            if !told {
                eprintln!("{}", t!("Building the search index (first search only)..."));
                told = true;
            }
            eprint!("\r{}", t!("{n, plural, one {# item} other {# items}}", n = n));
            let _ = std::io::stderr().flush();
        }
    })?;
    if told {
        eprintln!();
    }
    let found = search::find(dir, &store, text, limit)?;
    drop(store);
    let inside = if onedrive {
        let graph = search::graph(dir, client_id)?;
        Some(search::find_in_onedrive(dir, &graph, text, limit).await?)
    } else {
        None
    };
    if json {
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
pub fn settings(dir: &Path, onedrive: Option<bool>, hotkey: Option<&str>) -> Result<()> {
    let mut s = oneup_app::config::load_search(dir)?;
    let before = s.clone();
    if let Some(on) = onedrive {
        s.onedrive = on;
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
