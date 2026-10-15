//! USN change-journal record parsing.
//!
//! The output of `FSCTL_READ_USN_JOURNAL` / `FSCTL_READ_UNPRIVILEGED_USN_JOURNAL`
//! is an 8-byte "next USN" followed by packed variable-length records. This
//! module parses that buffer without `unsafe` and treats every length and
//! offset as untrusted, because a malformed buffer must never crash the agent
//! or produce a misattributed event.

use crate::{FileId, le, utf16le_name};

/// `USN_REASON_*` flags.
pub mod reason {
    pub const DATA_OVERWRITE: u32 = 0x0000_0001;
    pub const DATA_EXTEND: u32 = 0x0000_0002;
    pub const DATA_TRUNCATION: u32 = 0x0000_0004;
    pub const NAMED_DATA_OVERWRITE: u32 = 0x0000_0010;
    pub const NAMED_DATA_EXTEND: u32 = 0x0000_0020;
    pub const NAMED_DATA_TRUNCATION: u32 = 0x0000_0040;
    pub const FILE_CREATE: u32 = 0x0000_0100;
    pub const FILE_DELETE: u32 = 0x0000_0200;
    pub const EA_CHANGE: u32 = 0x0000_0400;
    pub const SECURITY_CHANGE: u32 = 0x0000_0800;
    pub const RENAME_OLD_NAME: u32 = 0x0000_1000;
    pub const RENAME_NEW_NAME: u32 = 0x0000_2000;
    pub const INDEXABLE_CHANGE: u32 = 0x0000_4000;
    pub const BASIC_INFO_CHANGE: u32 = 0x0000_8000;
    pub const HARD_LINK_CHANGE: u32 = 0x0001_0000;
    pub const COMPRESSION_CHANGE: u32 = 0x0002_0000;
    pub const ENCRYPTION_CHANGE: u32 = 0x0004_0000;
    pub const OBJECT_ID_CHANGE: u32 = 0x0008_0000;
    pub const REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
    pub const STREAM_CHANGE: u32 = 0x0020_0000;
    pub const TRANSACTED_CHANGE: u32 = 0x0040_0000;
    pub const INTEGRITY_CHANGE: u32 = 0x0080_0000;
    pub const DESIRED_STORAGE_CLASS_CHANGE: u32 = 0x0100_0000;
    pub const CLOSE: u32 = 0x8000_0000;

    /// Any change to the default data stream's contents.
    pub const CONTENT: u32 = DATA_OVERWRITE | DATA_EXTEND | DATA_TRUNCATION;

    const NAMES: &[(u32, &str)] = &[
        (DATA_OVERWRITE, "DATA_OVERWRITE"),
        (DATA_EXTEND, "DATA_EXTEND"),
        (DATA_TRUNCATION, "DATA_TRUNCATION"),
        (NAMED_DATA_OVERWRITE, "NAMED_DATA_OVERWRITE"),
        (NAMED_DATA_EXTEND, "NAMED_DATA_EXTEND"),
        (NAMED_DATA_TRUNCATION, "NAMED_DATA_TRUNCATION"),
        (FILE_CREATE, "FILE_CREATE"),
        (FILE_DELETE, "FILE_DELETE"),
        (EA_CHANGE, "EA_CHANGE"),
        (SECURITY_CHANGE, "SECURITY_CHANGE"),
        (RENAME_OLD_NAME, "RENAME_OLD_NAME"),
        (RENAME_NEW_NAME, "RENAME_NEW_NAME"),
        (INDEXABLE_CHANGE, "INDEXABLE_CHANGE"),
        (BASIC_INFO_CHANGE, "BASIC_INFO_CHANGE"),
        (HARD_LINK_CHANGE, "HARD_LINK_CHANGE"),
        (COMPRESSION_CHANGE, "COMPRESSION_CHANGE"),
        (ENCRYPTION_CHANGE, "ENCRYPTION_CHANGE"),
        (OBJECT_ID_CHANGE, "OBJECT_ID_CHANGE"),
        (REPARSE_POINT_CHANGE, "REPARSE_POINT_CHANGE"),
        (STREAM_CHANGE, "STREAM_CHANGE"),
        (TRANSACTED_CHANGE, "TRANSACTED_CHANGE"),
        (INTEGRITY_CHANGE, "INTEGRITY_CHANGE"),
        (DESIRED_STORAGE_CLASS_CHANGE, "DESIRED_STORAGE_CLASS_CHANGE"),
        (CLOSE, "CLOSE"),
    ];

