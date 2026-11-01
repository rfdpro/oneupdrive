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
    write_networks(&mut doc, nets)?;
    let p = config::path(data_dir);
    crate::ownfile::write(&p, doc.to_string().as_bytes()).with_context(|| format!("writing {}", p.display()))
}

/// `[metered]` and `[networks]` with `[agent]` (its `pause_on_metered`
/// goes with the metered choice), in one write.
pub fn save_with_agent(data_dir: &Path, nets: &Networks, agent: &Agent) -> Result<()> {
    config::save_agent_with(data_dir, agent, |doc| write_networks(doc, nets))
}

/// [`save`]'s change, in a document being edited.
fn write_networks(doc: &mut DocumentMut, nets: &Networks) -> Result<()> {
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
    Ok(())
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
            // transfer says what moves when it's off. Both in one write.
            let mut a = config::load_agent(dir)?;
            if let Some(t) = c.transfer {
                a.pause_on_metered = t == Transfer::Paused;
                if t == Transfer::Paused {
                    nets.metered.transfer = None;
                }
            }
            check(&nets.metered)?;
            save_with_agent(dir, &nets, &a)
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

/// What the agent does with the profile it just read (at a wake-up, on
/// Windows' event, or when it counted the data used): hands the rounds its
/// policy with what's left of the budget, and applies its caps to the
/// transport at once, so a round under way follows them from its next
/// byte. (The transport's budget is the agent's reservation of the shared
/// one, `crate::netusage`.) `cap_hours` still decides whether the caps apply now.
#[allow(clippy::too_many_arguments)]
pub fn apply_profile(
    control: &crate::control::Control,
    up: &oneup_graph::throttle::Throttle,
    down: &oneup_graph::throttle::Throttle,
    agent: &Agent,
    p: &Profile,
    budget_left: Option<u64>,
    minute_of_day: u32,
) {
    let capped = crate::autopause::caps_apply(agent, minute_of_day);
    let cap = |kib: u64| if capped { kib.saturating_mul(1024) } else { 0 };
    up.set(cap(p.upload_kib));
    down.set(cap(p.download_kib));
    let mut policy = Policy::of(p);
    policy.budget_left = budget_left;
    control.set_policy(policy);
}

/// Windows' network-change events, for the agent to wait on: what the
/// handler `oneup_sys::network::watch` calls pings it, and [`Events::next`]
/// waits for the next change.
#[derive(Debug, Default)]
pub struct Events {
    notify: tokio::sync::Notify,
}

impl Events {
    /// Windows reported a change (any thread).
    pub fn changed(&self) {
        self.notify.notify_one();
    }

    /// Waits for a change. At once, not after letting a burst settle:
    /// what the old network's profile allowed must stop as soon as the new
    /// one applies (each change of a burst is read again; that costs a
    /// look, not a request).
    pub async fn next(&self) {
        self.notify.notified().await;
    }
}

/// Why sync waits for the connection itself, if it does: a network that
/// wants a sign-in first (a captive portal), or none at all. Retrying then
/// only collects errors; Windows reports when that changes. A network
/// Windows says reaches only local addresses isn't waited for: a blocked
/// connectivity probe looks the same, and sync may still work there.
#[cfg(windows)]
pub fn connection_reason(c: oneup_sys::network::Connectivity) -> Option<crate::autopause::Reason> {
    use crate::autopause::Reason;
    use oneup_sys::network::Connectivity;
    match c {
        Connectivity::Constrained => {
            Some(Reason::fixed(crate::t_key!("waiting for the internet: sign in to this network first")))
        }
        Connectivity::None => Some(Reason::fixed(crate::t_key!("waiting for a network connection"))),
        _ => None,
    }
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
    pub transfer: Transfer,
    /// [`Transfer::Small`]'s limit, in bytes.
    pub small_bytes: u64,
    /// The name of the network, for saying why something waits.
    pub network: String,
    /// Bytes left of this month's data budget (`crate::netusage`); `None`:
    /// no budget.
    pub budget_left: Option<u64>,
    /// The user chose to work offline (`crate::control`): nothing goes
    /// over the network but what they open.
    pub offline: bool,
}

impl Policy {
    pub fn of(p: &Profile) -> Self {
        Self {
            caps: Some((p.upload_kib, p.download_kib)),
            transfer: p.transfer,
            small_bytes: p.small_bytes(),
            network: p.network.clone(),
            budget_left: None,
            offline: false,
        }
    }

    /// Whether this policy lets a transfer of `bytes` (`None`: not known
    /// yet) go `way`. Only what moves file contents is ever held:
    /// names, folders, moves, deletions and online-only placeholders go.
    pub fn allows(&self, way: Way, bytes: Option<u64>) -> bool {
        if self.offline {
            return false;
        }
        match self.transfer {
            Transfer::Everything => true,
            Transfer::Paused | Transfer::Metadata => false,
            Transfer::Small => bytes.is_some_and(|b| b < self.small_bytes),
            Transfer::Opened => way == Way::Up,
        }
    }

    /// Why what the budget holds waits.
    pub fn why_budget(&self) -> String {
        let here = if self.network.is_empty() { "this network" } else { self.network.as_str() };
        format!("over this month's data budget on {here}; waits for another network or more allowed")
    }

    /// Why what it holds waits, for `oneup status` and `oneup explain`.
    pub fn why(&self) -> String {
        if self.offline {
            return OFFLINE_WHY.into();
        }
        let here = if self.network.is_empty() { "this network".to_string() } else { self.network.clone() };
        match self.transfer {
            Transfer::Small => format!(
                "larger than {} MB: waits for a network that allows it ({here} doesn't)",
                self.small_bytes / 1_000_000
            ),
            Transfer::Opened => {
                format!("downloads wait for a network that allows them ({here} syncs only files you open or change)")
            }
            _ => format!("file contents wait for a network that allows them ({here} syncs names and folders only)"),
        }
    }
}

/// Why everything waits while working offline.
pub const OFFLINE_WHY: &str = "working offline; sent and fetched once you go back online";

/// Which way a transfer goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    Up,
    Down,
}

