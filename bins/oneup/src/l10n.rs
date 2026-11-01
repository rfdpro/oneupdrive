//! Messages in the user's language, numbers and dates in their format.
//!
//! The English text is the key (as gettext does it): code says
//! `t!("Nothing found.")` or `t!("{n, plural, one {# file} other {# files}}", n = count)`,
//! and a language's catalogue (`locales/<lang>.json`, built into the
//! programs) maps that text to its translation. A message without one, in
//! any language, shows in English, so a string nobody has extracted yet, or
//! added since, still reads correctly.
//!
//! Messages use a subset of ICU MessageFormat, which translation tools know:
//! `{name}` puts in an argument (numbers formatted for the locale),
//! `{n, number}` a number, and `{n, plural, =0 {…} one {…} other {…}}` picks
//! by the language's plural rules (CLDR), with `#` for the number. An
//! apostrophe quotes: `'{'` is a brace, `''` an apostrophe; any other
//! apostrophe (as in "can't") is itself. The settings page has the same
//! formatter in JavaScript (`settings_ui.html`, `tr()`), with the browser's
//! own plural rules and number formats.
//!
//! The display language comes from Windows (Settings > Time & language >
//! Language: the first one with a catalogue, else English), or from
//! `ONEUP_LANG` (`de`, or a list like `de-AT,en`); numbers and dates follow
//! the regional format.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// The built-in catalogues: language tag and its JSON. English is the
/// source language and has none. See `docs/architecture/localisation.md`
/// § Adding a language.
pub const CATALOGUES: &[(&str, &str)] = &[];

/// A message's argument.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Int(i128),
    Float(f64),
    Str(String),
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Arg {
            fn from(n: $t) -> Arg {
                Arg::Int(i128::from(n))
            }
        }
    )*};
}
from_int!(u8, u16, u32, u64, i8, i16, i32, i64);

impl From<usize> for Arg {
    fn from(n: usize) -> Arg {
        Arg::Int(i128::try_from(n).unwrap_or(i128::MAX))
    }
}

impl From<f64> for Arg {
    fn from(x: f64) -> Arg {
        Arg::Float(x)
    }
}

impl From<&str> for Arg {
    fn from(s: &str) -> Arg {
        Arg::Str(s.to_string())
    }
}

impl From<String> for Arg {
    fn from(s: String) -> Arg {
        Arg::Str(s)
    }
}

impl From<&String> for Arg {
    fn from(s: &String) -> Arg {
        Arg::Str(s.clone())
    }
}

/// A message in the user's language: `t!("Nothing found.")`, or with
/// arguments, `t!("{n} items", n = 4)`.
#[macro_export]
macro_rules! t {
    ($msg:literal) => {
        $crate::l10n::message($msg, &[])
    };
    ($msg:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::l10n::message($msg, &[$((stringify!($name), $crate::l10n::Arg::from($value))),+])
    };
}

/// A message to translate where it's shown rather than here (gettext's
/// `N_`): the English text itself, so constants can hold it. Shown with
/// `l10n::message(text, &[])`; the string list finds it by this mark.
#[macro_export]
macro_rules! t_key {
    ($msg:literal) => {
        $msg
    };
}

/// [`t!`] in a given language: `t_in!(locale, "{n} items", n = 4)`. For
/// text that is shown or stays machine-stable (English for `--json`)
/// depending on who reads it.
#[macro_export]
macro_rules! t_in {
    ($loc:expr, $msg:literal) => {
        $loc.message($msg, &[])
    };
    ($loc:expr, $msg:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $loc.message($msg, &[$((stringify!($name), $crate::l10n::Arg::from($value))),+])
    };
}

// ---- numbers

/// How numbers are written: decimal separator, group separator, and group
/// sizes from the right (the last one repeating when `repeat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Numbers {
    pub decimal: String,
    pub group: String,
    pub groups: Vec<usize>,
    pub repeat: bool,
}

impl Numbers {
    pub fn english() -> Numbers {
        Numbers { decimal: ".".into(), group: ",".into(), groups: vec![3], repeat: true }
    }

