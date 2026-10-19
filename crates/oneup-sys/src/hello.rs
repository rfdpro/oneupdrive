//! Windows Hello (face, fingerprint or PIN) as a key: a secret only Windows
//! Hello can produce, for unlocking a vault without typing its password.
//!
//! Windows Hello keeps a private key (in the TPM where there is one) per
//! credential name, and signs only after the person proves it's them. The
//! signature of a fixed challenge (RSA PKCS #1 v1.5, deterministic) hashed
//! with SHA-256 is the secret: the same every time, and impossible to get
//! without Windows Hello on this PC for this user. The secret is never
//! stored; what it protects is (by the caller, encrypted with it).

use windows::Security::Credentials::{
    KeyCredentialCreationOption, KeyCredentialManager, KeyCredentialStatus, UI::UserConsentVerifierAvailability,
};
use windows::Security::Cryptography::CryptographicBuffer;
use windows::core::HSTRING;

/// Whether this PC has Windows Hello set up for the user.
pub fn available() -> bool {
    let supported = KeyCredentialManager::IsSupportedAsync().and_then(|op| op.join()).unwrap_or(false);
    let consent = windows::Security::Credentials::UI::UserConsentVerifier::CheckAvailabilityAsync()
        .and_then(|op| op.join())
        .is_ok_and(|a| a == UserConsentVerifierAvailability::Available);
    supported && consent
}

/// What went wrong.
#[derive(Debug, thiserror::Error)]
pub enum HelloError {
    #[error("Windows Hello isn't set up on this PC")]
    NotAvailable,
    #[error("Windows Hello was cancelled")]
    Cancelled,
    #[error("no Windows Hello key for this vault on this PC; set it up again")]
    NotFound,
    #[error("Windows Hello: {0}")]
    Other(String),
}

fn status(s: KeyCredentialStatus) -> Result<(), HelloError> {
    match s {
        KeyCredentialStatus::Success => Ok(()),
        KeyCredentialStatus::UserCanceled | KeyCredentialStatus::UserPrefersPassword => Err(HelloError::Cancelled),
        KeyCredentialStatus::NotFound => Err(HelloError::NotFound),
        KeyCredentialStatus::SecurityDeviceLocked => Err(HelloError::Other("the security device is locked".into())),
        other => Err(HelloError::Other(format!("status {}", other.0))),
    }
}

fn other(e: windows::core::Error) -> HelloError {
    HelloError::Other(e.message().to_string())
}

/// The secret for credential `name` (see the module docs). `create`: make
/// the credential first (replacing one of that name); otherwise it must
/// exist. Shows Windows Hello's prompt and waits for the person.
pub fn secret(name: &str, challenge: &[u8], create: bool) -> Result<[u8; 32], HelloError> {
    use sha2::{Digest, Sha256};
    if !KeyCredentialManager::IsSupportedAsync().and_then(|op| op.join()).unwrap_or(false) {
        return Err(HelloError::NotAvailable);
    }
    let name = HSTRING::from(name);
    let result = if create {
        KeyCredentialManager::RequestCreateAsync(&name, KeyCredentialCreationOption::ReplaceExisting)
            .and_then(|op| op.join())
            .map_err(other)?
    } else {
        KeyCredentialManager::OpenAsync(&name).and_then(|op| op.join()).map_err(other)?
    };
    status(result.Status().map_err(other)?)?;
    let credential = result.Credential().map_err(other)?;
    let data = CryptographicBuffer::CreateFromByteArray(challenge).map_err(other)?;
    let signed = credential.RequestSignAsync(&data).and_then(|op| op.join()).map_err(other)?;
    status(signed.Status().map_err(other)?)?;
    let buffer = signed.Result().map_err(other)?;
    let mut bytes = windows::core::Array::<u8>::new();
    CryptographicBuffer::CopyToByteArray(&buffer, &mut bytes).map_err(other)?;
    let mut digest = Sha256::digest(bytes.as_slice());
    // The signature is as good as the secret (the secret is its hash): both
    // wiped. The WinRT buffer it came in can't be.
    zeroize::Zeroize::zeroize(&mut bytes[..]);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    zeroize::Zeroize::zeroize(&mut digest[..]);
    Ok(out)
}

/// Removes credential `name` (nothing if there's none).
pub fn delete(name: &str) {
    let _ = KeyCredentialManager::DeleteAsync(&HSTRING::from(name)).and_then(|op| op.join());
}

#[cfg(test)]
mod tests {
    #[test]
    fn asking_whether_hello_is_there_never_prompts() {
        // No prompt, whatever the answer; the prompts themselves need a person.
        let _ = super::available();
    }
}
