//! Push notifications: OneDrive's socket.io channel.
//!
//! `GET /me/drive/root/subscriptions/socketIo` returns a `notificationUrl`.
//! Connecting a socket.io client to it delivers an event whenever something
//! in the drive changes. An event says only *that* something changed (no
//! item details), so its one use is to trigger the same delta read the
//! mirror does anyway. Push never carries data into the trees, and if it
//! fails, polling the delta feed still finds everything, only later.
//!
//! Only the small part of the protocol that's needed is implemented, from
//! the public Engine.IO v4 / Socket.IO v5 specifications:
//! * Engine.IO packets are text frames whose first character is the type:
//!   `0` open (JSON with `pingInterval`/`pingTimeout`), `1` close, `2` ping
//!   (the *server* pings in v4), `3` pong, `4` message, `6` noop.
//! * A message's next character is the Socket.IO packet type: `0` connect
//!   (we send `40/ns,` to join the namespace, the server acknowledges with
//!   the same), `1` disconnect, `2` event, `4` connect error.
//!
//! Measured: v4 on the first try; the server pings every
//! 30 s; a change on onedrive.com arrives about 16 s later.
//!
//! **The URL is a credential** (its query holds a token): it's never logged
//! or put in an error message, and [`Endpoint`]'s `Debug` hides it.

use std::fmt;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::client::GRAPH;
use crate::{Error, Graph, Result};

/// Where to connect: derived from a `notificationUrl`.
pub struct Endpoint {
    /// `wss://host/socket.io/?EIO=4&transport=websocket&<query>`. Secret.
    ws_url: String,
    /// The socket.io namespace, `/notifications` in practice.
    namespace: String,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint").field("namespace", &self.namespace).finish_non_exhaustive()
    }
}

impl Endpoint {
    /// Parses a `notificationUrl`. The documented shape is
    /// `https://host/<namespace>/callback?<query>`; the shape OneDrive
    /// Personal actually returned is `https://host/<namespace>?<query>`.
    /// Both are accepted: path → namespace, query → passed through.
    pub fn parse(notification_url: &str) -> Result<Self> {
        let bad = || Error::Push("unusable push address".into());
        let (base, query) = match notification_url.split_once("/callback?") {
            Some((base, q)) => (base, q.to_owned()),
            None => {
                let u = url::Url::parse(notification_url).map_err(|_| bad())?;
                let q = u.query().unwrap_or_default().to_owned();
                let base = notification_url.split('?').next().unwrap_or_default();
                (base, q)
            }
        };
        let u = url::Url::parse(base).map_err(|_| bad())?;
        if u.scheme() != "https" {
            return Err(bad());
        }
        let host = u.host_str().ok_or_else(bad)?;
        let namespace = u.path().trim_end_matches('/').to_owned();
        let sep = if query.is_empty() { "" } else { "&" };
        let ws_url = format!("wss://{host}/socket.io/?EIO=4&transport=websocket{sep}{query}");
        Ok(Self { ws_url, namespace })
    }

    fn join_frame(&self) -> String {
        if self.namespace.is_empty() || self.namespace == "/" { "40".into() } else { format!("40{},", self.namespace) }
    }
}

impl Graph {
    /// Asks OneDrive for a push channel for the whole drive.
    pub async fn push_endpoint(&self) -> Result<Endpoint> {
        let v: serde_json::Value = self.get(&format!("{GRAPH}/me/drive/root/subscriptions/socketIo")).await?;
        let url = v
            .get("notificationUrl")
            .and_then(|u| u.as_str())
            .ok_or_else(|| Error::Push("no notificationUrl".into()))?;
        Endpoint::parse(url)
    }
}

/// What one text frame means to us.
#[derive(Debug, Clone, PartialEq)]
enum Frame {
    /// Engine.IO open; the server pings every `ping` and gives up after
    /// `ping + timeout` without a pong.
    Open {
        ping: Duration,
        timeout: Duration,
    },
    /// The server's ping: answer with a pong.
    Ping,
    /// The namespace join was accepted.
    Joined,
    /// A change notification.
    Event,
    /// The server refused the namespace, or is closing.
    Refused,
    Closed,
    /// Anything else (pong, noop, binary placeholders): ignored.
    Other,
}

fn classify(text: &str) -> Frame {
    let mut chars = text.chars();
    match chars.next() {
        Some('0') => {
            let v: serde_json::Value = serde_json::from_str(chars.as_str()).unwrap_or_default();
            let ms = |k: &str, d: u64| Duration::from_millis(v.get(k).and_then(|x| x.as_u64()).unwrap_or(d));
            // Engine.IO v4's defaults, if the server leaves them out.
            Frame::Open { ping: ms("pingInterval", 25_000), timeout: ms("pingTimeout", 20_000) }
        }
        Some('1') => Frame::Closed,
        Some('2') => Frame::Ping,
        Some('4') => match chars.next() {
            Some('0') => Frame::Joined,
            Some('1') => Frame::Closed,
            Some('2') => Frame::Event,
            Some('4') => Frame::Refused,
            _ => Frame::Other,
        },
        _ => Frame::Other,
    }
}