    /// From Windows' grouping string: "3;0" is threes, "3;2;0" a three
    /// then twos, "3" one group of three, "0" none.
    pub fn new(decimal: &str, group: &str, grouping: &str) -> Numbers {
        let mut groups: Vec<usize> = grouping.split(';').filter_map(|g| g.trim().parse().ok()).collect();
        let repeat = groups.last() == Some(&0);
        groups.retain(|&g| g > 0);
        Numbers { decimal: decimal.into(), group: group.into(), groups, repeat }
    }

    fn grouped(&self, digits: &str) -> String {
        let mut cuts = Vec::new();
        let mut at = digits.len();
        let mut sizes = self.groups.iter().copied();
        let mut last = None;
        loop {
            let size = match sizes.next() {
                Some(s) => {
                    last = Some(s);
                    s
                }
                None if self.repeat => match last {
                    Some(s) => s,
                    None => break,
                },
                None => break,
            };
            if at <= size {
                break;
            }
            at -= size;
            cuts.push(at);
        }
        let mut out = String::with_capacity(digits.len() + cuts.len() * self.group.len());
        for (i, c) in digits.char_indices() {
            if cuts.contains(&i) {
                out.push_str(&self.group);
            }
            out.push(c);
        }
        out
    }

    /// A whole number: `1830` as "1,830".
    pub fn int(&self, n: i128) -> String {
        let digits = n.unsigned_abs().to_string();
        let sign = if n < 0 { "-" } else { "" };
        format!("{sign}{}", self.grouped(&digits))
    }

    /// A number with `places` decimals: `4.25, 1` as "4.3".
    pub fn fixed(&self, x: f64, places: usize) -> String {
        let s = format!("{:.*}", places, x.abs());
        let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
        let sign = if x < 0.0 && s.chars().any(|c| c.is_ascii_digit() && c != '0') { "-" } else { "" };
        let mut out = format!("{sign}{}", self.grouped(int));
        if !frac.is_empty() {
            out.push_str(&self.decimal);
            out.push_str(frac);
        }
        out
    }

    /// A number as short as it can be written (up to 3 decimals).
    pub fn float(&self, x: f64) -> String {
        let s = format!("{:.3}", x);
        let places = s.split_once('.').map_or(0, |(_, f)| f.trim_end_matches('0').len());
        self.fixed(x, places)
    }
}

// ---- plural rules

