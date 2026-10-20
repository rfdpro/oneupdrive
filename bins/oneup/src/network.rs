//! Network profiles: what the agent does on which network
//! (`docs/user-guide/slow-and-metered-networks.md`).
//!
//! Three layers, each setting only what it changes:
//! * **default**: the bandwidth caps of `[agent]` and the interval of
//!   `[sync]`; everything transfers.
//! * **metered** (`[metered]`): on a network Windows says is metered. What
//!   transfers there: paused while `[agent] pause_on_metered` is on (the
//!   default), otherwise `[metered] transfer`.
//! * **a network of its own** (`[networks."{GUID}"]`): keyed by the ID
//!   Windows gives the network (`oneup_sys::network`), never a name the
//!   user types. It goes over the default (or the metered profile, if
//!   Windows says the network is metered).
//!
//! A network nobody set up gets the default profile, or the metered one.
//! Pure, apart from reading and writing `config.toml`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::config::{self, Agent};
use crate::pace::Interval;

/// What content moves on a network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transfer {
    /// Nothing syncs (the agent pauses itself); opening an online-only file
    /// still downloads it.
    Paused,
    #[default]
    Everything,
    /// Names, folders, moves, deletions and online-only placeholders; no
    /// file contents either way.
    Metadata,
    /// Contents of files under the profile's size limit; larger ones wait.
    Small,
    /// Only what the user touches: files opened download (always), files
    /// changed here upload; nothing else downloads in the background.
    Opened,
}

impl Transfer {
    pub const ALL: [Transfer; 5] =
        [Transfer::Paused, Transfer::Everything, Transfer::Metadata, Transfer::Small, Transfer::Opened];

    pub fn name(self) -> &'static str {
        match self {
            Transfer::Paused => "paused",
            Transfer::Everything => "everything",
            Transfer::Metadata => "metadata",
            Transfer::Small => "small",
            Transfer::Opened => "opened",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.name().eq_ignore_ascii_case(s.trim()))
    }

    /// For people (`oneup status`, `oneup network`).
    pub fn label(self, small_mb: u64) -> String {
        match self {
            Transfer::Paused => "nothing (paused)".into(),
            Transfer::Everything => "everything".into(),
            Transfer::Metadata => "names and folders only".into(),
            Transfer::Small => format!("files under {small_mb} MB"),
            Transfer::Opened => "only files you open or change".into(),
        }
    }
}

/// Where a profile applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Default,
    Metered,
    /// A network of its own, by Windows' ID.
    Network(String),
}

impl Key {
    /// How usage and the budget are kept apart (`crate::netusage`).
    pub fn id(&self) -> &str {
        match self {
            Key::Default => "default",
            Key::Metered => "metered",
            Key::Network(id) => id,
        }
    }
}

/// One layer's settings; `None`: as the layer below.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    /// A network's name when it was set up (Windows' ID is the key).
    pub name: Option<String>,
    pub transfer: Option<Transfer>,
    pub small_mb: Option<u64>,
    pub upload_kib: Option<u64>,
    pub download_kib: Option<u64>,
    pub interval_minutes: Option<u32>,
    pub keep_push: Option<bool>,
    /// A monthly data budget in MB; `Some(0)`: none.
    pub budget_mb: Option<u64>,
}

impl Layer {
    fn over(&self, p: &mut Profile) {
        if let Some(t) = self.transfer {
            p.transfer = t;
        }
        if let Some(n) = self.small_mb {
            p.small_mb = n;
        }
        if let Some(n) = self.upload_kib {
            p.upload_kib = n;
        }
        if let Some(n) = self.download_kib {
            p.download_kib = n;
        }
        if let Some(n) = self.interval_minutes {
            p.interval.minutes = n;
        }
        if let Some(b) = self.keep_push {
            p.interval.keep_push = b;
        }
        if let Some(n) = self.budget_mb {
            p.budget_mb = n;
        }
    }
}

/// `[metered]` and `[networks]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Networks {
    pub metered: Layer,
    /// By Windows' network ID.
    pub own: Vec<(String, Layer)>,
}

impl Networks {
    pub fn get(&self, id: &str) -> Option<&Layer> {
        self.own.iter().find(|(k, _)| k.eq_ignore_ascii_case(id)).map(|(_, l)| l)
    }

    pub fn get_mut(&mut self, id: &str) -> &mut Layer {
        let at = match self.own.iter().position(|(k, _)| k.eq_ignore_ascii_case(id)) {
            Some(i) => i,
            None => {
                self.own.push((id.to_string(), Layer::default()));
                self.own.len() - 1
            }
        };
        &mut self.own[at].1
    }

