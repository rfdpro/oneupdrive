//! A headless accessibility check of the settings window's page
//! (`src/settings_ui.html`): every control has a name, dialogs are named,
//! icons are hidden from screen readers or described, tabs sit in a tab
//! list, the colour palette meets WCAG 2.2 AA contrast in light and dark,
//! and the page answers Windows' contrast themes and reduced motion.
//!
//! It reads the page as text, so it runs anywhere `cargo test` does. The
//! markup is checked element by element; the script's own controls (the
//! `el("input", …)` calls and the like) are checked call by call. What it
//! can't see (what a screen reader actually says, focus order at run time)
//! is listed in `docs/architecture/settings-window.md` § Accessibility.

use std::collections::{HashMap, HashSet};

const PAGE: &str = include_str!("../src/settings_ui.html");

fn between<'a>(s: &'a str, from: &str, to: &str) -> &'a str {
    let a = s.find(from).map_or(0, |i| i + from.len());
    let b = s[a..].find(to).map_or(s.len(), |i| a + i);
    &s[a..b]
}

fn css() -> &'static str {
    between(PAGE, "<style>", "</style>")
}

fn markup() -> &'static str {
    between(PAGE, "<body>", "<script>")
}

fn script() -> &'static str {
    between(PAGE, "<script>", "</script>")
}

// ---- colours

/// The custom properties set in a block of CSS (`--name: #rrggbb;`).
fn palette(block: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for decl in block.split(';') {
        let Some((name, value)) = decl.split_once(':') else { continue };
        let (name, value) = (name.trim(), value.trim());
        if let Some(name) = name.strip_prefix("--")
            && value.starts_with('#')
        {
            out.insert(name.to_string(), value.to_string());
        }
    }
    out
}

fn luminance(hex: &str) -> f64 {
    let h = hex.trim_start_matches('#');
    let channel = |i: usize| {
        let c = f64::from(u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0)) / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
}

