//! Settings owns one outgoing offer and one incoming session. Workers use
//! private pipes for commands and status, including when no page is visible.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use oneup_sys::{clipboard, webview::Poster};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, oneshot},
};

#[derive(Clone, Copy)]
enum Side {
    Send,
    Receive,
}

impl Side {
    fn key(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Receive => "receive",
        }
    }
}

#[derive(Default)]
struct Slot {
    view: Map<String, Value>,
    cancel: Option<oneshot::Sender<()>>,
    running: bool,
}

impl Slot {
    fn snapshot(&self) -> Value {
        let mut view = self.view.clone();
        view.insert("running".into(), json!(self.running));
        Value::Object(view)
    }
}

pub(super) struct Peer {
    root: PathBuf,
    send: Arc<Mutex<Slot>>,
    receive: Arc<Mutex<Slot>>,
}

impl Peer {
    pub(super) fn new(dir: &Path) -> Self {
        Self { root: dir.join("peer-transfers"), send: Default::default(), receive: Default::default() }
    }

    pub(super) async fn handle(&self, op: &str, args: &Value, poster: &Poster) -> Result<Value> {
        match op {
            "peer_state" => {
                let mut recv = self.receive.lock().await;
                if recv.view.is_empty() && !recv.running && self.root.join("current-receive").is_file() {
                    self.start(&mut recv, Side::Receive, json!({"op": "inspect"}), false, poster)?;
                }
                Ok(json!({"send": self.send.lock().await.snapshot(), "receive": recv.snapshot()}))
            }
            "peer_prepare" => {
                let mut slot = self.receive.lock().await;
                self.start(&mut slot, Side::Receive, json!({"op": "prepare"}), true, poster)?;
                Ok(slot.snapshot())
            }
            "peer_receive" => {
                let mut slot = self.receive.lock().await;
                let session =
                    slot.view.get("session").and_then(Value::as_str).context("Get your receive code first.")?;
                let ticket = args["ticket"].as_str().map(str::trim).filter(|s| !s.is_empty());
                ensure!(ticket.is_none_or(|s| s.len() <= 8192), "The download code is too long.");
                let req = json!({"op": "receive", "session": session, "ticket": ticket, "network": network(args)});
                self.start(&mut slot, Side::Receive, req, false, poster)?;
                Ok(slot.snapshot())
            }
            "peer_send" => {
                let path = Path::new(args["path"].as_str().context("Choose a file.")?);
                ensure!(path.is_absolute(), "Choose a local file.");
                if let Some((dir, _)) = oneup_app::datafolders::for_path(path) {
                    ensure!(!oneup_app::vaultpair::is_vault_dir(&dir), "Files in a vault can't be sent this way.");
                }
                let recipient = args["recipient"].as_str().context("Enter their receive code.")?.trim();
                ensure!(!recipient.is_empty() && recipient.len() <= 128, "Check the receive code they sent you.");
                let mut slot = self.send.lock().await;
                self.start(
                    &mut slot,
                    Side::Send,
                    json!({"op": "send", "path": path, "recipient": recipient, "network": network(args)}),
                    true,
                    poster,
                )?;
                Ok(slot.snapshot())
            }
            "peer_stop" => {
                let side = if args["side"] == "send" { Side::Send } else { Side::Receive };
                let mut slot = self.slot(side).lock().await;
                if let Some(cancel) = slot.cancel.take() {
                    let _ = cancel.send(());
                }
                Ok(Value::Null)
            }
            "peer_copy" => {
                let (side, key) = if args["side"] == "send" { (Side::Send, "ticket") } else { (Side::Receive, "code") };
                let slot = self.slot(side).lock().await;
                let text = slot.view.get(key).and_then(Value::as_str).context("Nothing to copy.")?;
                clipboard::set_text(text)?;
                Ok(Value::Null)
            }
            "peer_open_folder" => {
                let slot = self.receive.lock().await;
                let path = slot
                    .view
                    .get("saved")
                    .and_then(Value::as_str)
                    .map(Path::new)
                    .and_then(Path::parent)
                    .context("No received file.")?;
                std::process::Command::new("explorer.exe").arg(path).spawn()?;
                Ok(Value::Null)
            }
            "peer_show_receiver" => {
                let executable = oneup_app::launch::sibling("oneup-receive.exe")?;
                std::process::Command::new("explorer.exe").arg(format!("/select,{}", executable.display())).spawn()?;
                Ok(Value::Null)
            }
            _ => bail!("Unknown P2P action."),
        }
    }

