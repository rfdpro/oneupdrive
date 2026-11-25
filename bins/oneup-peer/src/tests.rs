use std::{fs, time::Duration};

use anyhow::{Result, ensure};
use iroh::SecretKey;
use iroh_blobs::{
    protocol::{ChunkRangesSeq, GetRequest},
    store::fs::FsStore,
};

use crate::{Network, Ticket, disk, ticket, transfer};

#[test]
fn filenames_cannot_escape_the_inbox_or_name_devices() {
    for name in
        ["..", "../escape", "a/b", "a\\b", "file:stream", "CON.txt", "lpt9.zip", "COM¹.log", "x.", "x ", "a\n.txt"]
    {
        assert!(ticket::file_name(name).is_err(), "{name:?}");
    }
    assert!(ticket::file_name("Report № 5.txt").is_ok());
}

#[cfg(windows)]
#[tokio::test]
async fn bundle_preserves_both_files_when_their_names_collide() -> Result<()> {
    use std::io::Read as _;

    let temp = tempfile::tempdir()?;
    let first = temp.path().join("report.txt");
    let other = temp.path().join("other");
    fs::create_dir(&other)?;
    let second = other.join("Report.txt");
    fs::write(&first, b"first report")?;
    fs::write(&second, b"second report")?;
    let store = FsStore::load(temp.path().join("blobs")).await?;
    let (tag, size) = disk::import(&store, vec![first, second]).await?;
    let output = temp.path().join("archive.zip");
    assert_eq!(store.blobs().export(tag.hash(), &output).await?, size);
    let mut archive = zip::ZipArchive::new(fs::File::open(output)?)?;
    assert_eq!(archive.len(), 2);
    for (name, expected) in [("1/report.txt", "first report"), ("2/Report.txt", "second report")] {
        let mut contents = String::new();
        archive.by_name(name)?.read_to_string(&mut contents)?;
        assert_eq!(contents, expected);
    }
    store.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn receive_resumes_with_the_same_identity_and_publishes_once() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), direct_transfer()).await?
}

async fn direct_transfer() -> Result<()> {
    #[cfg(windows)]
    oneup_cfapi::expose_placeholders()?;
    let scratch = tempfile::tempdir()?;
    let transfer_root = scratch.path().join("transfers");
    let path = disk::scratch_root(&transfer_root)?;
    let source = path.join("source");
    let content: Vec<_> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    fs::write(&source, &content)?;
    let send_store = FsStore::load(path.join("send-store")).await?;
    #[cfg(windows)]
    let (tag, _) = disk::import(&send_store, vec![source.clone()]).await?;
    #[cfg(not(windows))]
    let tag = send_store.blobs().add_slice(&content).temp_tag().await?;
    // A later edit is irrelevant to the offered snapshot.
    fs::write(&source, b"new version")?;
    let (id, recipient) = disk::prepare(&path)?;
    let (router, blob) = transfer::offer(&send_store, tag.hash(), recipient, Network::Loopback, false).await?;
    let ticket = Ticket { blob, name: "test.bin".to_owned(), size: content.len() as u64 };
    let session = disk::Session::open(&path, &id)?;
    ensure!(disk::Session::open(&path, &id).is_err(), "Concurrent receive obtained the session lock");
    assert_eq!(session.output()?, path.join(&id).join("received"));
    assert!(session.set_destination(&path).is_err());
    let first_choice = scratch.path().join("first choice");
    fs::create_dir(&first_choice)?;
    session.set_destination(&first_choice)?;
    assert_eq!(session.ticket(|| Ok(ticket.clone()))?, ticket);
    let recv_store = FsStore::load(session.blob_dir()?).await?;

    let stranger = transfer::endpoint(Network::Loopback, SecretKey::generate()).await?;
    assert!(transfer::fetch(&stranger, &recv_store, &ticket.blob, |_, _| {}).await.is_err());
    stranger.close().await;
    assert_eq!(recv_store.remote().local(ticket.blob.hash_and_format()).await?.local_bytes(), 0);

    let endpoint = transfer::endpoint(Network::Loopback, session.key()?).await?;
    let connection = endpoint.connect(ticket.blob.addr().clone(), iroh_blobs::ALPN).await?;
    let request = GetRequest::new(ticket.blob.hash(), ChunkRangesSeq::verified_size());
    recv_store.remote().execute_get(connection, request).complete().await?;
    endpoint.close().await;
    let cached = recv_store.remote().local(ticket.blob.hash_and_format()).await?.local_bytes();
    assert!(cached > 0 && cached < content.len() as u64);
    recv_store.shutdown().await?;
    drop(session);

    let session = disk::Session::open(&path, &id)?;
    assert_eq!(session.output()?, first_choice);
    let ticket = session.ticket(|| anyhow::bail!("The ticket should have been saved"))?;
    let recv_store = FsStore::load(session.blob_dir()?).await?;
    assert_eq!(recv_store.remote().local(ticket.blob.hash_and_format()).await?.local_bytes(), cached);
    let endpoint = transfer::endpoint(Network::Loopback, session.key()?).await?;
    let (stats, path_type) = transfer::fetch(&endpoint, &recv_store, &ticket.blob, |_, _| {}).await?;
    assert_eq!(path_type, "direct");
    assert_eq!(stats.payload_bytes_read + cached, content.len() as u64);
    endpoint.close().await;
    router.shutdown().await?;
    send_store.shutdown().await?;

    let existing = first_choice.join("test.bin");
    fs::write(&existing, b"existing file")?;
    assert!(session.publish(&recv_store, &ticket).await.is_err());
    assert_eq!(fs::read(&existing)?, b"existing file");
    assert_eq!(session.saved(&ticket)?, None);
    let second_choice = scratch.path().join("second choice");
    fs::create_dir(&second_choice)?;
    session.set_destination(&second_choice)?;
    let dest = second_choice.join("test.bin");
    assert_eq!(session.publish(&recv_store, &ticket).await?, dest);
    assert_eq!(fs::read(&dest)?, content);
    #[cfg(windows)]
    assert_eq!(fs::read(dest.with_file_name("test.bin:Zone.Identifier"))?, b"[ZoneTransfer]\r\nZoneId=3\r\n");
    recv_store.shutdown().await?;
    drop(session);
    assert_eq!(disk::Session::open(&path, &id)?.saved(&ticket)?, Some(dest));
    Ok(())
}
