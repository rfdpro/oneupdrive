//! Publishes the gate's exact settings bytes under an eTag precondition.
use oneup_gate::settings::SettingsWrite;
use oneup_graph::{DriveItem, Graph};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Graph(#[from] oneup_graph::Error),
    #[error("shared settings changed; review them again")]
    Changed,
    #[error("shared settings transfer did not verify")]
    Unverified,
}

pub async fn publish(graph: &Graph, token: SettingsWrite) -> Result<DriveItem, Error> {
    let parent = token.parent();
    let current = graph.child_named(&parent.0, "oneup.settings").await?;
    match (token.replace(), &current) {
        (None, None) => {}
        (Some(want), Some(now))
            if now.id == want.id.0
                && now.e_tag.as_deref() == Some(&want.etag)
                && now.file.is_some()
                && now.folder.is_none() => {}
        _ => return Err(Error::Changed),
    }
    let replace = token.replace().map(|p| (p.id.0.as_str(), p.etag.as_str()));
    let item = graph.upload_small(&parent.0, "oneup.settings", replace, token.content()).await?;
    let mut hash = oneup_hash::QuickXorHash::new();
    hash.update(token.content());
    let expected = hash.finalize();
    let actual = item.quick_xor_hash();
    if item.size != Some(token.content().len() as i64) || actual != Some(expected) {
        return Err(Error::Unverified);
    }
    Ok(item)
}
