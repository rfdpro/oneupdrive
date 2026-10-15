//! Names and places inside a vault.
//!
//! * Every folder has an ID (the top folder's is empty, others a random
//!   UUID). Its contents live in `d/XX/YYYY…`, from the Base32 SHA-1 of the
//!   AES-SIV-encrypted ID, so the vault's layout is flat and says nothing
//!   about the folder tree.
//! * A name is AES-SIV-encrypted with its parent folder's ID as associated
//!   data (so an entry moved to another folder doesn't decrypt), Base64url
//!   with padding, plus `.c9r`.
//! * A name longer than the shortening threshold is stored as
//!   `<Base64url SHA-1 of it>.c9s`, a folder holding `name.c9s` (the full
//!   name) and the entry (`contents.c9r` for a file).

use aes_siv::KeyInit as _;
use aes_siv::siv::Aes256Siv;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE as B64URL;
use sha1::{Digest, Sha1};
use unicode_normalization::UnicodeNormalization;

use crate::key::Masterkey;
use crate::{Error, Result};

/// The ID of the vault's top folder.
pub const ROOT_DIR_ID: &str = "";
/// In an entry's folder: a directory's ID.
pub const DIR_FILE: &str = "dir.c9r";
/// In an entry's folder: a symbolic link's encrypted target.
pub const SYMLINK_FILE: &str = "symlink.c9r";
/// In a shortened entry's folder: a file's contents.
pub const CONTENTS_FILE: &str = "contents.c9r";
/// In a shortened entry's folder: the full encrypted name.
pub const NAME_FILE: &str = "name.c9s";
/// In a folder's storage: an optional backup of its ID.
pub const DIR_ID_BACKUP: &str = "dirid.c9r";
pub const EXT: &str = ".c9r";
pub const SHORT_EXT: &str = ".c9s";

fn siv(key: &Masterkey) -> Aes256Siv {
    Aes256Siv::new(key.siv().as_ref().into())
}

/// Where the contents of the folder with this ID are stored, relative to
/// the vault's top folder (`d/XX/` + 30 characters).
pub fn dir_path(key: &Masterkey, dir_id: &str) -> Result<String> {
    let enc = siv(key)
        .encrypt(std::iter::empty::<&[u8]>(), dir_id.as_bytes())
        .map_err(|_| Error::Malformed("directory ID".into()))?;
    let hash = data_encoding::BASE32.encode(&Sha1::digest(&enc));
    Ok(format!("d/{}/{}", &hash[..2], &hash[2..32]))
}

/// The encrypted form of `name` in the folder with ID `parent` (`.c9r`
/// included), before shortening.
pub fn encrypt_name(key: &Masterkey, name: &str, parent: &str) -> Result<String> {
    let nfc: String = name.nfc().collect();
    let enc = siv(key).encrypt([parent.as_bytes()], nfc.as_bytes()).map_err(|_| Error::Malformed("name".into()))?;
    Ok(format!("{}{EXT}", B64URL.encode(enc)))
}

/// The name from its encrypted form (with `.c9r`), in the folder with ID
/// `parent`.
pub fn decrypt_name(key: &Masterkey, encrypted: &str, parent: &str) -> Result<String> {
    let body =
        encrypted.strip_suffix(EXT).ok_or_else(|| Error::Malformed(format!("{encrypted:?} isn't a .c9r name")))?;
    // Some tools drop the padding; both decode the same.
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body.trim_end_matches('='))
        .map_err(|_| Error::Malformed(format!("{encrypted:?} isn't Base64url")))?;
    let plain = siv(key).decrypt([parent.as_bytes()], &bytes).map_err(|_| Error::Tampered("a name"))?;
    String::from_utf8(plain).map_err(|_| Error::Malformed("a name isn't UTF-8".into()))
}

/// How an entry with this encrypted name is stored in its folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// Under the encrypted name itself.
    Full(String),
    /// Under `dir` (`….c9s`), with the encrypted name in its `name.c9s`.
    Short { dir: String, name: String },
}

impl Stored {
    pub fn of(encrypted: String, threshold: usize) -> Stored {
        if encrypted.len() <= threshold {
            return Stored::Full(encrypted);
        }
        let dir = format!("{}{SHORT_EXT}", B64URL.encode(Sha1::digest(encrypted.as_bytes())));
        Stored::Short { dir, name: encrypted }
    }

    /// The name in the folder.
    pub fn stored_name(&self) -> &str {
        match self {
            Stored::Full(n) => n,
            Stored::Short { dir, .. } => dir,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn key() -> Masterkey {
        Masterkey::from_keys([1; 32], [2; 32])
    }

    #[test]
    fn names_round_trip_only_in_their_folder() {
        let k = key();
        let e = encrypt_name(&k, "Report 2026.docx", "parent-id").unwrap();
        assert!(e.ends_with(".c9r"));
        assert_eq!(decrypt_name(&k, &e, "parent-id").unwrap(), "Report 2026.docx");
        // Deterministic: the same name in the same folder, the same result.
        assert_eq!(e, encrypt_name(&k, "Report 2026.docx", "parent-id").unwrap());
        assert!(matches!(decrypt_name(&k, &e, "other-id"), Err(Error::Tampered(_))));
        assert!(matches!(decrypt_name(&k, &e, ROOT_DIR_ID), Err(Error::Tampered(_))));
        // Normalised: both spellings of "é" are one name.
        assert_eq!(encrypt_name(&k, "caf\u{e9}", "p").unwrap(), encrypt_name(&k, "cafe\u{301}", "p").unwrap());
    }

    #[test]
    fn folder_paths_are_flat_and_fixed_length() {
        let k = key();
        let root = dir_path(&k, ROOT_DIR_ID).unwrap();
        let other = dir_path(&k, "6b5bd0a6-31ea-4d0b-9c5b-7d1e2f7b2f4f").unwrap();
        for p in [&root, &other] {
            assert_eq!(p.len(), "d/".len() + 2 + 1 + 30, "{p}");
            assert!(p[2..].chars().all(|c| c == '/' || c.is_ascii_uppercase() || ('2'..='7').contains(&c)));
        }
        assert_ne!(root, other);
    }

    #[test]
    fn long_names_are_shortened() {
        let k = key();
        let short = encrypt_name(&k, "a.txt", "p").unwrap();
        assert_eq!(Stored::of(short.clone(), 220), Stored::Full(short));
        let long = encrypt_name(&k, &"x".repeat(200), "p").unwrap();
        assert!(long.len() > 220);
        let Stored::Short { dir, name } = Stored::of(long.clone(), 220) else { panic!("not shortened") };
        assert_eq!(name, long);
        assert!(dir.ends_with(".c9s") && dir.len() == 28 + 4, "{dir}");
    }

    #[test]
    fn aes_siv_matches_rfc_5297() {
        // RFC 5297 appendix A.1 (AES-SIV-CMAC-256, one associated data
        // item), which checks the key order: S2V key first.
        let key = hex("fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
        let ad = hex("101112131415161718191a1b1c1d1e1f2021222324252627");
        let pt = hex("112233445566778899aabbccddee");
        let mut siv = aes_siv::siv::Aes128Siv::new(key.as_slice().into());
        let out = siv.encrypt([ad.as_slice()], &pt).unwrap();
        assert_eq!(out, hex("85632d07c6e8f37f950acd320a2ecc9340c02b9690c4dc04daef7f6afe5c"));
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