    fn slot(&self, side: Side) -> &Arc<Mutex<Slot>> {
        match side {
            Side::Send => &self.send,
            Side::Receive => &self.receive,
        }
    }

    fn start(&self, slot: &mut Slot, side: Side, mut request: Value, reset: bool, poster: &Poster) -> Result<()> {
        ensure!(!slot.running, "A transfer is already active.");
        let executable = oneup_app::launch::sibling("oneup-receive.exe")?;
        let parent = self.root.parent().context("No transfer data folder.")?;
        ensure!(
            [parent, self.root.as_path()].into_iter().all(|path| oneup_app::datafolders::for_path(path).is_none()),
            "Transfer storage must be outside synced folders."
        );
        request["root"] = json!(self.root);
        let child = tokio::process::Command::new(executable)
            .arg("--worker")
            .creation_flags(0x0800_0000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let (cancel, stopped) = oneshot::channel();
        if reset {
            slot.view.clear();
        }
        slot.view.insert("phase".into(), json!("starting"));
        slot.view.remove("error");
        slot.running = true;
        slot.cancel = Some(cancel);
        post(poster, side, slot);
        let state = self.slot(side).clone();
        let poster = poster.clone();
        tokio::spawn(async move {
            let result = watch_worker(child, request, stopped, side, &state, &poster).await;
            let mut slot = state.lock().await;
            slot.running = false;
            slot.cancel = None;
            slot.view.insert("connections".into(), json!(0));
            if let Err(e) = result {
                slot.view.insert("phase".into(), json!("failed"));
                slot.view.entry("error".to_owned()).or_insert_with(|| json!(e.to_string()));
            }
            if matches!(side, Side::Send) {
                slot.view.remove("ticket");
            }
            if slot.view.get("phase") == Some(&json!("starting")) {
                slot.view.remove("phase");
            }
            post(&poster, side, &slot);
        });
        Ok(())
    }
}

fn network(args: &Value) -> &'static str {
    if args["direct"] == true { "direct" } else { "auto" }
}

fn post(poster: &Poster, side: Side, slot: &Slot) {
    poster.post(json!({"event": "peer", "side": side.key(), "transfer": slot.snapshot()}).to_string());
}

async fn watch_worker(
    mut child: tokio::process::Child,
    request: Value,
    mut stopped: oneshot::Receiver<()>,
    side: Side,
    state: &Mutex<Slot>,
    poster: &Poster,
) -> Result<()> {
    let mut input = child.stdin.take().context("No worker control pipe.")?;
    input.write_all(format!("{request}\n").as_bytes()).await?;
    let mut input = Some(input);
    let output = child.stdout.take().context("No worker status pipe.")?;
    let mut lines = BufReader::new(output).lines();
    loop {
        tokio::select! {
            _ = &mut stopped, if input.is_some() => { input.take(); }
            next = lines.next_line() => {
                let Some(line) = next? else { break };
                let view: Map<String, Value> = serde_json::from_str(&line).context("Invalid worker status.")?;
                let mut slot = state.lock().await;
                slot.view.extend(view);
                post(poster, side, &slot);
            }
        }
    }
    ensure!(child.wait().await?.success(), "The P2P transfer stopped unexpectedly.");
    Ok(())
}
