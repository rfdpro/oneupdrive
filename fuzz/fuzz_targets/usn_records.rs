//! Change-journal and directory-enumeration buffers, as the kernel hands
//! them over: `oneup_ntfs::record` and `oneup_ntfs::dirent` must reject any
//! malformed buffer without panicking, and never report a record that
//! reaches past the buffer.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oneup_ntfs::{dirent, record};

fuzz_target!(|data: &[u8]| {
    if let Ok((_next, records)) = record::parse_read_buffer(data) {
        let mut seen = 0usize;
        for r in records {
            if r.is_err() {
                break;
            }
            seen += 1;
        }
        // Every record is at least a V2 header (60 bytes) long.
        assert!(seen <= data.len().saturating_sub(8) / 60);
    }
    if let Ok((_, len)) = record::parse_record(data) {
        assert!(len <= data.len() && len.is_multiple_of(8));
    }
    if let Ok(entries) = dirent::parse_dir_buffer(data) {
        assert!(!entries.is_empty());
    }
    let _ = dirent::last_name_of_file_name_info(data);
});
