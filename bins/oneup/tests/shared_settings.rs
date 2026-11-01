//! End-to-end settings transfer against a loopback Graph stand-in only.
#![allow(clippy::unwrap_used)]
use oneup_app::shared_settings as settings;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[derive(Default)]
struct Data {
    content: Option<Vec<u8>>,
    revision: u64,
    puts: Vec<Option<String>>,
    race_next_put: bool,
}
struct Cloud {
    base: String,
    data: Arc<Mutex<Data>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Cloud {
    fn new() -> Self {
        oneup_app::setup::tls();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let data = Arc::new(Mutex::new(Data::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (d, s, b) = (data.clone(), stop.clone(), base.clone());
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = stream.unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                    assert!(header.len() < 64 * 1024);
                }
                let header = String::from_utf8(header).unwrap();
                let first = header.lines().next().unwrap();
                let path = first.split_whitespace().nth(1).unwrap();
                let field = |name: &str| {
                    header
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.eq_ignore_ascii_case(name))
                        .map(|(_, v)| v.trim().to_string())
                };
                let len = field("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut body = vec![0; len];
                stream.read_exact(&mut body).unwrap();
                let mut data = d.lock().unwrap();
                let (status, response) = if first.starts_with("PUT ") {
                    if data.race_next_put {
                        data.revision += 1;
                        data.race_next_put = false;
                    }
                    let tag = format!("e{}", data.revision);
                    let cond = field("if-match");
                    if data.content.is_some() && cond.as_deref() != Some(tag.as_str()) {
                        (412, b"{}".to_vec())
                    } else if data.content.is_some() && path.contains("items/root:") {
                        (409, b"{}".to_vec())
                    } else {
                        data.puts.push(cond);
                        data.content = Some(body);
                        data.revision += 1;
                        (200, serde_json::to_vec(&metadata(&data, &b)).unwrap())
                    }
                } else if path == "/download" {
                    (200, data.content.clone().unwrap())
                } else if path.contains("/root?") {
                    (200, serde_json::to_vec(&json!({"id":"root","name":"root","folder":{},"root":{}})).unwrap())
                } else if data.content.is_some() {
                    (200, serde_json::to_vec(&metadata(&data, &b)).unwrap())
                } else {
                    (404, b"{}".to_vec())
                };
                drop(data);
                write!(stream,"HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",response.len()).unwrap();
                stream.write_all(&response).unwrap();
            }
        });
        Self { base, data, stop, worker: Some(worker) }
    }
}
fn metadata(d: &Data, base: &str) -> Value {
    let bytes = d.content.as_ref().unwrap();
    json!({"id":"settings","name":"oneup.settings","size":bytes.len(),"eTag":format!("e{}",d.revision),
        "file":{"hashes":{"quickXorHash":oneup_hash::quick_xor_hash_base64(bytes)}},
        "parentReference":{"id":"root"},"@microsoft.graph.downloadUrl":format!("{base}/download")})
}
impl Drop for Cloud {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base.trim_start_matches("http://"));
        self.worker.take().unwrap().join().unwrap();
    }
}

#[tokio::test]
async fn reviewed_settings_use_cas_preserve_remote_edits_and_recover_a_committed_publication() {
    let cloud = Cloud::new();
    let graph = oneup_graph::Graph::for_tests(&cloud.base, 2).await;
    let dir = std::env::temp_dir().join(format!("oneup-shared-http-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let review = settings::preview(&dir, &graph).await.unwrap();
    assert!(review["cloud"].is_null());
    settings::publish(&dir, &graph, review["code"].as_str().unwrap(), true).await.unwrap();
    assert_eq!(cloud.data.lock().unwrap().puts, [None]);
    let review = settings::preview(&dir, &graph).await.unwrap();
    let mut local = review["local"].clone();
    local["rules"]["exclude"] = json!(["build/"]);
    settings::import_document(&dir, &local).unwrap();
    assert!(
        settings::import(&dir, &graph, review["code"].as_str().unwrap(), false).await.is_err(),
        "local changes invalidate a review"
    );
    settings::publish_changed(&dir, &graph).await.unwrap();
    assert_eq!(cloud.data.lock().unwrap().puts, [None, Some("e1".into())]);
    let review = settings::preview(&dir, &graph).await.unwrap();
    {
        let mut d = cloud.data.lock().unwrap();
        let mut remote: Value = serde_json::from_slice(d.content.as_ref().unwrap()).unwrap();
        remote["rules"]["exclude"] = json!(["other-pc/"]);
        d.content = Some(serde_json::to_vec(&remote).unwrap());
        d.revision += 1;
    }
    assert!(
        settings::publish(&dir, &graph, review["code"].as_str().unwrap(), true).await.is_err(),
        "remote changes invalidate a review"
    );
    local["rules"]["exclude"] = json!(["local-pc/"]);
    settings::import_document(&dir, &local).unwrap();
    assert!(
        settings::publish_changed(&dir, &graph).await.is_err(),
        "background publication cannot replace another PC's edit"
    );
    assert_eq!(cloud.data.lock().unwrap().puts.len(), 2);
    let p = settings::preview(&dir, &graph).await.unwrap();
    settings::import(&dir, &graph, p["code"].as_str().unwrap(), false).await.unwrap();
    assert_eq!(settings::portable(&dir).unwrap()["rules"]["exclude"], json!(["other-pc/"]));
    assert_eq!(settings::state(&dir)["enabled"], false);
    // Simulate an interruption after a successful commit but before intent
    // completion. Recovery must acknowledge it without another PUT.
    let st = settings::state(&dir);
    let intent = json!({"enabled":false,"root":"root","etag":"old-tag","pending":p["cloud"],"digest":"old"});
    oneup_app::ownfile::write(&dir.join("shared-settings.json"), &serde_json::to_vec(&intent).unwrap()).unwrap();
    settings::publish_changed(&dir, &graph).await.unwrap();
    assert_eq!(cloud.data.lock().unwrap().puts.len(), 2);
    assert_eq!(settings::state(&dir)["enabled"], false);
    assert_eq!(settings::state(&dir)["etag"], st["etag"]);
    let mut unreviewed = settings::portable(&dir).unwrap();
    unreviewed["rules"]["exclude"] = json!(["new-local/"]);
    settings::import_document(&dir, &unreviewed).unwrap();
    let p = settings::preview(&dir, &graph).await.unwrap();
    cloud.data.lock().unwrap().race_next_put = true;
    assert!(
        settings::publish(&dir, &graph, p["code"].as_str().unwrap(), false).await.is_err(),
        "a change between check and PUT must fail its If-Match"
    );
    assert_eq!(cloud.data.lock().unwrap().puts.len(), 2);
    assert_eq!(settings::portable(&dir).unwrap(), unreviewed, "the local choice survives a refused publication");
    let invalid_intent = json!({"enabled":false,"root":"root","etag":"e4","pending":{"version":999},"digest":"old"});
    oneup_app::ownfile::write(&dir.join("shared-settings.json"), &serde_json::to_vec(&invalid_intent).unwrap())
        .unwrap();
    assert!(
        settings::publish_changed(&dir, &graph).await.is_err(),
        "an unsupported recovered document must not be published"
    );
    assert_eq!(cloud.data.lock().unwrap().puts.len(), 2);
    std::fs::remove_dir_all(dir).unwrap();
}
