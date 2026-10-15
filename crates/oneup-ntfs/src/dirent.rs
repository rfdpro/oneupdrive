//! Parser for `FILE_ID_EXTD_DIR_INFO` buffers, as returned by
//! `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)`.
//!
//! One syscall returns many entries, each with a 128-bit file ID, size,
//! timestamps, attributes and reparse tag. That is everything the initial
//! scan needs without opening a single file.

use crate::{FileId, le, utf16le_name};

/// Entry layout offsets (see `FILE_ID_EXTD_DIR_INFO` in WinBase.h).
const NEXT: usize = 0;
const CREATION: usize = 8;
const LAST_WRITE: usize = 24;
const CHANGE: usize = 32;
const END_OF_FILE: usize = 40;
const ATTRIBUTES: usize = 56;
const NAME_LEN: usize = 60;
const REPARSE_TAG: usize = 68;
const FILE_ID: usize = 72;
pub(crate) const NAME: usize = 88;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub file_id: FileId,
    pub name: String,
    pub name_lossy: bool,
    pub size: u64,
    pub attributes: u32,
    pub reparse_tag: u32,
    /// FILETIMEs.
    pub created: i64,
    pub modified: i64,
    pub changed: i64,
}

impl DirEntry {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// `.` and `..` are returned by the API and must be skipped.
    pub fn is_dot(&self) -> bool {
        self.name == "." || self.name == ".."
    }

    /// Symlinks and junctions/mount points lead outside the tree and must
    /// never be traversed. Cloud-file placeholders are also reparse points
    /// but are part of the tree.
    pub fn is_link(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && matches!(self.reparse_tag, IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT)
    }

    /// Reading this entry's data may trigger a cloud recall (hydration).
    pub fn is_recall_on_access(&self) -> bool {
        recalls(self.attributes)
    }
}

/// Whether a file with these attributes isn't (all) here: touching its data
/// downloads it (an online-only placeholder, or one partly downloaded).
pub fn recalls(attributes: u32) -> bool {
    attributes & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_OFFLINE) != 0
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed directory-info entry at offset {offset}")]
pub struct DirParseError {
    pub offset: usize,
}

/// Parses every entry in one enumeration buffer.
pub fn parse_dir_buffer(buf: &[u8]) -> Result<Vec<DirEntry>, DirParseError> {
    let mut out = Vec::new();
    let mut off = 0usize;
    loop {
        let entry = buf.get(off..).ok_or(DirParseError { offset: off })?;
        let err = || DirParseError { offset: off };
        let u32_at = |o| le::<4>(entry, o).map(u32::from_le_bytes).ok_or_else(err);
        let i64_at = |o| le::<8>(entry, o).map(i64::from_le_bytes).ok_or_else(err);

        let next = u32_at(NEXT)? as usize;
        let name_len = u32_at(NAME_LEN)? as usize;
        let name_bytes = NAME
            .checked_add(name_len)
            .filter(|_| name_len % 2 == 0)
            .and_then(|end| entry.get(NAME..end))
            .ok_or_else(err)?;
        if next != 0 && next < NAME + name_len {
            return Err(err());
        }
        let (name, name_lossy) = utf16le_name(name_bytes);
        out.push(DirEntry {
            file_id: FileId::from_le_bytes(le::<16>(entry, FILE_ID).ok_or_else(err)?),
            name,
            name_lossy,
            size: u64::try_from(i64_at(END_OF_FILE)?).map_err(|_| err())?,
            attributes: u32_at(ATTRIBUTES)?,
            reparse_tag: u32_at(REPARSE_TAG)?,
            created: i64_at(CREATION)?,
            modified: i64_at(LAST_WRITE)?,
            changed: i64_at(CHANGE)?,
        });
        if next == 0 {
            return Ok(out);
        }
        off = off.checked_add(next).ok_or_else(err)?;
    }
}

