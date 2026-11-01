//! The push channel's untrusted text: Engine.IO/Socket.IO frames
//! (`oneup_graph::push::classify`) and the `notificationUrl` Graph returns
//! (`Endpoint::parse`). Neither may panic, and a parsed endpoint's `Debug`
//! must never show the URL's query (it's a credential).
#![no_main]

use libfuzzer_sys::fuzz_target;
use oneup_graph::push::{Endpoint, Frame, classify};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if let Frame::Open { ping, timeout } = classify(text) {
        // `listen` waits this long for the next frame: it mustn't overflow.
        let _ = ping + timeout;
    }
    if let Ok(ep) = Endpoint::parse(text) {
        // The namespace is a URL path, which never holds a `?`.
        let shown = format!("{ep:?}");
        assert!(!shown.contains('?'), "{shown}");
    }
});