    pub fn forget(&mut self, id: &str) -> bool {
        let before = self.own.len();
        self.own.retain(|(k, _)| !k.eq_ignore_ascii_case(id));
        self.own.len() != before
    }
}

/// The settings in effect on a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub key: Key,
    /// The network's name, as Windows shows it (empty when not told).
    pub network: String,
    pub metered: bool,
    pub transfer: Transfer,
    pub small_mb: u64,
    /// Caps in KiB/s; 0: none.
    pub upload_kib: u64,
    pub download_kib: u64,
    pub interval: Interval,
    /// Monthly data budget in MB; 0: none.
    pub budget_mb: u64,
}

impl Profile {
    pub fn small_bytes(&self) -> u64 {
        self.small_mb.saturating_mul(1_000_000)
    }

    pub fn budget_bytes(&self) -> Option<u64> {
        (self.budget_mb > 0).then(|| self.budget_mb.saturating_mul(1_000_000))
    }

    /// Which profile, for people.
    pub fn describe(&self) -> String {
        match &self.key {
            Key::Default => "default".into(),
            Key::Metered => "metered".into(),
            Key::Network(_) => "its own".into(),
        }
    }
}

/// The network as the agent sees it (from `oneup_sys::network`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    pub id: Option<String>,
    pub name: String,
    pub metered: bool,
}

/// The size limit of [`Transfer::Small`] unless a profile sets one: 10 MB.
pub const SMALL_MB: u64 = 10;

/// The profile in effect on network `n`.
pub fn active(agent: &Agent, interval: Interval, nets: &Networks, n: &Seen) -> Profile {
    let mut p = Profile {
        key: Key::Default,
        network: n.name.clone(),
        metered: n.metered,
        transfer: Transfer::Everything,
        small_mb: SMALL_MB,
        upload_kib: agent.upload_kib,
        download_kib: agent.download_kib,
        interval,
        budget_mb: 0,
    };
    if n.metered {
        p.key = Key::Metered;
        nets.metered.over(&mut p);
        // The long-standing switch decides whether metered pauses.
        p.transfer = match nets.metered.transfer {
            _ if agent.pause_on_metered => Transfer::Paused,
            Some(Transfer::Paused) | None => Transfer::Everything,
            Some(t) => t,
        };
    }
    if let Some(id) = &n.id
        && let Some((key, own)) = nets.own.iter().find(|(k, _)| k.eq_ignore_ascii_case(id))
    {
        p.key = Key::Network(key.clone());
        own.over(&mut p);
    }
    p
}

fn bad(key: &str, what: &str) -> String {
    format!("{}: {key} must be {what}", config::FILE)
}

fn layer(t: &Table, at: &str) -> Result<Layer> {
    let num = |k: &str| config::number(t, k).with_context(|| format!("[{at}]"));
    let transfer = match t.get("transfer") {
        Some(v) => Some(
            v.as_str()
                .and_then(Transfer::from_name)
                .with_context(|| bad("transfer", "paused, everything, metadata, small or opened"))?,
        ),
        None => None,
    };
    let interval_minutes = num("interval_minutes")?.map(crate::pace::check_minutes).transpose()?;
    let keep_push = t.get("keep_push").map(|b| b.as_bool().with_context(|| bad("keep_push", "true or false")));
    Ok(Layer {
        name: t.get("name").and_then(|v| v.as_str()).map(str::to_string),
        transfer,
        small_mb: num("small_mb")?,
        upload_kib: num("upload_kib_per_s")?,
        download_kib: num("download_kib_per_s")?,
        interval_minutes,
        keep_push: keep_push.transpose()?,
        budget_mb: num("budget_mb")?,
    })
}

pub fn load(data_dir: &Path) -> Result<Networks> {
    let mut n = Networks::default();
    let Some(doc) = config::read_doc(data_dir)? else { return Ok(n) };
    if let Some(t) = config::section(&doc, "metered")? {
        n.metered = layer(t, "metered")?;
    }
    if let Some(t) = config::section(&doc, "networks")? {
        for (id, item) in t.iter() {
            let t =
                item.as_table().with_context(|| format!("{}: [networks] holds one table per network", config::FILE))?;
            n.own.push((id.to_string(), layer(t, &format!("networks.\"{id}\""))?));
        }
    }
    Ok(n)
}

