//! Database files: which files are databases, and what makes a copy of one
//! safe to send or replace (`docs/architecture/databases.md`).
//!
//! A program keeps a database open and writes it in place, often in
//! several steps per change, and SQLite keeps recent changes in a `-wal`
//! file beside it. A copy taken at the wrong moment is a broken database:
//! a password vault or a mailbox that no longer opens. So a database is
//! sent only in a consistent state and replaced only while it's closed.
//!
//! Recognised by their first bytes (SQLite, KeePass, Access, Outlook,
//! virtual disks, Thunderbird's summary files) or their name, and by the
//! user's own patterns (`[databases]` in `config.toml`, with opt-in
//! presets for formats that have no signature). Pure: callers read the
//! bytes and the settings.

use crate::pattern::Rule;

/// How many of a file's first bytes [`Format::of_header`] looks at.
pub const HEAD: usize = 64;

/// A kind of database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// SQLite, with its `-wal`, `-shm` and `-journal` files beside it.
    Sqlite,
    /// KeePass and KeePassXC password databases (`.kdbx`, `.kdb`).
    KeePass,
    /// Microsoft Access (`.accdb`, `.mdb`).
    Access,
    /// Thunderbird's mail folders and their summary files (`.msf`).
    Thunderbird,
    /// Outlook data files (`.pst`, `.ost`).
    Outlook,
    /// Virtual machine disks (`.vhdx`, `.vhd`, `.vmdk`, `.vdi`, `.qcow2`).
    VirtualDisk,
    /// One of the user's `[databases]` patterns.
    Other,
}

/// When a copy of a database is consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consistent {
    /// SQLite: once every change is in the file itself. Its `-wal` is
    /// absent or empty (checkpointed, or the program closed it) and its
    /// `-journal` isn't in a transaction. It may stay open meanwhile.
    Sqlite,
    /// Anything else: only while no other program has it open.
    WhileClosed,
}

impl Format {
    /// What its first bytes say it is (`head`: up to [`HEAD`] of them).
    pub fn of_header(head: &[u8]) -> Option<Format> {
        let at = |i: usize, m: &[u8]| head.get(i..i + m.len()) == Some(m);
        if at(0, b"SQLite format 3\0") {
            return Some(Format::Sqlite);
        }
        // KeePass: a first signature, then the version's (1.x, 2.x pre-release, 2.x and later).
        if at(0, &[0x03, 0xD9, 0xA2, 0x9A])
            && (at(4, &[0x65, 0xFB, 0x4B, 0xB5])
                || at(4, &[0x66, 0xFB, 0x4B, 0xB5])
                || at(4, &[0x67, 0xFB, 0x4B, 0xB5]))
        {
            return Some(Format::KeePass);
        }
        if at(4, b"Standard Jet DB") || at(4, b"Standard ACE DB") {
            return Some(Format::Access);
        }
        if at(0, b"!BDN") {
            return Some(Format::Outlook);
        }
        // VHDX, VMDK (sparse), VHD (dynamic: its footer's copy first), QCOW2.
        if at(0, b"vhdxfile") || at(0, b"KDMV") || at(0, b"conectix") || at(0, b"QFI\xFB") {
            return Some(Format::VirtualDisk);
        }
        // Mork, the format of Thunderbird's summary files.
        if at(0, b"// <!-- <mdb:mork") {
            return Some(Format::Thunderbird);
        }
        None
    }

