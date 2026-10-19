//! The masterkey: two random 256-bit keys, one for encryption and one for
//! authentication (MAC). In a `masterkey.cryptomator` file they're wrapped
//! (AES key wrap, RFC 3394) with a key derived from the password by scrypt
//! (RFC 7914, not parallel), next to the scrypt parameters and a MAC of the
//! legacy version number that guards against downgrades.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use unicode_normalization::UnicodeNormalization;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::{Error, Result, random};

/// The masterkey file's name in the vault's top folder.
pub const FILE: &str = "masterkey.cryptomator";
/// The legacy `version` field: 999 in format 8 (the real format is in the
/// vault configuration).
const LEGACY_VERSION: u32 = 999;
/// Cryptomator's defaults: N = 2^15, r = 8 (32 MiB of memory), 8-byte salt.
const COST: u32 = 32_768;
const BLOCK_SIZE: u32 = 8;
const SALT_LEN: usize = 8;
/// The most memory a masterkey file may make scrypt use (a hostile file
/// mustn't exhaust the PC): 128 · r · N bytes.
const MAX_SCRYPT_BYTES: u64 = 1 << 30;

/// The two keys. Zeroed when dropped.
///
/// Boxed: moving a `Masterkey` (returning it, putting it in an `Arc`) then
/// moves a pointer. Moving the keys themselves copies their bytes and can
/// leave copies on the stack that nothing wipes.
pub struct Masterkey {
    keys: Box<Keys>,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct Keys {
    enc: [u8; 32],
    mac: [u8; 32],
}

// Wiped by `Keys`, in place on the heap.
impl ZeroizeOnDrop for Masterkey {}

impl Clone for Masterkey {
    fn clone(&self) -> Self {
        // Copied heap to heap, not through a value on the stack.
        let mut k = Self::zeroed();
        k.keys.enc.copy_from_slice(&self.keys.enc);
        k.keys.mac.copy_from_slice(&self.keys.mac);
        k
    }
}

impl std::fmt::Debug for Masterkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Masterkey(..)")
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    version: u32,
    scrypt_salt: String,
    scrypt_cost_param: u64,
    scrypt_block_size: u32,
    primary_master_key: String,
    hmac_master_key: String,
    version_mac: String,
}

/// The key derived from a password: Unicode NFC, UTF-8, then scrypt.
fn kek(password: &str, salt: &[u8], cost: u64, block_size: u32) -> Result<Zeroizing<[u8; 32]>> {
    if !cost.is_power_of_two() || cost < 2 {
        return Err(Error::Malformed("the scrypt cost isn't a power of two".into()));
    }
    let memory = 128u64.saturating_mul(u64::from(block_size)).saturating_mul(cost);
    if block_size == 0 || memory > MAX_SCRYPT_BYTES {
        return Err(Error::Unsupported("its key derivation would need more than 1 GiB of memory".into()));
    }
    let params = scrypt::Params::new(cost.trailing_zeros() as u8, block_size, 1, 32)
        .map_err(|e| Error::Malformed(format!("scrypt parameters: {e}")))?;
    // Room for the longest NFC form up front (NFC grows text at most
    // threefold, UAX #15): a string that grows reallocates and leaves the
    // old, unwiped buffer behind.
    let mut pw = Zeroizing::new(String::with_capacity(password.len().saturating_mul(3)));
    pw.extend(password.nfc());
    let mut out = Zeroizing::new([0u8; 32]);
    scrypt::scrypt(pw.as_bytes(), salt, &params, out.as_mut()).map_err(|e| Error::Malformed(format!("scrypt: {e}")))?;
    Ok(out)
}

fn version_mac(mac_key: &[u8; 32], version: u32) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(mac_key).unwrap_or_else(|_| unreachable!("any key length"));
    m.update(&version.to_be_bytes());
    m
}

fn b64(field: &str, s: &str) -> Result<Vec<u8>> {
    B64.decode(s).map_err(|_| Error::Malformed(format!("{field} isn't Base64")))
}

impl Masterkey {
    fn zeroed() -> Self {
        Self { keys: Box::new(Keys { enc: [0u8; 32], mac: [0u8; 32] }) }
    }

    /// A new random masterkey.
    pub fn generate() -> Result<Self> {
        let mut key = Self::zeroed();
        crate::fill_random(&mut key.keys.enc)?;
        crate::fill_random(&mut key.keys.mac)?;
        Ok(key)
    }

