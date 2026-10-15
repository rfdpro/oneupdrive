//! What the user sent to this PC's Recycle Bin, as deletion evidence.
//!
//! Deleting in File Explorer moves the item into the Recycle Bin, which the
//! change journal records as a move out of the synced folder, not a
//! delete: such items were held as "missing without evidence" and never
//! deleted in OneDrive. So an item found in
//! the user's own Recycle Bin under its synced path is deleted by the user.
//! Its OneDrive copy goes to OneDrive's recycle bin through the safety gate
//! (a large batch stops and asks), so it's recoverable on both sides.
//!
//! Windows keeps, for each deleted item, a small index file `$I<id>` in
//! `<drive>:\$Recycle.Bin\<user SID>\` holding its original path. Only
//! those are read (never the deleted items themselves, `$R<id>`), and only
//! when they're ordinary files.

use std::path::{Path, PathBuf};

/// The original paths (lowercase, without `\\?\`) of what this Windows
/// user sent to the Recycle Bin on the drive holding `root`. Empty if the
/// bin can't be read.
pub fn originals(root: &Path) -> Vec<String> {
    let Some(bin) = bin_of(root) else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&bin) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().starts_with("$I") {
            continue;
        }
        // Ordinary small files only (attributes, without opening).
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        if !meta.is_file() || meta.len() > 64 * 1024 || {
            use std::os::windows::fs::MetadataExt as _;
            meta.file_attributes() & 0x0040_0400 != 0 // REPARSE_POINT | RECALL_ON_DATA_ACCESS
        } {
            continue;
        }
        if let Ok(bytes) = std::fs::read(e.path())
            && let Some(p) = parse(&bytes)
        {
            out.push(p.to_lowercase());
        }
    }
    out
}

/// `<drive>:\$Recycle.Bin\<SID>` for the drive holding `root`.
fn bin_of(root: &Path) -> Option<PathBuf> {
    let s = root.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let drive = s.get(..2).filter(|d| d.ends_with(':'))?;
    let sid = oneup_sys::pipe::current_user_sid().ok()?;
    Some(PathBuf::from(format!(r"{drive}\$Recycle.Bin\{sid}")))
}

/// The original path in a `$I` index file: version 2 (Windows 10 on) has
/// the length at 24 and the path after it; version 1 a fixed 260
/// characters at 24.
fn parse(b: &[u8]) -> Option<String> {
    let u64_at = |o: usize| b.get(o..o + 8).map(|x| u64::from_le_bytes(x.try_into().unwrap_or([0; 8])));
    let utf16 = |bytes: &[u8]| {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        String::from_utf16(&units[..end]).ok()
    };
    let path = match u64_at(0)? {
        2 => {
            let len = u32::from_le_bytes(b.get(24..28)?.try_into().ok()?) as usize;
            utf16(b.get(28..28 + len.checked_mul(2)?)?)?
        }
        1 => utf16(b.get(24..24 + 520)?)?,
        _ => return None,
    };
    (!path.is_empty()).then_some(path)
}

/// Whether `path` (full, any case) is one of `originals` or inside one.
pub fn sent(path: &Path, originals: &[String]) -> bool {
    let s = path.to_string_lossy();
    let p = s.strip_prefix(r"\\?\").unwrap_or(&s).to_lowercase();
    originals.iter().any(|o| p == *o || (p.starts_with(o.as_str()) && p.as_bytes().get(o.len()) == Some(&b'\\')))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(path: &str) -> Vec<u8> {
        let units: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut b = Vec::new();
        b.extend(2u64.to_le_bytes());
        b.extend(1234u64.to_le_bytes()); // size
        b.extend(0u64.to_le_bytes()); // deletion time
        b.extend((units.len() as u32).to_le_bytes());
        for u in units {
            b.extend(u.to_le_bytes());
        }
        b
    }

    #[test]
    fn index_files_give_the_original_path() {
        assert_eq!(parse(&v2(r"C:\Users\x\OneDrive\Old")).as_deref(), Some(r"C:\Users\x\OneDrive\Old"));
        let mut v1 = Vec::new();
        v1.extend(1u64.to_le_bytes());
        v1.extend([0u8; 16]);
        let mut path: Vec<u16> = r"D:\a.txt".encode_utf16().collect();
        path.resize(260, 0);
        v1.extend(path.iter().flat_map(|u| u.to_le_bytes()));
        assert_eq!(parse(&v1).as_deref(), Some(r"D:\a.txt"));
        assert_eq!(parse(&[9, 0, 0]), None);
        assert_eq!(parse(&v2("")), None);
    }

    #[test]
    fn an_item_counts_as_sent_with_the_folder_it_was_in() {
        let bin = vec![r"c:\users\x\onedrive\project".to_string()];
        assert!(sent(Path::new(r"\\?\C:\Users\x\OneDrive\Project"), &bin));
        assert!(sent(Path::new(r"C:\Users\x\OneDrive\Project\apps\a.js"), &bin));
        assert!(!sent(Path::new(r"C:\Users\x\OneDrive\Project-2"), &bin), "a sibling sharing the prefix");
        assert!(!sent(Path::new(r"C:\Users\x\OneDrive"), &bin));
    }
}