/// Error text for a WebSocket failure, without the URL (a credential): the
/// URL-carrying variants are replaced by a fixed message.
fn ws_error(e: tokio_tungstenite::tungstenite::Error) -> Error {
    use tokio_tungstenite::tungstenite::Error as W;
    Error::Push(match e {
        W::Url(_) => "unusable push address".into(),
        W::Http(r) => format!("push connection refused (HTTP {})", r.status().as_u16()),
        other => format!("push connection: {other}"),
    })
}

/// Connects and listens until the channel ends. Calls `joined` once the
/// subscription is live and `changed` for every notification. Returns
/// `Ok` when the server closes the channel normally (the caller asks for a
/// new endpoint and reconnects), `Err` on a failure, including a server
/// that goes silent for longer than its own ping deadline.
pub async fn listen(ep: &Endpoint, mut joined: impl FnMut(), mut changed: impl FnMut()) -> Result<()> {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
    let (mut ws, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(ep.ws_url.as_str()))
        .await
        .map_err(|_| Error::Push("push connection timed out".into()))?
        .map_err(ws_error)?;
    // Until the open packet says otherwise: a generous first deadline.
    let mut silence = Duration::from_secs(60);
    loop {
        let msg = match tokio::time::timeout(silence, ws.next()).await {
            Err(_) => return Err(Error::Push("the push channel went silent".into())),
            Ok(None) => return Ok(()),
            Ok(Some(m)) => m.map_err(ws_error)?,
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return Ok(()),
            // tungstenite answers WebSocket-level pings itself.
            _ => continue,
        };
        match classify(text.as_str()) {
            Frame::Open { ping, timeout } => {
                silence = ping + timeout;
                ws.send(Message::text(ep.join_frame())).await.map_err(ws_error)?;
            }
            Frame::Ping => ws.send(Message::text("3")).await.map_err(ws_error)?,
            Frame::Joined => joined(),
            Frame::Event => changed(),
            Frame::Refused => return Err(Error::Push("the push channel refused the subscription".into())),
            Frame::Closed => return Ok(()),
            Frame::Other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_url_shapes_parse_and_the_secret_stays_hidden() {
        // The shape OneDrive Personal returned when measured.
        let e = Endpoint::parse("https://eu1.pushnp.svc.ms/notifications?token=SECRET&applicationId=app").unwrap();
        assert_eq!(e.namespace, "/notifications");
        assert_eq!(
            e.ws_url,
            "wss://eu1.pushnp.svc.ms/socket.io/?EIO=4&transport=websocket&token=SECRET&applicationId=app"
        );
        assert_eq!(e.join_frame(), "40/notifications,");
        assert!(!format!("{e:?}").contains("SECRET"));
        // The documented shape.
        let e = Endpoint::parse("https://host.example/ns/callback?token=T").unwrap();
        assert_eq!(
            (e.namespace.as_str(), e.ws_url.as_str()),
            ("/ns", "wss://host.example/socket.io/?EIO=4&transport=websocket&token=T")
        );
        // The default namespace joins without a name.
        assert_eq!(Endpoint::parse("https://h.example/?a=b").unwrap().join_frame(), "40");
        // Only HTTPS, and the error doesn't echo the URL.
        let err = Endpoint::parse("http://h.example/n?token=SECRET").unwrap_err().to_string();
        assert!(!err.contains("SECRET"));
    }

    #[test]
    fn frames_are_classified() {
        assert_eq!(
            classify(r#"0{"sid":"x","pingInterval":30000,"pingTimeout":5000}"#),
            Frame::Open { ping: Duration::from_secs(30), timeout: Duration::from_secs(5) }
        );
        assert_eq!(classify("0{}"), Frame::Open { ping: Duration::from_secs(25), timeout: Duration::from_secs(20) });
        assert_eq!(classify("2"), Frame::Ping);
        assert_eq!(classify(r#"40/notifications,{"sid":"y"}"#), Frame::Joined);
        assert_eq!(classify(r#"42/notifications,["notification","{}"]"#), Frame::Event);
        assert_eq!(classify(r#"44/notifications,{"message":"no"}"#), Frame::Refused);
        assert_eq!(classify("1"), Frame::Closed);
        assert_eq!(classify("41/notifications,"), Frame::Closed);
        assert_eq!(classify("3"), Frame::Other);
        assert_eq!(classify(""), Frame::Other);
    }
}
