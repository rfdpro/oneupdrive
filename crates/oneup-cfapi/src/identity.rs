//! The file identity OneUpDrive gives its placeholders
//! (`docs/architecture/files-on-demand.md` § Placeholders and identity).
//! Windows stores it with the placeholder and hands it back in every
//! callback, so a fetch knows exactly which cloud version the placeholder
//! stands for (`data-safety.md` invariant 13).

/// Version 1: `1 | id length (u16 LE) | id | eTag length (u16 LE) | eTag |
/// size (i64 LE) | QuickXorHash (20 bytes)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The OneDrive item ID.
    pub item: String,
    pub etag: String,
    pub size: i64,
    pub hash: [u8; 20],
}

const VERSION: u8 = 1;
/// Windows allows at most 4 KiB of identity.
pub const MAX_LEN: usize = 4096;

impl Identity {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(1 + 4 + self.item.len() + self.etag.len() + 8 + 20);
        v.push(VERSION);
        for s in [&self.item, &self.etag] {
            let b = s.as_bytes();
            v.extend_from_slice(&(b.len().min(usize::from(u16::MAX)) as u16).to_le_bytes());
            v.extend_from_slice(&b[..b.len().min(usize::from(u16::MAX))]);
        }
        v.extend_from_slice(&self.size.to_le_bytes());
        v.extend_from_slice(&self.hash);
        v
    }

    /// `None` for anything that isn't a version-1 identity of ours.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let (&version, mut rest) = b.split_first()?;
        if version != VERSION {
            return None;
        }
        let mut text = || -> Option<String> {
            let (len, r) = rest.split_at_checked(2)?;
            let len = usize::from(u16::from_le_bytes([len[0], len[1]]));
            let (s, r) = r.split_at_checked(len)?;
            rest = r;
            String::from_utf8(s.to_vec()).ok()
        };
        let item = text()?;
        let etag = text()?;
        let (size, r) = rest.split_at_checked(8)?;
        let (hash, r) = r.split_at_checked(20)?;
        if !r.is_empty() {
            return None;
        }
        Some(Self { item, etag, size: i64::from_le_bytes(size.try_into().ok()?), hash: hash.try_into().ok()? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_round_trip_and_reject_anything_else() {
        let id = Identity { item: "ABC!123".into(), etag: "aBc,1".into(), size: 1 << 40, hash: [7; 20] };
        let b = id.encode();
        assert!(b.len() < MAX_LEN);
        assert_eq!(Identity::decode(&b), Some(id));
        assert_eq!(Identity::decode(b"oneup-spike-root"), None);
        assert_eq!(Identity::decode(&b[..b.len() - 1]), None, "truncated");
        let mut longer = b.clone();
        longer.push(0);
        assert_eq!(Identity::decode(&longer), None, "trailing bytes");
        assert_eq!(Identity::decode(&[]), None);
    }
}
