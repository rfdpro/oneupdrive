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
pub mod client;
pub mod fragment;
pub mod limiter;
pub mod model;
pub mod push;
pub mod throttle;

pub use auth::{Auth, DpapiFileStore, TokenStore};
pub use client::{Graph, NetStats};
pub use limiter::Aimd;
pub use model::{Drive, DriveItem, FileVersion, Page, SharedFolder};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Graph returned {status} for {what}: {body}")]
    Status { status: u16, what: String, body: String },
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
    /// The push channel failed (never contains its URL, a credential).
    #[error("{0}")]
    Push(String),
}

impl Error {
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
}

pub type Result<T> = std::result::Result<T, Error>;

/// The `error.code` of a Graph error body.
pub fn error_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body).ok()?.pointer("/error/code")?.as_str().map(str::to_string)
}
