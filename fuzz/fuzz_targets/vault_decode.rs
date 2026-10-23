//! Everything `oneup_vault` reads from a vault folder, which lives in
//! OneDrive and so is untrusted: the masterkey file, the signed
//! configuration, encrypted names and contents, recovery keys and sealed
//! keys. The first input byte picks what the rest is fed to. Decoding must
//! never panic, and tampered ciphertext must never decrypt to anything but
//! the original (a file cut at a chunk boundary is the format's one
//! documented exception).
#![no_main]

use std::io::Write;

use libfuzzer_sys::fuzz_target;
use oneup_vault::content::{self, CHUNK, Decryptor};
use oneup_vault::key::Masterkey;
use oneup_vault::{config, names, recovery, sealed};

/// Most scrypt memory a masterkey file may ask for here (128 · r · N bytes).
/// The library allows 1 GiB; that's correct for a real vault and far too
/// slow per fuzz input, and the cost check itself is still exercised.
const FUZZ_SCRYPT_BYTES: u64 = 1 << 20;

fn key() -> Masterkey {
    Masterkey::from_keys([0x11; 32], [0x22; 32])
}

fn cheap_scrypt(json: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(json) else { return true };
    let n = v.get("scryptCostParam").and_then(|x| x.as_u64()).unwrap_or(0);
    let r = v.get("scryptBlockSize").and_then(|x| x.as_u64()).unwrap_or(0);
    128u64.saturating_mul(n).saturating_mul(r) <= FUZZ_SCRYPT_BYTES
}

/// Decrypts `data` with the streaming decryptor, written in pieces whose
/// sizes come from `splits`.
fn stream(data: &[u8], splits: &[u8]) -> Result<Vec<u8>, ()> {
    let mut d = Decryptor::new(&key(), Vec::new());
    let mut rest = data;
    let mut i = 0;
    while !rest.is_empty() {
        let step = splits.get(i % splits.len().max(1)).map_or(rest.len(), |s| usize::from(*s) * 97 + 1);
        let (now, later) = rest.split_at(step.min(rest.len()));
        d.write_all(now).map_err(|_| ())?;
        rest = later;
        i += 1;
    }
    d.finish().map(|(_, out)| out).map_err(|_| ())
}

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else { return };
    let text = String::from_utf8_lossy(rest);
    match which % 8 {
        0 => {
            if cheap_scrypt(rest) {
                let _ = Masterkey::from_file(rest, "password");
            }
        }
        1 => {
            let _ = config::key_file(&text);
            let _ = config::Config::verify(&text, &key());
        }
        2 => {
            let whole = content::decrypt_bytes(&key(), rest);
            let streamed = stream(rest, rest.get(..4).unwrap_or_default());
            assert_eq!(whole.is_ok(), streamed.is_ok());
            if let Some(n) = rest.get(..8) {
                let _ = content::content_size(u64::from_le_bytes(n.try_into().unwrap_or_default()));
            }
        }
        3 => {
            let (parent, name) = text.split_once('\n').unwrap_or(("", &text));
            let _ = names::decrypt_name(&key(), name, parent);
        }
        4 => {
            let _ = recovery::from_recovery_key(&text);
        }
        5 => {
            let _ = sealed::challenge(rest);
            let _ = sealed::open(rest, &[0x33; 32]);
        }
        6 => {
            // Tamper with a real encrypted file: plain text is the input
            // repeated up to a size spanning several chunks; then one byte
            // is flipped, or the file is cut short.
            let Some((&op, body)) = rest.split_first() else { return };
            let Some((pos, body)) = body.split_first_chunk::<4>() else { return };
            let plain: Vec<u8> = if body.is_empty() {
                Vec::new()
            } else {
                body.iter().copied().cycle().take(usize::from(op) * 512 + body.len()).collect()
            };
            let Ok(enc) = content::encrypt_bytes(&key(), &plain) else { return };
            assert_eq!(content::content_size(enc.len() as u64), Some(plain.len() as u64));
            assert_eq!(content::decrypt_bytes(&key(), &enc).ok().as_deref(), Some(plain.as_slice()));
            let at = u32::from_le_bytes(*pos) as usize % enc.len();
            let mut bad = enc.clone();
            if op & 1 == 0 {
                bad[at] ^= 1 << (op % 8);
                assert!(content::decrypt_bytes(&key(), &bad).is_err(), "a flipped bit at {at} decrypted");
            } else {
                bad.truncate(at);
                if let Ok(got) = content::decrypt_bytes(&key(), &bad) {
                    assert!(got.len().is_multiple_of(CHUNK) && plain.starts_with(&got), "cut at {at}");
                }
            }
        }
        _ => {
            // Names round-trip in their folder only.
            let (parent, name) = text.split_once('\n').unwrap_or(("p", &text));
            let Ok(enc) = names::encrypt_name(&key(), name, parent) else { return };
            // Except the names a reader refuses even when authentic: no
            // file has them, and they could reach outside their folder.
            if !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0']) {
                assert!(names::decrypt_name(&key(), &enc, parent).is_ok());
            }
            assert!(names::decrypt_name(&key(), &enc, &format!("{parent}x")).is_err());
        }
    }
});
