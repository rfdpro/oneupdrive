//! A vault kept through `VaultCloud` over an in-memory cloud: the decrypted
//! view follows every change, and what ends up in the cloud is a vault that
//! `oneup_vault::folder::Folder` (checked against Cryptomator's own app)
//! opens and reads.
#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;

use common::*;
use oneup_exec::Cloud;
use oneup_store::RemoteRow;
use oneup_sync::{RemoteId, RemotePre};
use oneup_vault::folder::{Dir, Folder};
use oneup_vaultsync::{Source, apply, project};

#[tokio::test]
async fn a_vault_kept_through_the_vault_cloud_is_a_valid_cryptomator_vault() {
    let mut s = setup();
    s.refresh().await;
    let root = RemoteId(s.vault.root.clone());
    let long = format!("{}.txt", "long name ".repeat(20));

    let docs = s.vc.mkdir(&root, "Docs").await.unwrap();
    let docs = RemoteId(docs.id);
    let (a, h) = s.vc.upload(&docs, "a.txt", None, 5, &mut &b"alpha"[..]).await.unwrap();
    assert_eq!((a.name.as_str(), a.size, a.qxh), ("a.txt", 5, h));
    let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    s.vc.upload(&docs, "big.bin", None, big.len() as i64, &mut big.as_slice()).await.unwrap();
    s.vc.upload(&root, &long, None, 4, &mut &b"long"[..]).await.unwrap();
    let deep = s.vc.mkdir(&root, &"Folder with a very long name ".repeat(6)).await.unwrap();
    s.vc.upload(&RemoteId(deep.id), "inside.txt", None, 6, &mut &b"inside"[..]).await.unwrap();
    s.vc.mkdir(&docs, "Empty").await.unwrap();
    s.refresh().await;

    // Rename and move, a new version, and a deletion.
    s.vc.move_item(&s.pre("/Docs/a.txt"), &root, "alpha.txt").await.unwrap();
    s.vc.upload(&root, "alpha.txt", Some(&s.pre("/Docs/big.bin")), 3, &mut &b"v2!"[..]).await.unwrap();
    s.vc.delete(&s.pre("/Docs/Empty")).await.unwrap();
    s.refresh().await;

    let mut want = BTreeMap::new();
    want.insert("/alpha.txt".to_string(), Some(b"alpha".to_vec()));
    want.insert("/Docs".into(), None);
    want.insert("/Docs/big.bin".into(), Some(b"v2!".to_vec()));
    want.insert(format!("/{long}"), Some(b"long".to_vec()));
    let deep_name = "Folder with a very long name ".repeat(6);
    want.insert(format!("/{deep_name}"), None);
    want.insert(format!("/{deep_name}/inside.txt"), Some(b"inside".to_vec()));
    assert_eq!(decrypted(&s.pairs, &s.vc, &s.vault.root).await, want);

    // The same vault, opened from its encrypted files by the format library.
    let dir = std::env::temp_dir().join(format!("oneup-vaultsync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    export(&s.cloud, &s.vault.root, &dir);
    let v = Folder::open(&dir, "pw").unwrap();
    let top = v.list(&Dir::root()).unwrap();
    assert!(top.problems.is_empty(), "{:?}", top.problems);
    let mut names: Vec<String> = top.entries.iter().map(|e| e.name.clone()).collect();
    names.sort();
    let mut expect = vec!["Docs".to_string(), "alpha.txt".into(), deep_name.clone(), long.clone()];
    expect.sort();
    assert_eq!(names, expect);
    let mut got = Vec::new();
    v.read(&v.find("/Docs/big.bin").unwrap().unwrap(), &mut got).unwrap();
    assert_eq!(got, b"v2!");
    let mut got = Vec::new();
    v.read(&v.find(&format!("/{deep_name}/inside.txt")).unwrap().unwrap(), &mut got).unwrap();
    assert_eq!(got, b"inside");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_view_removes_only_what_is_gone_from_the_cloud_and_refuses_tampering() {
    let mut s = setup();
    s.refresh().await;
    let root = RemoteId(s.vault.root.clone());
    let d = s.vc.mkdir(&root, "D").await.unwrap();
    let (f, _) = s.vc.upload(&RemoteId(d.id.clone()), "f.txt", None, 3, &mut &b"eff"[..]).await.unwrap();
    s.refresh().await;

    // A folder whose dir.c9r can't be read any more is kept, not deleted.
    let dir_file = s.src.children(&d.id).unwrap().into_iter().find(|r| r.name == "dir.c9r").unwrap();
    s.listing.lock().unwrap().write(|o| o.upsert_item(&RemoteRow { size: 99_999, ..dir_file.clone() }, false)).unwrap();
    let mut cache = oneup_vaultsync::project::Cache::default();
    let p = project(&s.src, &s.vault, &mut cache).await.unwrap();
    assert!(!p.problems.is_empty());
    let applied = apply(&mut s.pairs, &s.src, &p).unwrap();
    assert_eq!((applied.removed, applied.kept), (0, 2), "D and f.txt kept: their encrypted items are still there");
    assert!(s.pairs.read(|o| o.get_item(&f.id)).unwrap().is_some());
    s.listing.lock().unwrap().write(|o| o.upsert_item(&dir_file, false)).unwrap();
    s.refresh().await;

    // A ciphertext changed in the cloud fails to decrypt; nothing comes out
    // as if it were the file.
    {
        let mut st = s.cloud.0.lock().unwrap();
        let it = st.items.get_mut(&f.id).unwrap();
        let n = it.content.len();
        it.content[n - 20] ^= 1;
    }
    let mut out = Vec::new();
    assert!(
        s.vc.download(&RemotePre { id: RemoteId(f.id.clone()), etag: f.etag.clone().unwrap() }, &mut out)
            .await
            .is_err()
    );

    // Deleted in the cloud (the listing drops it): removed from the view
    // the way a delta deletion is (`delete_subtree`, which records evidence
    // for its Synced items).
    s.listing.lock().unwrap().write(|o| o.delete_subtree(&f.id).map(|_| ())).unwrap();
    let applied = s.refresh().await;
    assert_eq!(applied.removed, 1);
    assert!(s.pairs.read(|o| o.get_item(&f.id)).unwrap().is_none());
}

#[tokio::test]
async fn long_names_move_in_every_direction_and_the_vault_stays_valid() {
    let mut s = setup();
    s.refresh().await;
    let root = RemoteId(s.vault.root.clone());
    let long = |w: &str| format!("{}.txt", format!("{w} long name ").repeat(12));
    let long_dir = "A folder whose name is far too long to store as it is ".repeat(4);

    let a = RemoteId(s.vc.mkdir(&root, "A").await.unwrap().id);
    let b = RemoteId(s.vc.mkdir(&root, "B").await.unwrap().id);
    s.vc.upload(&a, "short.txt", None, 5, &mut &b"short"[..]).await.unwrap();
    s.vc.upload(&a, &long("one"), None, 3, &mut &b"one"[..]).await.unwrap();
    s.vc.upload(&a, &long("two"), None, 3, &mut &b"two"[..]).await.unwrap();
    let d = s.vc.mkdir(&a, "Dir").await.unwrap();
    s.vc.upload(&RemoteId(d.id), "in.txt", None, 2, &mut &b"in"[..]).await.unwrap();
    s.refresh().await;

    // Short to long (into another folder), long to long (another folder),
    // long to short (same folder), a folder short to long and back again.
    let moved = s.vc.move_item(&s.pre("/A/short.txt"), &b, &long("now")).await.unwrap();
    assert_eq!(moved.name, long("now"));
    s.vc.move_item(&s.pre(&format!("/A/{}", long("one"))), &b, &long("uno")).await.unwrap();
    s.vc.move_item(&s.pre(&format!("/A/{}", long("two"))), &a, "two.txt").await.unwrap();
    s.vc.move_item(&s.pre("/A/Dir"), &b, &long_dir).await.unwrap();
    s.refresh().await;
    s.vc.move_item(&s.pre(&format!("/B/{long_dir}")), &a, "Dir again").await.unwrap();
    s.refresh().await;

    let mut want = BTreeMap::new();
    want.insert("/A".to_string(), None);
    want.insert("/B".into(), None);
    want.insert(format!("/B/{}", long("now")), Some(b"short".to_vec()));
    want.insert(format!("/B/{}", long("uno")), Some(b"one".to_vec()));
    want.insert("/A/two.txt".into(), Some(b"two".to_vec()));
    want.insert("/A/Dir again".into(), None);
    want.insert("/A/Dir again/in.txt".into(), Some(b"in".to_vec()));
    assert_eq!(decrypted(&s.pairs, &s.vc, &s.vault.root).await, want);

    // Nothing left behind, and the format library reads it all.
    let dir = std::env::temp_dir().join(format!("oneup-vaultsync-moves-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    export(&s.cloud, &s.vault.root, &dir);
    let v = Folder::open(&dir, "pw").unwrap();
    for folder in ["/A", "/B"] {
        let Some(entry) = v.find(folder).unwrap() else { panic!("{folder} missing") };
        let oneup_vault::folder::Kind::Dir(d) = entry.kind else { panic!("{folder} isn't a folder") };
        let listed = v.list(&d).unwrap();
        assert!(listed.problems.is_empty(), "{folder}: {:?}", listed.problems);
    }
    let mut got = Vec::new();
    v.read(&v.find(&format!("/B/{}", long("uno"))).unwrap().unwrap(), &mut got).unwrap();
    assert_eq!(got, b"one");
    let mut got = Vec::new();
    v.read(&v.find("/A/Dir again/in.txt").unwrap().unwrap(), &mut got).unwrap();
    assert_eq!(got, b"in");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A long-name upload whose file can't be read to the end (its program
/// saved it meanwhile) leaves its holder folder behind; sending the file
/// again uses that folder instead of failing on its name.
#[tokio::test]
async fn a_long_name_upload_cut_short_can_be_sent_again() {
    struct CutShort;
    impl std::io::Read for CutShort {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("changed while read"))
        }
    }
    let mut s = setup();
    s.refresh().await;
    let root = RemoteId(s.vault.root.clone());
    let long = format!("{}.txt", "cut short long name ".repeat(10));

    assert!(s.vc.upload(&root, &long, None, 5, &mut CutShort).await.is_err());
    s.vc.upload(&root, &long, None, 5, &mut &b"saved"[..]).await.unwrap();
    s.refresh().await;
    let got = decrypted(&s.pairs, &s.vc, &s.vault.root).await;
    assert_eq!(got.get(&format!("/{long}")), Some(&Some(b"saved".to_vec())), "{got:?}");
}