    /// Human-readable `A|B|C` rendering for logs and diagnostics.
    pub fn describe(mask: u32) -> String {
        let parts: Vec<&str> = NAMES.iter().filter(|(b, _)| mask & b != 0).map(|(_, n)| *n).collect();
        if parts.is_empty() { format!("{mask:#x}") } else { parts.join("|") }
    }
}

/// One parsed change-journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    pub major_version: u16,
    pub file_id: FileId,
    pub parent_id: FileId,
    pub usn: i64,
    /// FILETIME (100 ns ticks since 1601-01-01 UTC).
    pub timestamp: i64,
    pub reason: u32,
    pub source_info: u32,
    pub attributes: u32,
    /// `None` when the record carries no name (possible in unprivileged mode).
    pub name: Option<String>,
    /// The name contained unpaired UTF-16 surrogates and was decoded lossily.
    pub name_lossy: bool,
}

impl UsnRecord {
    pub fn has(&self, flags: u32) -> bool {
        self.reason & flags != 0
    }

    pub fn is_directory(&self) -> bool {
        const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("buffer shorter than the 8-byte next-USN header")]
    MissingHeader,
    #[error("record at offset {offset}: length {len} is invalid")]
    BadLength { offset: usize, len: u32 },
    #[error("record at offset {offset}: unsupported major version {version}")]
    UnsupportedVersion { offset: usize, version: u16 },
    #[error("record at offset {offset}: file name lies outside the record")]
    BadName { offset: usize },
}

/// Splits a journal read buffer into its next-USN cursor and a record iterator.
pub fn parse_read_buffer(buf: &[u8]) -> Result<(i64, Records<'_>), ParseError> {
    let next = le::<8>(buf, 0).map(i64::from_le_bytes).ok_or(ParseError::MissingHeader)?;
    Ok((next, Records { buf, offset: 8 }))
}

/// Iterator over packed records. Stops after the first error, because once a
/// length is wrong nothing after it can be trusted.
pub struct Records<'a> {
    buf: &'a [u8],
    offset: usize,
}

impl Iterator for Records<'_> {
    type Item = Result<UsnRecord, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.buf.len() {
            return None;
        }
        let offset = self.offset;
        let rec = parse_record(&self.buf[offset..]).map_err(|e| e.at(offset));
        match &rec {
            Ok((_, len)) => self.offset += len,
            Err(_) => self.offset = self.buf.len(),
        }
        Some(rec.map(|(r, _)| r))
    }
}

/// Parse errors are produced relative to the record start and re-based here.
trait At {
    fn at(self, offset: usize) -> ParseError;
}

impl At for ParseError {
    fn at(self, base: usize) -> ParseError {
        match self {
            ParseError::BadLength { offset, len } => ParseError::BadLength { offset: base + offset, len },
            ParseError::UnsupportedVersion { offset, version } => {
                ParseError::UnsupportedVersion { offset: base + offset, version }
            }
            ParseError::BadName { offset } => ParseError::BadName { offset: base + offset },
            e => e,
        }
    }
}

