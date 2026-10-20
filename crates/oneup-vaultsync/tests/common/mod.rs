//! Shared by the vault tests: an in-memory cloud, its listing, and a vault
//! made in it.
#![allow(dead_code, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use oneup_exec::{Cloud, CloudError, Ours, Replaced, VersionMark};
use oneup_store::{Kind, RemoteRow, Store};
use oneup_sync::{RemoteId, RemotePre};
use oneup_vault::key::Masterkey;
use oneup_vaultsync::{Nodes, Source, Vault, VaultCloud, apply, project};

// ------------------------------------------------------------ fake cloud

#[derive(Clone)]
pub struct Item {
    pub parent: Option<String>,
    pub name: String,
    pub folder: bool,
    pub content: Vec<u8>,
    pub etag: u64,
}

#[derive(Default)]
pub struct State {
    pub items: BTreeMap<String, Item>,
    pub next: u64,
}

impl State {
    pub fn bump(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
    pub fn child(&self, parent: &str, name: &str) -> Option<String> {
        self.items
            .iter()
            .find(|(_, i)| i.parent.as_deref() == Some(parent) && i.name.eq_ignore_ascii_case(name))
            .map(|(id, _)| id.clone())
    }
    pub fn check(&self, pre: &RemotePre) -> Result<&Item, CloudError> {
        match self.items.get(&pre.id.0) {
            Some(i) if i.etag.to_string() == pre.etag => Ok(i),
            _ => Err(CloudError::Changed),
        }
    }
    pub fn row(&self, id: &str) -> RemoteRow {
        let i = &self.items[id];
        RemoteRow {
            id: id.into(),
            parent_id: i.parent.clone(),
            name: i.name.clone(),
            kind: if i.parent.is_none() {
                Kind::Root
            } else if i.folder {
                Kind::Folder
            } else {
                Kind::File
            },
            size: i.content.len() as i64,
            etag: Some(i.etag.to_string()),
            qxh: (!i.folder).then(|| qxh(&i.content)),
            mtime_ms: None,
        }
    }
    pub fn create(&mut self, parent: &str, name: &str, folder: bool, content: Vec<u8>) -> Result<String, CloudError> {
        if self.child(parent, name).is_some() {
            return Err(CloudError::NameTaken);
        }
        let id = format!("r{}", self.bump());
        let etag = self.bump();
        self.items.insert(id.clone(), Item { parent: Some(parent.into()), name: name.into(), folder, content, etag });
        Ok(id)
    }
}

pub fn qxh(b: &[u8]) -> [u8; 20] {
    let mut h = oneup_hash::QuickXorHash::new();
    h.update(b);
    h.finalize()
}

#[derive(Clone)]
pub struct Fake(pub Arc<Mutex<State>>);

impl Cloud for Fake {
    fn concurrent(&self) -> bool {
        true
    }