/// CLDR's plural category of a number in a language (cardinal rules). The
/// languages listed are those with rules other than "one for 1, other for
/// the rest"; add yours if its rule differs (`unicode.org/cldr`, Language
/// Plural Rules).
pub fn plural(lang: &str, n: &Arg) -> &'static str {
    // CLDR's operands: n the absolute value, i its integer digits, v the
    // number of visible fraction digits, f those digits.
    let (i, v, f, whole) = match n {
        Arg::Int(n) => (n.unsigned_abs(), 0usize, 0u128, true),
        Arg::Float(x) => {
            let s = format!("{:.3}", x.abs());
            let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
            let frac = frac.trim_end_matches('0');
            (int.parse().unwrap_or(0), frac.len(), frac.parse().unwrap_or(0), frac.is_empty())
        }
        Arg::Str(_) => return "other",
    };
    let base = lang.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    let (i10, i100) = (i % 10, i % 100);
    match base.as_str() {
        // No plural forms.
        "ja" | "zh" | "ko" | "vi" | "th" | "id" | "ms" | "lo" | "my" => "other",
        // Portugal: 1 is singular; 0 and 1 are in Brazil and France (below).
        "pt" if lang.eq_ignore_ascii_case("pt-pt") => {
            if i == 1 && v == 0 {
                "one"
            } else if v == 0 && i != 0 && i % 1_000_000 == 0 {
                "many"
            } else {
                "other"
            }
        }
        "fr" | "pt" => {
            if i <= 1 {
                "one"
            } else if v == 0 && i != 0 && i % 1_000_000 == 0 {
                "many"
            } else {
                "other"
            }
        }
        "es" | "it" | "ca" => {
            if i == 1 && v == 0 {
                "one"
            } else if v == 0 && i != 0 && i % 1_000_000 == 0 {
                "many"
            } else {
                "other"
            }
        }
        "ru" | "uk" | "be" => {
            if v != 0 {
                "other"
            } else if i10 == 1 && i100 != 11 {
                "one"
            } else if (2..=4).contains(&i10) && !(12..=14).contains(&i100) {
                "few"
            } else {
                "many"
            }
        }
        "pl" => {
            if v != 0 {
                "other"
            } else if i == 1 {
                "one"
            } else if (2..=4).contains(&i10) && !(12..=14).contains(&i100) {
                "few"
            } else {
                "many"
            }
        }
        "cs" | "sk" => {
            if v != 0 {
                "many"
            } else if i == 1 {
                "one"
            } else if (2..=4).contains(&i) {
                "few"
            } else {
                "other"
            }
        }
        "he" | "iw" => {
            if (i == 1 && v == 0) || (i == 0 && v != 0) {
                "one"
            } else if i == 2 && v == 0 {
                "two"
            } else {
                "other"
            }
        }
        "ar" => {
            if !whole {
                "other"
            } else if i == 0 {
                "zero"
            } else if i == 1 {
                "one"
            } else if i == 2 {
                "two"
            } else if (3..=10).contains(&i100) {
                "few"
            } else if (11..=99).contains(&i100) {
                "many"
            } else {
                "other"
            }
        }
        "da" => {
            if i == 1 || (f != 0 && i == 0) {
                "one"
            } else {
                "other"
            }
        }
        "is" | "mk" => {
            // Icelandic says t (the fraction without trailing zeros) and
            // Macedonian f; `f` here has no trailing zeros, so they agree.
            if (v == 0 && i10 == 1 && i100 != 11) || (v != 0 && f % 10 == 1 && f % 100 != 11) { "one" } else { "other" }
        }
        // Everything else: English's rule.
        _ => {
            if i == 1 && v == 0 {
                "one"
            } else {
                "other"
            }
        }
    }
}

// ---- messages

#[derive(Debug, Clone, PartialEq)]
enum Part {
    Text(String),
    /// `{name}` or `{name, number}`.
    Arg(String),
    /// `#` inside a plural case: the number.
    Hash,
    Plural {
        arg: String,
        cases: Vec<(String, Vec<Part>)>,
    },
}

/// A message that isn't well formed: where (in characters), and what's wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    pub at: usize,
    pub what: &'static str,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at {}: {}", self.at, self.what)
    }
}

impl std::error::Error for Malformed {}

struct Parser<'a> {
    s: &'a [char],
    i: usize,
}

