//! `vault.cryptomator`: a JWT naming the vault's format, cipher combination
//! and key, signed (HMAC) with the raw masterkey (encryption key, then MAC
//! key). Opening reads the key's location unverified, loads the key, then
//! verifies the signature **before** trusting any other field.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Sha384, Sha512};

use crate::key::Masterkey;
use crate::{Error, FORMAT, Result};

/// The configuration file's name in the vault's top folder.
pub const FILE: &str = "vault.cryptomator";
/// The key location of a masterkey-file vault.
pub const KID_MASTERKEY_FILE: &str = "masterkeyfile:masterkey.cryptomator";
/// Encrypted names longer than this are shortened (Cryptomator's default).
pub const SHORTENING_THRESHOLD: usize = 220;
const SIV_GCM: &str = "SIV_GCM";

/// A verified configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The vault's random ID.
    pub jti: String,
    pub shortening_threshold: usize,
}

#[derive(Serialize, Deserialize)]
struct Header {
    kid: String,
    typ: String,
    alg: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    format: u32,
    shortening_threshold: usize,
    jti: String,
    cipher_combo: String,
}

fn part<T: serde::de::DeserializeOwned>(s: &str, what: &str) -> Result<T> {
    let bytes = B64URL.decode(s.trim_end_matches('=')).map_err(|_| Error::Malformed(format!("{FILE}: bad {what}")))?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Malformed(format!("{FILE}: {what}: {e}")))
}

fn split(token: &str) -> Result<(&str, &str, &str)> {
    let mut it = token.trim().split('.');
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some(h), Some(p), Some(s), None) => Ok((h, p, s)),
        _ => Err(Error::Malformed(format!("{FILE} isn't a JWT"))),
    }
}

/// Where the key is, read **without** verifying (that needs the key).
/// Only masterkey files are supported; the file name is returned.
pub fn key_file(token: &str) -> Result<String> {
    let (h, _, _) = split(token)?;
    let header: Header = part(h, "header")?;
    if header.kid.starts_with("hub+") {
        return Err(Error::Unsupported("Cryptomator Hub vaults aren't supported, only password vaults".into()));
    }
    let Some(name) = header.kid.strip_prefix("masterkeyfile:") else {
        return Err(Error::Unsupported(format!("key location {:?}", header.kid)));
    };
    // A plain file name next to the configuration: never a path elsewhere.
    if name.is_empty() || name.contains(['/', '\\', ':']) || name == "." || name == ".." {
        return Err(Error::Malformed(format!("{FILE}: key file name {name:?}")));
    }
    Ok(name.to_string())
}

fn sign(alg: &str, key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    fn mac<M: Mac + hmac::digest::KeyInit>(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut m = <M as Mac>::new_from_slice(key).unwrap_or_else(|_| unreachable!("any key length"));
        m.update(data);
        m.finalize().into_bytes().to_vec()
    }
    Ok(match alg {
        "HS256" => mac::<Hmac<Sha256>>(key, data),
        "HS384" => mac::<Hmac<Sha384>>(key, data),
        "HS512" => mac::<Hmac<Sha512>>(key, data),
        other => return Err(Error::Unsupported(format!("{FILE} signature algorithm {other:?}"))),
    })
}

impl Config {
    /// A new vault's configuration, with a random ID.
    pub fn new() -> Result<Self> {
        Ok(Self { jti: crate::new_uuid()?, shortening_threshold: SHORTENING_THRESHOLD })
    }

