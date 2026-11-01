//! The strings to translate: every message the settings page passes to
//! `tr()`, its own text (headings, labels, placeholders), and every `t!` and
//! `t_key!` in the programs' code. Each must be a well-formed message.
//!
//! `locales/template.json` lists them all for translators. To write it again
//! after strings changed: `ONEUP_WRITE_STRINGS=1 cargo test -p oneup --test
//! l10n_strings`. It isn't checked for being current: a string missing from
//! it still shows, in English (see `docs/architecture/localisation.md`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use oneup_app::l10n::{self, Arg, Locale, Numbers};

const PAGE: &str = include_str!("../src/settings_ui.html");

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A string literal starting at `b[i]` (a `"`), decoded (JavaScript's and
/// Rust's common escapes), and where it ends.
fn literal(s: &str, i: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut chars = s[i + 1..].char_indices();
    while let Some((j, c)) = chars.next() {
        match c {
            '"' => return Some((out, i + 1 + j + 1)),
            '\\' => match chars.next()?.1 {
                // Rust: a backslash at the end of a line joins it to the next.
                '\n' => {
                    while chars.clone().next().is_some_and(|(_, c)| c.is_whitespace()) {
                        chars.next();
                    }
                }
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'u' => {
                    // \uXXXX (JavaScript) or \u{X…} (Rust).
                    let rest = &s[i + 1 + j + 2..];
                    let (hex, len) = if let Some(r) = rest.strip_prefix('{') {
                        let end = r.find('}')?;
                        (&r[..end], end + 2)
                    } else {
                        (rest.get(..4)?, 4)
                    };
                    out.push(char::from_u32(u32::from_str_radix(hex, 16).ok()?)?);
                    for _ in 0..len {
                        chars.next();
                    }
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
    None
}

/// A message argument as written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Msg {
    /// A whole string literal: the message.
    Literal(String),
    /// A name (`p.title`): a message whose English is said elsewhere.
    Named,
    /// Anything else (`"a " + b`, a ternary, a template): pieces, which a
    /// translation can't reorder.
    Composed(String),
}

fn skip_space(src: &str, mut i: usize) -> usize {
    while src.as_bytes().get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// The message argument of each call to `name(` (the first argument; with
/// `skip_first`, the second, as in `t_in!(loc, "…")`), outside comment lines.
fn messages(src: &str, name: &str, skip_first: bool) -> Vec<Msg> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = src[from..].find(name) {
        let start = from + at;
        from = start + name.len();
        let before = src[..start].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            continue;
        }
        let line_start = src[..start].rfind('\n').map_or(0, |n| n + 1);
        if src[line_start..start].trim_start().starts_with("//") {
            continue;
        }
        let mut i = from;
        if skip_first {
            match src[i..].find(',') {
                Some(c) => i += c + 1,
                None => continue,
            }
        }
        i = skip_space(src, i);
        let ends = |j: usize| matches!(src.as_bytes().get(skip_space(src, j)), Some(b',' | b')'));
        let snippet = || src[i..src.len().min(i + 80)].lines().next().unwrap_or("").to_string();
        if let Some((text, end)) = literal(src, i) {
            out.push(if ends(end) { Msg::Literal(text) } else { Msg::Composed(snippet()) });
        } else {
            let name_len = src[i..].find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')).unwrap_or(0);
            // A function's own parameter (`tr(english, …)`) isn't a call.
            if src[i + name_len..].starts_with(" =") {
                continue;
            }
            out.push(if name_len > 0 && ends(i + name_len) { Msg::Named } else { Msg::Composed(snippet()) });
        }
    }
    out
}

fn literals(msgs: Vec<Msg>) -> Vec<String> {
    msgs.into_iter()
        .filter_map(|m| match m {
            Msg::Literal(s) => Some(s),
            _ => None,
        })
        .collect()
}

fn entities(s: &str) -> String {
    s.replace("&gt;", ">").replace("&lt;", "<").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

fn worth(text: &str) -> bool {
    text.chars().any(char::is_alphabetic) && !text.contains("{{")
}

/// The page's own text: text between tags and the attributes people see or
/// hear, as `translatePage` looks them up (spaces run together, trimmed).
fn page_markup() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let head_title = PAGE.split("<title>").nth(1).and_then(|r| r.split("</title>").next()).unwrap_or("");
    out.insert(head_title.to_string());
    let body = PAGE.split("<body>").nth(1).and_then(|r| r.split("<script>").next()).unwrap_or("");
    let mut rest = body;
    while let Some(lt) = rest.find('<') {
        let text = entities(&rest[..lt]).split_whitespace().collect::<Vec<_>>().join(" ");
        if worth(&text) {
            out.insert(text);
        }
        let Some(gt) = rest[lt..].find('>') else { break };
        let tag = &rest[lt..lt + gt];
        for attr in ["placeholder=\"", "title=\"", "aria-label=\""] {
            let mut t = tag;
            while let Some(a) = t.find(attr) {
                let v = &t[a + attr.len()..];
                let value = entities(&v[..v.find('"').unwrap_or(0)]);
                // `title` must be the attribute, not the end of `subtitle` and the like.
                if t[..a].ends_with(char::is_whitespace) && worth(&value) {
                    out.insert(value.split_whitespace().collect::<Vec<_>>().join(" "));
                }
                t = &t[a + attr.len()..];
            }
        }
        rest = &rest[lt + gt + 1..];
    }
    out
}

fn script() -> &'static str {
    PAGE.split("<script>").nth(1).and_then(|r| r.split("</script>").next()).unwrap_or("")
}

fn page_script() -> Vec<String> {
    literals(messages(script(), "tr(", false))
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("source folder").flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Every `t!`, `t_in!` and `t_key!` message in the programs' code.
fn rust_messages() -> Vec<Msg> {
    let mut files = Vec::new();
    rust_sources(&crate_dir().join("src"), &mut files);
    let mut out = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(&f).expect("a source file");
        out.extend(messages(&src, "t!(", false));
        out.extend(messages(&src, "t_in!(", true));
        out.extend(messages(&src, "t_key!(", false));
    }
    out
}

fn rust_strings() -> Vec<String> {
    literals(rust_messages())
}

fn all_strings() -> BTreeSet<String> {
    let mut all = page_markup();
    all.extend(page_script());
    all.extend(rust_strings());
    all
}

#[test]
fn every_english_message_is_well_formed() {
    let bad: Vec<String> =
        all_strings().into_iter().filter_map(|m| l10n::check(&m, &m).err().map(|e| format!("{m:?}: {e}"))).collect();
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn the_string_list_has_the_page_and_the_programs() {
    let all = all_strings();
    for s in [
        "OneUpDrive",
        "Files",
        "Sync now",
        "Folder on this PC",
        "Change it later in Files > Exclude.",
        "{n, plural, one {# shortcut} other {# shortcuts}} to update",
        "Nothing found.",
        "To kept copies on this PC (.oneup\\versions).",
        "30 minutes",
        "Nothing needs your attention.",
    ] {
        assert!(all.contains(s), "{s:?} isn't in the list");
    }
    assert!(
        !all.iter().any(|s| s.contains("{{START") || s.contains("{{L10N")),
        "a host placeholder was taken for text"
    );
    assert!(all.len() > 500, "only {} strings found", all.len());
}

/// Messages are whole: no `tr("a " + b)` or `tr(x ? "a" : "b")`, which
/// the string list can't see and a translation can't reorder; and none is a
/// piece of a sentence (starting with a space or a comma, ending in a space).
#[test]
fn messages_are_whole_sentences_given_as_literals() {
    let mut bad = Vec::new();
    for (whose, msgs) in [("page", messages(script(), "tr(", false)), ("code", rust_messages())] {
        for m in msgs {
            match m {
                Msg::Composed(s) => bad.push(format!("{whose}: built from pieces: {s}")),
                Msg::Literal(s) if s.starts_with([' ', ',', '.', ';', '·']) || s.ends_with(' ') => {
                    bad.push(format!("{whose}: a piece of a sentence: {s:?}"))
                }
                _ => {}
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The template lists every message code gives (`tr`, `t!`, `t_in!`,
/// `t_key!`). The page's own markup isn't required (another branch's new
/// label still shows, in English); regenerate to include it.
#[test]
fn the_template_has_every_message_in_code() {
    if std::env::var_os("ONEUP_WRITE_STRINGS").is_some() {
        return; // being written meanwhile (`write_the_string_template`)
    }
    let text = std::fs::read_to_string(crate_dir().join("locales").join("template.json")).expect("template");
    let template: BTreeMap<String, String> = serde_json::from_str(&text).expect("a JSON object");
    let missing: BTreeSet<String> =
        page_script().into_iter().chain(rust_strings()).filter(|s| !template.contains_key(s)).collect();
    assert!(
        missing.is_empty(),
        "not in locales/template.json (ONEUP_WRITE_STRINGS=1 cargo test -p oneup --test l10n_strings writes it):\n{}",
        missing.into_iter().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn the_checks_tell_whole_messages_from_pieces() {
    let src = r#"tr("Whole {n}", { n }); tr("a " + b); tr(x ? "a" : "b"); tr(p.title); tr(`t ${x}`);
        // tr("in a comment")
        t_in!(loc, "In {lang}"); t_key!("Joined \
                 line");"#;
    let page = messages(src, "tr(", false);
    assert_eq!(page[0], Msg::Literal("Whole {n}".into()));
    assert!(matches!(page[1], Msg::Composed(_)));
    assert!(matches!(page[2], Msg::Composed(_)));
    assert_eq!(page[3], Msg::Named);
    assert!(matches!(page[4], Msg::Composed(_)));
    assert_eq!(page.len(), 5, "comments are skipped");
    assert_eq!(messages(src, "t_in!(", true), [Msg::Literal("In {lang}".into())]);
    assert_eq!(messages(src, "t_key!(", false), [Msg::Literal("Joined line".into())]);
}

#[test]
fn write_the_string_template() {
    let path = crate_dir().join("locales").join("template.json");
    if std::env::var_os("ONEUP_WRITE_STRINGS").is_some() {
        let template: BTreeMap<String, String> = all_strings().into_iter().map(|s| (s, String::new())).collect();
        let text = serde_json::to_string_pretty(&template).expect("JSON") + "\n";
        std::fs::write(&path, text).expect("writing the template");
    }
    // Whatever its age, it's a catalogue a translator can start from.
    let text = std::fs::read_to_string(&path).expect("locales/template.json");
    let template: BTreeMap<String, String> = serde_json::from_str(&text).expect("a JSON object of strings");
    assert!(template.values().all(String::is_empty), "the template has translations");
    for key in template.keys() {
        assert!(l10n::check(key, key).is_ok(), "{key:?} in the template isn't well formed");
    }
}

// ---- the page's formatter and the programs' agree

struct Case {
    lang: &'static str,
    locale: &'static str,
    numbers: fn() -> Numbers,
    strings: &'static [(&'static str, &'static str)],
    english: &'static str,
    args: &'static [(&'static str, Value)],
}

#[derive(Clone, Copy)]
enum Value {
    N(i64),
    F(f64),
    S(&'static str),
}

const FILES: &str = "{n, plural, one {# file} other {# files}}";
const CATS: &str = "{n, plural, other {x}}";
const CATS_SHOWN: &str = "{n, plural, zero {zero} one {one} two {two} few {few} many {many} other {other}}";
const RU_FILES: &str = "{n, plural, one {# файл} few {# файла} many {# файлов} other {# файла}}";
const PL_FILES: &str = "{n, plural, one {# plik} few {# pliki} many {# plików} other {# pliku}}";
const AR_FILES: &str = "{n, plural, zero {لا ملفات} one {ملف} two {ملفان} few {# ملفات} many {# ملفًا} other {# ملف}}";
const FR_FILES: &str = "{n, plural, one {# fichier} other {# fichiers}}";

fn cases() -> Vec<Case> {
    let en = Numbers::english;
    let de = || Numbers::new(",", ".", "3;0");
    let plain = || Numbers::new(",", " ", "3;0");
    let mut out = vec![
        Case { lang: "en", locale: "en-US", numbers: en, strings: &[], english: "Nothing found.", args: &[] },
        Case {
            lang: "en",
            locale: "en-US",
            numbers: en,
            strings: &[],
            english: "{size} in {n, plural, one {# file} other {# files}}",
            args: &[("size", Value::S("4.2 GB")), ("n", Value::N(1830))],
        },
        Case { lang: "en", locale: "en-US", numbers: en, strings: &[], english: FILES, args: &[("n", Value::N(1))] },
        Case {
            lang: "en",
            locale: "en-US",
            numbers: en,
            strings: &[],
            english: "'{'n'}' is literal, it''s; can't",
            args: &[],
        },
        Case {
            lang: "en",
            locale: "en-US",
            numbers: en,
            strings: &[],
            english: "{n, plural, =0 {Nothing} one {One item} other {# items}}",
            args: &[("n", Value::N(0))],
        },
        Case { lang: "en", locale: "en-US", numbers: en, strings: &[], english: "Hello {name}", args: &[] },
        Case {
            lang: "en",
            locale: "en-US",
            numbers: en,
            strings: &[],
            english: "{n, plural, one {{folders} moves} other {{folders} move}} over with it.",
            args: &[("n", Value::N(2)), ("folders", Value::S("Desktop and Documents"))],
        },
        Case {
            lang: "de",
            locale: "de-DE",
            numbers: de,
            strings: &[(FILES, "{n, plural, one {# Datei} other {# Dateien}}")],
            english: FILES,
            args: &[("n", Value::N(1830))],
        },
        Case {
            lang: "de",
            locale: "de-DE",
            numbers: de,
            strings: &[(FILES, "{n, plural, one {# Datei} other {# Dateien}}")],
            english: "Not translated",
            args: &[],
        },
    ];
    for n in [1, 3, 11, 21, 25, 112] {
        out.push(Case {
            lang: "ru",
            locale: "ru-RU",
            numbers: plain,
            strings: &[(FILES, RU_FILES)],
            english: FILES,
            args: [("n", Value::N(n))].to_vec().leak(),
        });
        out.push(Case {
            lang: "pl",
            locale: "pl-PL",
            numbers: plain,
            strings: &[(FILES, PL_FILES)],
            english: FILES,
            args: [("n", Value::N(n))].to_vec().leak(),
        });
    }
    for n in [0, 1, 2, 3, 11, 100, 102] {
        out.push(Case {
            lang: "ar",
            locale: "en-US",
            numbers: en,
            strings: &[(FILES, AR_FILES)],
            english: FILES,
            args: [("n", Value::N(n))].to_vec().leak(),
        });
    }
    // The categories alone (`#` would bring in number formats).
    for (lang, n) in [
        ("pt-PT", 1_000_000.0_f64),
        ("pt-PT", 1.0),
        ("pt-PT", 0.0),
        ("pt", 0.0),
        ("is", 1.11),
        ("is", 1.1),
        ("is", 21.0),
        ("mk", 1.11),
        ("mk", 0.21),
        ("fr", 2_000_000.0),
    ] {
        out.push(Case {
            lang,
            locale: "en-US",
            numbers: en,
            strings: &[(CATS, CATS_SHOWN)],
            english: CATS,
            args: [("n", if n.fract() == 0.0 { Value::N(n as i64) } else { Value::F(n) })].to_vec().leak(),
        });
    }
    // A translation with a case given twice is passed over, in both.
    out.push(Case {
        lang: "de",
        locale: "de-DE",
        numbers: de,
        strings: &[(FILES, "{n, plural, one {# Datei} one {# Dateien} other {# Dateien}}")],
        english: FILES,
        args: &[("n", Value::N(1))],
    });
    for n in [0, 1, 2] {
        out.push(Case {
            lang: "fr",
            locale: "fr-FR",
            numbers: plain,
            strings: &[(FILES, FR_FILES)],
            english: FILES,
            args: [("n", Value::N(n))].to_vec().leak(),
        });
    }
    out
}

/// The page's formatter (from `const L10N` to `translatePage`), run by
/// node on every case. Skipped where node isn't installed.
#[test]
fn the_page_formats_messages_as_the_programs_do() {
    let Ok(v) = std::process::Command::new("node").arg("--version").output() else {
        eprintln!("node isn't installed: skipped");
        return;
    };
    assert!(v.status.success());
    let script = PAGE.split("<script>").nth(1).expect("a script");
    let start = script.find("        const L10N = ").expect("the l10n block");
    let end = script.find("        // The page's own text").expect("the end of the l10n block");
    let block = script[start..end].replace("\"{{L10N}}\"", "CAT_IN");
    let cases = cases();
    let mut js = String::from("const out = [];\n");
    for c in &cases {
        let strings: serde_json::Map<String, serde_json::Value> =
            c.strings.iter().map(|(k, v)| ((*k).to_string(), serde_json::Value::from(*v))).collect();
        let cat = serde_json::json!({ "lang": c.lang, "locale": c.locale, "strings": strings });
        let args: serde_json::Map<String, serde_json::Value> = c
            .args
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::N(n) => serde_json::Value::from(*n),
                    Value::F(x) => serde_json::Value::from(*x),
                    Value::S(s) => serde_json::Value::from(*s),
                };
                ((*k).to_string(), v)
            })
            .collect();
        js.push_str(&format!(
            "out.push((CAT_IN => {{\n{block}\nreturn tr({}, {});\n}})({cat}));\n",
            serde_json::Value::from(c.english),
            serde_json::Value::Object(args)
        ));
    }
    js.push_str("console.log(JSON.stringify(out));\n");
    let dir = std::env::temp_dir().join(format!("oneup-l10n-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp folder");
    let file = dir.join("l10n.js");
    std::fs::write(&file, js).expect("the script");
    let run = std::process::Command::new("node").arg(&file).output().expect("node");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    let page: Vec<String> = serde_json::from_slice(&run.stdout).expect("node's answers");
    for (c, got) in cases.iter().zip(page) {
        let strings: BTreeMap<&str, &str> = c.strings.iter().copied().collect();
        let json = serde_json::to_string(&strings).expect("JSON");
        let locale = Locale::with_catalogue(c.lang, &json, c.locale, (c.numbers)()).expect("a catalogue");
        let args: Vec<(&str, Arg)> = c
            .args
            .iter()
            .map(|(k, v)| {
                let a = match v {
                    Value::N(n) => Arg::from(*n),
                    Value::F(x) => Arg::from(*x),
                    Value::S(s) => Arg::from(*s),
                };
                (*k, a)
            })
            .collect();
        let want = locale.message(c.english, &args);
        assert_eq!(got, want, "{} {:?}", c.lang, c.english);
    }
}