/// The file contents `op` moves over the network, if it moves any: which
/// way, and how many bytes (`None`: not known here). An upload's size is
/// its file's as planned; a download's comes from `remote_size`. With
/// Files On-Demand, cloud files arrive as online-only placeholders: no
/// contents.
pub fn content(
    op: &oneup_sync::Op,
    on_demand: bool,
    remote_size: impl FnOnce(&str) -> Option<u64>,
) -> Option<(Way, Option<u64>)> {
    use oneup_sync::Op;
    match op {
        Op::Upload { local, .. } => Some((Way::Up, local.stamp.and_then(|(size, _)| u64::try_from(size).ok()))),
        Op::Download { remote, .. } if !on_demand => Some((Way::Down, remote_size(&remote.id.0))),
        _ => None,
    }
}

/// Bounded list of what waits for another network (`oneup status`,
/// `oneup explain`, `oneup network waiting`): the largest first.
pub const WAITING_MAX: usize = 500;
/// The `meta` key it's kept under.
pub const KEY_WAITING: &str = "net.waiting";

/// What waits for another network, by size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Waiting {
    pub count: u64,
    pub bytes: u64,
    pub why: String,
    /// The largest, at most [`WAITING_MAX`]: (path, bytes, upload).
    pub items: Vec<(String, u64, bool)>,
}

impl Waiting {
    /// One more waiting item.
    pub fn add(&mut self, path: String, bytes: u64, way: Way) {
        self.count += 1;
        self.bytes = self.bytes.saturating_add(bytes);
        self.items.push((path, bytes, way == Way::Up));
        if self.items.len() > 2 * WAITING_MAX {
            self.trim();
        }
    }

    fn trim(&mut self) {
        self.items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        self.items.truncate(WAITING_MAX);
    }

    pub fn to_json(&self) -> String {
        let items: Vec<serde_json::Value> =
            self.items.iter().map(|(p, b, up)| serde_json::json!([p, b, if *up { "up" } else { "down" }])).collect();
        serde_json::json!({ "count": self.count, "bytes": self.bytes, "why": self.why, "items": items }).to_string()
    }

