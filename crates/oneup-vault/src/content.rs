//! File contents (`SIV_GCM`).
//!
//! * **Header**, 68 bytes: a 12-byte nonce, then, AES-GCM-encrypted with the
//!   vault's encryption key, eight `0xFF` bytes and the file's random 256-bit
//!   content key (40 bytes), then the 16-byte tag.
//! * **Chunks** of up to 32 KiB of content, each: a random 12-byte nonce,
//!   the AES-GCM ciphertext under the content key, and the tag, with the
//!   chunk's number (64-bit big-endian) and the header's nonce as
//!   associated data. So chunks can't be reordered, dropped from the middle,
//!   or moved to another file without failing authentication.
//!
//! An empty file is the header alone. A file cut at a chunk boundary still
//! authenticates (the format has no end marker); a cut inside a chunk
//! doesn't.

use std::io::{Read, Write};

use aes_gcm::aead::AeadInPlace as _;
use aes_gcm::{Aes256Gcm, KeyInit as _, Nonce, Tag};
use zeroize::Zeroizing;

use crate::key::Masterkey;
use crate::{Error, Result, random};

pub const HEADER_LEN: usize = 68;
pub const CHUNK: usize = 32 * 1024;
const NONCE: usize = 12;
const TAG: usize = 16;
/// A full encrypted chunk.
pub const CHUNK_LEN: usize = NONCE + CHUNK + TAG;
const RESERVED: [u8; 8] = [0xff; 8];

/// The encrypted size of `n` bytes.
pub fn encrypted_size(n: u64) -> u64 {
    let chunks = n.div_ceil(CHUNK as u64);
    HEADER_LEN as u64 + n + chunks * (NONCE + TAG) as u64
}

/// The content size of an encrypted file of `n` bytes; `None` if no valid
/// file has that size.
pub fn content_size(n: u64) -> Option<u64> {
    let body = n.checked_sub(HEADER_LEN as u64)?;
    let (full, rest) = (body / CHUNK_LEN as u64, body % CHUNK_LEN as u64);
    match rest {
        0 => Some(full * CHUNK as u64),
        r if r <= (NONCE + TAG) as u64 => None,
        r => Some(full * CHUNK as u64 + r - (NONCE + TAG) as u64),
    }
}

fn aad(chunk: u64, header_nonce: &[u8; NONCE]) -> [u8; 8 + NONCE] {
    let mut a = [0u8; 8 + NONCE];
    a[..8].copy_from_slice(&chunk.to_be_bytes());
    a[8..].copy_from_slice(header_nonce);
    a
}

/// Reads until `buf` is full or the input ends; how much was read.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// A new file's header: a random nonce, and a random content key, made in
/// the buffer it's encrypted in (no other copy of it), then encrypted with
/// the vault's key. Returns the nonce, the content cipher and the header.
fn new_header(key: &Masterkey) -> Result<([u8; NONCE], Aes256Gcm, [u8; HEADER_LEN])> {
    let header_nonce: [u8; NONCE] = random()?;
    let mut payload = Zeroizing::new([0u8; 40]);
    payload[..8].copy_from_slice(&RESERVED);
    crate::fill_random(&mut payload[8..])?;
    let gcm = Aes256Gcm::new((&payload[8..]).into());
    let tag = Aes256Gcm::new(key.enc().into())
        .encrypt_in_place_detached(Nonce::from_slice(&header_nonce), b"", payload.as_mut())
        .map_err(|_| Error::Malformed("header encryption".into()))?;
    let mut header = [0u8; HEADER_LEN];
    header[..NONCE].copy_from_slice(&header_nonce);
    header[NONCE..NONCE + 40].copy_from_slice(payload.as_ref());
    header[NONCE + 40..].copy_from_slice(&tag);
    Ok((header_nonce, gcm, header))
}

