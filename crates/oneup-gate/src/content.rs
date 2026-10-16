//! The content guard's judge: does a file's beginning look like its type?
//!
//! Ransomware usually encrypts files where they are, keeping their names.
//! Sync would then faithfully send every encrypted file to OneDrive. A
//! file's first bytes give it away: a `.docx` is a zip and starts with `PK`,
//! a `.pdf` starts with `%PDF`, a `.txt` is text; encrypted content is none
//! of these. Only types with a reliable signature (or plain text) are
//! judged; anything else is never called suspicious. Pure: the caller reads
//! the bytes (`LocalFs::sample`).

/// How many bytes of a file's beginning [`looks_wrong`] wants.
pub const HEAD: usize = 4096;

/// Whether `head` (a file's first bytes, up to [`HEAD`]) doesn't look like
/// what the extension of `name` promises. `false` for types it can't judge
/// and for files too short to tell.
pub fn looks_wrong(name: &str, head: &[u8]) -> bool {
    if head.len() < 16 {
        return false;
    }
    let ext = match name.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => return false,
    };
    let starts = |m: &[u8]| head.starts_with(m);
    let ok = match ext.as_str() {
        // Zip containers (Office, OpenDocument, e-books, archives).
        "docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm" | "odt" | "ods" | "odp" | "zip" | "jar" | "epub"
        | "vsdx" | "xps" | "apk" => starts(b"PK\x03\x04") || starts(b"PK\x05\x06") || starts(b"PK\x07\x08"),
        // Older Office files and Outlook messages.
        "doc" | "xls" | "ppt" | "msg" | "vsd" => starts(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]),
        // Some PDF writers put a few bytes first; the signature must come early.
        "pdf" => head[..head.len().min(1024)].windows(5).any(|w| w == b"%PDF-"),
        "rtf" => starts(b"{\\rtf"),
        "png" => starts(b"\x89PNG\r\n\x1a\n"),
        "jpg" | "jpeg" => starts(&[0xFF, 0xD8, 0xFF]),
        "gif" => starts(b"GIF87a") || starts(b"GIF89a"),
        "bmp" => starts(b"BM"),
        "tif" | "tiff" => starts(b"II*\0") || starts(b"MM\0*"),
        "psd" => starts(b"8BPS"),
        "webp" | "wav" | "avi" => starts(b"RIFF"),
        "mp4" | "m4a" | "m4v" | "heic" | "heif" | "3gp" => &head[4..8] == b"ftyp",
        // Older QuickTime files start with whatever atom came first.
        "mov" => matches!(&head[4..8], b"ftyp" | b"wide" | b"mdat" | b"moov" | b"free" | b"skip"),
        "mp3" => starts(b"ID3") || (head[0] == 0xFF && head[1] & 0xE0 == 0xE0),
        "flac" => starts(b"fLaC"),
        "ogg" => starts(b"OggS"),
        "7z" => starts(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]),
        "rar" => starts(b"Rar!\x1a\x07"),
        "gz" | "tgz" => starts(&[0x1F, 0x8B]),
        "exe" | "dll" => starts(b"MZ"),
        "sqlite" | "sqlite3" => starts(b"SQLite format 3\0"),
        "txt" | "md" | "csv" | "tsv" | "json" | "xml" | "html" | "htm" | "css" | "js" | "py" | "rs" | "c" | "h"
        | "cpp" | "hpp" | "cs" | "java" | "go" | "rb" | "php" | "sh" | "ps1" | "bat" | "cmd" | "ini" | "cfg"
        | "yaml" | "yml" | "toml" | "log" | "sql" | "svg" | "tex" | "srt" | "vtt" => is_text(head),
        // TypeScript, or an MPEG transport stream (video): 188-byte packets,
        // each starting with the sync byte 0x47.
        "ts" => is_text(head) || head.iter().step_by(188).all(|&b| b == 0x47),
        _ => return false,
    };
    !ok
}

/// Text in an 8-bit encoding (UTF-8 or a code page), or UTF-16 with its
/// byte-order mark. Encrypted bytes are random, so about one in nine is a
/// control character no text has; a little slack covers odd files.
fn is_text(head: &[u8]) -> bool {
    if head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        return true;
    }
    let control = head.iter().filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0C | 0x1A)).count();
    control * 100 <= head.len() * 2
}

#[cfg(test)]
mod tests {
    use super::looks_wrong;

    /// Bytes that look random, like encrypted content (a fixed generator,
    /// so the test is repeatable).
    fn noise(n: usize) -> Vec<u8> {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn with(prefix: &[u8]) -> Vec<u8> {
        let mut v = prefix.to_vec();
        v.extend(noise(4096 - prefix.len()));
        v
    }

    #[test]
    fn real_formats_pass() {
        assert!(!looks_wrong("Report.DOCX", &with(b"PK\x03\x04")));
        assert!(!looks_wrong("old.doc", &with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1])));
        assert!(!looks_wrong("a.pdf", &with(b"%PDF-1.7")));
        assert!(!looks_wrong("a.pdf", &with(b"\r\n\r\n%PDF-1.4")));
        assert!(!looks_wrong("p.jpg", &with(&[0xFF, 0xD8, 0xFF, 0xE0])));
        assert!(!looks_wrong("v.mov", &with(b"\0\0\0\x14ftypqt  ")));
        let text = "Grüße, 東京 — notes\r\n\tline two\n".repeat(80);
        assert!(!looks_wrong("notes.txt", text.as_bytes()));
        assert!(!looks_wrong("data.csv", b"name,size\nalpha,1\nbeta,2\ngamma,3\n"));
        let utf16: Vec<u8> =
            [0xFF, 0xFE].into_iter().chain("hello world".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert!(!looks_wrong("export.reg.txt", &utf16));
    }

    /// A QuickTime file needn't start with `ftyp`, and `.ts` is also MPEG
    /// transport stream video, not only TypeScript.
    #[test]
    fn older_quicktime_and_transport_streams_pass() {
        for atom in [b"wide", b"mdat", b"moov", b"free", b"skip"] {
            let mut v = b"\0\0\0\x08".to_vec();
            v.extend(atom);
            assert!(!looks_wrong("old.mov", &with(&v)), "{}", String::from_utf8_lossy(atom));
        }
        let mut stream = noise(188 * 22);
        for packet in stream.chunks_mut(188) {
            packet[0] = 0x47;
        }
        assert!(!looks_wrong("clip.ts", &stream[..4096]));
        assert!(!looks_wrong("app.ts", b"export const answer: number = 42;\n"));
    }

    #[test]
    fn encrypted_content_is_caught() {
        let n = noise(4096);
        for name in [
            "Report.docx",
            "a.pdf",
            "p.jpg",
            "notes.txt",
            "sheet.xlsx",
            "v.mp4",
            "v.mov",
            "old.xls",
            "page.html",
            "clip.ts",
        ] {
            assert!(looks_wrong(name, &n), "{name}");
        }
    }

    #[test]
    fn unknown_types_and_tiny_files_are_never_judged() {
        let n = noise(4096);
        assert!(!looks_wrong("game.pak", &n));
        assert!(!looks_wrong("noextension", &n));
        assert!(!looks_wrong("Report.docx.locked", &n));
        assert!(!looks_wrong("a.docx", &n[..10]));
    }
}