    /// For tests: a masterkey from given keys.
    pub fn from_keys(enc: [u8; 32], mac: [u8; 32]) -> Self {
        Self { keys: Box::new(Keys { enc, mac }) }
    }

    /// Both keys (encryption, then MAC), for keeping the vault unlocked
    /// (DPAPI-protected, or handed to the agent over a pipe). Zeroed when
    /// dropped.
    pub fn export(&self) -> Zeroizing<[u8; 64]> {
        self.raw()
    }

    /// The keys [`Masterkey::export`] gave.
    pub fn import(raw: &[u8; 64]) -> Self {
        // Copied straight into the zeroed-on-drop key, not through
        // temporaries that would linger on the stack.
        let mut key = Self::zeroed();
        key.keys.enc.copy_from_slice(&raw[..32]);
        key.keys.mac.copy_from_slice(&raw[32..]);
        key
    }

    pub(crate) fn enc(&self) -> &[u8; 32] {
        &self.keys.enc
    }

    /// Encryption key, then MAC key: what signs the vault configuration.
    pub(crate) fn raw(&self) -> Zeroizing<[u8; 64]> {
        let mut r = Zeroizing::new([0u8; 64]);
        r[..32].copy_from_slice(&self.keys.enc);
        r[32..].copy_from_slice(&self.keys.mac);
        r
    }

    /// The AES-SIV key: MAC key, then encryption key (RFC 5297's K1 for
    /// S2V, then K2 for CTR).
    pub(crate) fn siv(&self) -> Zeroizing<[u8; 64]> {
        let mut r = Zeroizing::new([0u8; 64]);
        r[..32].copy_from_slice(&self.keys.mac);
        r[32..].copy_from_slice(&self.keys.enc);
        r
    }

    /// The masterkey file for `password`, with Cryptomator's default
    /// parameters.
    pub fn to_file(&self, password: &str) -> Result<Vec<u8>> {
        self.to_file_with(password, u64::from(COST), BLOCK_SIZE)
    }

    /// As [`Masterkey::to_file`], with given scrypt parameters (tests use
    /// cheap ones).
    pub fn to_file_with(&self, password: &str, cost: u64, block_size: u32) -> Result<Vec<u8>> {
        let salt: [u8; SALT_LEN] = random()?;
        let kek = kek(password, &salt, cost, block_size)?;
        let wrap = |k: &[u8; 32]| -> Result<Vec<u8>> {
            aes_kw::KekAes256::new(kek.as_ref().into())
                .wrap_vec(k)
                .map_err(|e| Error::Malformed(format!("key wrap: {e}")))
        };
        let f = File {
            version: LEGACY_VERSION,
            scrypt_salt: B64.encode(salt),
            scrypt_cost_param: cost,
            scrypt_block_size: block_size,
            primary_master_key: B64.encode(wrap(&self.keys.enc)?),
            hmac_master_key: B64.encode(wrap(&self.keys.mac)?),
            version_mac: B64.encode(version_mac(&self.keys.mac, LEGACY_VERSION).finalize().into_bytes()),
        };
        serde_json::to_vec_pretty(&f).map_err(|e| Error::Malformed(e.to_string()))
    }