/// Parses one record at the start of `buf`; returns it and its length.
pub fn parse_record(buf: &[u8]) -> Result<(UsnRecord, usize), ParseError> {
    let bad_len = |len| ParseError::BadLength { offset: 0, len };
    let len = le::<4>(buf, 0).map(u32::from_le_bytes).ok_or(bad_len(0))?;
    let ulen = len as usize;
    // Records are 8-byte aligned and never shorter than a V2 header.
    if ulen < V2_NAME || ulen % 8 != 0 || ulen > buf.len() {
        return Err(bad_len(len));
    }
    let rec = &buf[..ulen];
    let version = u16::from_le_bytes(le::<2>(rec, 4).ok_or(bad_len(len))?);

    let (file_id, parent_id, base) = match version {
        2 => (
            FileId::from_frn64(u64::from_le_bytes(le::<8>(rec, 8).ok_or(bad_len(len))?)),
            FileId::from_frn64(u64::from_le_bytes(le::<8>(rec, 16).ok_or(bad_len(len))?)),
            24,
        ),
        3 => {
            if ulen < V3_NAME {
                return Err(bad_len(len));
            }
            (
                FileId::from_le_bytes(le::<16>(rec, 8).ok_or(bad_len(len))?),
                FileId::from_le_bytes(le::<16>(rec, 24).ok_or(bad_len(len))?),
                40,
            )
        }
        v => return Err(ParseError::UnsupportedVersion { offset: 0, version: v }),
    };

    // Fixed tail shared by V2 and V3, starting at `base`.
    let i64_at = |o: usize| le::<8>(rec, base + o).map(i64::from_le_bytes).ok_or(bad_len(len));
    let u32_at = |o: usize| le::<4>(rec, base + o).map(u32::from_le_bytes).ok_or(bad_len(len));
    let u16_at = |o: usize| le::<2>(rec, base + o).map(u16::from_le_bytes).ok_or(bad_len(len));
    let usn = i64_at(0)?;
    let timestamp = i64_at(8)?;
    let reason = u32_at(16)?;
    let source_info = u32_at(20)?;
    // +24: SecurityId (unused)
    let attributes = u32_at(28)?;
    let name_len = usize::from(u16_at(32)?);
    let name_off = usize::from(u16_at(34)?);

    let (name, name_lossy) = if name_len == 0 {
        (None, false)
    } else {
        let bytes = name_off
            .checked_add(name_len)
            .filter(|_| name_len % 2 == 0 && name_off >= base + 36)
            .and_then(|end| rec.get(name_off..end))
            .ok_or(ParseError::BadName { offset: 0 })?;
        let (s, lossy) = utf16le_name(bytes);
        (Some(s), lossy)
    };

    Ok((
        UsnRecord {
            major_version: version,
            file_id,
            parent_id,
            usn,
            timestamp,
            reason,
            source_info,
            attributes,
            name,
            name_lossy,
        },
        ulen,
    ))
}