    /// What its name says it is.
    pub fn of_name(name: &str) -> Option<Format> {
        let (_, ext) = name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty())?;
        Some(match ext.to_ascii_lowercase().as_str() {
            "sqlite" | "sqlite3" | "db" | "db3" => Format::Sqlite,
            "kdbx" | "kdb" => Format::KeePass,
            "accdb" | "accde" | "mdb" | "mde" => Format::Access,
            "msf" => Format::Thunderbird,
            "pst" | "ost" => Format::Outlook,
            "vhdx" | "avhdx" | "vhd" | "vmdk" | "vdi" | "qcow2" => Format::VirtualDisk,
            _ => return None,
        })
    }

    pub fn consistent(self) -> Consistent {
        match self {
            Format::Sqlite => Consistent::Sqlite,
            _ => Consistent::WhileClosed,
        }
    }

    /// Formats that shouldn't sync while in use at all: the program keeps
    /// them open, and writing, for as long as it runs, so they'd seldom be
    /// sent, and each copy is gigabytes. Warned about, with an offer to
    /// exclude them; never excluded without asking.
    pub fn never_live(self) -> bool {
        matches!(self, Format::Outlook | Format::VirtualDisk)
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Sqlite => "SQLite database",
            Format::KeePass => "KeePass database",
            Format::Access => "Access database",
            Format::Thunderbird => "Thunderbird mail file",
            Format::Outlook => "Outlook data file",
            Format::VirtualDisk => "virtual disk",
            Format::Other => "database (your setting)",
        }
    }

    /// Its name in settings and `--json` output.
    pub fn name(self) -> &'static str {
        match self {
            Format::Sqlite => "sqlite",
            Format::KeePass => "keepass",
            Format::Access => "access",
            Format::Thunderbird => "thunderbird",
            Format::Outlook => "outlook",
            Format::VirtualDisk => "virtual-disk",
            Format::Other => "other",
        }
    }
}

/// A file SQLite keeps beside a database while it works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Companion {
    /// `-wal`: changes not yet copied into the database.
    Wal,
    /// `-shm`: the index of the `-wal`, rebuilt by SQLite as needed.
    Shm,
    /// `-journal`: the rollback journal of a transaction under way.
    Journal,
}

pub const COMPANIONS: [Companion; 3] = [Companion::Wal, Companion::Shm, Companion::Journal];

impl Companion {
    pub fn suffix(self) -> &'static str {
        match self {
            Companion::Wal => "-wal",
            Companion::Shm => "-shm",
            Companion::Journal => "-journal",
        }
    }

    /// The companion's name for database `main`.
    pub fn name_for(self, main: &str) -> String {
        format!("{main}{}", self.suffix())
    }

    /// If `name` is a companion's name, which one and its database's name.
    pub fn of_name(name: &str) -> Option<(Companion, &str)> {
        COMPANIONS.into_iter().find_map(|c| {
            let cut = name.len().checked_sub(c.suffix().len())?;
            let (main, suffix) = (name.get(..cut)?, name.get(cut..)?);
            (!main.is_empty() && suffix.eq_ignore_ascii_case(c.suffix())).then_some((c, main))
        })
    }
}

/// Whether a rollback journal's first bytes (`head`, 8 or more of them, or
/// all it has) say a transaction is under way or was cut short. SQLite
/// writes the journal's header last, zeroes it when the transaction ends
/// (`journal_mode = PERSIST`), truncates it (`TRUNCATE`) or deletes it
/// (`DELETE`, the default).
pub fn journal_active(head: &[u8]) -> bool {
    head.iter().take(8).any(|&b| b != 0)
}

/// The lock file Access keeps beside a database it has open (`x.laccdb`
/// for `x.accdb`, `x.ldb` for `x.mdb`).
pub fn access_lock_name(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    match ext.to_ascii_lowercase().as_str() {
        "accdb" | "accde" => Some(format!("{stem}.laccdb")),
        "mdb" | "mde" => Some(format!("{stem}.ldb")),
        _ => None,
    }
}

/// How a file was recognised as a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum By {
    /// Its first bytes.
    Header,
    /// Its name.
    Name,
    /// A Thunderbird mail folder: a file with a `.msf` summary beside it.
    Summary,
    /// SQLite's `-wal` or `-journal` files are beside it (or it's one of
    /// them).
    Companions,
    /// A `[databases]` pattern (from a preset if named).
    Rule { preset: Option<&'static str>, rule: String },
}

impl By {
    pub fn describe(&self) -> String {
        match self {
            By::Header => "recognised by its content".into(),
            By::Name => "recognised by its name".into(),
            By::Summary => "a Thunderbird mail folder (it has a .msf file beside it)".into(),
            By::Companions => "SQLite's -wal or -journal files are beside it".into(),
            By::Rule { preset: Some(p), rule } => format!("database preset `{p}` ({rule})"),
            By::Rule { preset: None, rule } => format!("[databases] patterns ({rule})"),
        }
    }
}