    /// Opens a masterkey file with `password`.
    pub fn from_file(json: &[u8], password: &str) -> Result<Self> {
        let f: File = serde_json::from_slice(json).map_err(|e| Error::Malformed(format!("{FILE}: {e}")))?;
        let salt = b64("scryptSalt", &f.scrypt_salt)?;
        let kek = kek(password, &salt, f.scrypt_cost_param, f.scrypt_block_size)?;
        // Unwraps into `out`, a field of the zeroed-on-drop key, so no copy
        // of the plain key is left behind.
        let unwrap = |field: &str, s: &str, out: &mut [u8; 32]| -> Result<()> {
            let wrapped = b64(field, s)?;
            if wrapped.len() != 40 {
                return Err(Error::Malformed(format!("{field} isn't a wrapped 256-bit key")));
            }
            // The key wrap's integrity check fails for a wrong KEK, that is
            // a wrong password.
            aes_kw::KekAes256::new(kek.as_ref().into()).unwrap(&wrapped, out).map_err(|_| Error::WrongPassword)?;
            Ok(())
        };
        let mut key = Self::zeroed();
        unwrap("primaryMasterKey", &f.primary_master_key, &mut key.keys.enc)?;
        unwrap("hmacMasterKey", &f.hmac_master_key, &mut key.keys.mac)?;
        let given = b64("versionMac", &f.version_mac)?;
        version_mac(&key.keys.mac, f.version)
            .verify_slice(&given)
            .map_err(|_| Error::Tampered("the masterkey's version"))?;
        if f.version != LEGACY_VERSION {
            return Err(Error::Unsupported(format!("masterkey file version {} (format 8 vaults have 999)", f.version)));
        }
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_masterkey_file_opens_with_its_password_only() {
        let k = Masterkey::generate().unwrap();
        let file = k.to_file_with("correct horse", 1024, 8).unwrap();
        let back = Masterkey::from_file(&file, "correct horse").unwrap();
        assert_eq!(*back.export(), *k.export());
        assert!(matches!(Masterkey::from_file(&file, "wrong"), Err(Error::WrongPassword)));
        let json: serde_json::Value = serde_json::from_slice(&file).unwrap();
        assert_eq!(json["version"], 999);
        assert_eq!(B64.decode(json["scryptSalt"].as_str().unwrap()).unwrap().len(), 8);
    }

    #[test]
    fn the_password_is_compared_in_unicode_normal_form() {
        let k = Masterkey::generate().unwrap();
        // "é" precomposed, and as "e" + combining accent.
        let file = k.to_file_with("caf\u{e9}", 1024, 8).unwrap();
        assert!(Masterkey::from_file(&file, "cafe\u{301}").is_ok());
    }

    #[test]
    fn a_changed_version_or_hostile_parameters_are_refused() {
        let k = Masterkey::generate().unwrap();
        let file = k.to_file_with("pw", 1024, 8).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&file).unwrap();
        json["version"] = 7.into();
        let changed = serde_json::to_vec(&json).unwrap();
        assert!(matches!(Masterkey::from_file(&changed, "pw"), Err(Error::Tampered(_))));
        json["version"] = 999.into();
        json["scryptCostParam"] = (1u64 << 30).into();
        let greedy = serde_json::to_vec(&json).unwrap();
        assert!(matches!(Masterkey::from_file(&greedy, "pw"), Err(Error::Unsupported(_))));
        json["scryptCostParam"] = 1000.into();
        let odd = serde_json::to_vec(&json).unwrap();
        assert!(matches!(Masterkey::from_file(&odd, "pw"), Err(Error::Malformed(_))));
    }

    #[test]
    fn aes_key_schedules_are_wiped_on_drop() {
        // Compiles only with the `aes` crate's `zeroize` feature, which every
        // AES user here (aes-gcm, aes-kw, aes-siv) shares.
        fn wiped<T: ZeroizeOnDrop>() {}
        wiped::<aes::Aes256>();
        wiped::<aes::Aes256Enc>();
        wiped::<aes::Aes256Dec>();
        // And AES-SIV's CMAC state (the `cmac` crate's `zeroize` feature).
        wiped::<cmac::CmacCore<aes::Aes256>>();
        wiped::<Masterkey>();
    }

    #[test]
    fn clones_and_exports_hold_the_same_keys() {
        let k = Masterkey::generate().unwrap();
        assert_ne!(*k.export(), [0u8; 64]);
        assert_eq!(*k.clone().export(), *k.export());
        assert_eq!(*Masterkey::import(&k.export()).export(), *k.export());
        assert_eq!(&k.siv()[..32], &k.export()[32..], "SIV: MAC key first");
    }

    #[test]
    fn scrypt_matches_rfc_7914() {
        // RFC 7914 §12, the first vector (P = "", S = "", N = 16, r = 1,
        // p = 1): its first 32 bytes, all a KEK uses (PBKDF2's first block
        // doesn't depend on the output length).
        let k = kek("", b"", 16, 1).unwrap();
        assert_eq!(*k, hex::<32>("77D6576238657B203B19CA42C18A0497F16B4844E3074AE8DFDFFA3FEDE21442"));
    }

    #[test]
    fn key_wrap_matches_rfc_3394() {
        // RFC 3394 §4.6: 256-bit key data with a 256-bit KEK.
        let kek: [u8; 32] = hex("000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F");
        let data: [u8; 32] = hex("00112233445566778899AABBCCDDEEFF000102030405060708090A0B0C0D0E0F");
        let wrapped = aes_kw::KekAes256::from(kek).wrap_vec(&data).unwrap();
        assert_eq!(
            wrapped,
            hex::<40>("28C9F404C4B810F4CBCCB35CFB87F8263F5786E2D80ED326CBC7F0E71A99F43BFB988B9B7A02DD21")
        );
    }

    fn hex<const N: usize>(s: &str) -> [u8; N] {
        let mut out = [0u8; N];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }
}