/// Offset of `FileName` in `USN_RECORD_V2` / `USN_RECORD_V3`.
const V2_NAME: usize = 60;
const V3_NAME: usize = 76;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Builds a well-formed V3 record (test helper, also used by fuzz-style tests).
    pub fn v3(file: u128, parent: u128, usn: i64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let name16: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (V3_NAME + name16.len()).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&3u16.to_le_bytes());
        r[8..24].copy_from_slice(&file.to_le_bytes());
        r[24..40].copy_from_slice(&parent.to_le_bytes());
        r[40..48].copy_from_slice(&usn.to_le_bytes());
        r[56..60].copy_from_slice(&reason.to_le_bytes());
        r[68..72].copy_from_slice(&attrs.to_le_bytes());
        r[72..74].copy_from_slice(&(name16.len() as u16).to_le_bytes());
        r[74..76].copy_from_slice(&(V3_NAME as u16).to_le_bytes());
        r[V3_NAME..V3_NAME + name16.len()].copy_from_slice(&name16);
        r
    }

    fn v2(file: u64, parent: u64, usn: i64, reason: u32, name: &str) -> Vec<u8> {
        let name16: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let len = (V2_NAME + name16.len()).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&file.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[56..58].copy_from_slice(&(name16.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&(V2_NAME as u16).to_le_bytes());
        r[V2_NAME..V2_NAME + name16.len()].copy_from_slice(&name16);
        r
    }

    fn buffer(next: i64, recs: &[Vec<u8>]) -> Vec<u8> {
        let mut b = next.to_le_bytes().to_vec();
        recs.iter().for_each(|r| b.extend_from_slice(r));
        b
    }

    #[test]
    fn parses_v2_and_v3() {
        let buf = buffer(
            99,
            &[
                v3(7, 5, 10, reason::FILE_CREATE | reason::CLOSE, 0x10, "dir"),
                v2(8, 7, 11, reason::DATA_EXTEND, "a.txt"),
            ],
        );
        let (next, recs) = parse_read_buffer(&buf).unwrap();
        let recs: Vec<_> = recs.collect::<Result<_, _>>().unwrap();
        assert_eq!(next, 99);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].file_id, FileId(7));
        assert_eq!(recs[0].parent_id, FileId(5));
        assert!(recs[0].is_directory());
        assert!(recs[0].has(reason::CLOSE));
        assert_eq!(recs[0].name.as_deref(), Some("dir"));
        assert_eq!(recs[1].major_version, 2);
        assert_eq!(recs[1].file_id, FileId(8));
        assert_eq!(recs[1].usn, 11);
        assert_eq!(recs[1].name.as_deref(), Some("a.txt"));
    }

    #[test]
    fn empty_name_is_none() {
        let buf = buffer(1, &[v3(1, 2, 3, reason::CLOSE, 0, "")]);
        let rec = parse_read_buffer(&buf).unwrap().1.next().unwrap().unwrap();
        assert_eq!(rec.name, None);
    }

    #[test]
    fn unpaired_surrogate_is_flagged() {
        let mut r = v3(1, 2, 3, 0, 0, "ab");
        // Overwrite 'b' with a lone high surrogate.
        r[V3_NAME + 2..V3_NAME + 4].copy_from_slice(&0xD800u16.to_le_bytes());
        let rec = parse_record(&r).unwrap().0;
        assert!(rec.name_lossy);
    }

    #[test]
    fn rejects_bad_lengths_and_stops() {
        let mut bad = v3(1, 2, 3, 0, 0, "x");
        bad[0..4].copy_from_slice(&4u32.to_le_bytes());
        let buf = buffer(0, &[bad, v3(9, 9, 9, 0, 0, "never")]);
        let items: Vec<_> = parse_read_buffer(&buf).unwrap().1.collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Err(ParseError::BadLength { offset: 8, .. })));
    }

    #[test]
    fn rejects_name_outside_record() {
        let mut r = v3(1, 2, 3, 0, 0, "abc");
        let past_end = r.len() as u16;
        r[74..76].copy_from_slice(&past_end.to_le_bytes());
        assert!(matches!(parse_record(&r), Err(ParseError::BadName { .. })));
    }

    #[test]
    fn rejects_unknown_version() {
        let mut r = v3(1, 2, 3, 0, 0, "abc");
        r[4..6].copy_from_slice(&4u16.to_le_bytes());
        assert!(matches!(parse_record(&r), Err(ParseError::UnsupportedVersion { version: 4, .. })));
    }

    #[test]
    fn describe_reasons() {
        assert_eq!(reason::describe(reason::FILE_CREATE | reason::CLOSE), "FILE_CREATE|CLOSE");
    }

    proptest! {
        /// Arbitrary bytes never panic, and every record yielded lies within the buffer.
        #[test]
        fn never_panics(data in proptest::collection::vec(any::<u8>(), 0..1024)) {
            if let Ok((_, recs)) = parse_read_buffer(&data) {
                for r in recs.take(200) {
                    let _ = r;
                }
            }
        }

        /// Round trip of well-formed V3 records with arbitrary names.
        #[test]
        fn roundtrip(file in any::<u128>(), parent in any::<u128>(), usn in any::<i64>(),
                     reason in any::<u32>(), name in "\\PC{0,40}") {
            let r = v3(file, parent, usn, reason, 0, &name);
            let (rec, len) = parse_record(&r).unwrap();
            prop_assert_eq!(len, r.len());
            prop_assert_eq!(rec.file_id, FileId(file));
            prop_assert_eq!(rec.parent_id, FileId(parent));
            prop_assert_eq!(rec.usn, usn);
            prop_assert_eq!(rec.reason, reason);
            prop_assert_eq!(rec.name.unwrap_or_default(), name);
        }
    }
}