/// A file recognised as a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recognised {
    pub format: Format,
    pub by: By,
}

/// A preset of `[databases]` patterns: formats with no signature to
/// recognise them by. Offered, never on unless the user chose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabasePreset {
    pub name: &'static str,
    pub summary: &'static str,
    pub lines: &'static [&'static str],
}

pub const DATABASE_PRESETS: &[DatabasePreset] = &[
    DatabasePreset {
        name: "finance",
        summary: "QuickBooks, Quicken, GnuCash and Moneydance files",
        lines: &["*.qbw", "*.qbb", "*.tlg", "*.qdf", "*.gnucash", "*.moneydance"],
    },
    DatabasePreset { name: "firebird", summary: "Firebird and InterBase databases", lines: &["*.fdb", "*.gdb"] },
    DatabasePreset { name: "filemaker", summary: "FileMaker databases", lines: &["*.fmp12", "*.fp7"] },
    DatabasePreset { name: "dbase", summary: "dBase and FoxPro tables", lines: &["*.dbf", "*.cdx", "*.mdx", "*.ndx"] },
    DatabasePreset { name: "lotus-notes", summary: "Lotus Notes databases", lines: &["*.nsf", "*.ntf"] },
];

pub fn database_preset(name: &str) -> Option<&'static DatabasePreset> {
    DATABASE_PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

/// A `[databases] presets` entry that doesn't exist.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown database preset `{0}` (known: finance, firebird, filemaker, dbase, lotus-notes)")]
pub struct UnknownDatabasePreset(pub String);

/// What counts as a database, and whether databases other than SQLite wait
/// for their programs to close them (`[databases] wait`, on by default;
/// SQLite's consistency is checked whatever it says).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Databases {
    pub wait: bool,
    /// Presets first, then the user's patterns: the last match wins, and
    /// a `!` pattern says "not a database".
    rules: Vec<(Option<&'static str>, Rule)>,
}

impl Default for Databases {
    fn default() -> Self {
        Self { wait: true, rules: Vec::new() }
    }
}

impl Databases {
    pub fn new<S: AsRef<str>>(wait: bool, presets: &[S], patterns: &[S]) -> Result<Self, UnknownDatabasePreset> {
        let mut rules = Vec::new();
        for p in presets {
            let preset = database_preset(p.as_ref()).ok_or_else(|| UnknownDatabasePreset(p.as_ref().into()))?;
            rules.extend(preset.lines.iter().filter_map(|l| Rule::parse(l)).map(|r| (Some(preset.name), r)));
        }
        rules.extend(patterns.iter().filter_map(|l| Rule::parse(l.as_ref())).map(|r| (None, r)));
        Ok(Self { wait, rules })
    }

    /// Nothing waits (`[databases] wait = false`).
    pub fn off() -> Self {
        Self { wait: false, rules: Vec::new() }
    }

