//! Search presets belong to the window's data folder, across its file roots.

use std::path::Path;

use anyhow::{Result, bail};
use oneup_store::{Error, Ops};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const KEY: &str = "ui.saved_searches";
const MAX_SAVED: usize = 50;

#[derive(Serialize, Deserialize)]
pub struct Saved {
    name: String,
    form: Value,
}

fn read(o: &Ops<'_>) -> oneup_store::Result<Vec<Saved>> {
    o.meta_get(KEY)?
        .map_or(Ok(Vec::new()), |v| serde_json::from_str(&v).map_err(|_| Error::Corrupt("saved searches".into())))
}

pub fn list(dir: &Path) -> Result<Vec<Saved>> {
    Ok(crate::setup::open_store(dir, 1024)?.read(read)?)
}

/// Replace a preset of the same name or delete it when no form is supplied.
pub fn change(dir: &Path, name: &str, form: Option<Value>) -> Result<Vec<Saved>> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        bail!("Enter a name of up to 80 characters.");
    }
    if let Some(form) = &form
        && (!form.is_object() || serde_json::to_vec(form)?.len() > 16_384)
    {
        bail!("Search settings are too large.");
    }
    let mut store = crate::setup::open_store(dir, 1024)?;
    let saved = store.write(|o| {
        let mut items = read(o)?;
        items.retain(|item| !oneup_sync::same_name(&item.name, name));
        if let Some(form) = form {
            if items.len() >= MAX_SAVED {
                return Ok(None);
            }
            items.push(Saved { name: name.to_owned(), form });
        }
        items.sort_by_cached_key(|item| oneup_sync::fold(&item.name));
        let value = serde_json::to_string(&items).map_err(|_| Error::Corrupt("saved searches".into()))?;
        o.meta_set(KEY, &value)?;
        Ok(Some(items))
    })?;
    saved.ok_or_else(|| anyhow::anyhow!("You can save up to 50 searches."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn presets_survive_reopening_and_can_be_updated_or_removed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        assert!(list(dir).unwrap().is_empty());
        change(dir, "Large files", Some(json!({ "size": "1000", "availability": "offline" }))).unwrap();
        assert_eq!(list(dir).unwrap()[0].form["size"], "1000");
        change(dir, "large files", Some(json!({ "size": "2000" }))).unwrap();
        let presets = list(dir).unwrap();
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].form["size"], "2000");
        assert!(change(dir, "Large files", None).unwrap().is_empty());
        assert!(list(dir).unwrap().is_empty());
    }
}