impl Parser<'_> {
    fn bad(&self, what: &'static str) -> Malformed {
        Malformed { at: self.i, what }
    }

    /// Parts up to an unmatched `}` (inside a plural case) or the end.
    fn parts(&mut self, in_plural: bool) -> Result<Vec<Part>, Malformed> {
        let mut out = Vec::new();
        let mut text = String::new();
        while self.i < self.s.len() {
            let c = self.s[self.i];
            match c {
                '\'' => {
                    let next = self.s.get(self.i + 1).copied();
                    if next == Some('\'') {
                        text.push('\'');
                        self.i += 2;
                    } else if matches!(next, Some('{' | '}')) || (in_plural && next == Some('#')) {
                        // Quoted until the next lone apostrophe.
                        self.i += 1;
                        while self.i < self.s.len() {
                            if self.s[self.i] == '\'' {
                                if self.s.get(self.i + 1) == Some(&'\'') {
                                    text.push('\'');
                                    self.i += 2;
                                    continue;
                                }
                                self.i += 1;
                                break;
                            }
                            text.push(self.s[self.i]);
                            self.i += 1;
                        }
                    } else {
                        text.push('\'');
                        self.i += 1;
                    }
                }
                '#' if in_plural => {
                    if !text.is_empty() {
                        out.push(Part::Text(std::mem::take(&mut text)));
                    }
                    out.push(Part::Hash);
                    self.i += 1;
                }
                '{' => {
                    if !text.is_empty() {
                        out.push(Part::Text(std::mem::take(&mut text)));
                    }
                    self.i += 1;
                    out.push(self.placeholder()?);
                }
                '}' if in_plural => break,
                '}' => return Err(self.bad("a } without its {")),
                c => {
                    text.push(c);
                    self.i += 1;
                }
            }
        }
        if !text.is_empty() {
            out.push(Part::Text(text));
        }
        Ok(out)
    }

    fn word(&mut self) -> String {
        self.space();
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_alphanumeric() || matches!(self.s[self.i], '_' | '=' | '-')) {
            self.i += 1;
        }
        self.s[start..self.i].iter().collect()
    }

    fn space(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_whitespace() {
            self.i += 1;
        }
    }

    fn expect(&mut self, c: char, what: &'static str) -> Result<(), Malformed> {
        self.space();
        if self.s.get(self.i) != Some(&c) {
            return Err(self.bad(what));
        }
        self.i += 1;
        Ok(())
    }

    /// After a `{`: `name}`, `name, number}` or `name, plural, …}`.
    fn placeholder(&mut self) -> Result<Part, Malformed> {
        let name = self.word();
        if name.is_empty() {
            return Err(self.bad("a placeholder without a name"));
        }
        self.space();
        match self.s.get(self.i) {
            Some('}') => {
                self.i += 1;
                Ok(Part::Arg(name))
            }
            Some(',') => {
                self.i += 1;
                match self.word().as_str() {
                    "number" => {
                        self.expect('}', "a } after number")?;
                        Ok(Part::Arg(name))
                    }
                    "plural" => {
                        self.expect(',', "a , after plural")?;
                        let mut cases = Vec::new();
                        loop {
                            self.space();
                            if self.s.get(self.i) == Some(&'}') {
                                self.i += 1;
                                break;
                            }
                            let key = self.word();
                            if key.is_empty() {
                                return Err(self.bad("a plural case without a name"));
                            }
                            // Which of two would win is undefined: refused.
                            if cases.iter().any(|(k, _)| *k == key) {
                                return Err(self.bad("a plural case given twice"));
                            }
                            self.expect('{', "a { after a plural case")?;
                            let parts = self.parts(true)?;
                            self.expect('}', "a } closing a plural case")?;
                            cases.push((key, parts));
                        }
                        if !cases.iter().any(|(k, _)| k == "other") {
                            return Err(self.bad("a plural without other"));
                        }
                        Ok(Part::Plural { arg: name, cases })
                    }
                    _ => Err(self.bad("only number and plural can follow a name")),
                }
            }
            _ => Err(self.bad("a placeholder not closed by }")),
        }
    }
}

fn parse(pattern: &str) -> Result<Vec<Part>, Malformed> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut p = Parser { s: &chars, i: 0 };
    let parts = p.parts(false)?;
    if p.i < chars.len() {
        return Err(p.bad("text after the end"));
    }
    Ok(parts)
}

/// Checks a message: well formed, and naming only the placeholders the
/// English names (for a translation, `english` is its key).
pub fn check(pattern: &str, english: &str) -> Result<(), String> {
    let parts = parse(pattern).map_err(|e| e.to_string())?;
    let source = parse(english).map_err(|e| format!("the English: {e}"))?;
    let mut names = Vec::new();
    collect(&source, &mut names);
    let mut used = Vec::new();
    collect(&parts, &mut used);
    match used.iter().find(|n| !names.contains(n)) {
        Some(n) => Err(format!("{{{n}}} isn't in the English")),
        None => Ok(()),
    }
}

fn collect(parts: &[Part], out: &mut Vec<String>) {
    for p in parts {
        match p {
            Part::Arg(n) => out.push(n.clone()),
            Part::Plural { arg, cases } => {
                out.push(arg.clone());
                for (_, c) in cases {
                    collect(c, out);
                }
            }
            Part::Text(_) | Part::Hash => {}
        }
    }
}

/// A language: its catalogue and how numbers are written.
#[derive(Debug, Clone)]
pub struct Locale {
    /// The catalogue's language ("en" for none).
    pub lang: String,
    /// The regional format's tag, for the settings page's `Intl` ("en-GB").
    pub format: String,
    pub numbers: Numbers,
    strings: HashMap<String, String>,
}