    pub fn from_json(s: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        let items = v["items"]
            .as_array()?
            .iter()
            .filter_map(|i| Some((i[0].as_str()?.to_string(), i[1].as_u64()?, i[2].as_str()? == "up")))
            .collect();
        Some(Self { count: v["count"].as_u64()?, bytes: v["bytes"].as_u64()?, why: v["why"].as_str()?.into(), items })
    }
}

/// Keeps the list after a round (`None`: nothing waits); written only when
/// it changed.
pub fn save_waiting(store: &mut oneup_store::Store, w: Option<Waiting>) -> oneup_store::Result<()> {
    let now = w.map(|mut w| {
        w.trim();
        w.to_json()
    });
    let before = store.read(|o| o.meta_get(KEY_WAITING))?;
    if before == now {
        return Ok(());
    }
    store.write(|o| match &now {
        Some(t) => o.meta_set(KEY_WAITING, t),
        None => o.meta_delete(KEY_WAITING),
    })
}

/// What waits for another network, as the last round left it.
pub fn load_waiting(store: &oneup_store::Store) -> oneup_store::Result<Option<Waiting>> {
    Ok(store.read(|o| o.meta_get(KEY_WAITING))?.and_then(|s| Waiting::from_json(&s)))
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
    fn only_file_contents_wait_and_each_choice_lets_its_own_through() {
        use oneup_sync::{Link, LocalId, LocalPre, Op, RemoteId, RemotePre};
        let up = |size: i64| Op::Upload {
            local: LocalPre { id: LocalId(1), stamp: Some((size, 0)) },
            to: RemoteId("p".into()),
            parent: 1,
            name: "a".into(),
            replace: None,
            link: Link::New,
        };
        let down = Op::Download {
            remote: RemotePre { id: RemoteId("r".into()), etag: "e".into() },
            to: LocalId(2),
            parent: 1,
            name: "b".into(),
            replace: None,
            link: Link::New,
        };
        assert_eq!(content(&up(5), false, |_| None), Some((Way::Up, Some(5))));
        assert_eq!(content(&down, false, |_| Some(9)), Some((Way::Down, Some(9))));
        assert_eq!(content(&down, true, |_| Some(9)), None, "an online-only placeholder moves no contents");
        assert_eq!(content(&Op::Forget(3), false, |_| None), None);
        let p = |transfer| Policy { transfer, small_bytes: 10, ..Policy::default() };
        assert!(p(Transfer::Everything).allows(Way::Down, None));
        assert!(!p(Transfer::Metadata).allows(Way::Up, Some(1)));
        assert!(p(Transfer::Small).allows(Way::Down, Some(9)) && !p(Transfer::Small).allows(Way::Down, Some(10)));
        assert!(!p(Transfer::Small).allows(Way::Down, None), "an unknown size waits");
        assert!(p(Transfer::Opened).allows(Way::Up, Some(1 << 40)) && !p(Transfer::Opened).allows(Way::Down, Some(1)));
    }

    #[test]
    fn the_waiting_list_keeps_the_largest_and_round_trips() {
        let mut w = Waiting { why: "x".into(), ..Waiting::default() };
        for i in 0..(3 * WAITING_MAX as u64) {
            w.add(format!("/f{i}"), i, if i % 2 == 0 { Way::Up } else { Way::Down });
        }
        let mut store = oneup_store::Store::open_in_memory().unwrap();
        save_waiting(&mut store, Some(w.clone())).unwrap();
        let back = load_waiting(&store).unwrap().unwrap();
        assert_eq!((back.count, back.bytes, back.items.len()), (w.count, w.bytes, WAITING_MAX));
        assert_eq!(back.items[0].1, 3 * WAITING_MAX as u64 - 1, "largest first");
        save_waiting(&mut store, None).unwrap();
        assert!(load_waiting(&store).unwrap().is_none());
    }

    /// The roadmap's exit test for network changes: Windows reporting a
    /// metered network (an injected event, simulated time) stops the next
    /// upload of a round under way within 10 s, and pauses sync where the
    /// metered profile pauses. The agent reacts to [`Events::next`] by
    /// reading the network and handing the profile's policy and pause to
    /// its control state, as here.
    #[tokio::test(start_paused = true)]
    async fn a_metered_switch_takes_effect_within_10_seconds() {
        use oneup_sync::{Link, LocalId, LocalPre, Op, RemoteId};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        let control = Arc::new(crate::control::Control::new(None, "L".into(), "/R".into(), Default::default()));
        let events = Arc::new(Events::default());
        let metered = Arc::new(AtomicBool::new(false));
        let free = Agent { pause_on_metered: false, ..Agent::default() };
        let nets = Networks {
            metered: Layer { transfer: Some(Transfer::Metadata), upload_kib: Some(100), ..Layer::default() },
            own: Vec::new(),
        };
        // The agent's reaction, for two agents: metadata only on metered
        // networks, and the default (paused there).
        let up = Arc::new(oneup_graph::throttle::Throttle::default());
        let down = Arc::new(oneup_graph::throttle::Throttle::default());
        let react = |agent: Agent| {
            let (control, events, metered, nets) =
                (Arc::clone(&control), Arc::clone(&events), Arc::clone(&metered), nets.clone());
            let (up, down) = (Arc::clone(&up), Arc::clone(&down));
            tokio::spawn(async move {
                loop {
                    events.next().await;
                    let seen = Seen { id: Some(HOME.into()), name: "Phone".into(), metered: metered.load(Relaxed) };
                    let p = active(&agent, Interval::default(), &nets, &seen);
                    apply_profile(&control, &up, &down, &agent, &p, None, 600);
                    control.set_auto_pause(pause_reason(&p));
                }
            })
        };
        let upload = Op::Upload {
            local: LocalPre { id: LocalId(1), stamp: Some((50_000_000, 0)) },
            to: RemoteId("p".into()),
            parent: 1,
            name: "a".into(),
            replace: None,
            link: Link::New,
        };
        let task = react(free);
        let live = control.controls().live.unwrap();
        let check = live.round_check();
        assert!(check(&upload), "everything goes before the switch");
        let at = tokio::time::Instant::now();
        metered.store(true, Relaxed);
        events.changed();
        while check(&upload) {
            assert!(at.elapsed() < std::time::Duration::from_secs(10), "not within 10 s");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(control.controls().net.transfer == Transfer::Metadata && !control.controls().pause_remote);
        // Uncapped to capped, for a round under way: the transport has the
        // cap at once (rounds otherwise set it as they begin).
        assert_eq!((up.get(), down.get()), (100 * 1024, 0));
        task.abort();

        // With the default, metered pauses: both sides, at once.
        let task = react(Agent::default());
        metered.store(false, Relaxed);
        events.changed();
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        assert!(!control.controls().pause_remote);
        let at = tokio::time::Instant::now();
        metered.store(true, Relaxed);
        events.changed();
        while !control.controls().pause_remote {
            assert!(at.elapsed() < std::time::Duration::from_secs(10), "not within 10 s");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(!live.round_check()(&upload));
        task.abort();
    }

    /// Opting metered networks into a choice turns `pause_on_metered` off
    /// and sets `[metered] transfer` in one write: if it fails, neither
    /// changed (metered networks stay paused, never "everything").
    #[test]
    fn the_metered_choice_and_its_switch_are_written_together() {
        let d = std::env::temp_dir().join(format!("oneup-network-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let mut nets = Networks::default();
        nets.metered.transfer = Some(Transfer::Metadata);
        let a = Agent { pause_on_metered: false, ..Agent::default() };
        let failed = config::save_agent_with(&d, &a, |doc| {
            write_networks(doc, &nets)?;
            anyhow::bail!("the disk is full")
        });
        assert!(failed.is_err());
        assert!(config::load_agent(&d).unwrap().pause_on_metered, "still paused");
        assert_eq!(load(&d).unwrap(), Networks::default());
        save_with_agent(&d, &nets, &a).unwrap();
        assert!(!config::load_agent(&d).unwrap().pause_on_metered);
        assert_eq!(load(&d).unwrap().metered.transfer, Some(Transfer::Metadata));
        let _ = std::fs::remove_dir_all(&d);
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
