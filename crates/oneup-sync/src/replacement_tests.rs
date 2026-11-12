#![allow(clippy::unwrap_used)]

use super::*;

fn node(id: u128, name: &str, kind: NodeKind) -> LocalNode {
    LocalNode { id: LocalId(id), parent: Some(LocalId(1)), name: name.into(), kind, size: 1, mtime: 2, hash: None }
}

#[test]
fn indexed_candidates_match_the_original_consumption_and_leftover_order() {
    let nodes = vec![
        node(1, "Report", NodeKind::Folder),
        node(2, "REPORT", NodeKind::File),
        node(3, "report", NodeKind::File),
        node(4, "untouched", NodeKind::File),
        node(5, "\u{03c2}", NodeKind::File),
        node(6, "\u{03c3}", NodeKind::File),
        node(7, "REPORT", NodeKind::Folder),
        node(8, "last", NodeKind::File),
    ];
    let mut original = nodes.clone();
    let mut indexed = LocalReplacements::new(nodes);
    for (kind, name) in [
        (NodeKind::File, "report"),
        (NodeKind::Folder, "missing"),
        (NodeKind::Folder, "REPORT"),
        (NodeKind::File, "\u{03a3}"),
        (NodeKind::File, "RePoRt"),
        (NodeKind::File, "report"),
        (NodeKind::Folder, "last"),
    ] {
        let expected =
            original.iter().position(|n| n.kind == kind && fold(&n.name) == fold(name)).map(|i| original.remove(i));
        assert_eq!(indexed.take(kind, name), expected);
    }
    assert_eq!(indexed.remaining(), original);
}

#[test]
fn bulk_replacements_match_in_reverse_order_and_leave_other_items_in_order() {
    let nodes: Vec<_> = (0..20_000).map(|id| node(id, &format!("file{id}"), NodeKind::File)).collect();
    let mut indexed = LocalReplacements::new(nodes);
    for id in (0..20_000).rev().filter(|id| id % 2 == 0) {
        assert_eq!(indexed.take(NodeKind::File, &format!("FILE{id}")).unwrap().id, LocalId(id));
    }
    let remaining: Vec<_> = indexed.remaining().into_iter().map(|n| n.id).collect();
    assert_eq!(remaining, (0..20_000).filter(|id| id % 2 != 0).map(LocalId).collect::<Vec<_>>());
}
