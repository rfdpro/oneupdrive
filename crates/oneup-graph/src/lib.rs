//! Microsoft Graph client for OneUpDrive.
//!
//! * [`auth`]: sign-in (authorization code + PKCE, loopback redirect) and
//!   token refresh; refresh tokens are stored DPAPI-encrypted.
//! * [`limiter`]: AIMD concurrency limiter shared by every request.
//! * [`client`]: GET with throttling/retry handling and the drive endpoints
//!   the mirror needs.
//! * [`model`]: the subset of `driveItem` OneUpDrive uses.
//! * [`push`]: the socket.io change-notification channel.
//!
//! The scope a sign-in asks for is chosen by the caller (`auth`):
//! `Files.ReadWrite` for sync, `Files.ReadWrite.All` only when the person
//! asks to sync folders others shared, and `Files.Read` for read-only use.

pub mod auth;
pub mod budget;
pub mod client;
pub mod fragment;
pub mod limiter;
pub mod model;
pub mod push;
pub mod throttle;

pub use auth::{Auth, DpapiFileStore, TokenStore};
pub use client::{Graph, NetStats};
pub use limiter::Aimd;
pub use model::{Drive, DriveItem, FileVersion, ItemSharing, Page, Permission, SharedFolder, SharedItem};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    /// Build with [`Error::refused`], which bounds `body`.
    #[error("Graph returned {status} for {what}: {body}")]
    Status { status: u16, what: String, body: String },
    /// A transfer received nothing for this many seconds (a connection
    /// that went dead without closing, e.g. after sleep or a NAT drop).
    #[error("network error: the {0} stalled (no data for {1} s)")]
    Stalled(&'static str, u64),
    #[error("unexpected response: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sign-in failed: {0}")]
    Auth(String),
    #[error("not signed in; run `oneup login`")]
    NotSignedIn,
    #[error("token storage: {0}")]
    Sys(#[from] oneup_sys::Error),
    #[error("token storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("gave up after {0} attempts")]
    Exhausted(u32),
    /// Writing downloaded content locally failed.
    #[error("writing the download: {0}")]
    Local(std::io::Error),
    /// This month's data budget on the network is used up: the transfer
    /// stopped, or wasn't started ([`budget`]).
    #[error("this month's data budget on this network is used up")]
    OverBudget,
    /// The push channel failed (never contains its URL, a credential).
    #[error("{0}")]
    Push(String),
    /// A request the caller asked for can't be made (nothing was sent).
    #[error("{0}")]
    Invalid(&'static str),
}

/// The most of a refused request's body an [`Error::Status`] keeps. The
/// error travels into problem reports and the control pipe's replies, whose
/// lines are bounded; a proxy's or captive portal's HTML page mustn't make
/// them fail. Graph's own error bodies are far smaller.
pub const MAX_ERROR_BODY: usize = 2048;

/// Reads the body of a refused response, but only as much of it as
/// [`Error::refused`] keeps (plus a little, so it knows the body was cut):
/// a huge page is never downloaded only to be thrown away.
pub(crate) async fn error_body(resp: reqwest::Response) -> reqwest::Result<Vec<u8>> {
    error_body_counted(resp, None).await
}

/// [`error_body`], telling `counted` each chunk's size as it arrives (also
/// of a read that fails later).
pub(crate) async fn error_body_counted(
    mut resp: reqwest::Response,
    counted: Option<&(dyn Fn(u64) + Sync)>,
) -> reqwest::Result<Vec<u8>> {
    let limit = MAX_ERROR_BODY + 4;
    let mut body = Vec::new();
    while body.len() < limit
        && let Some(chunk) = resp.chunk().await?
    {
        if let Some(c) = counted {
            c(chunk.len() as u64);
        }
        body.extend_from_slice(&chunk);
    }
    body.truncate(limit);
    Ok(body)
}

impl Error {
    /// [`Error::Status`] for a refused request, keeping at most
    /// [`MAX_ERROR_BODY`] bytes of its body (cut on a character boundary).
    pub fn refused(status: u16, what: impl Into<String>, mut body: String) -> Self {
        if body.len() > MAX_ERROR_BODY {
            let mut end = MAX_ERROR_BODY;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
            body.push_str("… (cut)");
        }
        Error::Status { status, what: what.into(), body }
    }

    /// Whether trying again later may succeed: the network failed, or the
    /// service was busy or failing (429, 5xx). Not a refusal.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Http(_) | Error::Stalled(..) => true,
            Error::Status { status, .. } => *status == 429 || (500..600).contains(status),
            _ => false,
        }
    }

    /// The HTTP status, if this error came from a response.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// OneDrive's error code from a refused request's body (e.g.
    /// `nameAlreadyExists`, `resourceModified`).
    pub fn code(&self) -> Option<String> {
        match self {
            Error::Status { body, .. } => error_code(body),
            _ => None,
        }
    }

    /// What went wrong, without anything from a response body or a URL:
    /// for records that outlive the moment (the activity log). A refused
    /// request's body can hold names, e-mail addresses or sharing links.
    pub fn summary(&self) -> String {
        match self {
            Error::Status { status, .. } => {
                // OneDrive's codes are identifiers (`accessDenied`); anything
                // else in that place isn't kept.
                let code = self.code().filter(|c| {
                    !c.is_empty() && c.len() <= 64 && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                });
                match code {
                    Some(c) => format!("OneDrive refused it ({status}, {c})"),
                    None => format!("OneDrive refused it ({status})"),
                }
            }
            Error::Http(_) | Error::Stalled(..) => "network error".into(),
            Error::Exhausted(n) => format!("gave up after {n} attempts"),
            Error::OverBudget => "data budget used up".into(),
            Error::Auth(_) => "sign-in failed".into(),
            Error::NotSignedIn => "not signed in".into(),
            Error::Invalid(what) => (*what).into(),
            Error::Json(_) => "unexpected answer from OneDrive".into(),
            Error::Sys(_) | Error::Io(_) | Error::Local(_) | Error::Push(_) => "error on this PC".into(),
        }
    }

    /// Whether OneDrive refused the request because it would touch too
    /// many items at once (see [`too_many_items`]).
    pub fn is_too_many_items(&self) -> bool {
        match self {
            Error::Status { body, .. } => serde_json::from_str(body).is_ok_and(|v| too_many_items(&v)),
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The `error.code` of a Graph error body.
pub fn error_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body).ok()?.pointer("/error/code")?.as_str().map(str::to_string)
}

/// Whether a Graph error body is OneDrive's refusal to change a folder
/// with too many items below it in one request: moving or renaming one
/// with more than its "list view threshold" (5,000) of descendants answers
/// 422 `notSupported`, inner code `tooManyResources`.
pub fn too_many_items(body: &serde_json::Value) -> bool {
    body.pointer("/error/innerError/code").and_then(|c| c.as_str()) == Some("tooManyResources")
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_list_view_threshold_refusal_is_recognised() {
        let body = r#"{"error":{"code":"notSupported","message":"The attempted operation is prohibited because it exceeds the list view threshold.","innerError":{"code":"tooManyResources","date":"2026-10-16T14:49:19"}}}"#;
        let e = super::Error::Status { status: 422, what: "x".into(), body: body.into() };
        assert!(e.is_too_many_items());
        let other =
            super::Error::Status { status: 422, what: "x".into(), body: r#"{"error":{"code":"notSupported"}}"#.into() };
        assert!(!other.is_too_many_items());
        assert!(!super::Error::Exhausted(8).is_too_many_items());
    }

    #[test]
    fn a_summary_keeps_nothing_from_the_body() {
        let body = r#"{"error":{"code":"accessDenied","message":"sam@example.com can't use https://1drv.ms/x?authkey=!SECRET"}}"#;
        let e = super::Error::refused(403, "https://graph.microsoft.com/v1.0/me/drive/items/A!1", body.into());
        assert_eq!(e.summary(), "OneDrive refused it (403, accessDenied)");
        let odd = super::Error::refused(400, "x", r#"{"error":{"code":"see https://1drv.ms/y"}}"#.into());
        assert_eq!(odd.summary(), "OneDrive refused it (400)", "a code that isn't an identifier isn't kept");
        assert_eq!(super::Error::Exhausted(8).summary(), "gave up after 8 attempts");
    }

    #[test]
    fn a_huge_error_body_is_cut_on_a_character_boundary() {
        // A captive portal's page, with a multi-byte character across the limit.
        let page = format!("{}é{}", "x".repeat(super::MAX_ERROR_BODY - 1), "<p>".repeat(100_000));
        let super::Error::Status { body, .. } = super::Error::refused(403, "x", page) else { unreachable!() };
        assert!(body.len() <= super::MAX_ERROR_BODY + 16, "kept {} bytes", body.len());
        assert!(body.starts_with(&"x".repeat(super::MAX_ERROR_BODY - 1)) && body.ends_with("(cut)"));
        // Graph's own bodies are kept whole, so their codes still parse.
        let small = r#"{"error":{"code":"resourceModified"}}"#;
        let e = super::Error::refused(412, "x", small.into());
        assert_eq!(e.code().as_deref(), Some("resourceModified"));
    }
}