    fn moves_keep_ids(&self) -> bool {
        true
    }
    async fn download(&self, pre: &RemotePre, out: &mut (dyn Write + Send)) -> Result<Option<[u8; 20]>, CloudError> {
        let c = self.0.lock().unwrap().check(pre)?.content.clone();
        out.write_all(&c)?;
        Ok(None)
    }
    async fn upload(
        &self,
        parent: &RemoteId,
        name: &str,
        replace: Option<&RemotePre>,
        size: i64,
        src: &mut dyn oneup_exec::Source,
    ) -> Result<(RemoteRow, Option<[u8; 20]>), CloudError> {
        let mut content = Vec::new();
        src.read_to_end(&mut content)?;
        assert_eq!(content.len() as i64, size);
        let mut s = self.0.lock().unwrap();
        let id = match replace {
            Some(pre) => {
                s.check(pre)?;
                let e = s.bump();
                let it = s.items.get_mut(&pre.id.0).unwrap();
                it.content = content;
                it.etag = e;
                pre.id.0.clone()
            }
            None => s.create(&parent.0, name, false, content)?,
        };
        Ok((s.row(&id), None))
    }
    // Replacements here commit under their precondition.
    async fn version_mark(&self, _: &RemotePre, _: i64) -> Result<Option<VersionMark>, CloudError> {
        Ok(None)
    }
    async fn replaced_other(&self, _: &RemoteId, _: &VersionMark, _: &mut Ours) -> Result<Replaced, CloudError> {
        Ok(Replaced::Planned)
    }
    async fn download_version(&self, _: &RemoteId, _: &str, _: &mut (dyn Write + Send)) -> Result<u64, CloudError> {
        Err(CloudError::Other("no version history".into()))
    }
    async fn children(&self, id: &RemoteId) -> Result<Vec<RemoteRow>, CloudError> {
        let s = self.0.lock().unwrap();
        let ids: Vec<&String> =
            s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&id.0)).map(|(id, _)| id).collect();
        Ok(ids.into_iter().map(|id| s.row(id)).collect())
    }
    async fn copy(&self, _: &RemotePre, _: &[u8; 20], _: &RemoteId, _: &str) -> Result<RemoteRow, CloudError> {
        Err(CloudError::Other("no".into()))
    }
    async fn mkdir(&self, parent: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        let mut s = self.0.lock().unwrap();
        let id = s.create(&parent.0, name, true, vec![])?;
        Ok(s.row(&id))
    }
    async fn move_item(&self, pre: &RemotePre, to: &RemoteId, name: &str) -> Result<RemoteRow, CloudError> {
        let mut s = self.0.lock().unwrap();
        s.check(pre)?;
        if s.child(&to.0, name).is_some_and(|c| c != pre.id.0) {
            return Err(CloudError::NameTaken);
        }
        let e = s.bump();
        let it = s.items.get_mut(&pre.id.0).unwrap();
        it.parent = Some(to.0.clone());
        it.name = name.into();
        it.etag = e;
        Ok(s.row(&pre.id.0))
    }
    async fn delete(&self, pre: &RemotePre) -> Result<(), CloudError> {
        let mut s = self.0.lock().unwrap();
        s.check(pre)?;
        if s.items.values().any(|i| i.parent.as_deref() == Some(&pre.id.0)) {
            return Err(CloudError::Changed);
        }
        s.items.remove(&pre.id.0);
        Ok(())
    }
    async fn item(&self, id: &RemoteId) -> Result<Option<RemoteRow>, CloudError> {
        let s = self.0.lock().unwrap();
        Ok(s.items.contains_key(&id.0).then(|| s.row(&id.0)))
    }
    async fn prune(&self, pre: &RemotePre) -> Result<(), CloudError> {
        let mut s = self.0.lock().unwrap();
        s.check(pre)?;
        let mut gone = vec![pre.id.0.clone()];
        let mut i = 0;
        while i < gone.len() {
            let p = gone[i].clone();
            gone.extend(s.items.iter().filter(|(_, it)| it.parent.as_deref() == Some(&p)).map(|(id, _)| id.clone()));
            i += 1;
        }
        for id in gone {
            s.items.remove(&id);
        }
        Ok(())
    }
}

/// The cloud listing (as the main data folder's mirror would hold it) plus
/// downloads of small files.
pub struct Listing {
    pub store: Arc<Mutex<Store>>,
    pub cloud: Fake,
}

impl Source for Listing {
    fn children(&self, id: &str) -> oneup_vaultsync::Result<Vec<RemoteRow>> {
        let s = self.store.lock().unwrap();
        Ok(s.read(|o| o.remote_children(id))?.into_iter().map(|(r, _)| r).collect())
    }
    fn item(&self, id: &str) -> oneup_vaultsync::Result<Option<RemoteRow>> {
        Ok(self.store.lock().unwrap().read(|o| o.get_item(id))?)
    }
    async fn fetch(&self, item: &RemoteRow) -> oneup_vaultsync::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.cloud
            .download(
                &RemotePre { id: RemoteId(item.id.clone()), etag: item.etag.clone().unwrap_or_default() },
                &mut out,
            )
            .await
            .map_err(|e| oneup_vaultsync::Error::Fetch(e.to_string()))?;
        Ok(out)
    }
}