    /// The signed configuration (HS256), for `vault.cryptomator`.
    pub fn sign(&self, key: &Masterkey) -> Result<String> {
        let header = Header { kid: KID_MASTERKEY_FILE.into(), typ: "JWT".into(), alg: "HS256".into() };
        let payload = Payload {
            format: FORMAT,
            shortening_threshold: self.shortening_threshold,
            jti: self.jti.clone(),
            cipher_combo: SIV_GCM.into(),
        };
        let signing_input = format!("{}.{}", encode(&header)?, encode(&payload)?);
        let sig = sign("HS256", key.raw().as_ref(), signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", B64URL.encode(sig)))
    }

    /// Verifies `token` with `key`, then reads it.
    pub fn verify(token: &str, key: &Masterkey) -> Result<Self> {
        let (h, p, s) = split(token)?;
        let header: Header = part(h, "header")?;
        let given =
            B64URL.decode(s.trim_end_matches('=')).map_err(|_| Error::Malformed(format!("{FILE}: bad signature")))?;
        let expected = sign(&header.alg, key.raw().as_ref(), format!("{h}.{p}").as_bytes())?;
        // Constant time: no hint about how much of a forged signature matched.
        if !bool::from(subtle::ConstantTimeEq::ct_eq(expected.as_slice(), given.as_slice())) {
            return Err(Error::Tampered("the vault configuration"));
        }
        let payload: Payload = part(p, "payload")?;
        if payload.format != FORMAT {
            return Err(Error::Unsupported(format!("vault format {} (only {FORMAT} is supported)", payload.format)));
        }
        if payload.cipher_combo != SIV_GCM {
            return Err(Error::Unsupported(format!(
                "cipher combination {} (only {SIV_GCM} is supported)",
                payload.cipher_combo
            )));
        }
        if payload.shortening_threshold < 36 {
            return Err(Error::Malformed(format!("{FILE}: shortening threshold {}", payload.shortening_threshold)));
        }
        Ok(Self { jti: payload.jti, shortening_threshold: payload.shortening_threshold })
    }
}

fn encode<T: Serialize>(v: &T) -> Result<String> {
    Ok(B64URL.encode(serde_json::to_vec(v).map_err(|e| Error::Malformed(e.to_string()))?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_configuration_verifies_with_its_key_only() {
        let key = Masterkey::generate().unwrap();
        let c = Config::new().unwrap();
        let token = c.sign(&key).unwrap();
        assert_eq!(key_file(&token).unwrap(), "masterkey.cryptomator");
        assert_eq!(Config::verify(&token, &key).unwrap(), c);
        let other = Masterkey::generate().unwrap();
        assert!(matches!(Config::verify(&token, &other), Err(Error::Tampered(_))));
    }

    fn token_with(payload: &str, key: &Masterkey) -> String {
        let h = B64URL.encode(br#"{"kid":"masterkeyfile:masterkey.cryptomator","typ":"JWT","alg":"HS512"}"#);
        let p = B64URL.encode(payload);
        let sig = sign("HS512", key.raw().as_ref(), format!("{h}.{p}").as_bytes()).unwrap();
        format!("{h}.{p}.{}", B64URL.encode(sig))
    }

    #[test]
    fn signed_but_unsupported_vaults_are_refused() {
        let key = Masterkey::generate().unwrap();
        let ok = token_with(r#"{"format":8,"shorteningThreshold":220,"jti":"x","cipherCombo":"SIV_GCM"}"#, &key);
        assert!(Config::verify(&ok, &key).is_ok(), "HS512 is accepted");
        let ctr = token_with(r#"{"format":8,"shorteningThreshold":220,"jti":"x","cipherCombo":"SIV_CTRMAC"}"#, &key);
        assert!(matches!(Config::verify(&ctr, &key), Err(Error::Unsupported(_))));
        let seven = token_with(r#"{"format":7,"shorteningThreshold":220,"jti":"x","cipherCombo":"SIV_GCM"}"#, &key);
        assert!(matches!(Config::verify(&seven, &key), Err(Error::Unsupported(_))));
    }

    #[test]
    fn unsafe_headers_are_refused() {
        let enc = |s: &str| B64URL.encode(s);
        let t = |kid: &str, alg: &str| {
            format!("{}.{}.AA", enc(&format!(r#"{{"kid":"{kid}","typ":"JWT","alg":"{alg}"}}"#)), enc("{}"))
        };
        assert!(matches!(key_file(&t("hub+https://x/vaults/1", "HS256")), Err(Error::Unsupported(_))));
        assert!(matches!(key_file(&t("masterkeyfile:../../secret", "HS256")), Err(Error::Malformed(_))));
        assert!(matches!(key_file(&t("masterkeyfile:C:\\\\x", "HS256")), Err(Error::Malformed(_))));
        // "none" is never an accepted algorithm.
        let key = Masterkey::generate().unwrap();
        assert!(matches!(Config::verify(&t(KID_MASTERKEY_FILE, "none"), &key), Err(Error::Unsupported(_))));
        assert!(key_file("not.a.jwt.at.all").is_err());
    }

    #[test]
    fn the_specifications_example_reads_as_expected_but_not_verified() {
        // From the specification: its key isn't published, so only the
        // unverified header can be read.
        let example = "eyJraWQiOiJtYXN0ZXJrZXlmaWxlOm1hc3RlcmtleS5jcnlwdG9tYXRvciIsInR5cCI6IkpXVCIsImFsZyI6IkhTMjU2In0.eyJmb3JtYXQiOjgsInNob3J0ZW5pbmdUaHJlc2hvbGQiOjIyMCwianRpIjoiY2U5NzZmN2EtN2I5Mi00Y2MwLWI0YzEtYzc0YTZhYTE3Y2Y1IiwiY2lwaGVyQ29tYm8iOiJTSVZfQ1RSTUFDIn0.IJlu4dHb3fqB2fAk9lf8G8zyEXc7OLB-5m9aNxOEXIQ";
        assert_eq!(key_file(example).unwrap(), "masterkey.cryptomator");
        let key = Masterkey::generate().unwrap();
        assert!(matches!(Config::verify(example, &key), Err(Error::Tampered(_))));
    }
}