fn write_layer(t: &mut Table, l: &Layer) {
    let n = |v: u64| value(i64::try_from(v).unwrap_or(i64::MAX));
    let set = |t: &mut Table, k: &str, v: Option<Item>| match v {
        Some(v) => match t.get_mut(k) {
            Some(item) => *item = v,
            None => _ = t.insert(k, v),
        },
        None => _ = t.remove(k),
    };
    set(t, "name", l.name.as_deref().map(value));
    set(t, "transfer", l.transfer.map(|x| value(x.name())));
    set(t, "small_mb", l.small_mb.map(n));
    set(t, "upload_kib_per_s", l.upload_kib.map(n));
    set(t, "download_kib_per_s", l.download_kib.map(n));
    set(t, "interval_minutes", l.interval_minutes.map(|m| value(i64::from(m))));
    set(t, "keep_push", l.keep_push.map(value));
    set(t, "budget_mb", l.budget_mb.map(n));
}

/// Writes `[metered]` and `[networks]`, keeping everything else in the
/// file (and the comments around these keys) as it was.
pub fn save(data_dir: &Path, nets: &Networks) -> Result<()> {
    let mut doc = config::read_doc(data_dir)?.unwrap_or_else(DocumentMut::new);
    if !doc.contains_table("metered") {
        let mut t = Table::new();
        t.decor_mut().set_prefix(
            "\n# On a metered network: transfer = everything, metadata (names and folders only), small (files\n\
             # under small_mb) or opened (only files you open or change); paused while [agent]\n\
             # pause_on_metered is on. Caps in KiB/s, interval in minutes, budget_mb a month (0: none).\n",
        );
        doc.insert("metered", Item::Table(t));
    }
    let t = doc["metered"].as_table_mut().context("[metered] must be a section")?;
    write_layer(t, &nets.metered);
    if !nets.own.is_empty() || doc.contains_table("networks") {
        if !doc.contains_table("networks") {
            let mut t = Table::new();
            t.set_implicit(true);
            t.decor_mut().set_prefix("\n# Networks with settings of their own, by Windows' network ID.\n");
            doc.insert("networks", Item::Table(t));
        }
        let all = doc["networks"].as_table_mut().context("[networks] must be a section")?;
        all.set_implicit(true);
        let gone: Vec<String> = all.iter().map(|(k, _)| k.to_string()).filter(|k| nets.get(k).is_none()).collect();
        for k in gone {
            all.remove(&k);
        }
        for (id, l) in &nets.own {
            if !all.contains_table(id) {
                all.insert(id, Item::Table(Table::new()));
            }
            let t = all[id.as_str()].as_table_mut().context("a network's settings must be a table")?;
            write_layer(t, l);
        }
    }
    let p = config::path(data_dir);
    crate::ownfile::write(&p, doc.to_string().as_bytes()).with_context(|| format!("writing {}", p.display()))
}

/// Which profile `oneup network set` and the settings window change.
#[derive(Debug, Clone)]
pub enum Target {
    /// `[agent]` caps and `[sync]`.
    Default,
    Metered,
    /// The network this PC is on now.
    This,
    /// A network by Windows' ID.
    Id(String),
}

/// What `oneup network set` and the settings window change; `None`: as it was.
#[derive(Debug, Default)]
pub struct Changes {
    pub transfer: Option<Transfer>,
    pub small_mb: Option<u64>,
    pub upload_kib: Option<u64>,
    pub download_kib: Option<u64>,
    pub interval: Option<u32>,
    pub keep_push: Option<bool>,
    /// MB a month; 0: none.
    pub budget_mb: Option<u64>,
}

impl Changes {
    pub fn onto(&self, l: &mut Layer) {
        if let Some(t) = self.transfer {
            l.transfer = Some(t);
        }
        if let Some(n) = self.small_mb {
            l.small_mb = Some(n);
        }
        if let Some(n) = self.upload_kib {
            l.upload_kib = Some(n);
        }
        if let Some(n) = self.download_kib {
            l.download_kib = Some(n);
        }
        if let Some(n) = self.interval {
            l.interval_minutes = Some(n);
        }
        if let Some(b) = self.keep_push {
            l.keep_push = Some(b);
        }
        if let Some(n) = self.budget_mb {
            l.budget_mb = Some(n);
        }
    }
}

/// The network this PC is on: Windows' ID for it and its name.
#[cfg(windows)]
pub fn this_network() -> Result<(String, String)> {
    let n = oneup_sys::network::current();
    match n.id {
        Some(id) => Ok((id, n.name)),
        None => bail!("this PC isn't on a network Windows can name; connect first"),
    }
}

