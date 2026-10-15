//! A vault's keys sealed with a 256-bit secret (Windows Hello's, see
//! `oneup_sys::hello`), for unlocking without the password: AES-256-GCM
//! with a random nonce, the purpose as associated data. Layout: a random
//! 32-byte challenge (what Windows Hello signs; not secret), the 12-byte
//! nonce, then the 64 encrypted key bytes and the 16-byte tag.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use zeroize::Zeroizing;

use crate::key::Masterkey;
use crate::{Error, Result, random};

const AAD: &[u8] = b"OneUpDrive vault keys, sealed v1";
const LEN: usize = 32 + 12 + 64 + 16;

/// A new challenge for [`seal`].
pub fn new_challenge() -> Result<[u8; 32]> {
    random()
}

/// The challenge a sealed blob was made with.
pub fn challenge(blob: &[u8]) -> Result<[u8; 32]> {
    if blob.len() != LEN {
        return Err(Error::Malformed("not a sealed vault key".into()));
    }
    let mut c = [0u8; 32];
    c.copy_from_slice(&blob[..32]);
    Ok(c)
}

/// Seals `key` with `secret`; `challenge` is stored in front.
pub fn seal(key: &Masterkey, secret: &[u8; 32], challenge: &[u8; 32]) -> Result<Vec<u8>> {
    let nonce: [u8; 12] = random()?;
    let cipher = Aes256Gcm::new(secret.into());
    let raw = key.export();
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: raw.as_ref(), aad: AAD })
        .map_err(|_| Error::Malformed("sealing the keys".into()))?;
    let mut out = Vec::with_capacity(LEN);
    out.extend_from_slice(challenge);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Opens a blob [`seal`] made; a wrong secret or a changed blob fails.
pub fn open(blob: &[u8], secret: &[u8; 32]) -> Result<Masterkey> {
    challenge(blob)?;
    let cipher = Aes256Gcm::new(secret.into());
    let raw = Zeroizing::new(
        cipher
            .decrypt(Nonce::from_slice(&blob[32..44]), Payload { msg: &blob[44..], aad: AAD })
            .map_err(|_| Error::Tampered("the sealed vault keys"))?,
    );
    let raw: &[u8; 64] = raw.as_slice().try_into().map_err(|_| Error::Malformed("sealed keys".into()))?;
    Ok(Masterkey::import(raw))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn sealed_keys_open_only_with_the_same_secret() {
        let key = Masterkey::generate().unwrap();
        let c = new_challenge().unwrap();
        let blob = seal(&key, &[7; 32], &c).unwrap();
        assert_eq!(challenge(&blob).unwrap(), c);
        assert_eq!(*open(&blob, &[7; 32]).unwrap().export(), *key.export());
        assert!(open(&blob, &[8; 32]).is_err());
        let mut bad = blob.clone();
        bad[50] ^= 1;
        assert!(open(&bad, &[7; 32]).is_err());
        assert!(open(&blob[..40], &[7; 32]).is_err());
    }
}
