//! A file's QuickXorHash, and with a [`MacKey`] its keyed hash too, from one
//! pass over its bytes.
//!
//! A vault's cloud copy is encrypted, so OneDrive's hash says nothing about
//! the decrypted file on this PC. Its keyed hash (HMAC-SHA256, truncated to
//! the 20 bytes the stores keep for every hash) does: it's recorded for
//! exactly the bytes a download wrote or an upload sent, and checked again
//! on the file before its decrypted copy is freed. Keyed, so the database
//! can't be used to test whether the vault holds a file someone already has.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::{HASH_LEN, QuickXorHash};

/// The key of a vault's keyed hashes. Zeroed when dropped.
#[derive(Clone)]
pub struct MacKey(Zeroizing<[u8; 32]>);

impl MacKey {
    pub fn new(key: Zeroizing<[u8; 32]>) -> Self {
        Self(key)
    }
}

impl std::fmt::Debug for MacKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MacKey(..)")
    }
}

/// QuickXorHash, plus the keyed hash when made with a key.
#[derive(Clone, Default)]
pub struct Digest {
    qxh: QuickXorHash,
    mac: Option<Hmac<Sha256>>,
}

impl std::fmt::Debug for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Digest").field("len", &self.qxh.len()).field("keyed", &self.mac.is_some()).finish()
    }
}

impl Digest {
    pub fn new(key: Option<&MacKey>) -> Self {
        let mac = key.map(|k| {
            <Hmac<Sha256> as Mac>::new_from_slice(k.0.as_ref()).unwrap_or_else(|_| unreachable!("any key length"))
        });
        Self { qxh: QuickXorHash::new(), mac }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.qxh.update(data);
        if let Some(m) = &mut self.mac {
            m.update(data);
        }
    }

    pub fn len(&self) -> u64 {
        self.qxh.len()
    }

    pub fn is_empty(&self) -> bool {
        self.qxh.is_empty()
    }

    /// The QuickXorHash.
    pub fn finalize(&self) -> [u8; HASH_LEN] {
        self.qxh.finalize()
    }

    /// The keyed hash, if made with a key.
    pub fn mac(&self) -> Option<[u8; HASH_LEN]> {
        let full = self.mac.clone()?.finalize().into_bytes();
        let mut out = [0u8; HASH_LEN];
        out.copy_from_slice(&full[..HASH_LEN]);
        Some(out)
    }
}

impl std::io::Write for Digest {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// [`crate::hash_reader`] with a key: reads `reader` to its end.
pub fn digest_reader<R: std::io::Read>(mut reader: R, key: Option<&MacKey>, buf: &mut [u8]) -> std::io::Result<Digest> {
    let mut d = Digest::new(key);
    loop {
        match reader.read(buf) {
            Ok(0) => return Ok(d),
            Ok(n) => d.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn key(b: u8) -> MacKey {
        MacKey::new(Zeroizing::new([b; 32]))
    }

    #[test]
    fn the_keyed_hash_depends_on_the_key_and_every_byte_and_split_does_not_matter() {
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        let mut whole = Digest::new(Some(&key(1)));
        whole.update(&data);
        let mut split = Digest::new(Some(&key(1)));
        for c in data.chunks(777) {
            split.update(c);
        }
        assert_eq!(whole.mac(), split.mac());
        assert_eq!(whole.finalize(), crate::hash_reader(&data[..], &mut [0; 4096]).unwrap().finalize());
        let mut other_key = Digest::new(Some(&key(2)));
        other_key.update(&data);
        assert_ne!(whole.mac(), other_key.mac());
        let mut changed = data.clone();
        changed[50_000] ^= 1;
        let read = digest_reader(&changed[..], Some(&key(1)), &mut [0; 4096]).unwrap();
        assert_ne!(read.mac(), whole.mac());
        assert_eq!(Digest::new(None).mac(), None);
    }

    #[test]
    fn a_prefix_never_shares_the_keyed_hash() {
        let mut a = Digest::new(Some(&key(1)));
        a.update(b"abc");
        let mut b = Digest::new(Some(&key(1)));
        b.update(b"abc\0");
        assert_ne!(a.mac(), b.mac());
        // The mac is read without consuming the state.
        assert_eq!(a.mac(), a.mac());
    }
}