/// Saves `c` into profile `target` (`oneup network set`, the settings window).
#[cfg(windows)]
pub fn apply(dir: &Path, target: &Target, c: &Changes) -> Result<()> {
    match target {
        Target::Default => {
            if c.transfer.is_some_and(|t| t != Transfer::Everything) || c.small_mb.is_some() || c.budget_mb.is_some() {
                bail!(
                    "the default profile transfers everything; for less, set up this network (--this) or metered networks (--metered)"
                );
            }
            let mut a = config::load_agent(dir)?;
            if let Some(n) = c.upload_kib {
                a.upload_kib = n;
            }
            if let Some(n) = c.download_kib {
                a.download_kib = n;
            }
            config::save_agent(dir, &a)?;
            let mut i = crate::pace::load(dir)?;
            if let Some(m) = c.interval {
                i.minutes = m;
            }
            if let Some(b) = c.keep_push {
                i.keep_push = b;
            }
            crate::pace::save(dir, &i)
        }
        Target::Metered => {
            let mut nets = load(dir)?;
            c.onto(&mut nets.metered);
            // "Paused" is the long-standing [agent] switch; [metered]
            // transfer says what moves when it's off.
            if let Some(t) = c.transfer {
                let mut a = config::load_agent(dir)?;
                a.pause_on_metered = t == Transfer::Paused;
                config::save_agent(dir, &a)?;
                if t == Transfer::Paused {
                    nets.metered.transfer = None;
                }
            }
            check(&nets.metered)?;
            save(dir, &nets)
        }
        Target::This | Target::Id(_) => {
            let (id, name) = match target {
                Target::Id(id) => (id.clone(), String::new()),
                _ => this_network()?,
            };
            let mut nets = load(dir)?;
            let l = nets.get_mut(&id);
            if l.name.is_none() && !name.is_empty() {
                l.name = Some(name);
            }
            c.onto(l);
            check(&l.clone())?;
            save(dir, &nets)
        }
    }
}

/// Checks a profile's numbers.
pub fn check(l: &Layer) -> Result<()> {
    if l.small_mb == Some(0) {
        bail!("small_mb must be at least 1");
    }
    Ok(())
}

/// Why the profile pauses sync, if it does (for `crate::autopause`).
pub fn pause_reason(p: &Profile) -> Option<crate::autopause::Reason> {
    use crate::autopause::Reason;
    (p.transfer == Transfer::Paused).then(|| match &p.key {
        Key::Network(_) => Reason::fixed(crate::t_key!("on this network (its settings pause sync)")),
        _ => Reason::fixed(crate::t_key!("on a metered connection")),
    })
}

/// What a round may send and fetch, as the agent's profile says
/// (`crate::sync::Controls`). The default lets everything through at the
/// `[agent]` caps: a one-shot `oneup sync`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// Caps in KiB/s (0: none), instead of `[agent]`'s.
    pub caps: Option<(u64, u64)>,
}

impl Policy {
    pub fn of(p: &Profile) -> Self {
        Self { caps: Some((p.upload_kib, p.download_kib)) }
    }
}

