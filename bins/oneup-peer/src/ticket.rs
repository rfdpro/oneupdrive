use std::str::FromStr;

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh_blobs::ticket::BlobTicket;
use serde::{Deserialize, Serialize};

pub const LIMIT: usize = 8192;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub blob: BlobTicket,
    pub name: String,
    pub size: u64,
}

impl Ticket {
    pub fn encode(&self) -> Result<String> {
        self.validate()?;
        let text = format!("oneup1.{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?));
        ensure!(text.len() <= LIMIT, "Ticket is too long.");
        Ok(text)
    }

    fn validate(&self) -> Result<()> {
        file_name(&self.name)?;
        ensure!(!self.blob.recursive() && !self.blob.addr().addrs.is_empty(), "Expected a single-file ticket.");
        ensure!(self.blob.addr().relay_urls().all(|url| url.scheme() == "https"), "Expected an HTTPS relay address.");
        Ok(())
    }
}

impl FromStr for Ticket {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let text = text.trim();
        ensure!(text.len() <= LIMIT, "Ticket is too long.");
        let encoded = text.strip_prefix("oneup1.").ok_or_else(|| anyhow::anyhow!("Invalid P2P ticket."))?;
        let ticket: Self = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded)?)?;
        ticket.validate()?;
        Ok(ticket)
    }
}

// Treat the name as one path component on every supported OS. The prefix
// before the first dot is also significant for Windows device names.
pub fn file_name(name: &str) -> Result<()> {
    let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                (suffix.len() == 1 && suffix.as_bytes()[0].is_ascii_digit()) || matches!(suffix, "¹" | "²" | "³")
            })
        });
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && !name.ends_with(['.', ' '])
            && !device
            && !name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)),
        "Invalid file name in P2P share."
    );
    Ok(())
}
