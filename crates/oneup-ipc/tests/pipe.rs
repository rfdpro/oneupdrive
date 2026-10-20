//! The control pipe for real, in a temp data folder.
#![cfg(windows)]
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use oneup_ipc::{
    Activity, Call, ClientError, Direction, Handler, PIPE_FILE, PauseState, RpcError, Server, Status, connect,
};

struct Agent(Mutex<PauseState>, tokio::sync::watch::Sender<u64>);

impl Handler for Agent {
    fn handle(&self, call: Call) -> Result<Status, RpcError> {
        let mut p = self.0.lock().unwrap();
        if let Call::Pause { direction: Direction::Both, minutes } = call {
            *p = PauseState { local: true, remote: true, until_ms: minutes.map(|m| m as i64 * 60_000) };
            self.1.send_modify(|r| *r += 1);
        }
        Ok(Status {
            version: "test".into(),
            rev: *self.1.borrow(),
            local: "L".into(),
            remote: "/R".into(),
            activity: if p.local { Activity::Paused } else { Activity::Idle },
            paused: *p,
            push: false,
            checked_ms: None,
            last: None,
            safety_stops: Vec::new(),
            overflow: None,
            problem: None,
            auto_pause: None,
            auto_pause_hours: None,
            prune_offer: None,
            progress: None,
            unsynced: None,
            retrying: None,
        })
    }

    fn changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.1.subscribe())
    }
}

fn agent() -> Arc<Agent> {
    Arc::new(Agent(Mutex::new(PauseState::default()), tokio::sync::watch::Sender::new(0)))
}

#[tokio::test]
async fn the_agent_is_reachable_through_its_data_folder_only_while_it_runs() {
    let dir = std::env::temp_dir().join(format!("oneup-ipc-pipe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(matches!(connect(&dir).await, Err(ClientError::NotRunning)));

    let server = Server::start(&dir, agent()).unwrap();
    let name = std::fs::read_to_string(dir.join(PIPE_FILE)).unwrap();

    // Nobody else can take over the name while the agent serves it.
    assert!(oneup_sys::pipe::create(&name, true).is_err());

    // Several clients, one after another and at once.
    let mut a = connect(&dir).await.unwrap();
    let mut b = connect(&dir).await.unwrap();
    assert_eq!(a.call(&Call::Status).await.unwrap().activity, Activity::Idle);
    let s = b.call(&Call::Pause { direction: Direction::Both, minutes: Some(5) }).await.unwrap();
    assert_eq!((s.activity, s.paused.until_ms), (Activity::Paused, Some(300_000)));
    assert_eq!(a.call(&Call::Status).await.unwrap().activity, Activity::Paused);
    drop((a, b));
    let mut c = connect(&dir).await.unwrap();
    assert!(c.call(&Call::Status).await.unwrap().paused.remote);
    drop(c);

    // Stopped: the name is gone and clients say so.
    drop(server);
    assert!(!dir.join(PIPE_FILE).exists());
    assert!(matches!(connect(&dir).await, Err(ClientError::NotRunning)));
    // A name left behind by a crashed agent reads as "not running" too
    // (once the aborted task has let go of its pipe instance).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    std::fs::write(dir.join(PIPE_FILE), &name).unwrap();
    assert!(matches!(connect(&dir).await, Err(ClientError::NotRunning)));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_watching_client_hears_of_another_clients_change() {
    let dir = std::env::temp_dir().join(format!("oneup-ipc-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let server = Server::start(&dir, agent()).unwrap();
    let mut watcher = connect(&dir).await.unwrap();
    let rev = watcher.call(&Call::Status).await.unwrap().rev;
    let watching = tokio::spawn(async move { watcher.call(&Call::Watch { since: rev }).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!watching.is_finished(), "nothing changed yet");
    let mut other = connect(&dir).await.unwrap();
    other.call(&Call::Pause { direction: Direction::Both, minutes: None }).await.unwrap();
    let s = tokio::time::timeout(std::time::Duration::from_secs(5), watching).await.unwrap().unwrap().unwrap();
    assert_eq!((s.activity, s.rev), (Activity::Paused, rev + 1));
    drop((other, server));
    std::fs::remove_dir_all(&dir).unwrap();
}