/// The network now and the profile in effect, read fresh: the agent asks
/// at every wake-up and whenever Windows reports a change. A broken
/// `config.toml` keeps the last good settings (`config::kept`).
#[cfg(windows)]
pub fn situation(data_dir: &Path, agent: &Agent) -> (oneup_sys::network::Network, Profile) {
    let interval = config::kept(data_dir, crate::pace::load(data_dir)).unwrap_or_default();
    let nets = config::kept(data_dir, load(data_dir)).unwrap_or_default();
    let n = oneup_sys::network::current();
    let seen = Seen { id: n.id.clone(), name: n.name.clone(), metered: n.metered };
    let p = active(agent, interval, &nets, &seen);
    (n, p)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const HOME: &str = "{11111111-2222-3333-4444-555555555555}";
    const PHONE: &str = "{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}";

    fn seen(id: &str, metered: bool) -> Seen {
        Seen { id: Some(id.into()), name: "Wi-Fi".into(), metered }
    }

    #[test]
    fn a_new_network_gets_the_default_and_a_metered_one_the_metered_profile() {
        let agent = Agent { upload_kib: 100, ..Agent::default() };
        let every = Interval { minutes: 5, keep_push: false };
        let nets = Networks::default();
        let p = active(&agent, every, &nets, &seen(HOME, false));
        assert_eq!((p.key, p.transfer, p.upload_kib, p.interval.minutes), (Key::Default, Transfer::Everything, 100, 5));
        // Metered stays paused by default (the long-standing setting).
        let p = active(&agent, every, &nets, &seen(PHONE, true));
        assert_eq!((p.key.clone(), p.transfer), (Key::Metered, Transfer::Paused));
        // Not paused: what [metered] says, everything if it says nothing.
        let free = Agent { pause_on_metered: false, ..agent.clone() };
        assert_eq!(active(&free, every, &nets, &seen(PHONE, true)).transfer, Transfer::Everything);
        let nets = Networks {
            metered: Layer {
                transfer: Some(Transfer::Small),
                small_mb: Some(5),
                interval_minutes: Some(30),
                ..Layer::default()
            },
            own: Vec::new(),
        };
        let p = active(&free, every, &nets, &seen(PHONE, true));
        assert_eq!(
            (p.transfer, p.small_bytes(), p.interval.minutes, p.upload_kib),
            (Transfer::Small, 5_000_000, 30, 100)
        );
        assert_eq!(active(&agent, every, &nets, &seen(PHONE, true)).transfer, Transfer::Paused, "the switch wins");
    }

    #[test]
    fn a_network_of_its_own_goes_over_the_layers_below_by_id() {
        let agent = Agent::default();
        let mut nets = Networks::default();
        *nets.get_mut(PHONE) = Layer { transfer: Some(Transfer::Metadata), budget_mb: Some(2000), ..Layer::default() };
        *nets.get_mut(HOME) = Layer { download_kib: Some(50), ..Layer::default() };
        let p = active(&agent, Interval::default(), &nets, &seen(&PHONE.to_lowercase(), true));
        assert_eq!((p.key.id(), p.transfer, p.budget_bytes()), (PHONE, Transfer::Metadata, Some(2_000_000_000)));
        let p = active(&agent, Interval::default(), &nets, &seen(HOME, false));
        assert_eq!((p.transfer, p.download_kib, p.describe().as_str()), (Transfer::Everything, 50, "its own"));
        // Same name, different network: the ID decides.
        let p = active(&agent, Interval::default(), &nets, &Seen { id: None, name: "Wi-Fi".into(), metered: false });
        assert_eq!(p.key, Key::Default);
        assert!(nets.forget(HOME) && !nets.forget(HOME));
    }

    #[test]
    fn a_pausing_profile_says_why() {
        let agent = Agent::default();
        let nets = Networks::default();
        let p = active(&agent, Interval::default(), &nets, &seen(PHONE, true));
        assert_eq!(pause_reason(&p).map(|r| r.english()).as_deref(), Some("on a metered connection"));
        assert_eq!(pause_reason(&active(&agent, Interval::default(), &nets, &seen(HOME, false))), None);
        let mut nets = Networks::default();
        nets.get_mut(HOME).transfer = Some(Transfer::Paused);
        let p = active(&agent, Interval::default(), &nets, &seen(HOME, false));
        assert!(pause_reason(&p).unwrap().english().contains("this network"));
    }

    #[test]
    fn profiles_round_trip_and_keep_the_rest_of_the_file() {
        let d = std::env::temp_dir().join(format!("oneup-network-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(config::path(&d), "# mine\n[agent]\nupload_kib_per_s = 7 # keep\n").unwrap();
        assert_eq!(load(&d).unwrap(), Networks::default());
        let mut nets = Networks {
            metered: Layer { transfer: Some(Transfer::Opened), budget_mb: Some(500), ..Layer::default() },
            own: Vec::new(),
        };
        *nets.get_mut(HOME) = Layer { name: Some("Home".into()), interval_minutes: Some(15), ..Layer::default() };
        save(&d, &nets).unwrap();
        assert_eq!(load(&d).unwrap(), nets);
        let text = std::fs::read_to_string(config::path(&d)).unwrap();
        assert!(text.contains("upload_kib_per_s = 7 # keep") && text.contains("# mine"), "{text}");
        assert_eq!(config::load_agent(&d).unwrap().upload_kib, 7);
        nets.forget(HOME);
        nets.metered.budget_mb = None;
        save(&d, &nets).unwrap();
        assert_eq!(load(&d).unwrap(), nets);
        std::fs::write(config::path(&d), "[metered]\ntransfer = \"some\"\n").unwrap();
        assert!(load(&d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
