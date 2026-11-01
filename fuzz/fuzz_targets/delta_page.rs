//! A delta (or children) page as Graph might send it: parsed with
//! `oneup_graph::Page`, then every entry applied to an in-memory Remote tree
//! through `oneup_remote::ingest`, exactly as a catch-up round does. Bad
//! JSON must be an error, and no entry, however odd, may panic the ingest
//! or the store.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oneup_graph::{DriveItem, Page};
use oneup_remote::ingest::{self, Mode, Tally};
use oneup_rules::ExcludeSet;
use oneup_store::Store;

fuzz_target!(|data: &[u8]| {
    let Ok(page) = serde_json::from_slice::<Page>(data) else { return };
    for item in &page.value {
        let _ = (item.quick_xor_hash(), item.modified_ms(), item.parent_id());
        let _ = ingest::to_row(item);
    }
    let Ok(mut store) = Store::open_in_memory() else { return };
    // A known root, so entries naming it as their parent reach the tree
    // rather than all waiting as orphans.
    let root: DriveItem = match serde_json::from_str(r#"{"id":"root","name":"root","root":{},"folder":{}}"#) {
        Ok(r) => r,
        Err(_) => return,
    };
    let rules = ExcludeSet::dev();
    // Store errors are allowed: they're reported, not panics.
    let _ = store.write(|o| {
        let mut t = Tally::default();
        ingest::apply(o, &rules, Mode::Enumerate, &root, &mut t)?;
        for (i, item) in page.value.iter().enumerate() {
            let mode = if i % 2 == 0 { Mode::CatchUp } else { Mode::Enumerate };
            ingest::apply(o, &rules, mode, item, &mut t)?;
        }
        ingest::resolve_local_orphans(o, &rules, &mut t)?;
        ingest::reapply_rules(o, &ExcludeSet::none())?;
        Ok(t)
    });
});
