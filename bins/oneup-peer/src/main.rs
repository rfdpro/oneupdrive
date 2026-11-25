//! The same executable runs as a standalone receiver or as a worker owned by Settings.
use std::{
    future::Future,
    io::{self, BufRead, IsTerminal as _, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use iroh_blobs::store::fs::FsStore;
use serde::Deserialize;
use serde_json::{Value, json};

mod disk;
#[cfg(test)]
mod tests;
mod ticket;
mod transfer;

use ticket::{LIMIT, Ticket};
use transfer::Network;

#[derive(Parser)]
#[command(about = "Receive a file shared with OneUpDrive", version)]
struct Cli {
    /// Resume the receive session shown during a previous download
    #[arg(long)]
    session: Option<String>,
    /// Where to keep transfer sessions and received files
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "auto")]
    network: Network,
    #[arg(long, hide = true)]
    worker: bool,
}

#[derive(Deserialize)]
struct Request {
    root: PathBuf,
    #[serde(default)]
    network: Network,
    #[serde(flatten)]
    action: Action,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Action {
    Prepare,
    Inspect,
    Receive {
        session: String,
        ticket: Option<String>,
    },
    #[cfg(windows)]
    Send {
        path: PathBuf,
        recipient: iroh::EndpointId,
    },
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.worker {
        let result = worker().await;
        if let Err(error) = &result {
            emit(json!({"phase": "failed", "error": error.to_string(), "connections": 0}));
        }
        return result;
    }

    let root = disk::scratch_root(&match cli.data_dir {
        Some(dir) => dir,
        None => companion_dir()?,
    })?;
    let id = match cli.session {
        Some(id) => id,
        None => disk::prepare(&root)?.0,
    };
    let session = disk::Session::open(&root, &id)?;
    println!("Recipient code: {}", session.key()?.public());
    println!("Share the code with the sender. To resume, run oneup-receive --session {id}.");
    let ticket = session.ticket(|| {
        println!("Paste the sender's ticket and press Enter.");
        line(&mut io::stdin().lock(), LIMIT + 1)?.parse()
    })?;
    println!("Receiving {} ({} bytes). Press Ctrl+C to pause.", ticket.name, ticket.size);
    receive(&session, &ticket, cli.network, false, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

fn companion_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let parent = PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is missing.")?);
    #[cfg(not(windows))]
    let parent = PathBuf::from(std::env::var_os("HOME").context("HOME is missing.")?);
    Ok(parent.join(".oneup-receive"))
}

fn line(reader: &mut impl BufRead, max: usize) -> Result<String> {
    let mut text = String::new();
    reader.take(max as u64).read_line(&mut text)?;
    ensure!(!text.is_empty() && text.len() < max, "Invalid transfer request.");
    Ok(text)
}

pub(crate) fn emit(value: Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

async fn worker() -> Result<()> {
    let request: Request = serde_json::from_str(&line(&mut io::stdin().lock(), 64 * 1024)?)?;
    let root = disk::scratch_root(&request.root)?;
    match request.action {
        Action::Prepare => {
            let (id, key) = disk::prepare(&root)?;
            disk::set_current(&root, &id)?;
            emit(json!({"phase": "ready", "session": id, "code": key.to_string()}));
        }
        Action::Inspect => {
            let Some(session) = disk::current(&root)? else { return Ok(()) };
            let opened = disk::Session::open(&root, &session)?;
            let code = opened.key()?.public().to_string();
            let ticket = opened.saved_ticket()?;
            let saved = ticket.as_ref().map(|t| opened.saved(t)).transpose()?.flatten();
            let phase = if saved.is_some() {
                "complete"
            } else if ticket.is_some() {
                "paused"
            } else {
                "ready"
            };
            emit(json!({"phase": phase, "session": session, "code": code, "saved": saved,
                "has_ticket": ticket.is_some(), "name": ticket.map(|t| t.name)}));
        }
        Action::Receive { session, ticket } => {
            let session = disk::Session::open(&root, &session)?;
            let ticket = session.ticket(|| ticket.context("Paste a P2P ticket.")?.parse())?;
            receive(&session, &ticket, request.network, true, parent_closed()).await?;
        }
        #[cfg(windows)]
        Action::Send { path, recipient } => send(&root, path, recipient, request.network, parent_closed()).await?,
    }
    Ok(())
}

fn parent_closed() -> impl Future<Output = ()> {
    // Tokio's stdin uses a blocking pool and would hold up runtime shutdown
    // when a transfer finishes before its controlling pipe closes.
    let (done, finished) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = io::stdin().read(&mut [0u8]);
        let _ = done.send(());
    });
    async move {
        let _ = finished.await;
    }
}

#[cfg(windows)]
async fn send(
    root: &Path,
    path: PathBuf,
    recipient: iroh::EndpointId,
    network: Network,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).context("Invalid source name.")?.to_owned();
    ticket::file_name(&name)?;
    let stage = tempfile::Builder::new().prefix("send-").tempdir_in(root)?;
    let store = FsStore::load(stage.path().join("blobs")).await?;
    tokio::pin!(stop);
    emit(json!({"phase": "preparing", "name": name}));
    let result = async {
        let (tag, size) = tokio::select! {
            result = disk::import(&store, path) => result?,
            () = &mut stop => return Ok("stopped"),
        };
        let (router, blob) = tokio::select! {
            result = transfer::offer(&store, tag.hash(), recipient, network, true) => result?,
            () = &mut stop => return Ok("stopped"),
        };
        let ticket = Ticket { blob, name, size }.encode()?;
        emit(json!({"phase": "offered", "ticket": ticket, "total": size}));
        let phase = tokio::select! {
            () = &mut stop => "stopped",
            () = tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)) => "expired",
        };
        router.shutdown().await?;
        Ok::<_, anyhow::Error>(phase)
    }
    .await;
    let shutdown = store.shutdown().await;
    let phase = result?;
    shutdown?;
    stage.close()?;
    emit(json!({"phase": phase, "connections": 0}));
    Ok(())
}