fn contrast(a: &str, b: &str) -> f64 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// The light palette (the first `:root`) and the dark one (the `:root`
/// inside `prefers-color-scheme: dark`, over the light one).
fn palettes() -> [(&'static str, HashMap<String, String>); 2] {
    let css = css();
    let light = palette(between(css, ":root {", "}"));
    let dark_at = css.find("@media (prefers-color-scheme: dark)").expect("a dark palette");
    let mut dark = light.clone();
    dark.extend(palette(between(&css[dark_at..], ":root {", "}")));
    [("light", light), ("dark", dark)]
}

/// Text (4.5:1) and the parts of controls that identify them, such as a
/// field's border or the focus ring (3:1, WCAG 1.4.11).
const TEXT: &[(&str, &[&str])] = &[
    ("ink", &["canvas", "panel", "sunk", "sel"]),
    ("muted", &["canvas", "panel", "sunk", "sel"]),
    ("accent", &["canvas", "panel", "accent-soft"]),
    ("accent-ink", &["accent"]),
    ("ok", &["canvas", "panel", "sunk"]),
    ("wait", &["canvas", "panel", "sunk"]),
    ("bad", &["canvas", "panel", "sunk"]),
    ("bad-ink", &["bad"]),
    ("panel", &["ok"]),
];
const PARTS: &[(&str, &[&str])] = &[
    ("field", &["canvas", "panel"]),
    ("muted", &["panel"]),
    ("accent", &["canvas", "panel", "sel"]),
    ("off", &["panel"]),
    ("busy", &["panel"]),
];

#[test]
fn the_palette_meets_wcag_aa_contrast_in_light_and_dark() {
    let mut failures = Vec::new();
    for (scheme, p) in palettes() {
        for (pairs, min) in [(TEXT, 4.5), (PARTS, 3.0)] {
            for (fg, bgs) in pairs {
                for bg in *bgs {
                    let (Some(f), Some(b)) = (p.get(*fg), p.get(*bg)) else {
                        failures.push(format!("{scheme}: --{fg} or --{bg} isn't set"));
                        continue;
                    };
                    let r = contrast(f, b);
                    if r < min {
                        failures.push(format!("{scheme}: --{fg} {f} on --{bg} {b} is {r:.2}:1, needs {min}:1"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_page_follows_windows_colours_contrast_themes_and_motion() {
    let css = css();
    for needed in [
        "@media (prefers-color-scheme: dark)",
        "@media (forced-colors: active)",
        "@media (prefers-reduced-motion: reduce)",
        ":focus-visible",
    ] {
        assert!(css.contains(needed), "the page's CSS has no {needed}");
    }
    // What a contrast theme would otherwise wipe out: the selection, a
    // pressed button, a switch that's on.
    let forced = between(css, "@media (forced-colors: active)", "\n        }\n    ");
    for needed in [".ti[aria-selected=\"true\"]", ".seg button[aria-pressed=\"true\"]", ".toggle input:checked+span"] {
        assert!(forced.contains(needed), "forced colours don't draw {needed}");
    }
    // Focus is never shown by a background tint alone.
    for rule in css.split('}') {
        if rule.contains(":focus-visible") {
            assert!(!rule.contains("outline: none"), "a focus style removes the outline: {rule}");
        }
    }
}

// ---- markup

#[derive(Debug)]
struct Tag {
    name: String,
    attrs: HashMap<String, String>,
    /// Where the tag ends in the markup.
    end: usize,
    /// Inside a `<label>`.
    in_label: bool,
}

fn attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'/' {
            i += 1;
        }
        if start == i {
            i += 1;
            continue;
        }
        let name = s[start..i].to_ascii_lowercase();
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let v = i + 1;
                i = v;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                value = s[v..i].to_string();
                i += 1;
            } else {
                let v = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = s[v..i].to_string();
            }
        }
        out.insert(name, value);
    }
    out
}

/// The markup's opening tags, in order.
fn tags(html: &str) -> Vec<Tag> {
    let mut out = Vec::new();
    let mut labels = 0usize;
    let mut i = 0;
    while let Some(at) = html[i..].find('<') {
        let start = i + at;
        let Some(len) = html[start..].find('>') else { break };
        let end = start + len + 1;
        let inner = &html[start + 1..end - 1];
        i = end;
        if inner.starts_with('!') {
            continue;
        }
        if let Some(close) = inner.strip_prefix('/') {
            if close.trim().eq_ignore_ascii_case("label") {
                labels = labels.saturating_sub(1);
            }
            continue;
        }
        let name_len = inner.find(|c: char| c.is_ascii_whitespace() || c == '/').unwrap_or(inner.len());
        let name = inner[..name_len].to_ascii_lowercase();
        if name == "label" {
            labels += 1;
        }
        out.push(Tag { attrs: attrs(&inner[name_len..]), name, end, in_label: labels > 0 });
    }
    out
}

/// The text inside an element (tags dropped), from the end of its opening
/// tag to its closing tag.
fn text_after(html: &str, from: usize, name: &str) -> String {
    let close = format!("</{name}>");
    let inner = &html[from..from + html[from..].find(&close).unwrap_or(0)];
    let mut text = String::new();
    let mut in_tag = false;
    for c in inner.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    text.trim().to_string()
}

/// Whether the script gives the element with this id its text (a button
/// whose label depends on what it does).
fn given_text(id: &str) -> bool {
    let s = script();
    let pat = format!("$(\"#{id}\")");
    !id.is_empty() && s.match_indices(&pat).any(|(at, _)| s[at..s.len().min(at + 400)].contains(".textContent ="))
}

fn named(t: &Tag) -> bool {
    ["aria-label", "aria-labelledby"].iter().any(|a| t.attrs.get(*a).is_some_and(|v| !v.trim().is_empty()))
}

#[test]
fn every_control_in_the_markup_has_a_name() {
    let html = markup();
    let all = tags(html);
    let labelled: HashSet<&str> =
        all.iter().filter(|t| t.name == "label").filter_map(|t| t.attrs.get("for").map(String::as_str)).collect();
    let mut failures = Vec::new();
    for t in &all {
        let id = t.attrs.get("id").map(String::as_str).unwrap_or("");
        let what = format!("<{} id={id:?} {:?}>", t.name, t.attrs.keys().collect::<Vec<_>>());
        match t.name.as_str() {
            "input" | "select" | "textarea" => {
                if t.attrs.get("type").is_some_and(|v| v == "hidden") {
                    continue;
                }
                if !(named(t) || t.in_label || labelled.contains(id)) {
                    failures.push(format!("{what}: no label"));
                }
            }
            "button" => {
                if !named(t) && text_after(html, t.end, "button").is_empty() && !given_text(id) {
                    failures.push(format!("{what}: no text"));
                }
            }
            "dialog" => {
                let by = t.attrs.get("aria-labelledby").map(String::as_str).unwrap_or("");
                let heading = all.iter().any(|h| !by.is_empty() && h.attrs.get("id").is_some_and(|i| i == by));
                if !heading && !t.attrs.contains_key("aria-label") {
                    failures.push(format!("{what}: not named by its heading"));
                }
            }
            "svg" => {
                let hidden = t.attrs.get("aria-hidden").is_some_and(|v| v == "true");
                let image = t.attrs.get("role").is_some_and(|v| v == "img") && named(t);
                if !hidden && !image {
                    failures.push(format!("{what}: neither hidden nor described"));
                }
            }
            "img" if !t.attrs.contains_key("alt") => failures.push(format!("{what}: no alt text")),
            _ => {}
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn ids_are_unique_and_references_resolve() {
    let all = tags(markup());
    let mut ids = HashSet::new();
    for t in &all {
        if let Some(id) = t.attrs.get("id") {
            assert!(ids.insert(id.as_str()), "id {id} is used twice");
        }
    }
    for t in &all {
        for a in ["aria-labelledby", "aria-describedby", "aria-controls", "popovertarget"] {
            if let Some(v) = t.attrs.get(a) {
                for id in v.split_whitespace() {
                    assert!(ids.contains(id), "{a}={id} on <{}> names no element", t.name);
                }
            }
        }
    }
    assert!(PAGE.contains("<html lang="), "the page has no language");
}

#[test]
fn roles_sit_where_they_belong() {
    let all = tags(markup());
    // A dialog's modal state, focus and Esc come with <dialog>'s showModal.
    assert!(all.iter().all(|t| t.attrs.get("role").is_none_or(|r| r != "dialog")), "use <dialog>, not role=dialog");
    let tablists: Vec<&Tag> =
        all.iter().filter(|t| t.attrs.get("role").is_some_and(|r| r == "tablist") && named(t)).collect();
    assert!(tablists.len() >= 2, "the folder tabs and the views are named tab lists");
    // Live regions for what changes by itself.
    for id in ["toast", "ls-sum", "fd-sum", "setup-busy"] {
        let t = all.iter().find(|t| t.attrs.get("id").is_some_and(|i| i == id)).expect(id);
        let live =
            t.attrs.get("role").is_some_and(|r| r == "status" || r == "alert") || t.attrs.contains_key("aria-live");
        assert!(live, "#{id} isn't a live region");
    }
    let tree = all.iter().find(|t| t.attrs.get("id").is_some_and(|i| i == "tree")).expect("the tree");
    assert_eq!(tree.attrs.get("role").map(String::as_str), Some("tree"));
    assert!(named(tree), "the tree has no name");
    // The details panel changes with every arrow key in the tree: it must
    // not be read out each time.
    let details = all.iter().find(|t| t.attrs.get("id").is_some_and(|i| i == "details")).expect("details");
    assert!(!details.attrs.contains_key("aria-live"), "the details panel is a live region");
}

// ---- the script's own controls

/// Where the string starting at `i` (a quote) ends: its closing quote.
fn skip_quoted(b: &[u8], i: usize) -> usize {
    let q = b[i];
    let mut j = i + 1;
    while j < b.len() && b[j] != q {
        if b[j] == b'\\' {
            j += 1;
        }
        j += 1;
    }
    j
}

/// Where the template literal starting at `i` ends; `${…}` inside is code.
fn skip_template(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() && b[j] != b'`' {
        match b[j] {
            b'\\' => j += 1,
            b'$' if b.get(j + 1) == Some(&b'{') => j = skip_code(b, j + 2, b'}'),
            _ => {}
        }
        j += 1;
    }
    j
}

/// Where the regular expression literal starting at `i` ends.
fn skip_regex(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    let mut class = false;
    while j < b.len() && (class || b[j] != b'/') && b[j] != b'\n' {
        match b[j] {
            b'\\' => j += 1,
            b'[' => class = true,
            b']' => class = false,
            _ => {}
        }
        j += 1;
    }
    j
}

/// Where the code from `i` reaches `close` outside any bracket, string,
/// template, regular expression or comment.
fn skip_code(b: &[u8], i: usize, close: u8) -> usize {
    let mut j = i;
    // The last character that wasn't a space: a `/` after one of these
    // starts a regular expression, not a division.
    let mut prev = b'(';
    while j < b.len() {
        let c = b[j];
        match c {
            _ if c == close => return j,
            b'"' | b'\'' => j = skip_quoted(b, j),
            b'`' => j = skip_template(b, j),
            b'(' => j = skip_code(b, j + 1, b')'),
            b'[' => j = skip_code(b, j + 1, b']'),
            b'{' => j = skip_code(b, j + 1, b'}'),
            b'/' if b.get(j + 1) == Some(&b'/') => {
                while j < b.len() && b[j] != b'\n' {
                    j += 1;
                }
            }
            b'/' if b"(,=:[!&|?{};".contains(&prev) => j = skip_regex(b, j),
            _ => {}
        }
        if !c.is_ascii_whitespace() {
            prev = c;
        }
        j += 1;
    }
    b.len()
}

/// The arguments of the call whose `(` is at `open`, up to its `)`.
fn call_args(s: &str, open: usize) -> &str {
    let end = skip_code(s.as_bytes(), open + 1, b')');
    &s[open + 1..end.min(s.len())]
}

/// How many arguments a call has (commas at its top level, plus one).
fn arg_count(args: &str) -> usize {
    let b = args.as_bytes();
    let mut n = 1;
    let mut j = 0;
    while j < b.len() {
        match b[j] {
            b'"' | b'\'' => j = skip_quoted(b, j),
            b'`' => j = skip_template(b, j),
            b'(' => j = skip_code(b, j + 1, b')'),
            b'[' => j = skip_code(b, j + 1, b']'),
            b'{' => j = skip_code(b, j + 1, b'}'),
            b',' => n += 1,
            _ => {}
        }
        j += 1;
    }
    n
}

/// The calls that make an element of one of `tags` in the script, with
/// their arguments and the script's next few lines (where a label may be
/// set afterwards).
fn made(tags: &[&str]) -> Vec<(String, &'static str, &'static str)> {
    let s = script();
    let mut out = Vec::new();
    for tag in tags {
        for pat in [format!("el(\"{tag}\""), format!("createElement(\"{tag}\"")] {
            let mut from = 0;
            while let Some(at) = s[from..].find(&pat) {
                let open = from + at + pat.find('(').unwrap_or(0);
                let args = call_args(s, open);
                let after = &s[open..s.len().min(open + args.len() + 700)];
                out.push((tag.to_string(), args, after));
                from = open + 1;
            }
        }
    }
    out
}

#[test]
fn every_control_the_script_makes_has_a_name() {
    let mut failures = Vec::new();
    for (tag, args, after) in made(&["input", "select", "textarea"]) {
        // A checkbox inside a <label> with text is named by it.
        let labelled = args.contains("aria-label") || after.contains("setAttribute(\"aria-label\"");
        if !labelled {
            failures.push(format!("<{tag}> made without a label: {}", args.lines().take(4).collect::<String>()));
        }
    }
    for (tag, args, after) in made(&["button"]) {
        let text = args.contains("textContent") || args.contains("aria-label") || arg_count(args) > 2;
        let later = after.contains(".textContent =") || after.contains("setAttribute(\"aria-label\"");
        if !(text || later) {
            failures.push(format!("<{tag}> made without text: {}", args.lines().take(4).collect::<String>()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_checks_find_what_they_look_for() {
    // The scanners themselves, on small pieces.
    let t = tags(r#"<label><input type="checkbox"></label><input id="a"><button>Go</button>"#);
    assert!(t[1].in_label && !t[2].in_label);
    assert_eq!(t[2].attrs.get("id").map(String::as_str), Some("a"));
    let s = r#"el("button", { textContent: `a ${f({ x: ")" })} b` }, svg("i"))"#;
    let open = s.find('(').expect("a call");
    let args = call_args(s, open);
    assert!(args.ends_with(r#"svg("i")"#), "{args}");
    assert_eq!(arg_count(args), 3);
    assert_eq!(arg_count(r#""input", { type: "checkbox" }"#), 2);
    assert!((contrast("#000000", "#ffffff") - 21.0).abs() < 0.01);
    assert!(made(&["input"]).len() >= 4, "the script's inputs aren't found");
}
