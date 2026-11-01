//! The **recovery key**: the vault's two keys written out as text, so a
//! forgotten password isn't the end of the vault. With it a new password
//! can be set (a new `masterkey.cryptomator` wrapping the same keys; nothing
//! is re-encrypted).
//!
//! OneUpDrive's own format: Cryptomator's recovery key encoding isn't in its
//! published specification, so the two can't read each other's. A vault is
//! still the same Cryptomator vault; only this text is OneUpDrive's.
//!
//! Layout: `OUD1-` then the 64 key bytes and a 4-byte check (the start of
//! SHA-256 over a label and the keys), in Base32 (RFC 4648, no padding) in
//! groups of four. Reading is forgiving: case, spaces, dashes and the
//! look-alikes 0/O, 1/I, 8/B don't matter, and the check catches typos.
//! Which vault the keys belong to is checked by the caller against the
//! vault's signed configuration (`config::Config::verify`).

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::key::Masterkey;
use crate::{Error, Result};

const PREFIX: &str = "OUD1";
const LABEL: &[u8] = b"OneUpDrive vault recovery key v1";

fn check(raw: &[u8; 64]) -> [u8; 4] {
    let mut h = Sha256::new();
    h.update(LABEL);
    h.update(raw);
    let d = h.finalize();
    [d[0], d[1], d[2], d[3]]
}

/// The recovery key of `key`, for showing once and writing down.
pub fn recovery_key(key: &Masterkey) -> Zeroizing<String> {
    let raw = key.export();
    let mut bytes = Zeroizing::new(Vec::with_capacity(68));
    bytes.extend_from_slice(raw.as_ref());
    bytes.extend_from_slice(&check(&raw));
    let text = Zeroizing::new(data_encoding::BASE32_NOPAD.encode(&bytes));
    // Sized up front: a string that grows leaves its old buffer unwiped.
    let mut out = Zeroizing::new(String::with_capacity(PREFIX.len() + text.len() + text.len().div_ceil(4)));
    out.push_str(PREFIX);
    for chunk in text.as_bytes().chunks(4) {
        out.push('-');
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    out
}

/// The keys in a recovery key, if it is one and has no typo.
pub fn from_recovery_key(text: &str) -> Result<Masterkey> {
    let bad = || Error::Malformed("that isn't a OneUpDrive recovery key".into());
    // Sized up front (never longer than the text): see `recovery_key`.
    let mut clean = Zeroizing::new(String::with_capacity(text.len()));
    clean.extend(text.chars().filter(|c| !c.is_whitespace() && *c != '-').map(|c| match c.to_ascii_uppercase() {
        '0' => 'O',
        '1' => 'I',
        '8' => 'B',
        c => c,
    }));
    let body = clean.strip_prefix("OUDI").ok_or_else(bad)?;
    let bytes = Zeroizing::new(data_encoding::BASE32_NOPAD.decode(body.as_bytes()).map_err(|_| bad())?);
    if bytes.len() != 68 {
        return Err(bad());
    }
    let mut raw = Zeroizing::new([0u8; 64]);
    raw.copy_from_slice(&bytes[..64]);
    // Not secret-dependent in a way that matters (the person typing it has
    // the key), but compared in constant time all the same.
    if !bool::from(subtle::ConstantTimeEq::ct_eq(&check(&raw)[..], &bytes[64..])) {
        return Err(Error::Malformed("the recovery key has a typo (its check doesn't match)".into()));
    }
    Ok(Masterkey::import(&raw))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_recovery_key_round_trips_and_forgives_how_it_was_typed() {
        let key = Masterkey::generate().unwrap();
        let text = recovery_key(&key);
        assert!(text.starts_with("OUD1-"));
        assert_eq!(text.len(), text.capacity(), "sized exactly, never regrown");
        let back = from_recovery_key(&text).unwrap();
        assert_eq!(*back.export(), *key.export());
        // Lower case, spaces instead of dashes, look-alike digits.
        let sloppy = text.to_lowercase().replace('-', " ").replace('o', "0").replace('i', "1");
        assert_eq!(*from_recovery_key(&sloppy).unwrap().export(), *key.export());
    }

    #[test]
    fn typos_and_other_text_are_refused() {
        let text = recovery_key(&Masterkey::generate().unwrap()).to_string();
        let i = text.len() - 3;
        let flipped: String =
            text.char_indices().map(|(j, c)| if j == i { if c == 'A' { 'C' } else { 'A' } } else { c }).collect();
        assert!(from_recovery_key(&flipped).unwrap_err().to_string().contains("typo"));
        assert!(from_recovery_key("hello").is_err());
        assert!(from_recovery_key(&text[..text.len() - 5]).is_err());
    }
}
