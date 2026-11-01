//! Approval to publish one reviewed portable-settings document.
use oneup_sync::{RemoteId, RemotePre};

#[derive(Debug)]
pub struct SettingsWrite {
    parent: RemoteId,
    replace: Option<RemotePre>,
    content: Vec<u8>,
}
impl SettingsWrite {
    pub fn parent(&self) -> &RemoteId {
        &self.parent
    }
    pub fn replace(&self) -> Option<&RemotePre> {
        self.replace.as_ref()
    }
    pub fn content(&self) -> &[u8] {
        &self.content
    }
}

/// The caller verified the schema and the review code over the local and
/// remote documents. The token owns the exact bytes reviewed.
pub fn review(
    parent: RemoteId,
    replace: Option<RemotePre>,
    content: Vec<u8>,
    confirmed: bool,
) -> Option<SettingsWrite> {
    if !confirmed
        || parent.0.is_empty()
        || content.is_empty()
        || content.len() > 1024 * 1024
        || replace.as_ref().is_some_and(|r| r.etag.is_empty() || r.id.0.is_empty())
    {
        return None;
    }
    Some(SettingsWrite { parent, replace, content })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publishing_requires_confirmation_and_preconditions() {
        let parent = || RemoteId("root".into());
        assert!(review(parent(), None, b"{}".to_vec(), false).is_none());
        assert!(
            review(
                parent(),
                Some(RemotePre { id: oneup_sync::RemoteId("s".into()), etag: String::new() }),
                b"{}".to_vec(),
                true
            )
            .is_none()
        );
        assert_eq!(review(parent(), None, b"{}".to_vec(), true).unwrap().content(), b"{}");
    }
}