/// Encrypts everything from `input` to `output`; returns the content size.
pub fn encrypt(key: &Masterkey, mut input: impl Read, mut output: impl Write) -> Result<u64> {
    let (header_nonce, gcm, header) = new_header(key)?;
    output.write_all(&header)?;
    let mut buf = Zeroizing::new(vec![0u8; CHUNK]);
    let mut total = 0u64;
    for n in 0u64.. {
        let len = fill(&mut input, &mut buf)?;
        if len == 0 {
            break;
        }
        let nonce: [u8; NONCE] = random()?;
        let tag = gcm
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &aad(n, &header_nonce), &mut buf[..len])
            .map_err(|_| Error::Malformed("chunk encryption".into()))?;
        output.write_all(&nonce)?;
        output.write_all(&buf[..len])?;
        output.write_all(&tag)?;
        total += len as u64;
        if len < CHUNK {
            break;
        }
    }
    output.flush()?;
    Ok(total)
}

/// Decrypts everything from `input` to `output`; returns the content size.
/// Nothing unauthenticated is written: each chunk is checked before it's
/// passed on (an error can still come after earlier chunks were written).
pub fn decrypt(key: &Masterkey, mut input: impl Read, mut output: impl Write) -> Result<u64> {
    let mut header = [0u8; HEADER_LEN];
    if fill(&mut input, &mut header)? != HEADER_LEN {
        return Err(Error::Malformed("the file is shorter than its header".into()));
    }
    let mut header_nonce = [0u8; NONCE];
    header_nonce.copy_from_slice(&header[..NONCE]);
    let mut payload = Zeroizing::new([0u8; 40]);
    payload.copy_from_slice(&header[NONCE..NONCE + 40]);
    Aes256Gcm::new(key.enc().into())
        .decrypt_in_place_detached(
            Nonce::from_slice(&header_nonce),
            b"",
            payload.as_mut(),
            Tag::from_slice(&header[NONCE + 40..]),
        )
        .map_err(|_| Error::Tampered("a file header"))?;
    let gcm = Aes256Gcm::new((&payload[8..]).into());

    let mut buf = Zeroizing::new(vec![0u8; CHUNK_LEN]);
    let mut total = 0u64;
    for n in 0u64.. {
        let len = fill(&mut input, &mut buf)?;
        if len == 0 {
            break;
        }
        if len <= NONCE + TAG {
            return Err(Error::Malformed("a chunk is cut short".into()));
        }
        let (nonce, rest) = buf[..len].split_at_mut(NONCE);
        let (body, tag) = rest.split_at_mut(len - NONCE - TAG);
        gcm.decrypt_in_place_detached(Nonce::from_slice(nonce), &aad(n, &header_nonce), body, Tag::from_slice(tag))
            .map_err(|_| Error::Tampered("a file chunk"))?;
        output.write_all(body)?;
        total += body.len() as u64;
        if len < CHUNK_LEN {
            break;
        }
    }
    output.flush()?;
    Ok(total)
}

/// Encrypts as it is read: a [`Read`] over `input`'s content that yields
/// the encrypted file, for streaming an upload without holding the file.
pub struct Encryptor<R> {
    input: R,
    gcm: Aes256Gcm,
    header_nonce: [u8; NONCE],
    /// Encrypted bytes not yet handed out, and how far into them.
    out: Zeroizing<Vec<u8>>,
    at: usize,
    chunk: u64,
    done: bool,
    plain: Zeroizing<Vec<u8>>,
    /// Content bytes read so far.
    pub content_len: u64,
}

impl<R: Read> Encryptor<R> {
    pub fn new(key: &Masterkey, input: R) -> Result<Self> {
        let (header_nonce, gcm, header) = new_header(key)?;
        let mut out = Zeroizing::new(Vec::with_capacity(CHUNK_LEN));
        out.extend_from_slice(&header);
        Ok(Self {
            input,
            gcm,
            header_nonce,
            out,
            at: 0,
            chunk: 0,
            done: false,
            plain: Zeroizing::new(vec![0u8; CHUNK]),
            content_len: 0,
        })
    }

