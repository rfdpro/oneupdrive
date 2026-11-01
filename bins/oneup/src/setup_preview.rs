//! Bounded pages of a completed setup comparison, stored in our data folder.

use std::io::{BufRead, BufReader, Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub fn path(dir: &Path, token: &str) -> Result<PathBuf> {
    if token.is_empty() || token.len() > 64 || !token.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        bail!("invalid preview token");
    }
    Ok(dir.join(format!("setup-preview-{token}.txt")))
}

pub fn page(dir: &Path, token: &str, offset: u64) -> Result<Value> {
    let file = std::fs::File::open(path(dir, token)?).context("open the setup preview")?;
    let len = file.metadata()?.len();
    if offset > len {
        bail!("invalid preview position");
    }
    let mut input = BufReader::new(file);
    input.seek(std::io::SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    for _ in 0..200 {
        let room = 128 * 1024 - bytes.len();
        if room == 0 || input.by_ref().take(room as u64).read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
    }
    let next = input.stream_position()?;
    Ok(json!({ "token": token, "text": String::from_utf8_lossy(&bytes),
        "next": (next < len).then_some(next) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preview_line_can_be_read_with_bounded_pages() {
        let dir = std::env::temp_dir().join(format!("oneup-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text: String = (0..501).map(|n| format!("item {n}\n")).collect();
        let p = path(&dir, "abc-1").unwrap();
        std::fs::write(&p, &text).unwrap();
        let mut got = String::new();
        let mut at = 0;
        loop {
            let v = page(&dir, "abc-1", at).unwrap();
            let part = v["text"].as_str().unwrap();
            assert!(part.lines().count() <= 200);
            got.push_str(part);
            let Some(next) = v["next"].as_u64() else { break };
            assert!(next > at);
            at = next;
        }
        assert_eq!(got, text);
        assert!(path(&dir, "../config.toml").is_err());
        assert!(page(&dir, "abc-1", u64::MAX).is_err());
        std::fs::remove_file(p).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