async fn receive(
    session: &disk::Session,
    ticket: &Ticket,
    network: Network,
    managed: bool,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    if session.saved(ticket)?.is_some() {
        bail!("The destination already exists. Open the receive folder to review it.");
    }
    let store = FsStore::load(session.blob_dir()?).await?;
    let saved = download(session, &store, ticket, network, managed, stop).await;
    let shutdown = store.shutdown().await;
    match saved? {
        Some(path) if managed => emit(json!({"phase": "complete", "connections": 0, "saved": path})),
        Some(path) => {
            println!("Saved {}", path.display());
            if io::stdin().is_terminal() {
                println!("Open the receive folder? [y/N]");
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                if answer.trim().eq_ignore_ascii_case("y") {
                    open_folder(path.parent().context("No receive folder.")?)?;
                }
            }
        }
        None if managed => emit(json!({"phase": "paused", "connections": 0})),
        None => println!("Paused. Run the same command to resume."),
    }
    shutdown?;
    Ok(())
}

fn open_folder(path: &Path) -> Result<()> {
    #[cfg(windows)]
    let opener =
        PathBuf::from(std::env::var_os("WINDIR").context("Windows directory is missing.")?).join("explorer.exe");
    #[cfg(target_os = "macos")]
    let opener = "/usr/bin/open";
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let opener = "xdg-open";
    std::process::Command::new(opener).arg(path).spawn().context("Could not open the receive folder.")?;
    Ok(())
}

async fn download(
    session: &disk::Session,
    store: &FsStore,
    ticket: &Ticket,
    network: Network,
    managed: bool,
    stop: impl Future<Output = ()>,
) -> Result<Option<PathBuf>> {
    let local = store.remote().local(ticket.blob.hash_and_format()).await?;
    if !local.is_complete() {
        let cached = local.local_bytes();
        if managed {
            emit(
                json!({"phase": "receiving", "has_ticket": true, "name": ticket.name, "bytes": cached, "total": ticket.size}),
            );
        }
        let endpoint = transfer::endpoint(network, session.key()?).await?;
        let result = tokio::select! {
            result = transfer::fetch(&endpoint, store, &ticket.blob, |bytes, route| {
                if managed { emit(json!({"connections": 1, "bytes": bytes + cached, "route": route})); }
                else { eprintln!("{} / {} bytes ({route})", bytes + cached, ticket.size); }
            }) => Some(result),
            () = stop => None,
        };
        endpoint.close().await;
        match result {
            Some(result) => {
                result?;
            }
            None => return Ok(None),
        }
    }
    if managed {
        emit(
            json!({"phase": "verifying", "has_ticket": true, "name": ticket.name, "bytes": ticket.size, "total": ticket.size, "connections": 0}),
        );
    }
    Ok(Some(session.publish(store, ticket).await?))
}