    /// Encrypts the next chunk into `out`; false at the end.
    fn refill(&mut self) -> std::io::Result<bool> {
        if self.done {
            return Ok(false);
        }
        let len = fill(&mut self.input, &mut self.plain)?;
        if len < CHUNK {
            self.done = true;
        }
        if len == 0 {
            return Ok(false);
        }
        let nonce: [u8; NONCE] = random().map_err(std::io::Error::other)?;
        let aad = aad(self.chunk, &self.header_nonce);
        let tag = self
            .gcm
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &aad, &mut self.plain[..len])
            .map_err(|_| std::io::Error::other("chunk encryption"))?;
        self.out.clear();
        self.at = 0;
        self.out.extend_from_slice(&nonce);
        self.out.extend_from_slice(&self.plain[..len]);
        self.out.extend_from_slice(&tag);
        self.chunk += 1;
        self.content_len += len as u64;
        Ok(true)
    }
}

impl<R: Read> Read for Encryptor<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.out.len() && !self.refill()? {
            return Ok(0);
        }
        let n = buf.len().min(self.out.len() - self.at);
        buf[..n].copy_from_slice(&self.out[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Decrypts as it is written: a [`Write`] that takes an encrypted file and
/// passes its content on to `output`, each chunk only once authenticated.
/// [`Decryptor::finish`] must be called at the end: it checks the last
/// chunk and returns the content size.
pub struct Decryptor<W> {
    key_enc: Zeroizing<[u8; 32]>,
    gcm: Option<Aes256Gcm>,
    header_nonce: [u8; NONCE],
    buf: Zeroizing<Vec<u8>>,
    chunk: u64,
    output: W,
    pub content_len: u64,
}

impl<W: Write> Decryptor<W> {
    pub fn new(key: &Masterkey, output: W) -> Self {
        let mut k = Zeroizing::new([0u8; 32]);
        k.copy_from_slice(key.enc());
        Self {
            key_enc: k,
            gcm: None,
            header_nonce: [0; NONCE],
            buf: Zeroizing::new(Vec::with_capacity(2 * CHUNK_LEN)),
            chunk: 0,
            output,
            content_len: 0,
        }
    }

    fn header(&mut self) -> Result<()> {
        self.header_nonce.copy_from_slice(&self.buf[..NONCE]);
        let mut payload = Zeroizing::new([0u8; 40]);
        payload.copy_from_slice(&self.buf[NONCE..NONCE + 40]);
        Aes256Gcm::new(self.key_enc.as_ref().into())
            .decrypt_in_place_detached(
                Nonce::from_slice(&self.header_nonce),
                b"",
                payload.as_mut(),
                Tag::from_slice(&self.buf[NONCE + 40..HEADER_LEN]),
            )
            .map_err(|_| Error::Tampered("a file header"))?;
        self.gcm = Some(Aes256Gcm::new((&payload[8..]).into()));
        self.buf.drain(..HEADER_LEN);
        Ok(())
    }

    /// Decrypts the `len` bytes at the start of the buffer as one chunk.
    fn chunk(&mut self, len: usize) -> Result<()> {
        if len <= NONCE + TAG {
            return Err(Error::Malformed("a chunk is cut short".into()));
        }
        let Some(gcm) = &self.gcm else { return Err(Error::Malformed("no header".into())) };
        let aad = aad(self.chunk, &self.header_nonce);
        let (nonce, rest) = self.buf[..len].split_at_mut(NONCE);
        let (body, tag) = rest.split_at_mut(len - NONCE - TAG);
        gcm.decrypt_in_place_detached(Nonce::from_slice(nonce), &aad, body, Tag::from_slice(tag))
            .map_err(|_| Error::Tampered("a file chunk"))?;
        self.output.write_all(body)?;
        self.content_len += body.len() as u64;
        self.chunk += 1;
        self.buf.drain(..len);
        Ok(())
    }

    /// The end of the encrypted file: decrypts what's left, flushes, and
    /// returns the content size and the output.
    pub fn finish(mut self) -> Result<(u64, W)> {
        if self.gcm.is_none() {
            if self.buf.len() < HEADER_LEN {
                return Err(Error::Malformed("the file is shorter than its header".into()));
            }
            self.header()?;
        }
        if !self.buf.is_empty() {
            let len = self.buf.len();
            self.chunk(len)?;
        }
        self.output.flush()?;
        Ok((self.content_len, self.output))
    }
}

impl<W: Write> Write for Decryptor<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // Taken in pieces that fit the buffer: growing it would move it,
        // and leave the old allocation, with the plaintext of chunks
        // decrypted in place there, unwiped. After each piece at most a
        // chunk stays, so there's always room for more.
        let mut rest = data;
        while !rest.is_empty() {
            let room = self.buf.capacity() - self.buf.len();
            let (now, later) = rest.split_at(room.min(rest.len()));
            self.buf.extend_from_slice(now);
            rest = later;
            if self.gcm.is_none() {
                if self.buf.len() < HEADER_LEN {
                    continue;
                }
                self.header().map_err(std::io::Error::other)?;
            }
            // A whole chunk stays buffered until more arrives or `finish`.
            while self.buf.len() > CHUNK_LEN {
                self.chunk(CHUNK_LEN).map_err(std::io::Error::other)?;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Encrypts a small value whole (a symbolic link's target, a folder ID
/// backup).
pub fn encrypt_bytes(key: &Masterkey, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(encrypted_size(data.len() as u64) as usize);
    encrypt(key, data, &mut out)?;
    Ok(out)
}

pub fn decrypt_bytes(key: &Masterkey, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decrypt(key, data, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn key() -> Masterkey {
        Masterkey::from_keys([7; 32], [9; 32])
    }

    #[test]
    fn contents_round_trip_at_every_interesting_size() {
        let k = key();
        for n in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK, 3 * CHUNK + 5] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
            let enc = encrypt_bytes(&k, &data).unwrap();
            assert_eq!(enc.len() as u64, encrypted_size(n as u64), "{n}");
            assert_eq!(content_size(enc.len() as u64), Some(n as u64), "{n}");
            assert_eq!(decrypt_bytes(&k, &enc).unwrap(), data, "{n}");
        }
        // Random nonces: the same content encrypts differently each time.
        assert_ne!(encrypt_bytes(&k, b"x").unwrap(), encrypt_bytes(&k, b"x").unwrap());
    }

    #[test]
    fn the_header_payload_is_reserved_bytes_then_the_key() {
        let k = key();
        let enc = encrypt_bytes(&k, b"").unwrap();
        assert_eq!(enc.len(), HEADER_LEN);
        let mut payload = enc[NONCE..NONCE + 40].to_vec();
        Aes256Gcm::new(k.enc().into())
            .decrypt_in_place_detached(Nonce::from_slice(&enc[..NONCE]), b"", &mut payload, Tag::from_slice(&enc[52..]))
            .unwrap();
        assert_eq!(payload[..8], RESERVED);
    }

    #[test]
    fn changed_swapped_or_cut_chunks_fail() {
        let k = key();
        let data = vec![5u8; 3 * CHUNK];
        let enc = encrypt_bytes(&k, &data).unwrap();
        let mut flipped = enc.clone();
        flipped[HEADER_LEN + 100] ^= 1;
        assert!(matches!(decrypt_bytes(&k, &flipped), Err(Error::Tampered(_))));
        // Chunks 0 and 1 swapped.
        let mut swapped = enc.clone();
        let (a, b) = (HEADER_LEN, HEADER_LEN + CHUNK_LEN);
        let first = enc[a..b].to_vec();
        swapped[a..b].copy_from_slice(&enc[b..b + CHUNK_LEN]);
        swapped[b..b + CHUNK_LEN].copy_from_slice(&first);
        assert!(matches!(decrypt_bytes(&k, &swapped), Err(Error::Tampered(_))));
        // A chunk from another file (another header nonce).
        let other = encrypt_bytes(&k, &data).unwrap();
        let mut mixed = enc.clone();
        mixed[a..b].copy_from_slice(&other[a..b]);
        assert!(matches!(decrypt_bytes(&k, &mixed), Err(Error::Tampered(_))));
        // Cut inside a chunk; or the header changed; or another vault.
        assert!(decrypt_bytes(&k, &enc[..enc.len() - 3]).is_err());
        let mut head = enc.clone();
        head[20] ^= 1;
        assert!(matches!(decrypt_bytes(&k, &head), Err(Error::Tampered("a file header"))));
        assert!(matches!(decrypt_bytes(&Masterkey::from_keys([1; 32], [9; 32]), &enc), Err(Error::Tampered(_))));
    }

    #[test]
    fn streaming_matches_the_whole_file_functions() {
        let k = key();
        for n in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK, 3 * CHUNK + 5] {
            let data: Vec<u8> = (0..n).map(|i| (i * 17 % 253) as u8).collect();
            // Read in odd-sized pieces, as an uploader would.
            let mut e = Encryptor::new(&k, data.as_slice()).unwrap();
            let mut enc = Vec::new();
            let mut piece = vec![0u8; 7777];
            loop {
                let got = e.read(&mut piece).unwrap();
                if got == 0 {
                    break;
                }
                enc.extend_from_slice(&piece[..got]);
            }
            assert_eq!(e.content_len, n as u64);
            assert_eq!(enc.len() as u64, encrypted_size(n as u64), "{n}");
            assert_eq!(decrypt_bytes(&k, &enc).unwrap(), data, "{n}");
            // Written in odd-sized pieces, as a download arrives.
            let mut d = Decryptor::new(&k, Vec::new());
            for c in enc.chunks(5000) {
                d.write_all(c).unwrap();
            }
            let (len, out) = d.finish().unwrap();
            assert_eq!((len, out), (n as u64, data), "{n}");
        }
    }

    #[test]
    fn streaming_decryption_never_regrows_its_buffer() {
        // One big write (a download arriving in large pieces) must not move
        // the buffer that held decrypted chunks.
        let k = key();
        let data = vec![4u8; 5 * CHUNK + 3];
        let enc = encrypt_bytes(&k, &data).unwrap();
        let mut d = Decryptor::new(&k, Vec::new());
        let cap = d.buf.capacity();
        d.write_all(&enc).unwrap();
        assert_eq!(d.buf.capacity(), cap);
        assert_eq!(d.finish().unwrap().1, data);
    }

    #[test]
    fn streaming_decryption_refuses_tampering() {
        let k = key();
        let enc = encrypt_bytes(&k, &vec![3u8; 2 * CHUNK + 9]).unwrap();
        let run = |e: &[u8]| {
            let mut d = Decryptor::new(&k, Vec::new());
            d.write_all(e).map_err(|_| ()).and_then(|()| d.finish().map(|_| ()).map_err(|_| ()))
        };
        assert!(run(&enc).is_ok());
        let mut flipped = enc.clone();
        flipped[HEADER_LEN + CHUNK_LEN + 3] ^= 1;
        assert!(run(&flipped).is_err());
        assert!(run(&enc[..enc.len() - 2]).is_err(), "cut inside the last chunk");
        assert!(run(&enc[..10]).is_err(), "cut inside the header");
    }

    #[test]
    fn impossible_sizes_are_recognised() {
        assert_eq!(content_size(10), None);
        assert_eq!(content_size(HEADER_LEN as u64), Some(0));
        assert_eq!(content_size(HEADER_LEN as u64 + 20), None);
        assert_eq!(content_size(HEADER_LEN as u64 + 29), Some(1));
    }
}
