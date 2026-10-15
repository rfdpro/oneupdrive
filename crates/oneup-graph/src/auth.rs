//! Sign-in and token management for personal Microsoft accounts.
//!
//! Interactive sign-in uses the authorization-code flow with PKCE and a
//! loopback redirect (RFC 8252). The refresh token is persisted through a
//! [`TokenStore`]. The default store encrypts it with DPAPI for the current
//! Windows user. Access tokens live only in memory.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use sha2::Digest as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::{Error, Result};

const AUTHORITY: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";

/// Read-only by construction: this scope cannot modify OneDrive.
pub const READ_ONLY_SCOPE: &str = "Files.Read offline_access";

/// Write access, which every sign-in asks for (a pre-release build asks for
/// it only for a dedicated **test account**, see the CLI's `test_account`
/// module).
pub const READ_WRITE_SCOPE: &str = "Files.ReadWrite offline_access";
/// [`READ_WRITE_SCOPE`] plus the files others share with the account
/// (folder pairs of shared folders). Asked for only when the user wants
/// that, since it's a broader permission.
pub const READ_WRITE_ALL_SCOPE: &str = "Files.ReadWrite.All offline_access";

/// Where the refresh token lives between runs.
pub trait TokenStore: Send + Sync {
    fn load(&self) -> Result<Option<String>>;
    fn save(&self, refresh_token: &str) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

/// Refresh token in a file, encrypted with DPAPI for the current Windows user.
#[derive(Debug, Clone)]
pub struct DpapiFileStore {
    path: PathBuf,
}

impl DpapiFileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl TokenStore for DpapiFileStore {
    fn load(&self) -> Result<Option<String>> {
        match std::fs::read(&self.path) {
            Ok(sealed) => {
                let plain = oneup_sys::dpapi::unprotect(&sealed)?;
                Ok(Some(String::from_utf8(plain).map_err(|_| Error::Auth("stored token is not UTF-8".into()))?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, refresh_token: &str) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let sealed = oneup_sys::dpapi::protect(refresh_token.as_bytes())?;
        // Write-then-rename so a crash never leaves a truncated token file.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, sealed)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

struct Access {
    token: String,
    refresh_at: Instant,
}

pub struct Auth {
    http: reqwest::Client,
    client_id: String,
    scope: String,
    store: Box<dyn TokenStore>,
    /// Serialises refreshes so concurrent requests don't stampede the token endpoint.
    access: Mutex<Option<Access>>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth").field("scope", &self.scope).finish_non_exhaustive()
    }
}

impl Auth {
    pub fn new(http: reqwest::Client, client_id: String, scope: &str, store: Box<dyn TokenStore>) -> Self {
        Self { http, client_id, scope: scope.to_string(), store, access: Mutex::new(None) }
    }

    pub fn is_signed_in(&self) -> Result<bool> {
        Ok(self.store.load()?.is_some())
    }

    pub async fn sign_out(&self) -> Result<()> {
        *self.access.lock().await = None;
        self.store.clear()
    }

    /// Returns a valid access token, refreshing it if needed.
    pub async fn access_token(&self) -> Result<String> {
        let mut guard = self.access.lock().await;
        if let Some(a) = guard.as_ref() {
            if Instant::now() < a.refresh_at {
                return Ok(a.token.clone());
            }
        }
        let rt = self.store.load()?.ok_or(Error::NotSignedIn)?;
        let t = self
            .token_request(&[
                ("grant_type", "refresh_token"),
                ("client_id", &self.client_id),
                ("refresh_token", &rt),
                ("scope", &self.scope),
            ])
            .await?;
        let token = t.access_token.clone();
        *guard = Some(self.accept(t)?);
        Ok(token)
    }

    /// Forces the next request to refresh (after a 401).
    pub async fn invalidate(&self) {
        *self.access.lock().await = None;
    }

    fn accept(&self, t: TokenResponse) -> Result<Access> {
        if let Some(rt) = &t.refresh_token {
            self.store.save(rt)?;
        }
        Ok(Access {
            token: t.access_token,
            refresh_at: Instant::now() + Duration::from_secs(t.expires_in.saturating_sub(300)),
        })
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenResponse> {
        let resp = self.http.post(format!("{AUTHORITY}/token")).form(form).send().await?;
        if resp.status().is_success() {
            return Ok(resp.json().await?);
        }
        let status = resp.status().as_u16();
        let e: OAuthError =
            resp.json().await.unwrap_or(OAuthError { error: format!("HTTP {status}"), error_description: None });
        if e.error == "invalid_grant" {
            // Refresh token revoked or expired: the user must sign in again.
            return Err(Error::NotSignedIn);
        }
        Err(Error::Auth(format!("{} {}", e.error, e.error_description.unwrap_or_default())))
    }

    /// Interactive sign-in in the default browser. `open_browser` receives
    /// the sign-in URL (the caller decides how to open or print it). With
    /// `choose_account`, Microsoft shows its account picker even if the
    /// browser is already signed in, so a different account can be used
    /// without signing the browser out of the usual one.
    pub async fn sign_in_with_browser(&self, choose_account: bool, open_browser: impl FnOnce(&str)) -> Result<()> {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let random = |n: usize| -> Result<String> {
            let mut b = vec![0u8; n];
            getrandom::fill(&mut b).map_err(|e| Error::Auth(format!("random: {e}")))?;
            Ok(b64.encode(b))
        };
        let verifier = random(32)?;
        let challenge = b64.encode(sha2::Sha256::digest(verifier.as_bytes()));
        let state = random(16)?;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let redirect = format!("http://localhost:{}", listener.local_addr()?.port());
        let mut params = vec![
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("response_mode", "query"),
            ("scope", self.scope.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
        ];
        if choose_account {
            params.push(("prompt", "select_account"));
        }
        let url = url::Url::parse_with_params(&format!("{AUTHORITY}/authorize"), &params)
            .map_err(|e| Error::Auth(e.to_string()))?;
        open_browser(url.as_str());

        let code = tokio::time::timeout(Duration::from_secs(600), accept_redirect(&listener, &state))
            .await
            .map_err(|_| Error::Auth("timed out waiting for the browser".into()))??;

        let t = self
            .token_request(&[
                ("grant_type", "authorization_code"),
                ("client_id", &self.client_id),
                ("code", &code),
                ("redirect_uri", &redirect),
                ("code_verifier", &verifier),
                ("scope", &self.scope),
            ])
            .await?;
        if t.refresh_token.is_none() {
            return Err(Error::Auth("no refresh token returned (is offline_access granted?)".into()));
        }
        *self.access.lock().await = Some(self.accept(t)?);
        Ok(())
    }
}

/// Waits for the browser's redirect and returns the authorization code.
async fn accept_redirect(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 16 * 1024];
        let n = sock.read(&mut buf).await?;
        let req = String::from_utf8_lossy(&buf[..n]);
        let target = req.split_whitespace().nth(1).unwrap_or("/");
        let Ok(u) = url::Url::parse(&format!("http://localhost{target}")) else { continue };
        let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
        if q.is_empty() {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
            continue;
        }
        let ok = q.get("state").map(String::as_str) == Some(state) && q.contains_key("code");
        let body = if ok {
            "Signed in to OneUpDrive. You can close this tab."
        } else {
            "OneUpDrive sign-in failed. See the terminal for details."
        };
        let _ = sock
            .write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                    .as_bytes(),
            )
            .await;
        if let Some(err) = q.get("error") {
            return Err(Error::Auth(format!("{err} {}", q.get("error_description").cloned().unwrap_or_default())));
        }
        if q.get("state").map(String::as_str) != Some(state) {
            return Err(Error::Auth("state mismatch; aborting (possible CSRF)".into()));
        }
        return q.get("code").cloned().ok_or_else(|| Error::Auth("no code in redirect".into()));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn dpapi_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("oneup-auth-test-{}", std::process::id()));
        let store = DpapiFileStore::new(dir.join("token.dpapi"));
        assert_eq!(store.load().unwrap(), None);
        store.save("rt-123").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("rt-123"));
        assert_ne!(std::fs::read(dir.join("token.dpapi")).unwrap(), b"rt-123");
        store.clear().unwrap();
        store.clear().unwrap();
        assert_eq!(store.load().unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
