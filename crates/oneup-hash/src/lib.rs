//! QuickXorHash — the content hash OneDrive reports for every file
//! (`file.hashes.quickXorHash` in Microsoft Graph).
//!
//! The algorithm XORs byte `n` of the input into a 160-bit circular register
//! at bit offset `(n * 11) mod 160`, then XORs the little-endian 64-bit input
//! length into the last 8 bytes of the register.
//!
//! Because `160 * 11 ≡ 0 (mod 160)`, every byte whose index is congruent mod
//! 160 lands at the same offset. So the hot path just XOR-folds the input
//! into a 160-byte accumulator (which vectorises well), and the shifting
//! happens once, in [`QuickXorHash::finalize`].
//!
//! Reference: <https://learn.microsoft.com/onedrive/developer/code-snippets/quickxorhash>

use base64::Engine as _;

/// Width of the hash register in bits.
const WIDTH_BITS: usize = 160;
/// Hash output length in bytes.
pub const HASH_LEN: usize = WIDTH_BITS / 8;
/// Bit distance between the positions of consecutive input bytes.
const SHIFT: usize = 11;

/// Streaming QuickXorHash state.
#[derive(Clone)]
pub struct QuickXorHash {
    /// `acc[k]` is the XOR of every input byte whose index ≡ k (mod 160).
    acc: [u8; WIDTH_BITS],
    len: u64,
}

impl Default for QuickXorHash {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for QuickXorHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickXorHash").field("len", &self.len).finish_non_exhaustive()
    }
}

impl QuickXorHash {
    pub fn new() -> Self {
        Self { acc: [0; WIDTH_BITS], len: 0 }
    }

    /// Feeds more input. May be called any number of times with any split.
    pub fn update(&mut self, mut data: &[u8]) {
        // Truncation is intended: only the index modulo 160 matters.
        let mut pos = (self.len % WIDTH_BITS as u64) as usize;
        self.len = self.len.wrapping_add(data.len() as u64);

        // Align to an accumulator boundary.
        if pos != 0 {
            let head = (WIDTH_BITS - pos).min(data.len());
            for (a, b) in self.acc[pos..pos + head].iter_mut().zip(&data[..head]) {
                *a ^= b;
            }
            data = &data[head..];
            pos = (pos + head) % WIDTH_BITS;
        }
        debug_assert!(pos == 0 || data.is_empty());

        let (blocks, rest) = data.as_chunks::<WIDTH_BITS>();
        for block in blocks {
            for (a, b) in self.acc.iter_mut().zip(block) {
                *a ^= b;
            }
        }
        for (a, b) in self.acc.iter_mut().zip(rest) {
            *a ^= b;
        }
    }

    /// Total number of bytes hashed so far.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Produces the 20-byte digest without consuming the state.
    pub fn finalize(&self) -> [u8; HASH_LEN] {
        let mut out = [0u8; HASH_LEN];
        for (k, &b) in self.acc.iter().enumerate() {
            if b == 0 {
                continue;
            }
            let bit = (k * SHIFT) % WIDTH_BITS;
            let (byte, off) = (bit / 8, bit % 8);
            out[byte] ^= b << off;
            if off != 0 {
                out[(byte + 1) % HASH_LEN] ^= b >> (8 - off);
            }
        }
        for (o, l) in out[HASH_LEN - 8..].iter_mut().zip(self.len.to_le_bytes()) {
            *o ^= l;
        }
        out
    }

    /// Digest encoded as standard base64 — the form Graph returns.
    pub fn finalize_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.finalize())
    }
}

impl std::io::Write for QuickXorHash {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One-shot convenience: base64 QuickXorHash of `data`.
pub fn quick_xor_hash_base64(data: &[u8]) -> String {
    let mut h = QuickXorHash::new();
    h.update(data);
    h.finalize_base64()
}

/// Hashes everything readable from `reader`, using a caller-supplied buffer so
/// memory use stays bounded and controlled by the memory profile.
pub fn hash_reader<R: std::io::Read>(mut reader: R, buf: &mut [u8]) -> std::io::Result<QuickXorHash> {
    let mut h = QuickXorHash::new();
    loop {
        match reader.read(buf) {
            Ok(0) => return Ok(h),
            Ok(n) => h.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}