/// What a sync of the vault's folder would compare: path → content (`None`
/// for folders).
pub async fn decrypted(store: &Store, cloud: &VaultCloud<Fake>, root: &str) -> BTreeMap<String, Option<Vec<u8>>> {
    let rows: Vec<RemoteRow> = store.read(|o| o.all_items()).unwrap().into_iter().map(|(r, _)| r).collect();
    let path = |id: &str| {
        let mut parts = Vec::new();
        let mut cur = id.to_string();
        while cur != root {
            let r = rows.iter().find(|r| r.id == cur).unwrap();
            parts.push(r.name.clone());
            cur = r.parent_id.clone().unwrap();
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    };
    let mut out = BTreeMap::new();
    for r in rows.iter().filter(|r| r.id != root) {
        let content = if r.kind == Kind::File {
            let mut got = Vec::new();
            let pre = RemotePre { id: RemoteId(r.id.clone()), etag: r.etag.clone().unwrap() };
            let h = cloud.download(&pre, &mut got).await.unwrap_or_else(|e| panic!("{}: {e}", path(&r.id)));
            assert_eq!(h, r.qxh, "the hash returned is the encrypted file's, as listed");
            assert_eq!(got.len() as i64, r.size, "decrypted size as listed");
            Some(got)
        } else {
            None
        };
        out.insert(path(&r.id), content);
    }
    out
}

/// Writes the vault's encrypted items to a local folder, as Cryptomator
/// would see them synced.
pub fn export(cloud: &Fake, id: &str, to: &Path) {
    let s = cloud.0.lock().unwrap();
    let mut stack = vec![(id.to_string(), to.to_path_buf())];
    while let Some((id, dir)) = stack.pop() {
        std::fs::create_dir_all(&dir).unwrap();
        for (cid, it) in s.items.iter().filter(|(_, i)| i.parent.as_deref() == Some(&id)) {
            let p = dir.join(&it.name);
            if it.folder {
                stack.push((cid.clone(), p));
            } else {
                std::fs::write(p, &it.content).unwrap();
            }
        }
    }
}

pub struct Setup {
    pub cloud: Fake,
    pub listing: Arc<Mutex<Store>>,
    pub vault: Vault,
    pub nodes: Arc<RwLock<Nodes>>,
    pub vc: VaultCloud<Fake>,
    pub src: Listing,
    pub pairs: Store,
}

pub fn setup() -> Setup {
    let fake = Fake(Arc::new(Mutex::new(State::default())));
    let key = Masterkey::generate().unwrap();
    {
        let mut s = fake.0.lock().unwrap();
        s.items
            .insert("root".into(), Item { parent: None, name: String::new(), folder: true, content: vec![], etag: 0 });
        let v = s.create("root", "Vault", true, vec![]).unwrap();
        s.create(&v, oneup_vault::key::FILE, false, key.to_file_with("pw", 1024, 8).unwrap()).unwrap();
        let cfg = oneup_vault::config::Config::new().unwrap().sign(&key).unwrap();
        s.create(&v, oneup_vault::config::FILE, false, cfg.into_bytes()).unwrap();
        s.create(&v, "d", true, vec![]).unwrap();
    }
    // The mirror's view of it.
    let mut listing = Store::open_in_memory().unwrap();
    let rows: Vec<RemoteRow> = {
        let s = fake.0.lock().unwrap();
        let mut ids: Vec<&String> = s.items.keys().collect();
        ids.sort_by_key(|id| s.items[*id].parent.is_some());
        ids.into_iter().map(|id| s.row(id)).collect()
    };
    listing.write(|o| rows.iter().try_for_each(|r| o.upsert_item(r, false))).unwrap();
    let root = rows.iter().find(|r| r.name == "Vault").unwrap().id.clone();
    let listing = Arc::new(Mutex::new(listing));
    let vault = Vault { key: Arc::new(key), threshold: oneup_vault::config::SHORTENING_THRESHOLD, root };
    let nodes = Arc::new(RwLock::new(Nodes::new()));
    let vc = VaultCloud::new(fake.clone(), vault.clone(), Arc::clone(&nodes), Arc::clone(&listing));
    let src = Listing { store: Arc::clone(&listing), cloud: fake.clone() };
    Setup { cloud: fake, listing, vault, nodes, vc, src, pairs: Store::open_in_memory().unwrap() }
}

impl Setup {
    pub async fn refresh(&mut self) -> oneup_vaultsync::project::Applied {
        let mut cache = oneup_vaultsync::project::Cache::default();
        let p = project(&self.src, &self.vault, &mut cache).await.unwrap();
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        *self.nodes.write().unwrap() = p.nodes.clone();
        apply(&mut self.pairs, &self.src, &p).unwrap()
    }
    pub fn id(&self, path: &str) -> RemoteId {
        let rows: Vec<RemoteRow> = self.pairs.read(|o| o.all_items()).unwrap().into_iter().map(|(r, _)| r).collect();
        let mut cur = self.vault.root.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = rows.iter().find(|r| r.parent_id.as_deref() == Some(&cur) && r.name == part).unwrap().id.clone();
        }
        RemoteId(cur)
    }
    pub fn pre(&self, path: &str) -> RemotePre {
        let id = self.id(path);
        let r = self.pairs.read(|o| o.get_item(&id.0)).unwrap().unwrap();
        RemotePre { id, etag: r.etag.unwrap() }
    }
}