impl Locale {
    pub fn english() -> Locale {
        Locale { lang: "en".into(), format: "en-US".into(), numbers: Numbers::english(), strings: HashMap::new() }
    }

    /// A language from its catalogue (a JSON object: English to
    /// translation; empty means not translated yet).
    pub fn with_catalogue(lang: &str, json: &str, format: &str, numbers: Numbers) -> Result<Locale, serde_json::Error> {
        let all: HashMap<String, String> = serde_json::from_str(json)?;
        let strings = all.into_iter().filter(|(_, v)| !v.is_empty()).collect();
        Ok(Locale { lang: lang.into(), format: format.into(), numbers, strings })
    }

    /// The message for `english`, with `args` put in.
    pub fn message(&self, english: &str, args: &[(&str, Arg)]) -> String {
        let translated = self.strings.get(english).and_then(|t| parse(t).ok());
        let (lang, parts) = match translated {
            Some(p) => (self.lang.as_str(), p),
            None => match parse(english) {
                Ok(p) => ("en", p),
                // A malformed English message shows as written.
                Err(_) => return english.to_string(),
            },
        };
        let mut out = String::new();
        self.render(lang, &parts, args, None, &mut out);
        out
    }

    fn arg_text(&self, a: &Arg) -> String {
        match a {
            Arg::Int(n) => self.numbers.int(*n),
            Arg::Float(x) => self.numbers.float(*x),
            Arg::Str(s) => s.clone(),
        }
    }

    fn render(&self, lang: &str, parts: &[Part], args: &[(&str, Arg)], hash: Option<&Arg>, out: &mut String) {
        let get = |name: &str| args.iter().find(|(n, _)| *n == name).map(|(_, a)| a);
        for p in parts {
            match p {
                Part::Text(t) => out.push_str(t),
                Part::Hash => {
                    if let Some(a) = hash {
                        out.push_str(&self.arg_text(a));
                    }
                }
                Part::Arg(name) => match get(name) {
                    Some(a) => out.push_str(&self.arg_text(a)),
                    None => {
                        let _ = write!(out, "{{{name}}}");
                    }
                },
                Part::Plural { arg, cases } => {
                    let Some(a) = get(arg) else {
                        let _ = write!(out, "{{{arg}}}");
                        continue;
                    };
                    let exact = |k: &str| {
                        k.strip_prefix('=').is_some_and(|v| match a {
                            Arg::Int(n) => v.parse::<i128>().ok() == Some(*n),
                            Arg::Float(x) => v.parse::<f64>().ok() == Some(*x),
                            Arg::Str(_) => false,
                        })
                    };
                    let cat = plural(lang, a);
                    let case = cases
                        .iter()
                        .find(|(k, _)| exact(k))
                        .or_else(|| cases.iter().find(|(k, _)| k == cat))
                        .or_else(|| cases.iter().find(|(k, _)| k == "other"));
                    if let Some((_, c)) = case {
                        self.render(lang, c, args, Some(a), out);
                    }
                }
            }
        }
    }

    /// For the settings page: the language, the format locale and the
    /// translations, as JSON that's safe inside a `<script>`.
    pub fn page_json(&self) -> String {
        serde_json::json!({ "lang": self.lang, "locale": self.format, "strings": self.strings })
            .to_string()
            .replace('<', "\\u003c")
    }
}

/// The first of the preferred languages (most preferred first) that has a
/// catalogue, by its whole tag or its language alone; else English.
pub fn choose<'a>(preferred: &[String], available: &[&'a str]) -> Option<&'a str> {
    for want in preferred {
        let want = want.trim();
        if want.is_empty() {
            continue;
        }
        if let Some(a) = available.iter().find(|a| a.eq_ignore_ascii_case(want)) {
            return Some(a);
        }
        let base = want.split(['-', '_']).next().unwrap_or(want);
        if base.eq_ignore_ascii_case("en") {
            // English is the source: an English user gets it, not a later choice.
            return None;
        }
        if let Some(a) =
            available.iter().find(|a| a.split(['-', '_']).next().is_some_and(|b| b.eq_ignore_ascii_case(base)))
        {
            return Some(a);
        }
    }
    None
}

