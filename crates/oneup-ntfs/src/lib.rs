//! Change-journal reading and directory enumeration for OneUpDrive.
//!
//! * [`record`]: pure, fuzzable parser for USN journal records (V2/V3).
//! * [`dirent`]: pure parser for `FILE_ID_EXTD_DIR_INFO` enumeration buffers.
//! * [`win`] (Windows only): the thin FFI layer that obtains those buffers.
//!
//! Every file is identified by a 128-bit [`FileId`]. That is native on ReFS,
//! and on NTFS it is the 64-bit file reference number zero-extended, so the
//! rest of the system never has to care which file system it is on.

pub mod dirent;
pub mod record;
#[cfg(windows)]
pub mod win;

/// A 128-bit file identifier, stable across renames and moves on the same volume.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct FileId(pub u128);

impl FileId {
    pub fn from_le_bytes(b: [u8; 16]) -> Self {
        Self(u128::from_le_bytes(b))
    }

    pub fn to_le_bytes(self) -> [u8; 16] {
        self.0.to_le_bytes()
    }

    /// Zero-extends an NTFS 64-bit file reference number.
    pub fn from_frn64(frn: u64) -> Self {
        Self(u128::from(frn))
    }
}

impl std::fmt::Debug for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileId({:#x})", self.0)
    }
}

/// Reads a little-endian integer at `off` from `buf`, or `None` if out of bounds.
pub(crate) fn le<const N: usize>(buf: &[u8], off: usize) -> Option<[u8; N]> {
    buf.get(off..off.checked_add(N)?)?.try_into().ok()
}

/// Decodes a UTF-16LE name. Unpaired surrogates (legal in NTFS names) are
/// replaced, and the caller is told so via the `bool`, so lossy names are
/// never used to address files.
pub(crate) fn utf16le_name(bytes: &[u8]) -> (String, bool) {
    let units = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    let mut lossy = false;
    let s = char::decode_utf16(units)
        .map(|r| {
            r.unwrap_or_else(|_| {
                lossy = true;
                char::REPLACEMENT_CHARACTER
            })
        })
        .collect();
    (s, lossy)
}