    /// What the patterns say about the file at `path` (relative to the
    /// synced folder): `Some(true)` a database, `Some(false)` a `!`
    /// pattern says it isn't, `None` no pattern matches.
    fn rule_for(&self, path: &str) -> Option<(bool, By)> {
        let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return None;
        }
        self.rules
            .iter()
            .rev()
            .find(|(_, r)| r.matches(&parts, false))
            .map(|(preset, r)| (!r.negated, By::Rule { preset: *preset, rule: r.text.clone() }))
    }

    /// Whether the file at `path` (relative to the synced folder) is a
    /// database, from its first bytes (`head`, if read) and its name. A
    /// `!` pattern is the user's say that it isn't, and wins over
    /// everything; then its content; then the other patterns; then its
    /// name. `summary`: a `.msf` file is beside it.
    pub fn recognise(&self, path: &str, head: Option<&[u8]>, summary: bool) -> Option<Recognised> {
        let rule = self.rule_for(path);
        if matches!(rule, Some((false, _))) {
            return None;
        }
        if let Some(format) = head.and_then(Format::of_header) {
            return Some(Recognised { format, by: By::Header });
        }
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if let Some((_, by)) = rule {
            return Some(Recognised { format: Format::of_name(name).unwrap_or(Format::Other), by });
        }
        if let Some(format) = Format::of_name(name) {
            return Some(Recognised { format, by: By::Name });
        }
        summary.then_some(Recognised { format: Format::Thunderbird, by: By::Summary })
    }

    /// Whether a `!` pattern says the file at `path` isn't a database.
    pub fn opted_out(&self, path: &str) -> bool {
        matches!(self.rule_for(path), Some((false, _)))
    }

    /// [`Databases::recognise`] from the name and the patterns only (no
    /// content): what `oneup explain` can say without reading the file.
    pub fn by_name(&self, path: &str) -> Option<Recognised> {
        self.recognise(path, None, false)
    }

    /// The patterns, as written, with the preset each came from.
    pub fn patterns(&self) -> impl Iterator<Item = (Option<&'static str>, &str)> {
        self.rules.iter().map(|(p, r)| (*p, r.text.as_str()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn head(prefix: &[u8], at: usize) -> Vec<u8> {
        let mut v = vec![0u8; HEAD];
        v[at..at + prefix.len()].copy_from_slice(prefix);
        v
    }

    #[test]
    fn formats_are_recognised_by_their_first_bytes() {
        assert_eq!(Format::of_header(b"SQLite format 3\0\x10\0"), Some(Format::Sqlite));
        let kdbx = [0x03, 0xD9, 0xA2, 0x9A, 0x67, 0xFB, 0x4B, 0xB5, 0, 0, 4, 0];
        assert_eq!(Format::of_header(&kdbx), Some(Format::KeePass));
        let kdb = [0x03, 0xD9, 0xA2, 0x9A, 0x65, 0xFB, 0x4B, 0xB5];
        assert_eq!(Format::of_header(&kdb), Some(Format::KeePass));
        assert_eq!(Format::of_header(&head(b"Standard ACE DB", 4)), Some(Format::Access));
        assert_eq!(Format::of_header(&head(b"Standard Jet DB", 4)), Some(Format::Access));
        assert_eq!(Format::of_header(b"!BDN\x01\x02"), Some(Format::Outlook));
        assert_eq!(Format::of_header(b"vhdxfile\0\0"), Some(Format::VirtualDisk));
        assert_eq!(Format::of_header(b"KDMV\x01\0\0\0"), Some(Format::VirtualDisk));
        assert_eq!(Format::of_header(b"// <!-- <mdb:mork:z v=\"1.4\"/> -->"), Some(Format::Thunderbird));
        // Close but not quite, and too short.
        assert_eq!(Format::of_header(b"SQLite format 2\0"), None);
        assert_eq!(Format::of_header(&[0x03, 0xD9, 0xA2, 0x9A, 0x00, 0xFB, 0x4B, 0xB5]), None);
        assert_eq!(Format::of_header(b"SQLite"), None);
        assert_eq!(Format::of_header(b""), None);
    }

    #[test]
    fn formats_are_recognised_by_their_names() {
        assert_eq!(Format::of_name("Passwords.KDBX"), Some(Format::KeePass));
        assert_eq!(Format::of_name("orders.accdb"), Some(Format::Access));
        assert_eq!(Format::of_name("Inbox.msf"), Some(Format::Thunderbird));
        assert_eq!(Format::of_name("archive.pst"), Some(Format::Outlook));
        assert_eq!(Format::of_name("win11.vhdx"), Some(Format::VirtualDisk));
        assert_eq!(Format::of_name("places.sqlite"), Some(Format::Sqlite));
        assert_eq!(Format::of_name("app.db"), Some(Format::Sqlite));
        assert_eq!(Format::of_name("notes.txt"), None);
        assert_eq!(Format::of_name(".pst"), None, "a name that is all extension");
        assert_eq!(Format::of_name("pst"), None);
    }

    #[test]
    fn only_sqlite_may_be_sent_while_open_and_outlook_and_disks_never_live() {
        assert_eq!(Format::Sqlite.consistent(), Consistent::Sqlite);
        assert_eq!(Format::KeePass.consistent(), Consistent::WhileClosed);
        assert!(Format::Outlook.never_live() && Format::VirtualDisk.never_live());
        assert!(!Format::Sqlite.never_live() && !Format::KeePass.never_live());
    }

    #[test]
    fn companions_are_told_by_their_suffix() {
        assert_eq!(Companion::of_name("app.db-wal"), Some((Companion::Wal, "app.db")));
        assert_eq!(Companion::of_name("app.db-SHM"), Some((Companion::Shm, "app.db")));
        assert_eq!(Companion::of_name("x-journal"), Some((Companion::Journal, "x")));
        assert_eq!(Companion::of_name("-wal"), None);
        assert_eq!(Companion::of_name("app.db"), None);
        assert_eq!(Companion::Wal.name_for("app.db"), "app.db-wal");
        // Not cut inside a character.
        assert_eq!(Companion::of_name("é-wal"), Some((Companion::Wal, "é")));
        assert_eq!(Companion::of_name("éé"), None);
    }

    #[test]
    fn a_journal_is_active_unless_empty_or_zeroed() {
        assert!(!journal_active(b""));
        assert!(!journal_active(&[0; 28]));
        assert!(journal_active(&[0xD9, 0xD5, 0x05, 0xF9, 0x20, 0xA1, 0x63, 0xD7]));
        assert!(journal_active(&[0, 0, 0, 0, 0, 0, 0, 1]));
    }

    #[test]
    fn access_lock_files_follow_the_database_name() {
        assert_eq!(access_lock_name("Orders.accdb").as_deref(), Some("Orders.laccdb"));
        assert_eq!(access_lock_name("old.MDB").as_deref(), Some("old.ldb"));
        assert_eq!(access_lock_name("a.kdbx"), None);
    }

    #[test]
    fn opting_out_wins_then_content_then_patterns_then_names() {
        let d = Databases::new(true, &["finance"], &["Books/*.dat", "!old.kdbx"]).unwrap();
        let sqlite = b"SQLite format 3\0";
        let r = d.recognise("x/cache.bin", Some(sqlite), false).unwrap();
        assert_eq!((r.format, r.by), (Format::Sqlite, By::Header));
        // A pattern, from a preset or the user's own.
        let r = d.recognise("Money/home.qdf", None, false).unwrap();
        assert_eq!(r.format, Format::Other);
        assert_eq!(r.by, By::Rule { preset: Some("finance"), rule: "*.qdf".into() });
        let r = d.recognise("Books/ledger.dat", Some(b"plain text"), false).unwrap();
        assert_eq!(r.by, By::Rule { preset: None, rule: "Books/*.dat".into() });
        assert!(d.recognise("Other/ledger.dat", None, false).is_none());
        // `!` says a file isn't a database, whatever its name or content.
        assert!(d.recognise("old.kdbx", None, false).is_none());
        assert!(d.recognise("old.kdbx", Some(sqlite), false).is_none());
        assert!(d.opted_out("old.kdbx") && !d.opted_out("vault.kdbx"));
        let r = d.recognise("vault.kdbx", None, false).unwrap();
        assert_eq!((r.format, r.by), (Format::KeePass, By::Name));
        // A mail folder, by the summary beside it.
        let r = d.recognise("Mail/Inbox", None, true).unwrap();
        assert_eq!((r.format, r.by), (Format::Thunderbird, By::Summary));
        assert!(d.recognise("notes.txt", Some(b"hello"), false).is_none());
    }

    #[test]
    fn presets_are_known_by_name_and_never_on_unless_chosen() {
        assert!(Databases::default().patterns().next().is_none());
        assert!(Databases::default().wait);
        assert_eq!(database_preset("FINANCE").map(|p| p.name), Some("finance"));
        assert_eq!(Databases::new(true, &["nope"], &[]), Err(UnknownDatabasePreset("nope".into())));
        for p in DATABASE_PRESETS {
            assert!(p.lines.iter().all(|l| Rule::parse(l).is_some()), "{}", p.name);
        }
    }
}