#[cfg(windows)]
fn detect() -> Locale {
    let preferred: Vec<String> = match std::env::var("ONEUP_LANG") {
        Ok(v) if !v.trim().is_empty() => v.split(',').map(str::to_string).collect(),
        _ => oneup_sys::locale::ui_languages(),
    };
    let format = oneup_sys::locale::format_locale().unwrap_or_else(|| "en-US".into());
    let numbers = oneup_sys::locale::number_symbols(&format)
        .map(|s| Numbers::new(&s.decimal, &s.group, &s.grouping))
        .unwrap_or_else(Numbers::english);
    let available: Vec<&str> = CATALOGUES.iter().map(|(l, _)| *l).collect();
    let lang = choose(&preferred, &available);
    let json = lang.and_then(|l| CATALOGUES.iter().find(|(c, _)| *c == l)).map(|(_, j)| *j);
    match (lang, json) {
        (Some(l), Some(j)) => Locale::with_catalogue(l, j, &format, numbers.clone()).unwrap_or_else(|e| {
            tracing::warn!("the {l} catalogue can't be read ({e}); using English");
            Locale { format: format.clone(), numbers: numbers.clone(), ..Locale::english() }
        }),
        _ => Locale { format, numbers, ..Locale::english() },
    }
}

#[cfg(not(windows))]
fn detect() -> Locale {
    Locale::english()
}

/// The user's language and formats, read once. Tests get English, so what
/// they compare doesn't depend on the PC.
pub fn current() -> &'static Locale {
    static CURRENT: OnceLock<Locale> = OnceLock::new();
    CURRENT.get_or_init(|| if cfg!(test) { Locale::english() } else { detect() })
}

/// A message in the user's language (what `t!` calls).
pub fn message(english: &str, args: &[(&str, Arg)]) -> String {
    current().message(english, args)
}

/// A whole number in the user's format: "1,830", "1.830", "1 830".
pub fn number(n: impl Into<Arg>) -> String {
    match n.into() {
        Arg::Int(n) => current().numbers.int(n),
        Arg::Float(x) => current().numbers.float(x),
        Arg::Str(s) => s,
    }
}

/// A number with `places` decimals in the user's format.
pub fn fixed(x: f64, places: usize) -> String {
    current().numbers.fixed(x, places)
}