/// The last path component in a `FILE_NAME_INFO` buffer (a u32 byte length,
/// then the volume-relative path in UTF-16, e.g. `\Users\me\a.txt`), and
/// whether it had to be decoded lossily. `None` if the buffer is malformed.
/// The volume root (`\`) yields an empty name.
pub fn last_name_of_file_name_info(buf: &[u8]) -> Option<(String, bool)> {
    let len = u32::from_le_bytes(le::<4>(buf, 0)?) as usize;
    let bytes = buf.get(4..4usize.checked_add(len)?).filter(|_| len % 2 == 0)?;
    // Split on the last `\` at a UTF-16 unit boundary, before decoding, so
    // a lossy name elsewhere in the path can't affect this one.
    let start = bytes.chunks_exact(2).rposition(|c| c == [b'\\', 0]).map_or(0, |i| (i + 1) * 2);
    Some(utf16le_name(&bytes[start..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn entry(id: u128, name: &str, size: i64, attrs: u32, last: bool) -> Vec<u8> {
        let n: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (NAME + n.len()).next_multiple_of(8);
        let mut e = vec![0u8; len];
        let next = if last { 0 } else { len as u32 };
        e[NEXT..NEXT + 4].copy_from_slice(&next.to_le_bytes());
        e[END_OF_FILE..END_OF_FILE + 8].copy_from_slice(&size.to_le_bytes());
        e[ATTRIBUTES..ATTRIBUTES + 4].copy_from_slice(&attrs.to_le_bytes());
        e[NAME_LEN..NAME_LEN + 4].copy_from_slice(&(n.len() as u32).to_le_bytes());
        e[FILE_ID..FILE_ID + 16].copy_from_slice(&id.to_le_bytes());
        e[NAME..NAME + n.len()].copy_from_slice(&n);
        e
    }

    #[test]
    fn parses_chain() {
        let mut buf = entry(1, ".", 0, FILE_ATTRIBUTE_DIRECTORY, false);
        buf.extend(entry(2, "naïve 😀.txt", 42, 0, true));
        let es = parse_dir_buffer(&buf).unwrap();
        assert_eq!(es.len(), 2);
        assert!(es[0].is_dot() && es[0].is_dir());
        assert_eq!(es[1].file_id, FileId(2));
        assert_eq!(es[1].name, "naïve 😀.txt");
        assert_eq!(es[1].size, 42);
    }

    #[test]
    fn rejects_negative_size_and_overlap() {
        assert!(parse_dir_buffer(&entry(1, "x", -1, 0, true)).is_err());
        let mut e = entry(1, "abcdef", 0, 0, false);
        e[NEXT..NEXT + 4].copy_from_slice(&8u32.to_le_bytes());
        assert!(parse_dir_buffer(&e).is_err());
    }

    #[test]
    fn link_detection() {
        let mut e = parse_dir_buffer(&entry(1, "l", 0, FILE_ATTRIBUTE_REPARSE_POINT, true)).unwrap().remove(0);
        e.reparse_tag = IO_REPARSE_TAG_SYMLINK;
        assert!(e.is_link());
        e.reparse_tag = 0x9000_601A; // IO_REPARSE_TAG_CLOUD_6: a placeholder, not a link
        assert!(!e.is_link());
    }

    fn name_info(path: &str) -> Vec<u8> {
        let n: Vec<u8> = path.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut b = (n.len() as u32).to_le_bytes().to_vec();
        b.extend(n);
        b.extend([0; 6]); // trailing garbage must be ignored
        b
    }

    #[test]
    fn last_name_of_a_path() {
        let name = |p: &str| last_name_of_file_name_info(&name_info(p));
        assert_eq!(name(r"\Users\me\naïve 😀.txt"), Some(("naïve 😀.txt".into(), false)));
        assert_eq!(name(r"\top"), Some(("top".into(), false)));
        assert_eq!(name(r"\"), Some((String::new(), false)));
        // A lone surrogate in a parent folder's name doesn't taint this one.
        let mut b = name_info(r"\ab\c");
        b[6..8].copy_from_slice(&0xD800u16.to_le_bytes());
        assert_eq!(last_name_of_file_name_info(&b), Some(("c".into(), false)));
        assert_eq!(last_name_of_file_name_info(&[9, 0, 0, 0, b'a']), None, "length past the end");
        assert_eq!(last_name_of_file_name_info(&[1, 0]), None, "truncated header");
    }

    proptest! {
        #[test]
        fn never_panics(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse_dir_buffer(&data);
            let _ = last_name_of_file_name_info(&data);
        }
    }
}
