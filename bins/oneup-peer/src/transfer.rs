use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::ValueEnum;
use iroh::{
    Endpoint, RelayMode, SecretKey,
    endpoint::{Connection, presets},
};
#[cfg(any(windows, test))]
use iroh::{
    EndpointId,
    protocol::{AcceptError, ProtocolHandler, Router},
};
#[cfg(any(windows, test))]
use iroh_blobs::{BlobFormat, BlobsProtocol, Hash};
use iroh_blobs::{api::remote::GetProgressItem, get::Stats, store::fs::FsStore, ticket::BlobTicket};
use serde::Deserialize;
#[cfg(any(windows, test))]
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_stream::StreamExt as _;

#[derive(Clone, Copy, Default, ValueEnum, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    /// Use direct connections with public relay fallback
    #[default]
    Auto,
    /// Use direct connections only, without public servers
    Direct,
    /// Use public relays only
    Relay,
    /// Use loopback for a two-process test on one PC
    Loopback,
}

pub async fn endpoint(network: Network, key: SecretKey) -> Result<Endpoint> {
    // The ticket contains the sender's addresses. Neither peer publishes to a discovery service.
    let builder = Endpoint::builder(presets::Minimal).secret_key(key);
    let builder = match network {
        Network::Auto => builder.relay_mode(iroh::endpoint::default_relay_mode()),
        Network::Direct => builder.relay_mode(RelayMode::Disabled),
        Network::Relay => builder.relay_mode(iroh::endpoint::default_relay_mode()).clear_ip_transports(),
        Network::Loopback => builder.relay_mode(RelayMode::Disabled).clear_ip_transports().bind_addr("127.0.0.1:0")?,
    };
    builder.bind().await.context("Could not start the peer connection.")
}

#[cfg(any(windows, test))]
#[derive(Debug)]
struct RecipientOnly {
    recipient: EndpointId,
    blobs: BlobsProtocol,
    connections: AtomicUsize,
    managed: bool,
}

#[cfg(any(windows, test))]
impl ProtocolHandler for RecipientOnly {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if connection.remote_id() != self.recipient {
            connection.close(1u32.into(), b"Recipient not authorized");
            return Ok(());
        }
        let open = self.connections.fetch_add(1, Ordering::Relaxed) + 1;
        let guard = OpenConnection(&self.connections, self.managed);
        if open > 4 {
            connection.close(2u32.into(), b"Too many connections");
            return Ok(());
        }
        let mut timer = tokio::time::interval(Duration::from_secs(1));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let work = self.blobs.accept(connection.clone());
        tokio::pin!(work);
        let result = loop {
            tokio::select! {
                result = &mut work => break result,
                _ = timer.tick(), if self.managed => crate::emit(serde_json::json!({
                    "connections": self.connections.load(Ordering::Relaxed),
                    "route": route(&connection), "network_bytes": connection.stats().udp_tx.bytes,
                })),
            }
        };
        drop(guard);
        result
    }
}

#[cfg(any(windows, test))]
struct OpenConnection<'a>(&'a AtomicUsize, bool);

#[cfg(any(windows, test))]
impl Drop for OpenConnection<'_> {
    fn drop(&mut self) {
        let open = self.0.fetch_sub(1, Ordering::Relaxed) - 1;
        if self.1 {
            crate::emit(serde_json::json!({"connections": open}));
        }
    }
}

#[cfg(any(windows, test))]
pub async fn offer(
    store: &FsStore,
    hash: Hash,
    recipient: EndpointId,
    network: Network,
    managed: bool,
) -> Result<(Router, BlobTicket)> {
    use anyhow::ensure;
    let endpoint = endpoint(network, SecretKey::generate()).await?;
    if matches!(network, Network::Auto | Network::Relay) {
        let ready = tokio::time::timeout(Duration::from_secs(20), endpoint.online()).await;
        if ready.is_err() {
            ensure!(matches!(network, Network::Auto), "Could not connect to an Iroh relay.");
            eprintln!("Iroh relay unavailable. This ticket can only connect directly.");
        }
    }
    let addr = endpoint.addr();
    ensure!(!addr.addrs.is_empty(), "No reachable peer address is available.");
    // A fresh store serves exactly this snapshot. The blob protocol's default disables pushes.
    let handler =
        RecipientOnly { recipient, blobs: BlobsProtocol::new(store, None), connections: Default::default(), managed };
    let router = Router::builder(endpoint).accept(iroh_blobs::ALPN, handler).spawn();
    Ok((router, BlobTicket::new(addr, hash, BlobFormat::Raw)))
}

pub async fn fetch(
    endpoint: &Endpoint,
    store: &FsStore,
    ticket: &BlobTicket,
    mut progress: impl FnMut(u64, &'static str),
) -> Result<(Stats, &'static str)> {
    let connection =
        tokio::time::timeout(Duration::from_secs(30), endpoint.connect(ticket.addr().clone(), iroh_blobs::ALPN))
            .await
            .context("Peer connection timed out.")??;
    progress(0, route(&connection));
    let mut reported = Instant::now();
    let stream = store.remote().fetch(connection.clone(), ticket.hash_and_format()).stream();
    tokio::pin!(stream);
    while let Some(item) = stream.next().await {
        match item {
            GetProgressItem::Progress(bytes) if reported.elapsed() >= Duration::from_secs(1) => {
                reported = Instant::now();
                progress(bytes, route(&connection));
            }
            GetProgressItem::Progress(_) => {}
            GetProgressItem::Done(stats) => return Ok((stats, route(&connection))),
            GetProgressItem::Error(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("Transfer ended without completing.")
}

fn route(connection: &Connection) -> &'static str {
    let paths = connection.paths();
    match paths.iter().find(|p| p.is_selected()) {
        Some(path) if path.is_relay() => "relay",
        Some(path) if path.is_ip() => "direct",
        _ => "connecting",
    }
}