/// A moment (Unix ms) as a local date and time in the user's short
/// formats, like "20/10/2026 14:05"; ISO-like where Windows can't say.
pub fn date_time(unix_ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_ms) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let t = time::UtcOffset::current_local_offset().map_or(t, |o| t.to_offset(o));
    let iso = || format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute());
    if cfg!(test) {
        return iso();
    }
    #[cfg(windows)]
    {
        let year = u16::try_from(t.year()).unwrap_or(1970);
        if let Some(s) =
            oneup_sys::locale::date_time(&current().format, year, u8::from(t.month()), t.day(), t.hour(), t.minute())
        {
            return s;
        }
    }
    iso()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn en(msg: &str, args: &[(&str, Arg)]) -> String {
        Locale::english().message(msg, args)
    }

    #[test]
    fn untranslated_messages_show_in_english() {
        assert_eq!(en("Nothing found.", &[]), "Nothing found.");
        assert_eq!(t!("Nothing found."), "Nothing found.");
        // An apostrophe is itself unless it quotes a brace.
        assert_eq!(en("Couldn't list this folder", &[]), "Couldn't list this folder");
        assert_eq!(en("'{'n'}' is literal, it''s", &[]), "{n} is literal, it's");
        // A malformed message shows as written rather than not at all.
        assert_eq!(en("Broken {", &[]), "Broken {");
    }

    #[test]
    fn placeholders_and_plurals_are_filled_in() {
        let msg = "{size} in {n, plural, one {# file} other {# files}}";
        assert_eq!(en(msg, &[("size", "4.2 GB".into()), ("n", 1830u64.into())]), "4.2 GB in 1,830 files");
        assert_eq!(en(msg, &[("size", "12 B".into()), ("n", 1u64.into())]), "12 B in 1 file");
        let zero = "{n, plural, =0 {Nothing} one {One item} other {# items}}";
        assert_eq!(en(zero, &[("n", 0u64.into())]), "Nothing");
        assert_eq!(en(zero, &[("n", 1u64.into())]), "One item");
        assert_eq!(en(zero, &[("n", 2u64.into())]), "2 items");
        assert_eq!(
            t!("{n, plural, one {# shortcut} other {# shortcuts}} to update", n = 3usize),
            "3 shortcuts to update"
        );
        // A missing argument stays visible as its placeholder.
        assert_eq!(en("Hello {name}", &[]), "Hello {name}");
        assert_eq!(en("{x, number} MB", &[("x", 2.5f64.into())]), "2.5 MB");
    }

    #[test]
    fn a_catalogue_translates_with_the_languages_plural_rules() {
        let de = Numbers::new(",", ".", "3;0");
        let json = r#"{
            "Nothing found.": "Nichts gefunden.",
            "{n, plural, one {# file} other {# files}}": "{n, plural, one {# Datei} other {# Dateien}}",
            "Not translated yet": ""
        }"#;
        let l = Locale::with_catalogue("de", json, "de-DE", de).expect("catalogue");
        assert_eq!(l.message("Nothing found.", &[]), "Nichts gefunden.");
        let files = "{n, plural, one {# file} other {# files}}";
        assert_eq!(l.message(files, &[("n", 1830u64.into())]), "1.830 Dateien");
        assert_eq!(l.message(files, &[("n", 1u64.into())]), "1 Datei");
        // Empty and missing entries fall back to English.
        assert_eq!(l.message("Not translated yet", &[]), "Not translated yet");
        assert_eq!(l.message("Added later", &[]), "Added later");
        let ru = r#"{"{n, plural, one {# file} other {# files}}": "{n, plural, one {# файл} few {# файла} many {# файлов} other {# файла}}"}"#;
        let l = Locale::with_catalogue("ru", ru, "ru-RU", Numbers::new(",", "\u{a0}", "3;0")).expect("catalogue");
        assert_eq!(l.message(files, &[("n", 1u64.into())]), "1 файл");
        assert_eq!(l.message(files, &[("n", 3u64.into())]), "3 файла");
        assert_eq!(l.message(files, &[("n", 11u64.into())]), "11 файлов");
        assert_eq!(l.message(files, &[("n", 21_000u64.into())]), "21\u{a0}000 файлов");
        assert!(l.page_json().contains("\"lang\":\"ru\""));
        // A malformed translation (a case twice, a brace left open) is
        // passed over for the English.
        let bad = r#"{
            "{n, plural, one {# file} other {# files}}": "{n, plural, one {# Datei} one {# Dateien} other {# Dateien}}",
            "Nothing found.": "Nichts {gefunden"
        }"#;
        let l = Locale::with_catalogue("de", bad, "de-DE", Numbers::new(",", ".", "3;0")).expect("catalogue");
        assert_eq!(l.message(files, &[("n", 2u64.into())]), "2 files");
        assert_eq!(l.message("Nothing found.", &[]), "Nothing found.");
        assert_eq!(t_in!(Locale::english(), "{n} more", n = 1830u64), "1,830 more");
    }

    #[test]
    fn plural_categories_follow_cldr() {
        let cat = |l: &str, n: i128| plural(l, &Arg::Int(n));
        assert_eq!((cat("en", 1), cat("en", 0), cat("en", 2)), ("one", "other", "other"));
        assert_eq!(plural("en", &Arg::Float(1.5)), "other");
        assert_eq!((cat("fr", 0), cat("fr", 1), cat("fr", 2), cat("fr", 1_000_000)), ("one", "one", "other", "many"));
        assert_eq!((cat("pt-PT", 0), cat("pt-PT", 1), cat("pt-PT", 1_000_000)), ("other", "one", "many"));
        for (lang, x, want) in
            [("is", 1.11, "other"), ("is", 1.1, "one"), ("is", 21.0, "one"), ("mk", 1.11, "other"), ("mk", 0.21, "one")]
        {
            assert_eq!(plural(lang, &Arg::Float(x)), want, "{lang} {x}");
        }
        assert_eq!(
            (cat("ru", 1), cat("ru", 2), cat("ru", 5), cat("ru", 11), cat("ru", 22)),
            ("one", "few", "many", "many", "few")
        );
        assert_eq!((cat("pl", 1), cat("pl", 3), cat("pl", 12), cat("pl", 25)), ("one", "few", "many", "many"));
        assert_eq!((cat("cs", 1), cat("cs", 4), cat("cs", 5)), ("one", "few", "other"));
        assert_eq!((cat("ja", 1), cat("zh-Hans", 2)), ("other", "other"));
        assert_eq!(
            (cat("ar", 0), cat("ar", 2), cat("ar", 3), cat("ar", 11), cat("ar", 100)),
            ("zero", "two", "few", "many", "other")
        );
        assert_eq!(plural("en", &Arg::Str("x".into())), "other");
    }

    #[test]
    fn numbers_follow_the_regional_format() {
        let en = Numbers::english();
        assert_eq!(
            (en.int(0), en.int(999), en.int(1830), en.int(-1_234_567)),
            ("0".into(), "999".into(), "1,830".into(), "-1,234,567".into())
        );
        assert_eq!(en.fixed(4.25, 1), "4.2");
        assert_eq!(en.fixed(1234.5, 2), "1,234.50");
        assert_eq!(en.fixed(-0.04, 1), "0.0");
        assert_eq!((en.float(2.5), en.float(3.0)), ("2.5".into(), "3".into()));
        let india = Numbers::new(".", ",", "3;2;0");
        assert_eq!(india.int(12_345_678), "1,23,45,678");
        let once = Numbers::new(",", " ", "3");
        assert_eq!(once.int(1_234_567), "1234 567");
        let none = Numbers::new(".", ",", "0");
        assert_eq!(none.int(1_234_567), "1234567");
        assert_eq!(number(1830u64), "1,830");
        assert_eq!(fixed(4.25, 1), "4.2");
    }

    #[test]
    fn the_language_is_the_first_preferred_with_a_catalogue() {
        let want = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let have = ["de", "fr-CA", "pt-BR"];
        assert_eq!(choose(&want(&["de-AT", "en-US"]), &have), Some("de"));
        assert_eq!(choose(&want(&["fr-CA"]), &have), Some("fr-CA"));
        assert_eq!(choose(&want(&["fr-FR"]), &have), Some("fr-CA"));
        assert_eq!(choose(&want(&["nl-NL", "pt-BR"]), &have), Some("pt-BR"));
        // English first means English, even with another language later.
        assert_eq!(choose(&want(&["en-GB", "de-DE"]), &have), None);
        assert_eq!(choose(&want(&["sv-SE"]), &have), None);
        assert_eq!(choose(&[], &have), None);
    }

    #[test]
    fn translations_are_checked_against_the_english() {
        let files = "{n, plural, one {# file} other {# files}}";
        assert!(check("{n, plural, one {# Datei} other {# Dateien}}", files).is_ok());
        assert!(check("{count} Dateien", files).is_err(), "a placeholder the English doesn't have");
        assert!(check("{n, plural, one {# Datei}}", files).is_err(), "no other case");
        assert!(check("{n", files).is_err());
        assert!(check("}", "x").is_err());
        assert!(check("{n, plural, one {a} one {b} other {c}}", files).is_err(), "a case given twice");
        assert!(check("{n, plural, other {a} other {b}}", files).is_err(), "other given twice");
    }

    #[test]
    fn the_built_in_catalogues_are_well_formed() {
        for (lang, json) in CATALOGUES {
            let all: HashMap<String, String> = serde_json::from_str(json).unwrap_or_else(|e| panic!("{lang}: {e}"));
            for (english, translated) in all.iter().filter(|(_, v)| !v.is_empty()) {
                if let Err(e) = check(translated, english) {
                    panic!("{lang}: {english:?} -> {translated:?}: {e}");
                }
            }
        }
    }
}
