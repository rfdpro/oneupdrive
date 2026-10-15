//! Cryptomator vault format 8 (`docs/architecture/vaults.md`), implemented from the
//! published specification (docs.cryptomator.org, "Security Architecture"
//! and "Vault Cryptography"); no Cryptomator code was used.
//!
//! Supported: vaults whose key is in a password-protected
//! `masterkey.cryptomator` file, with the `SIV_GCM` cipher combination.
//! Not supported, refused with a clear error: Cryptomator Hub vaults (their
//! key loading is where the 2026 CVEs were), and `SIV_CTRMAC` vaults (the
//! older content encryption, which the specification doesn't describe).
//!
//! * [`key`]: the masterkey and its file (scrypt, AES key wrap, version MAC).
//! * [`config`]: `vault.cryptomator`, a JWT signed with the masterkey; its
//!   signature is checked before any field is trusted.
//! * [`names`]: directory IDs and paths, file names (AES-SIV bound to the
//!   parent directory), name shortening.
//! * [`content`]: file contents (AES-GCM, 32 KiB chunks bound to their
//!   number and the file header).
//! * [`folder`]: a vault in a local folder, for the `oneup vault` tools.
//!
//! Key material is zeroed when dropped (`zeroize`).

pub mod config;
pub mod content;
pub mod folder;
pub mod key;
pub mod names;
pub mod recovery;
pub mod sealed;

/// The vault format this crate reads and writes.
pub const FORMAT: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wrong password")]
    WrongPassword,
    /// Authentication failed: the data was changed, damaged, or belongs
    /// elsewhere (another file, another position, another vault).
    #[error("{0} failed its integrity check (damaged or changed)")]
    Tampered(&'static str),
    #[error("not a valid vault: {0}")]
    Malformed(String),
    #[error("unsupported vault: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| Error::Io(std::io::Error::other(format!("random: {e}"))))?;
    Ok(b)
}

/// A random UUID (version 4), as Cryptomator uses for directory IDs and
/// vault IDs.
pub fn new_uuid() -> Result<String> {
    let mut b: [u8; 16] = random()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn uuids_are_version_4() {
        let u = super::new_uuid().unwrap();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(u, super::new_uuid().unwrap());
    }
}
