//! `oneup`: OneUpDrive's command-line tool.
//!
//! Signing in, folder pairs and sync, the agent's controls, rules, Files
//! On-Demand, vaults and the developer commands. Everything that changes
//! user content goes through the shared engine (`oneup_app::sync`), the
//! safety gate and the effects layer; the read-only commands (`mirror`,
//! `scan`, `plan`, `verify`, `explain`) change nothing outside OneUpDrive's
//! own data folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use oneup_graph::auth::{READ_WRITE_ALL_SCOPE, READ_WRITE_SCOPE};
use oneup_graph::{Aimd, Auth, DpapiFileStore, Graph};
use oneup_remote::{Config, Mirror, Phase, SplitHint};
use oneup_rules::ExcludeSet;
use oneup_store::Store;

mod activity;
mod agent;
mod cloud_check;
mod cloud_prune;
mod dev_remote;
mod migrate;
mod pair;
mod rules;
mod settings;
mod shellverb;
mod unsynced_cmd;
mod upload_bench;
mod vault;

use oneup_app::setup::DirLock;
use oneup_app::{setup, sync, test_account, versions};

#[derive(Parser)]
#[command(name = "oneup", version, about = "OneUpDrive: a lightweight, careful OneDrive client (pre-alpha, read-only)")]
struct Cli {
    /// Where OneUpDrive keeps its state [default: %LOCALAPPDATA%\OneUpDrive]
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// App registration client ID
    #[arg(long, global = true, env = "ONEUP_CLIENT_ID", hide_env_values = true)]
    client_id: Option<String>,
    /// More log output (-v, -vv)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sign in with your Microsoft account (opens your browser)
    Login(LoginArgs),
    /// Sign out and forget the stored credentials
    Logout,
    /// Build or update the local mirror of your OneDrive's file list (read-only)
    Mirror(MirrorArgs),
    /// Record the file list of a folder on this PC (read-only; never opens or downloads files)
    Scan(ScanArgs),
    /// Bring the last folder scan up to date from the drive's change journal (read-only; no rescan)
    Observe(ProfileArg),
    /// Compare the scanned folder with your OneDrive and report what a sync would do (changes nothing)
    Plan(PlanArgs),
    /// Show what the background agent is doing, and what the local mirror and the last scan contain
    Status(ProfileArg),
    /// Pause the background agent (oneupd): nothing syncs until `oneup resume`
    /// (or the time given with --for). Kept if oneupd restarts
    Pause {
        /// Pause only this folder (and everything in it), until
        /// `oneup resume --folder` (saved in config.toml; the agent needn't run)
        #[arg(long, value_name = "PATH", conflicts_with_all = ["only", "for_minutes"])]
        folder: Option<String>,
        /// Pause only one direction: `local` (changes to this PC) or `remote`
        /// (changes to OneDrive)
        #[arg(long, value_enum)]
        only: Option<DirectionArg>,
        /// Resume by itself after this long (e.g. 30m, 2h, 1d)
        #[arg(long = "for", value_name = "DURATION", value_parser = agent::parse_minutes)]
        for_minutes: Option<u64>,
    },
    /// Resume the background agent after `oneup pause`
    Resume {
        /// Resume a folder paused with `oneup pause --folder`
        #[arg(long, value_name = "PATH", conflicts_with = "only")]
        folder: Option<String>,
        /// Resume only one direction (`local` or `remote`)
        #[arg(long, value_enum)]
        only: Option<DirectionArg>,
    },
    /// Go ahead with a batch that tripped the background agent's safety stop
    /// (the code it showed; see `oneup status`)
    Approve {
        #[arg(value_parser = parse_code)]
        code: u64,
    },
    /// Keep a batch of removed files in the version store although it doesn't
    /// fit (the code the background agent showed)
    Keep {
        #[arg(value_parser = parse_code)]
        code: u64,
    },
    /// Remove a batch of files that doesn't fit in the version store from
    /// this PC without a local copy (the code the background agent showed)
    Discard {
        #[arg(value_parser = parse_code)]
        code: u64,
    },
    /// Have the background agent check both sides for changes now; with a
    /// path, transfer what's there before anything else
    SyncNow {
        /// A file or folder (full path, or inside the synced folder)
        path: Option<String>,
    },
    /// Start the background agent (oneupd) without a window; its output goes
    /// to oneupd.log in the data folder
    Start,
    /// Stop the background agent after its current step
    Stop,
    /// Show OneUpDrive's icon in the notification area (`oneup start` shows it
    /// too, unless `oneup settings --tray-icon off`)
    Tray,
    /// Start the background agent when you sign in to Windows: `on` or `off`
    /// (without either, shows whether it does)
    Autostart {
        #[arg(value_enum)]
        state: Option<Toggle>,
    },
    /// Show or change what syncs: presets, your own rules (config.toml)
    Rules {
        /// What happens to OneDrive's copies of excluded items (excluding
        /// always stops uploads): pull (the default: bring them and their
        /// changes here), keep (leave them alone, not brought here) or prune
        /// (bring them here, then remove them there automatically)
        #[arg(long, value_name = "MODE")]
        cloud: Option<String>,
        /// Turn a preset on (dev, temp, os-junk)
        #[arg(long, value_name = "PRESET")]
        on: Vec<String>,
        /// Turn a preset off
        #[arg(long, value_name = "PRESET")]
        off: Vec<String>,
        /// Add a rule, gitignore style, relative to the synced folder (e.g. "*.iso", "/Downloads/")
        #[arg(long, value_name = "RULE", allow_hyphen_values = true)]
        add: Vec<String>,
        /// Remove one of your rules
        #[arg(long, value_name = "RULE", allow_hyphen_values = true)]
        remove: Vec<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Show or change how the background agent uses this PC: bandwidth
    /// caps, memory, priority (config.toml [agent])
    Settings {
        /// Upload cap in KiB/s (0 = none)
        #[arg(long, value_name = "KIB_PER_S")]
        upload_kib: Option<u64>,
        /// Download cap in KiB/s (0 = none)
        #[arg(long, value_name = "KIB_PER_S")]
        download_kib: Option<u64>,
        /// How much memory the agent may use for caching
        #[arg(long, value_enum)]
        memory: Option<MemoryProfile>,
        /// Run at background priority so your programs come first (on by default)
        #[arg(long, value_enum)]
        background: Option<Toggle>,
        /// Pause by itself on a metered connection (on by default)
        #[arg(long, value_enum)]
        pause_on_metered: Option<Toggle>,
        /// Pause by itself while battery saver is on (on by default)
        #[arg(long, value_enum)]
        pause_on_battery_saver: Option<Toggle>,
        /// Pause by itself while a game, a presentation or another full-screen program runs (on by default)
        #[arg(long, value_enum)]
        pause_on_full_screen: Option<Toggle>,
        /// Pause by itself while a video plays full screen, in a browser or a video player (on by default)
        #[arg(long, value_enum)]
        pause_on_full_screen_video: Option<Toggle>,
        /// Don't sync during these hours, local time (e.g. 22:00-07:00), or `off`
        #[arg(long, value_name = "FROM-TO")]
        quiet_hours: Option<String>,
        /// Apply the bandwidth caps only in these hours (e.g. 09:00-18:00), or `always`
        #[arg(long, value_name = "FROM-TO")]
        cap_hours: Option<String>,
        /// Show OneUpDrive's icon in the notification area when the agent starts (on by default)
        #[arg(long, value_enum)]
        tray_icon: Option<Toggle>,
        /// Hold the uploads of files that keep changing until they're left alone (on by default)
        #[arg(long, value_enum)]
        hold_busy_files: Option<Toggle>,
        /// Changes within --busy-window-s that make a file busy (default 2)
        #[arg(long, value_name = "N")]
        busy_changes: Option<u32>,
        /// The window those changes must fall in, in seconds (default 5)
        #[arg(long, value_name = "SECONDS")]
        busy_window_s: Option<u64>,
        /// How long a busy file must go unchanged before it's sent, in seconds (default 15)
        #[arg(long, value_name = "SECONDS")]
        busy_quiet_s: Option<u64>,
        /// The longest a busy file's upload waits, in seconds (default 600)
        #[arg(long, value_name = "SECONDS")]
        busy_max_s: Option<u64>,
        /// Compact the database when the agent starts, if deleted rows left most of it empty (on by default)
        #[arg(long, value_enum)]
        compact_database: Option<Toggle>,
        /// Developers only: show and allow experimental options (off by default)
        #[arg(long, value_enum, hide = true)]
        experimental: Option<Toggle>,
    },
    /// Show or change freeing up space by itself (with Files On-Demand)
    Space {
        /// Make files online-only once they haven't been used for this many
        /// days (`off`: never)
        #[arg(long, value_name = "DAYS|off", value_parser = parse_guard)]
        free_unused_days: Option<usize>,
        /// When the disk runs low, make the files unused for longest
        /// online-only until there's room again
        #[arg(long, value_enum)]
        free_when_low: Option<Toggle>,
        /// What "low" means: less than this share of the disk free (default 10)
        #[arg(long, value_name = "PERCENT")]
        low_disk_percent: Option<u32>,
    },
    /// Show which programs changed and opened files in your synced folder
    Apps {
        /// Over the last this many days (1 to 90)
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Instead: the files programs used lately, newest first; only this
        /// program's if given (as listed, e.g. Word, or its WINWORD.EXE)
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        files: Option<String>,
    },
    /// List the largest files in your OneDrive, and whether each is on this PC
    Largest {
        /// How many
        #[arg(long, default_value_t = 10)]
        count: usize,
    },
    /// Show or change the safety stops: when sync stops and asks you first
    Safety {
        /// Stop when a round would delete, replace or move more than this
        /// many items on one side (default 200)
        #[arg(long, value_name = "N")]
        max_changes: Option<usize>,
        /// ...or more than this share of all synced items, in percent
        /// (default 10; small changes of 20 items or fewer never stop)
        #[arg(long, value_name = "PERCENT")]
        max_percent: Option<u32>,
        /// Stop when more than this many edited files no longer look like
        /// their type, as when ransomware encrypts them (default 20; `off`)
        #[arg(long, value_name = "N|off", value_parser = parse_guard)]
        content_guard: Option<usize>,
    },
    /// Show or change what to send or fetch first, and last (rules as for `oneup rules`)
    Priority {
        /// Transfer what this rule matches first (e.g. "/Work/", "*.docx")
        #[arg(long, value_name = "RULE", allow_hyphen_values = true)]
        first: Vec<String>,
        /// Transfer what this rule matches last (e.g. "*.iso")
        #[arg(long, value_name = "RULE", allow_hyphen_values = true)]
        last: Vec<String>,
        /// Remove a priority rule
        #[arg(long, value_name = "RULE", allow_hyphen_values = true)]
        remove: Vec<String>,
    },
    /// Why a file or folder does or doesn't sync
    Explain {
        /// A full path, or one inside the synced folder
        path: String,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Show what sync did recently (oldest first, newest last), for
    /// everything or one file or folder
    Activity {
        /// Only this file or folder, as a path inside the synced folders
        /// (e.g. docs\report.docx)
        path: Option<String>,
        /// How many entries to show at most
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// List what can't sync until you do something about it (hard links,
    /// names OneDrive refuses, files over its size limit), or resolve it
    Unsynced {
        #[command(subcommand)]
        cmd: Option<UnsyncedCmd>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Check the state database's internal consistency
    Verify(ProfileArg),
    /// Show where the initial enumeration spent its downloads (paths are shown locally only)
    Diagnose {
        /// How many units to list
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Two-way sync of a folder on this PC with a OneDrive folder
    Sync(SyncArgs),
    /// List what sync kept in the version store (replaced or removed files),
    /// or remove old items from it
    Versions {
        /// Remove items kept more than this many days ago
        #[arg(long)]
        older_than_days: Option<u64>,
        /// Remove the oldest items until the store is under this size (GiB)
        #[arg(long)]
        max_gib: Option<f64>,
        /// Actually remove (without it, only shows what would go)
        #[arg(long)]
        yes: bool,
        /// Automatic trimming: never remove items younger than this many days [default: 30]
        #[arg(long, value_name = "DAYS")]
        set_min_days: Option<u32>,
        /// Automatic trimming: size limit in GiB, 0 for none [default: 5]
        #[arg(long, value_name = "GIB")]
        set_max_gib: Option<f64>,
        /// Automatic trimming: size limit as a % of the disk, 0 for none [default: 2].
        /// With both limits at 0, nothing is ever trimmed automatically
        #[arg(long, value_name = "PERCENT")]
        set_max_percent: Option<f64>,
        /// When new items don't fit, remove the oldest kept items to make
        /// room, whatever their age [default: off]
        #[arg(long, value_enum, value_name = "ON|OFF")]
        set_rotate: Option<Toggle>,
        /// When removed files don't fit in the store: `hold` leaves them on
        /// this PC until you decide, `keep` keeps them anyway and tells you
        /// [default: hold]
        #[arg(long, value_enum, value_name = "HOLD|KEEP")]
        set_overflow: Option<OverflowChoice>,
        /// Also keep files replaced by a newer version from OneDrive
        /// (OneDrive's version history has them anyway) [default: off]
        #[arg(long, value_enum, value_name = "ON|OFF")]
        set_keep_replaced: Option<Toggle>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// Put an item from the version store back into the synced folder (never
    /// replaces anything: a taken name gets " (restored N)")
    Restore {
        /// The item's ID, from `oneup versions`
        id: String,
        /// Where to put it, relative to the synced folder [default: where it was]
        #[arg(long)]
        to: Option<String>,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// A file's versions in OneDrive; open a copy of one, or make one the
    /// current version again (the replaced one stays in the history)
    History {
        /// A full path in a synced folder
        path: PathBuf,
        /// Save a copy of this version (its number from the list) outside the
        /// synced folder and open it
        #[arg(long, value_name = "N", conflicts_with = "restore")]
        open: Option<usize>,
        /// Make this version (its number from the list) the current one
        #[arg(long, value_name = "N")]
        restore: Option<usize>,
    },
    /// Open a Word, Excel or PowerPoint file from OneDrive, so Office saves
    /// as you work (AutoSave) and others can edit with you; this PC gets
    /// each saved version by syncing. Office must be signed in with the
    /// same Microsoft account
    Office {
        path: PathBuf,
        /// Only print the address Office would open
        #[arg(long)]
        print: bool,
    },
    /// Leave OneDrive folders to the OneDrive app, so Office's AutoSave works
    /// there when you double-click a file: OneUpDrive leaves them alone.
    /// Without options, shows what's left to it and how to set it up
    OnedriveApp {
        /// A OneDrive folder to leave to the OneDrive app (e.g. /Office)
        #[arg(long, value_name = "PATH")]
        add: Vec<String>,
        /// Stop leaving this folder to the OneDrive app
        #[arg(long, value_name = "PATH")]
        remove: Vec<String>,
        /// Also leave the files at the top of your OneDrive to it (it always
        /// syncs those)
        #[arg(long, value_enum)]
        top_files: Option<Toggle>,
    },
    /// Copy a sharing link for a file or folder: anyone with the link can
    /// view it (`--edit`: also change it)
    Share {
        path: PathBuf,
        #[arg(long)]
        edit: bool,
    },
    /// Files On-Demand: always keep this file or folder on this PC
    Pin {
        /// A full path, or one inside the synced folder
        path: String,
    },
    /// Files On-Demand: make this file or folder online-only, freeing its space
    Free {
        /// A full path, or one inside the synced folder
        path: String,
    },
    /// Files On-Demand for the synced folder: `on` (files can be online-only,
    /// downloaded when opened), `off`, or show the state. Stop the agent first
    OnDemand {
        #[arg(value_enum)]
        state: Option<Toggle>,
    },
    /// Moving over from the OneDrive app
    #[command(subcommand)]
    Migrate(MigrateCmd),
    /// Act on OneDrive's copies of what your rules exclude
    #[command(subcommand)]
    Cloud(CloudCmd),
    /// Cryptomator vaults in a local folder (preview: for checking
    /// compatibility; never changes or removes what exists)
    #[command(subcommand)]
    Vault(VaultCmd),
    /// More folder pairs under this sign-in: any OneDrive folder, a folder
    /// shared with you, or Windows' own folders (PC folder backup)
    #[command(subcommand)]
    Pair(PairCmd),
    /// Show or change updating: installing by itself, preview versions;
    /// `--check` looks for a new version now (the icon in the taskbar
    /// corner installs it)
    Updates {
        #[arg(long, value_enum)]
        auto: Option<Toggle>,
        #[arg(long, value_parser = ["stable", "preview"])]
        channel: Option<String>,
        #[arg(long)]
        check: bool,
    },
    /// List broken shortcuts to OneDrive files (Desktop, Start menu,
    /// taskbar); `--fix` updates them to the synced folder, `--undo`
    /// restores them
    Shortcuts {
        /// Update them
        #[arg(long, conflicts_with = "undo")]
        fix: bool,
        /// Restore the shortcuts the last --fix updated
        #[arg(long)]
        undo: bool,
        /// The OneDrive app's folder, if it was somewhere else
        #[arg(long)]
        from: Option<PathBuf>,
        /// Print what was found as JSON
        #[arg(long)]
        json: bool,
    },
    /// The Microsoft accounts on this PC, each with its own data folder;
    /// `add NAME` makes one (sign in from the settings window it opens)
    Accounts {
        #[command(subcommand)]
        add: Option<AccountsCmd>,
    },
    /// Stop syncing the main folder for good (as `oneup pair remove` does for
    /// a pair): it keeps what it holds, OneDrive too; online-only files
    /// leave it. The sign-in, settings and other pairs stay
    Unpair {
        #[arg(long)]
        yes: bool,
    },
    /// Which way changes travel: `both` (the default) or `backup` (this PC →
    /// OneDrive only: deleting here never deletes there, nothing from
    /// OneDrive changes this PC). Without either, shows it. Stop the agent
    /// first; `--data-dir` picks a folder pair's
    Flow {
        #[arg(value_enum)]
        to: Option<FlowArg>,
    },
    /// Developers only: exercise every cloud write against the test account,
    /// inside a temporary folder that is removed afterwards
    #[command(hide = true)]
    CloudCheck,
    /// Developers only: change the test account's OneDrive directly (as
    /// another device would), to try `sync` in the cloud-to-PC direction
    #[command(hide = true, subcommand)]
    DevRemote(dev_remote::DevRemote),
    /// Developers only: whether this program has OneUpDrive's package
    /// identity (Explorer integration)
    /// What Explorer's OneUpDrive commands run (explain, activity, first,
    /// online) for a file or folder
    #[command(hide = true)]
    ShellVerb { verb: String, path: PathBuf },
    /// Developers only: the settings window's file tree for a folder (JSON)
    #[command(hide = true)]
    DevTree {
        /// The folder, relative to the synced folder
        #[arg(default_value = "")]
        rel: String,
        /// Search names instead
        #[arg(long)]
        find: Option<String>,
    },
    /// What Office asks about a file before opening it from OneDrive (for
    /// the shell extension's experimental Office integration)
    #[command(hide = true)]
    OfficeProps { path: PathBuf },
    /// Experimental: tell Office that files in OneUpDrive's folders are in
    /// OneDrive, so double-clicking one may open it with AutoSave: `on`,
    /// `off`, or show the state. Hidden; `on` needs `settings --experimental on`
    #[command(hide = true)]
    OfficeIntegration {
        #[arg(value_enum)]
        state: Option<Toggle>,
    },
    /// Let OneUpDrive open Office files when you make it their default
    /// program: in its folders from OneDrive with AutoSave, elsewhere in
    /// Office as usual. `on` offers it to Windows (then pick it in Default
    /// apps), `off` removes it, nothing shows the state
    OfficeOpener {
        #[arg(value_enum)]
        state: Option<Toggle>,
        /// How this file would open, and why
        #[arg(long, value_name = "FILE", conflicts_with = "state")]
        explain: Option<PathBuf>,
    },
    #[command(hide = true)]
    DevIdentity {
        /// Register the identity package unsigned for this build (Developer Mode)
        #[arg(long)]
        register: bool,
        /// Remove it
        #[arg(long, conflicts_with = "register")]
        remove: bool,
    },
    /// Brings the synced folder's Explorer entry up to date with these
    /// programs (run by the agent at start: registering needs the package
    /// identity, which the agent runs without)
    #[command(hide = true)]
    RefreshExplorerEntry {
        /// First register the folder again if Windows lost its Files
        /// On-Demand registration, when that's safe (the agent's start,
        /// before it connects)
        #[arg(long)]
        repair: bool,
    },
    /// Developers only: register (or with --remove, unregister) a temp
    /// folder with the Windows shell, to try Explorer integration
    #[command(hide = true)]
    DevShellRoot {
        folder: PathBuf,
        #[arg(long)]
        remove: bool,
    },
    /// Developers only: measure upload speeds on the test account, inside a
    /// temporary folder that is removed afterwards
    #[command(hide = true)]
    UploadBench {
        /// Size of the file sent through upload sessions, MiB
        #[arg(long, default_value_t = 64)]
        session_mib: usize,
        /// Repeat each measurement this many times
        #[arg(long, default_value_t = 1)]
        rounds: u32,
        /// Instead: upload files of this many MiB the way sync does
        /// (adaptive fragments), for each memory profile
        #[arg(long)]
        smart_mib: Option<usize>,
    },
}

#[derive(clap::Args)]
struct LoginArgs {
    /// Developers only: sign in a dedicated *test* account with write access.
    /// Needs its own --data-dir (never the default one) and shows Microsoft's
    /// account picker, so your usual account stays signed in everywhere
    #[arg(long)]
    test_account: bool,
    /// Also allow syncing folders others shared with the account (a broader
    /// permission, asked for only with this)
    #[arg(long)]
    shared_folders: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum FlowArg {
    Both,
    Backup,
}

#[derive(Subcommand)]
enum PairCmd {
    /// Sync a OneDrive folder with a folder on this PC (both may already hold
    /// files: equal files are matched, different ones both kept)
    Add {
        /// The folder in OneDrive (created if missing), e.g. /Photos
        remote: String,
        /// The folder on this PC (created if missing), outside every synced folder
        folder: PathBuf,
        /// One way, this PC → OneDrive (see `oneup flow`)
        #[arg(long)]
        backup: bool,
        /// Files from OneDrive arrive online-only (not with --backup)
        #[arg(long, conflicts_with = "backup")]
        on_demand: bool,
        /// Don't start syncing it now
        #[arg(long)]
        no_start: bool,
    },
    /// Sync a folder someone shared with you (as `oneup pair shared` lists it)
    AddShared {
        name: String,
        folder: PathBuf,
        #[arg(long)]
        backup: bool,
        #[arg(long, conflicts_with = "backup")]
        on_demand: bool,
        #[arg(long)]
        no_start: bool,
    },
    /// PC folder backup: back Windows' own folders (Desktop, Documents,
    /// Pictures, Music, Videos) up to OneDrive folders of the same name,
    /// where they are (nothing is moved)
    BackupPc {
        #[arg(required = true)]
        folders: Vec<String>,
        /// Sync both ways instead of backing up
        #[arg(long)]
        two_way: bool,
        #[arg(long)]
        no_start: bool,
    },
    /// The extra folder pairs
    List,
    /// The folders others shared with this account
    Shared,
    /// Stop syncing a pair for good (both folders keep what they hold)
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Args)]
struct MirrorArgs {
    /// Folder levels always listed before splitting the work into parallel units
    #[arg(long, default_value_t = 2)]
    depth: u32,
    /// Keep listing large folders (see --split-size-mib) down to this depth, so
    /// junk inside them is skipped instead of downloaded. Not used when a
    /// folder scan or previous mirror shows where the junk is (see --no-hints)
    #[arg(long, default_value_t = 6)]
    max_depth: u32,
    /// Folders at least this large (MiB, recursive) keep being listed below
    /// --depth (when no hints are available)
    #[arg(long, default_value_t = 256)]
    split_size_mib: i64,
    /// Upper limit on parallel requests (adapts down automatically if OneDrive throttles)
    #[arg(long, default_value_t = 16)]
    max_concurrency: usize,
    /// Which folders to leave out. Required on the first run, then remembered.
    /// `dev` skips dependency/build folders (node_modules, .venv, __pycache__, ...);
    /// `none` mirrors everything.
    #[arg(long, value_enum)]
    exclude: Option<ExcludePreset>,
    /// Throw away the local mirror and start from scratch
    #[arg(long)]
    rebuild: bool,
    /// Don't steer the first enumeration with the last folder scan or the
    /// previous mirror; use only --depth/--max-depth/--split-size-mib
    #[arg(long)]
    no_hints: bool,
    #[command(flatten)]
    profile: ProfileArg,
}

#[derive(clap::Args)]
struct ScanArgs {
    /// The folder to scan, e.g. your existing OneDrive folder
    path: PathBuf,
    /// Which folders to leave out (same choice as `mirror`, and remembered for both).
    /// Required the first time either command runs.
    #[arg(long, value_enum)]
    exclude: Option<ExcludePreset>,
    #[command(flatten)]
    profile: ProfileArg,
}

#[derive(clap::Args)]
struct PlanArgs {
    /// Also compare the contents of files that have the same name and size on
    /// both sides, by reading them. Files that are cloud placeholders (e.g.
    /// managed by the OneDrive app) are never read, so nothing is downloaded
    #[arg(long)]
    verify: bool,
    /// How many example paths to show per category
    #[arg(long, default_value_t = 5)]
    examples: usize,
    #[command(flatten)]
    profile: ProfileArg,
}

#[derive(clap::Args)]
struct SyncArgs {
    /// The folder on this PC
    local: PathBuf,
    /// The OneDrive folder, as a path from the top of your OneDrive (e.g.
    /// /Sync-Test). Created if missing
    #[arg(long)]
    remote: String,
    /// Show what the first round would do, and change nothing
    #[arg(long)]
    dry_run: bool,
    /// With --dry-run: list at most this many changes (the totals always)
    #[arg(long, value_name = "N", requires = "dry_run")]
    limit: Option<usize>,
    /// Set up the pair (scan, list, open the folder) and leave the syncing
    /// to the background agent (`oneup start`)
    #[arg(long, conflicts_with = "dry_run")]
    pair_only: bool,
    /// Use Files On-Demand from the start: OneDrive's files arrive
    /// online-only (see `oneup on-demand`)
    #[arg(long, conflicts_with = "dry_run")]
    on_demand: bool,
    /// Stop after this many rounds even if work remains
    #[arg(long, default_value_t = 20)]
    max_rounds: usize,
    /// Go ahead with a batch of deletions/overwrites/moves that triggered a
    /// safety stop. Takes the code the stop printed; it approves exactly that
    /// batch, nothing more
    #[arg(long, value_name = "CODE", value_parser = parse_code)]
    approve: Vec<u64>,
    /// Keep a batch of removed files in the version store although it
    /// doesn't fit (the code the "Version store full" message printed)
    #[arg(long, value_name = "CODE", value_parser = parse_code)]
    keep: Vec<u64>,
    /// Remove a batch of files that doesn't fit in the version store from
    /// this PC without a local copy (the code the message printed)
    #[arg(long, value_name = "CODE", value_parser = parse_code)]
    discard: Vec<u64>,
    /// Which folders to leave out (same choice as `mirror` and `scan`)
    #[arg(long, value_enum)]
    exclude: Option<ExcludePreset>,
    #[command(flatten)]
    profile: ProfileArg,
}

#[derive(Clone, Copy, ValueEnum)]
enum DirectionArg {
    Local,
    Remote,
}

fn direction(only: Option<DirectionArg>) -> oneup_ipc::Direction {
    match only {
        None => oneup_ipc::Direction::Both,
        Some(DirectionArg::Local) => oneup_ipc::Direction::Local,
        Some(DirectionArg::Remote) => oneup_ipc::Direction::Remote,
    }
}

#[derive(Subcommand)]
enum AccountsCmd {
    /// A new account, with its own data folder, called NAME (like Work)
    Add { name: String },
}

#[derive(Subcommand)]
enum MigrateCmd {
    /// What moving over would involve (read-only: changes, opens and downloads nothing)
    Check {
        /// Print the result as JSON (for the settings window)
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
    /// When the OneDrive app's unlinking is stuck ("Signing out" that never
    /// ends): gets it finished. First restarts the app, then tries the app's
    /// own reset, and as a last resort finishes it itself, after checking
    /// that every online-only file is in OneDrive (asks first)
    FinishUnlink {
        /// Only this step
        #[arg(long, value_enum)]
        step: Option<UnlinkStep>,
        /// Don't ask before OneUpDrive finishes the unlinking itself
        #[arg(long)]
        yes: bool,
        /// How long to wait for the app after restarting or resetting it
        #[arg(long, default_value_t = 3)]
        wait_minutes: u64,
        /// Print the result as JSON (for the settings window)
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        profile: ProfileArg,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum UnlinkStep {
    /// Restart the OneDrive app
    Restart,
    /// The OneDrive app's own reset (`OneDrive.exe /reset`)
    Reset,
    /// OneUpDrive finishes the unlinking itself
    Release,
}

#[derive(Subcommand)]
enum VaultCmd {
    /// Create a new vault in an empty (or new) folder
    Create { folder: PathBuf },
    /// List what's in the vault (or in PATH inside it)
    Ls {
        folder: PathBuf,
        path: Option<String>,
        /// Include everything below, too
        #[arg(short, long)]
        recursive: bool,
    },
    /// Decrypt the file at PATH in the vault to DEST (a new file)
    Get { folder: PathBuf, path: String, dest: PathBuf },
    /// Encrypt SRC into the vault as a new file at PATH
    Put { folder: PathBuf, src: PathBuf, path: String },
    /// Create a new folder at PATH in the vault
    Mkdir { folder: PathBuf, path: String },
    /// Read and verify every file in the vault
    Check { folder: PathBuf },
    /// Sync a vault in OneDrive with a folder on this PC, decrypted (test
    /// accounts only, for now). Files are online-only and decrypted when
    /// opened, unless --full-files
    Add {
        /// The vault's folder in OneDrive (the one with vault.cryptomator)
        remote: String,
        /// A new or empty folder on this PC, outside every synced folder
        folder: PathBuf,
        /// Keep every file decrypted on this PC instead of online-only
        #[arg(long)]
        full_files: bool,
        /// Remember the vault's keys on this PC (protected by your Windows
        /// sign-in), so it syncs without asking for the password
        #[arg(long)]
        remember: bool,
        /// Don't start syncing it now
        #[arg(long)]
        no_start: bool,
    },
    /// Create a new vault in OneDrive and sync it with a new or empty folder
    /// here (test accounts only, for now). Shows its recovery key
    New {
        /// A new or empty folder in OneDrive, e.g. /Private
        remote: String,
        /// A new or empty folder on this PC, outside every synced folder
        folder: PathBuf,
        #[arg(long)]
        full_files: bool,
        #[arg(long)]
        remember: bool,
        #[arg(long)]
        no_start: bool,
    },
    /// Show a synced vault's recovery key (asks for its password unless
    /// remembered)
    RecoveryKey { name: String },
    /// Set a new password for a synced vault, with the current password or
    /// the recovery key
    ChangePassword {
        name: String,
        /// Use the recovery key instead of the current password
        #[arg(long)]
        recovery_key: bool,
    },
    /// Unlock a synced vault with Windows Hello (face, fingerprint or PIN)
    /// instead of its password: `on`, `off`, or show whether it does
    Hello {
        name: String,
        #[arg(value_enum)]
        state: Option<Toggle>,
    },
    /// The vaults synced with folders on this PC
    List,
    /// Start syncing a vault (asks for its password unless remembered)
    Unlock {
        name: String,
        /// Remember the keys on this PC from now on
        #[arg(long)]
        remember: bool,
    },
    /// Stop syncing a vault; its keys leave memory
    Lock {
        name: String,
        /// Also forget remembered keys
        #[arg(long)]
        forget: bool,
    },
    /// Stop syncing a vault for good (the vault in OneDrive stays; so do
    /// the decrypted files on this PC that aren't online-only)
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum UnsyncedCmd {
    /// Resolve items `oneup unsynced` lists; without a choice, shows each
    /// item's choices. OneUpDrive carries it out in its next round
    Resolve(ResolveArgs),
}

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("choice")))]
struct ResolveArgs {
    /// Items as `oneup unsynced` lists them (or their full paths); a folder
    /// counts with what's in it
    paths: Vec<String>,
    /// Every listed item the choice fits
    #[arg(long, conflicts_with = "paths")]
    all: bool,
    /// Go ahead only if the choice concerns this many items, what's in
    /// chosen folders included (as it was shown; it may have changed since)
    #[arg(long, value_name = "N")]
    expect: Option<usize>,
    /// Gone from this PC: delete it in OneDrive too, into its recycle bin
    #[arg(long, group = "choice")]
    delete_in_onedrive: bool,
    /// Gone from this PC, in a backup pair: leave it in OneDrive
    #[arg(long, group = "choice")]
    keep_in_onedrive: bool,
    /// Gone from this PC: download it again from OneDrive
    #[arg(long, group = "choice")]
    restore: bool,
    /// Gone from OneDrive: delete it on this PC too, into its kept copies
    #[arg(long, group = "choice")]
    delete_here: bool,
    /// Gone from OneDrive: upload it again
    #[arg(long, group = "choice")]
    upload: bool,
    /// A name OneDrive refuses, or one differing only in case: rename it
    #[arg(long, group = "choice", value_name = "NAME")]
    rename: Option<String>,
    /// Leave it out of sync (adds a rule)
    #[arg(long, group = "choice")]
    exclude: bool,
    /// Go ahead with a deletion (without it, only shows what would be deleted)
    #[arg(long)]
    yes: bool,
    #[command(flatten)]
    profile: ProfileArg,
}

impl ResolveArgs {
    fn choice(&self) -> Option<oneup_app::resolve::Choice> {
        use oneup_app::resolve::Choice as C;
        [
            (self.delete_in_onedrive, C::DeleteInOnedrive),
            (self.keep_in_onedrive, C::KeepInOnedrive),
            (self.restore, C::Restore),
            (self.delete_here, C::DeleteHere),
            (self.upload, C::Upload),
            (self.rename.is_some(), C::Rename),
            (self.exclude, C::Exclude),
        ]
        .into_iter()
        .find_map(|(on, c)| on.then_some(c))
    }
}

#[derive(Subcommand)]
enum CloudCmd {
    /// List the items in OneDrive that your rules exclude; with --yes,
    /// remove them once they are on this PC (whatever is not is brought here
    /// first; into OneDrive's recycle bin; the copies on this PC stay)
    Prune {
        /// Remove them
        #[arg(long)]
        yes: bool,
        /// Keep them in OneDrive instead: they're brought here but never sent
        /// (`cloud_copies = "pull"`), and not offered for removal again
        #[arg(long, conflicts_with = "yes")]
        keep: bool,
        /// List what's inside one of them (a path as `oneup cloud prune` shows it)
        #[arg(long, value_name = "PATH", conflicts_with_all = ["yes", "keep"])]
        details: Option<String>,
        /// Which page of --details (100 entries each)
        #[arg(long, default_value_t = 1, requires = "details")]
        page: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Toggle {
    On,
    Off,
}

impl Toggle {
    fn on(self) -> bool {
        matches!(self, Toggle::On)
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum OverflowChoice {
    Hold,
    Keep,
}

fn shortcuts_cmd(dir: &Path, fix: bool, undo: bool, from: Option<&Path>, json: bool) -> Result<()> {
    use oneup_app::shortcuts;
    let n = |k: usize| format!("{k} shortcut{}", if k == 1 { "" } else { "s" });
    let skipped = |o: &shortcuts::Outcome| {
        for (link, why) in &o.skipped {
            println!("  skipped {} ({why})", link.display());
        }
    };
    if undo {
        let o = shortcuts::undo(dir)?;
        println!("Restored {}.", n(o.changed));
        skipped(&o);
        return Ok(());
    }
    let s = shortcuts::find(dir, from)?;
    if json && !fix {
        println!("{}", shortcuts::scan_json(dir, &s));
        return Ok(());
    }
    for f in &s.found {
        println!(
            "{:<10}  {}{}\n            → {}",
            f.place.label(),
            f.link.display(),
            if f.broken { "  (broken)" } else { "" },
            f.new_target.display()
        );
    }
    if s.unmatched > 0 {
        let verb = if s.unmatched == 1 { "leads" } else { "lead" };
        println!("{} {verb} to items not in the synced folder; not changed.", n(s.unmatched));
    }
    if s.found.is_empty() {
        println!("Nothing found.");
    } else if fix {
        let o = shortcuts::fix(dir, &s.found)?;
        println!("Updated {}. Undo: oneup shortcuts --undo", n(o.changed));
        skipped(&o);
    } else {
        println!("Update: oneup shortcuts --fix");
    }
    Ok(())
}

/// An approval code: the 16 hex digits a safety stop prints.
/// `oneup updates`: shows or changes `[updates]`; `check` looks for a new
/// version now, as the settings window's Check now does.
fn updates_cmd(dir: &Path, auto: Option<bool>, channel: Option<&str>, check: bool) -> Result<()> {
    let mut u = oneup_app::config::load_updates(dir)?;
    if auto.is_some() || channel.is_some() {
        if let Some(a) = auto {
            u.auto = a;
        }
        if let Some(c) = channel {
            u.channel = c.to_string();
        }
        oneup_app::config::save_updates(dir, &u)?;
    }
    println!(
        "Updates: {}; {}.",
        if u.auto { "installed by themselves" } else { "installed when you choose" },
        if u.channel == "preview" { "preview versions too" } else { "stable versions only" }
    );
    if check {
        let Some(um) = oneup_app::updates::manager(dir)? else {
            bail!(
                "this copy doesn't update itself (it wasn't installed with OneUpDrive's installer, or it has no release feed)"
            );
        };
        match oneup_app::updates::check(um)? {
            None => println!("This is the latest version."),
            Some(r) => {
                let told = oneup_sys::signal::raise(&oneup_app::updates::check_signal(dir)).unwrap_or(false);
                println!(
                    "Version {} is downloaded and checked. {}",
                    r.version,
                    if told {
                        "The OneUpDrive icon installs it."
                    } else {
                        "Install it from the OneUpDrive icon's menu."
                    }
                );
            }
        }
    }
    Ok(())
}

fn parse_code(s: &str) -> Result<u64, String> {
    u64::from_str_radix(s.trim(), 16).map_err(|_| "expected the 16-character code the safety stop printed".into())
}

#[derive(clap::Args)]
struct ProfileArg {
    /// How much memory OneUpDrive may use for caching
    #[arg(long, value_enum, default_value_t = MemoryProfile::Balanced)]
    memory_profile: MemoryProfile,
}

#[derive(Clone, Copy, ValueEnum)]
enum MemoryProfile {
    Minimal,
    Balanced,
    Performance,
}

impl MemoryProfile {
    /// As in config.toml.
    fn name(self) -> &'static str {
        match self {
            MemoryProfile::Minimal => "minimal",
            MemoryProfile::Balanced => "balanced",
            MemoryProfile::Performance => "performance",
        }
    }

    fn cache_kib(self) -> u32 {
        match self {
            MemoryProfile::Minimal => 4 * 1024,
            MemoryProfile::Balanced => 16 * 1024,
            MemoryProfile::Performance => 64 * 1024,
        }
    }

    /// Read buffer for hashing files (`plan --verify`).
    fn hash_buffer(self) -> usize {
        match self {
            MemoryProfile::Minimal => 256 * 1024,
            MemoryProfile::Balanced => 1024 * 1024,
            MemoryProfile::Performance => 4 * 1024 * 1024,
        }
    }
}

fn data_dir(cli: &Cli) -> Result<PathBuf> {
    match &cli.data_dir {
        Some(d) => Ok(d.clone()),
        None => test_account::default_dir(),
    }
}

fn db_path(cli: &Cli) -> Result<PathBuf> {
    Ok(data_dir(cli)?.join("state.db"))
}

fn open_store(cli: &Cli, profile: MemoryProfile) -> Result<Store> {
    setup::open_store(&data_dir(cli)?, profile.cache_kib())
}

/// The data folder's sign-in (see [`setup::auth`]).
fn auth(cli: &Cli, http: &reqwest::Client) -> Result<Arc<Auth>> {
    setup::auth(&data_dir(cli)?, cli.client_id.as_deref(), http)
}

fn http() -> Result<reqwest::Client> {
    setup::http()
}

/// The command-line tool's work is one large future (one arm per command);
/// Windows gives the main thread 1 MiB of stack, which a debug build of it
/// outgrows. So it runs on a thread with more, as do the runtime's workers.
const STACK: usize = 16 << 20;

fn main() -> Result<()> {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(STACK)
                .build()
                .context("starting the async runtime")?
                .block_on(run())
        })
        .context("starting the main thread")?
        .join()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the command panicked")))
}

async fn run() -> Result<()> {
    setup::tls();
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()))
        .with_writer(std::io::stderr)
        .init();

    if let Some(dir) = &cli.data_dir
        && oneup_app::vaultpair::is_child_dir(dir)
        && matches!(
            cli.cmd,
            Cmd::Login(_)
                | Cmd::Logout
                | Cmd::Mirror(_)
                | Cmd::Scan(_)
                | Cmd::Observe(_)
                | Cmd::Plan(_)
                | Cmd::Sync(_)
                | Cmd::OnDemand { .. }
                | Cmd::Cloud(_)
                | Cmd::Migrate(_)
                | Cmd::Vault(_)
        )
    {
        bail!(
            "that's a vault's or a folder pair's data folder; this command works on the main data folder \
             (see `oneup vault`, `oneup pair`)"
        );
    }
    match &cli.cmd {
        Cmd::Login(args) if args.test_account => login_test_account(&cli, args.shared_folders).await,
        Cmd::Login(args) => login(&cli, args.shared_folders).await,
        Cmd::Logout => {
            auth(&cli, &http()?)?.sign_out().await?;
            // Without its marker, the folder reverts to read-only.
            test_account::remove(&data_dir(&cli)?)?;
            match std::fs::remove_file(data_dir(&cli)?.join(oneup_app::accounts::SIGNED_IN_FILE)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            println!("Signed out. The local mirror was kept; delete {} to remove it.", data_dir(&cli)?.display());
            Ok(())
        }
        Cmd::Mirror(args) => mirror(&cli, args).await,
        Cmd::Scan(args) => scan(&cli, args).await,
        Cmd::Observe(p) => observe(&cli, p.memory_profile).await,
        Cmd::Plan(args) => plan(&cli, args).await,
        Cmd::Status(p) => {
            agent::status(&data_dir(&cli)?).await?;
            status(&cli, p.memory_profile)
        }
        Cmd::Pause { folder: Some(f), .. } => {
            rules::pause_folder(&data_dir(&cli)?, &open_store(&cli, MemoryProfile::Balanced)?, f, true).await
        }
        Cmd::Resume { folder: Some(f), .. } => {
            rules::pause_folder(&data_dir(&cli)?, &open_store(&cli, MemoryProfile::Balanced)?, f, false).await
        }
        Cmd::Pause { only, for_minutes, .. } => agent::pause(&data_dir(&cli)?, direction(*only), *for_minutes).await,
        Cmd::Resume { only, .. } => agent::resume(&data_dir(&cli)?, direction(*only)).await,
        Cmd::Approve { code } => agent::answer(&data_dir(&cli)?, *code, oneup_ipc::Answer::Approve).await,
        Cmd::Keep { code } => agent::answer(&data_dir(&cli)?, *code, oneup_ipc::Answer::Keep).await,
        Cmd::Discard { code } => agent::answer(&data_dir(&cli)?, *code, oneup_ipc::Answer::Discard).await,
        Cmd::SyncNow { path } => {
            let first = match path {
                Some(p) => {
                    let store = open_store(&cli, MemoryProfile::Balanced)?;
                    let pair = sync::pairing(&store)?;
                    let rel = activity::normalize(&activity::within(&pair.local, p)?);
                    Some(rel.trim_start_matches('/').to_string()).filter(|r| !r.is_empty())
                }
                None => None,
            };
            agent::sync_now(&data_dir(&cli)?, first).await
        }
        Cmd::RefreshExplorerEntry { repair } => {
            let store = open_store(&cli, MemoryProfile::Minimal)?;
            let pair = sync::pairing(&store)?;
            let Some(token) = store.read(|o| o.meta_get("sync.token"))? else { return Ok(()) };
            if *repair {
                use oneup_app::syncroot::Repair;
                match oneup_app::syncroot::repair(&pair.local, &token, &store)? {
                    Repair::NotNeeded => {}
                    Repair::Repaired => println!("the folder was registered for Files On-Demand again"),
                    Repair::Refused(why) => bail!("the folder wasn't registered again: {why}"),
                }
            }
            if oneup_app::syncroot::refresh(&pair.local, &token)? {
                println!("the folder's Explorer entry was brought up to date");
            }
            Ok(())
        }
        Cmd::DevShellRoot { folder, remove } => {
            let sid = oneup_sys::pipe::current_user_sid()?;
            let id = oneup_cfapi::shell::root_id(&sid, "devtoken00000000");
            if *remove {
                oneup_cfapi::shell::unregister(&id)?;
                println!("unregistered {id}");
            } else {
                std::fs::create_dir_all(folder)?;
                let icon = format!("{},0", oneup_app::launch::sibling("oneupd.exe")?.display());
                oneup_cfapi::shell::register(folder, &id, "devtoken00000000", "OneUpDrive (test)", &icon, "0.1.0")?;
                println!("registered {id}");
                println!("Cloud Files sees: {:?}", oneup_cfapi::provider::registration(folder));
            }
            Ok(())
        }
        Cmd::ShellVerb { verb, path } => shellverb::run(verb, path).await,
        Cmd::DevIdentity { register, remove } => {
            if *register {
                use oneup_app::identity::DevRegistration;
                match oneup_app::identity::register_dev()? {
                    DevRegistration::Done(at) => {
                        println!("Registered for the programs in {} (new processes get identity).", at.display())
                    }
                    DevRegistration::Pending(at) => println!(
                        "Re-registering for the programs in {} once every OneUpDrive program has exited \
                         (stop the agents and the tray; check with `oneup dev-identity`).",
                        at.display()
                    ),
                }
            } else if *remove {
                println!("{}", if oneup_app::identity::remove()? { "Removed." } else { "Wasn't registered." });
            } else {
                println!("{}", oneup_sys::identity::package_name().unwrap_or_else(|| "no package identity".into()));
            }
            Ok(())
        }
        Cmd::Start => {
            let dir = data_dir(&cli)?;
            agent::start(&dir).await?;
            for (what, why) in oneup_app::launch::start_others(&dir).await {
                println!("{what}: {why}");
            }
            Ok(())
        }
        Cmd::Stop => {
            let dir = data_dir(&cli)?;
            for p in oneup_app::pairs::list(&dir) {
                if agent::current(&p.dir).await.ok().flatten().is_some() {
                    println!("Folder pair {}:", p.name);
                    agent::stop(&p.dir).await?;
                }
            }
            agent::stop(&dir).await
        }
        Cmd::Tray => {
            oneup_app::launch::start_tray(&data_dir(&cli)?)?;
            println!("The OneUpDrive icon is in the notification area (it may be in the overflow, under ^).");
            Ok(())
        }
        Cmd::Autostart { state } => agent::autostart(&data_dir(&cli)?, state.map(Toggle::on)),
        Cmd::Rules { cloud, on, off, add, remove, profile } => {
            let changes = rules::Changes {
                cloud: cloud.clone(),
                on: on.clone(),
                off: off.clone(),
                add: add.clone(),
                remove: remove.clone(),
            };
            rules::run(&data_dir(&cli)?, &open_store(&cli, profile.memory_profile)?, &changes).await
        }
        Cmd::Settings {
            upload_kib,
            download_kib,
            memory,
            background,
            pause_on_metered,
            pause_on_battery_saver,
            pause_on_full_screen,
            pause_on_full_screen_video,
            quiet_hours,
            cap_hours,
            tray_icon,
            hold_busy_files,
            busy_changes,
            busy_window_s,
            busy_quiet_s,
            busy_max_s,
            compact_database,
            experimental,
        } => {
            let changes = settings::Changes {
                upload_kib: *upload_kib,
                download_kib: *download_kib,
                memory: memory.map(MemoryProfile::name),
                background: background.map(Toggle::on),
                pause_on_metered: pause_on_metered.map(Toggle::on),
                pause_on_battery_saver: pause_on_battery_saver.map(Toggle::on),
                pause_on_full_screen: pause_on_full_screen.map(Toggle::on),
                pause_on_full_screen_video: pause_on_full_screen_video.map(Toggle::on),
                quiet_hours: quiet_hours.clone(),
                cap_hours: cap_hours.clone(),
                tray_icon: tray_icon.map(Toggle::on),
                hold_busy_files: hold_busy_files.map(Toggle::on),
                busy_changes: *busy_changes,
                busy_window_s: *busy_window_s,
                busy_quiet_s: *busy_quiet_s,
                busy_max_s: *busy_max_s,
                compact_database: compact_database.map(Toggle::on),
                experimental: experimental.map(Toggle::on),
            };
            settings::run(&data_dir(&cli)?, &changes).await
        }
        Cmd::Space { free_unused_days, free_when_low, low_disk_percent } => {
            let dir = data_dir(&cli)?;
            let mut s = oneup_app::config::load_space(&dir)?;
            let before = s.clone();
            if let Some(d) = free_unused_days {
                s.free_unused_days = u32::try_from(*d).unwrap_or(u32::MAX);
            }
            if let Some(t) = free_when_low {
                s.free_when_low = t.on();
            }
            if let Some(p) = low_disk_percent {
                if !(1..=50).contains(p) {
                    anyhow::bail!("--low-disk-percent must be between 1 and 50");
                }
                s.low_disk_percent = *p;
            }
            if s != before {
                oneup_app::config::save_space(&dir, &s)?;
                agent::nudge(&dir).await;
            }
            match s.free_unused_days {
                0 => println!("Files not used for a while: kept on this PC."),
                d => println!("Files not used for {d} days: made online-only (their content stays in OneDrive)."),
            }
            if s.free_when_low {
                println!(
                    "When less than {}% of the disk is free: the files unused for longest are made online-only.",
                    s.low_disk_percent
                );
            } else {
                println!("When the disk runs low: nothing happens by itself.");
            }
            println!(
                "Files set to \"Always keep on this device\" are never freed. Needs Files On-Demand (`oneup on-demand on`)."
            );
            Ok(())
        }
        Cmd::Apps { files: Some(which), .. } => {
            let dir = data_dir(&cli)?;
            // A program as listed (its friendly name) or by its file name.
            let exe = if which.is_empty() {
                None
            } else {
                let s = oneup_app::apps::summary(&dir, 90)?;
                let apps = s["apps"].as_array().cloned().unwrap_or_default();
                let found = apps.iter().find(|a| {
                    a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(which))
                        || a["exe"].as_str().is_some_and(|e| e.eq_ignore_ascii_case(which))
                });
                Some(found.and_then(|a| a["exe"].as_str()).unwrap_or(which).to_string())
            };
            let list = oneup_app::apps::recent_files(&dir, exe.as_deref(), 100)?;
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("No files noted yet.");
            }
            let offset = activity::local_offset();
            for f in list.iter().rev() {
                println!(
                    "{}  {:<8} {:<28} {}",
                    activity::when(f["at_ms"].as_i64().unwrap_or(0), offset),
                    f["kind"].as_str().unwrap_or_default(),
                    f["name"].as_str().unwrap_or_default(),
                    f["path"].as_str().unwrap_or_default()
                );
            }
            Ok(())
        }
        Cmd::Apps { days, .. } => {
            let s = oneup_app::apps::summary(&data_dir(&cli)?, *days)?;
            let apps = s["apps"].as_array().cloned().unwrap_or_default();
            if apps.is_empty() {
                println!("No program has changed or opened files in the synced folder in that time.");
            } else {
                println!("{:<36} {:>8} {:>8} {:>10}", "Program", "changed", "opened", "size");
                for a in apps {
                    println!(
                        "{:<36} {:>8} {:>8} {:>10}",
                        a["name"].as_str().unwrap_or_default(),
                        a["changed"].as_i64().unwrap_or(0),
                        a["opened"].as_i64().unwrap_or(0),
                        oneup_app::versions::size(a["bytes"].as_u64().unwrap_or(0)),
                    );
                }
            }
            if !oneup_app::config::load_agent(&data_dir(&cli)?)?.track_apps {
                println!(
                    "(Noting programs is off: `[agent] track_apps = true` in config.toml, or the settings window, turns it on.)"
                );
            }
            Ok(())
        }
        Cmd::Largest { count } => {
            let size = oneup_app::versions::size;
            let list = oneup_app::settingsapi::largest(&data_dir(&cli)?, (*count).clamp(1, 1000))?;
            if list.is_empty() {
                println!("No files listed yet (the background agent lists your OneDrive when it starts).");
            }
            for l in list {
                println!("{:>10}  {:<13} {}", size(u64::try_from(l.size).unwrap_or(0)), l.state.name(), l.path);
            }
            Ok(())
        }
        Cmd::Safety { max_changes, max_percent, content_guard } => {
            let mut store = open_store(&cli, MemoryProfile::Balanced)?;
            let s = oneup_app::versions::Safety {
                max_changes: *max_changes,
                max_percent: *max_percent,
                content_guard: *content_guard,
            };
            oneup_app::versions::set_safety(&mut store, &s)?;
            let p = oneup_app::versions::policy(&store)?;
            println!("Safety stops (sync stops and asks you first):");
            println!(
                "  deletions, replacements and moves  more than {} on one side, or more than {}% of all synced \
                 items (over 20)",
                p.max_count, p.max_percent
            );
            match p.max_suspicious {
                0 => println!("  content guard                      off"),
                n => println!(
                    "  content guard                      more than {n} edited files that no longer look like their type"
                ),
            }
            if agent::nudge(&data_dir(&cli)?).await {
                println!("The background agent uses them from its next round.");
            }
            Ok(())
        }
        Cmd::History { path, open, restore } => history(path, *open, *restore).await,
        Cmd::OnedriveApp { add, remove, top_files } => {
            let dir = data_dir(&cli)?;
            let mut a = oneup_app::config::load_onedrive_app(&dir)?;
            let norm = |p: &str| format!("/{}", p.trim_matches(['/', '\\']).replace('\\', "/"));
            for p in add {
                let p = norm(p);
                if p == "/" {
                    bail!("give a folder, like /Office");
                }
                if !a.folders.iter().any(|f| f.eq_ignore_ascii_case(&p)) {
                    a.folders.push(p);
                }
            }
            for p in remove {
                let p = norm(p);
                a.folders.retain(|f| !f.eq_ignore_ascii_case(&p));
            }
            if let Some(t) = top_files {
                a.top_files = t.on();
            }
            if !add.is_empty() || !remove.is_empty() || top_files.is_some() {
                oneup_app::config::save_onedrive_app(&dir, &a)?;
                agent::nudge(&dir).await;
            }
            if a.folders.is_empty() {
                println!("Nothing is left to the OneDrive app (`oneup onedrive-app --add /Office`).");
            } else {
                println!("Left to the OneDrive app (OneUpDrive doesn't touch them): {}", a.folders.join(", "));
                if a.top_files {
                    println!("...and the files at the top of your OneDrive.");
                }
                println!(
                    "In the OneDrive app: Settings > Account > Choose folders, and tick only these. Its folder on this PC \
                     must be apart from OneUpDrive's. Then double-clicking an Office file there opens it with AutoSave."
                );
            }
            Ok(())
        }
        Cmd::OfficeProps { path } => {
            let p = oneup_app::item::office_props(path).await?;
            let hex: String = p.sha256.iter().map(|b| format!("{b:02x}")).collect();
            println!("uri={}\ninsync={}\nsha256={hex}", p.uri, u8::from(p.in_sync));
            Ok(())
        }
        Cmd::DevTree { rel, find } => {
            let dir = data_dir(&cli)?;
            let v = match find {
                Some(q) => oneup_app::filetree::find(&dir, q, 50)?,
                None => oneup_app::filetree::list(&dir, rel)?,
            };
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::OfficeOpener { state, explain } => {
            use oneup_app::officeopen;
            if let Some(path) = explain {
                match officeopen::choose(path).await {
                    officeopen::Choice::Cloud(uri) => println!("Opens from OneDrive, with AutoSave ({uri})."),
                    officeopen::Choice::Local(why) => println!("Opens on this PC in Office as usual: {why}."),
                }
                return Ok(());
            }
            match state.map(Toggle::on) {
                Some(true) => {
                    let exe = oneup_app::launch::sibling("oneup-open.exe")?;
                    let types = officeopen::register(&exe)?;
                    if types.is_empty() {
                        bail!("Office isn't installed for Word, Excel or PowerPoint files here");
                    }
                    println!(
                        "OneUpDrive can now open .{} files. Windows lets only you choose the default program: \
                         in Default apps (opening now), set OneUpDrive for the types you want.",
                        types.join(", .")
                    );
                    officeopen::choose_in_settings()?;
                }
                Some(false) => {
                    let was = officeopen::unregister()?;
                    if was.is_empty() {
                        println!("OneUpDrive no longer opens Office files.");
                    } else {
                        println!(
                            "OneUpDrive no longer opens Office files. It was the default for .{}: Windows asks \
                             which program to use the next time you open one (pick Word, Excel or PowerPoint).",
                            was.join(", .")
                        );
                    }
                }
                None => {
                    let d = officeopen::defaults();
                    println!(
                        "{}; default program for: {}.",
                        if officeopen::is_registered() { "Offered to Windows" } else { "Off" },
                        if d.is_empty() { "none".to_string() } else { format!(".{}", d.join(", .")) }
                    );
                }
            }
            Ok(())
        }
        Cmd::OfficeIntegration { state } => {
            let dir = data_dir(&cli)?;
            match state.map(Toggle::on) {
                Some(true) => {
                    let n = oneup_app::officeint::turn_on(&dir).await?;
                    println!(
                        "Office integration is on for {n} folder(s), experimentally. Open a Word, Excel or PowerPoint file \
                         there: if Word shows AutoSave switched on (and the title shows it's from OneDrive), it works."
                    );
                }
                Some(false) => {
                    oneup_app::officeint::turn_off(&dir)?;
                    println!("Office integration is off; Office opens files here as ordinary local files.");
                }
                None => println!(
                    "Office integration is {} (experimental; `oneup office-integration on|off`).",
                    if oneup_app::officeint::is_on(&dir) { "on" } else { "off" }
                ),
            }
            Ok(())
        }
        Cmd::Office { path, print } => {
            let uri = oneup_app::item::office_uri(path).await?;
            if *print {
                println!("{uri}");
            } else {
                oneup_app::item::launch(&uri)?;
            }
            Ok(())
        }
        Cmd::Share { path, edit } => {
            let link = oneup_app::item::share(path, *edit).await?;
            oneup_sys::clipboard::set_text(&link)?;
            println!(
                "{link}\n(copied: anyone with this link can {} it)",
                if *edit { "view and change" } else { "view" }
            );
            Ok(())
        }
        Cmd::Pin { path } => {
            rules::pin(&data_dir(&cli)?, &open_store(&cli, MemoryProfile::Balanced)?, path, true).await
        }
        Cmd::Free { path } => {
            rules::pin(&data_dir(&cli)?, &open_store(&cli, MemoryProfile::Balanced)?, path, false).await
        }
        Cmd::OnDemand { state } => {
            let dir = data_dir(&cli)?;
            let store = open_store(&cli, MemoryProfile::Balanced)?;
            match state.map(Toggle::on) {
                None => {
                    let pair = sync::pairing(&store)?;
                    let token = store.read(|o| o.meta_get("sync.token"))?.unwrap_or_default();
                    if oneup_app::ondemand::is_on(&pair.local, &token) {
                        let (need, _) = oneup_app::ondemand::off_needs(&store)?;
                        println!(
                            "Files On-Demand is on: {} of files are online-only.",
                            oneup_app::versions::size(need)
                        );
                    } else {
                        println!("Files On-Demand is off: every file is kept on this PC (`oneup on-demand on`).");
                    }
                    Ok(())
                }
                Some(true) => {
                    setup::require_test_account(&dir)?;
                    let _lock = DirLock::acquire(&dir)?;
                    let done = oneup_app::ondemand::turn_on(&store)?;
                    println!(
                        "Files On-Demand is on: {} folders and {} files are now placeholders that keep their content; {} changed files follow after their next upload. Nothing was removed from this PC.",
                        done.folders, done.files, done.left
                    );
                    println!("New files from OneDrive arrive online-only; `oneup start` serves them when opened.");
                    Ok(())
                }
                Some(false) => {
                    setup::require_test_account(&dir)?;
                    let _lock = DirLock::acquire(&dir)?;
                    let http = http()?;
                    let auth = auth(&cli, &http)?;
                    if !auth.is_signed_in()? {
                        bail!("not signed in; run `oneup login --test-account` first");
                    }
                    let graph = Arc::new(Graph::new(http, auth, Aimd::new(4, 8)));
                    let got = oneup_app::ondemand::turn_off(&dir, store, graph).await?;
                    println!(
                        "Files On-Demand is off: downloaded {}; every file is an ordinary file on this PC again.",
                        oneup_app::versions::size(got)
                    );
                    Ok(())
                }
            }
        }
        Cmd::Migrate(MigrateCmd::Check { json, profile }) => {
            migrate::check(&data_dir(&cli)?, &open_store(&cli, profile.memory_profile)?, *json)
        }
        Cmd::Migrate(MigrateCmd::FinishUnlink { step, yes, wait_minutes, json, profile }) => {
            use oneup_app::moveover::Step;
            let steps: Vec<Step> = match step {
                Some(UnlinkStep::Restart) => vec![Step::Restart],
                Some(UnlinkStep::Reset) => vec![Step::Reset],
                Some(UnlinkStep::Release) => vec![Step::Release],
                None => Step::ALL.to_vec(),
            };
            migrate::finish_unlink(&cli, &steps, *yes, *wait_minutes, *json, profile.memory_profile).await
        }
        Cmd::Cloud(CloudCmd::Prune { keep: true, .. }) => cloud_prune::keep(&data_dir(&cli)?).await,
        Cmd::Cloud(CloudCmd::Prune { yes, details, page, .. }) => {
            let dir = data_dir(&cli)?;
            let store = open_store(&cli, MemoryProfile::Balanced)?;
            let http = http()?;
            let auth = auth(&cli, &http)?;
            if !auth.is_signed_in()? {
                bail!("not signed in; run `oneup login --test-account` first");
            }
            let graph = Arc::new(Graph::new(http, auth, Aimd::new(4, 8)));
            cloud_prune::run(&dir, store, graph, *yes, details.as_deref(), *page).await
        }
        Cmd::Vault(v) => match v {
            VaultCmd::Create { folder } => vault::create(folder),
            VaultCmd::Ls { folder, path, recursive } => vault::ls(folder, path.as_deref(), *recursive),
            VaultCmd::Get { folder, path, dest } => vault::get(folder, path, dest),
            VaultCmd::Put { folder, src, path } => vault::put(folder, src, path),
            VaultCmd::Mkdir { folder, path } => vault::mkdir(folder, path),
            VaultCmd::Check { folder } => vault::check(folder),
            VaultCmd::Add { remote, folder, full_files, remember, no_start } => {
                let http = http()?;
                let graph = Arc::new(Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(4, 8)));
                vault::add(&data_dir(&cli)?, graph, remote, folder, !*full_files, *remember, !*no_start).await
            }
            VaultCmd::List => vault::list(&data_dir(&cli)?).await,
            VaultCmd::Hello { name, state } => {
                let http = http()?;
                let graph = Arc::new(Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(2, 4)));
                vault::hello(&data_dir(&cli)?, graph, name, state.map(Toggle::on)).await
            }
            VaultCmd::New { remote, folder, full_files, remember, no_start } => {
                let http = http()?;
                let graph = Arc::new(Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(4, 8)));
                vault::new(&data_dir(&cli)?, graph, remote, folder, !*full_files, *remember, !*no_start).await
            }
            VaultCmd::RecoveryKey { name } => {
                let http = http()?;
                let graph = Arc::new(Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(2, 4)));
                vault::recovery_key(&data_dir(&cli)?, graph, name).await
            }
            VaultCmd::ChangePassword { name, recovery_key } => {
                let http = http()?;
                let graph = Arc::new(Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(2, 4)));
                vault::change_password(&data_dir(&cli)?, graph, name, *recovery_key).await
            }
            VaultCmd::Unlock { name, remember } => {
                let http = http()?;
                let graph = Arc::new(Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(4, 8)));
                vault::unlock(&data_dir(&cli)?, graph, name, *remember).await
            }
            VaultCmd::Lock { name, forget } => vault::lock(&data_dir(&cli)?, name, *forget).await,
            VaultCmd::Remove { name, yes } => vault::remove(&data_dir(&cli)?, name, *yes).await,
        },
        Cmd::Pair(p) => {
            let dir = data_dir(&cli)?;
            let http = http()?;
            match p {
                PairCmd::List => pair::list(&dir).await,
                PairCmd::Remove { name, yes } => pair::remove(&dir, name, *yes).await,
                PairCmd::Shared => {
                    let graph = Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(2, 4));
                    pair::shared(&graph).await
                }
                PairCmd::Add { remote, folder, backup, on_demand, no_start } => {
                    let a = pair::Add {
                        cloud: oneup_app::pairs::Cloud::Path(remote.clone()),
                        folder,
                        backup: *backup,
                        on_demand: *on_demand,
                        start: !*no_start,
                        known: None,
                    };
                    pair::add(&dir, http.clone(), auth(&cli, &http)?, a).await
                }
                PairCmd::AddShared { name, folder, backup, on_demand, no_start } => {
                    let graph = Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(2, 4));
                    let s = pair::find_shared(&graph, name).await?;
                    let a = pair::Add {
                        cloud: oneup_app::pairs::Cloud::Shared(s),
                        folder,
                        backup: *backup,
                        on_demand: *on_demand,
                        start: !*no_start,
                        known: None,
                    };
                    pair::add(&dir, http.clone(), auth(&cli, &http)?, a).await
                }
                PairCmd::BackupPc { folders, two_way, no_start } => {
                    pair::backup_pc(&dir, http.clone(), auth(&cli, &http)?, folders, *two_way, !*no_start).await
                }
            }
        }
        Cmd::Updates { auto, channel, check } => {
            updates_cmd(&data_dir(&cli)?, auto.map(Toggle::on), channel.as_deref(), *check)
        }
        Cmd::Shortcuts { fix, undo, from, json } => {
            shortcuts_cmd(&data_dir(&cli)?, *fix, *undo, from.as_deref(), *json)
        }
        Cmd::Accounts { add } => match add {
            Some(AccountsCmd::Add { name }) => {
                let dir = oneup_app::accounts::add(name)?;
                println!("Added {name}: its data folder is {}.", dir.display());
                println!(
                    "Sign it in from the settings window that opens (or `oneup --data-dir \"{}\" login`).",
                    dir.display()
                );
                oneup_app::launch::start_settings(&dir)
            }
            None => {
                for a in oneup_app::accounts::list() {
                    println!(
                        "{}{}: {}{}
  {}",
                        a.name,
                        if a.default { " (default)" } else { "" },
                        if a.signed_in { "signed in" } else { "not signed in" },
                        if a.test_account { ", test account" } else { "" },
                        a.dir.display()
                    );
                }
                Ok(())
            }
        },
        Cmd::Unpair { yes } => pair::unpair(&data_dir(&cli)?, *yes).await,
        Cmd::Flow { to } => pair::set_flow(
            &data_dir(&cli)?,
            to.map(|f| match f {
                FlowArg::Both => oneup_app::flow::Flow::TwoWay,
                FlowArg::Backup => oneup_app::flow::Flow::Backup,
            }),
        ),
        Cmd::Priority { first, last, remove } => rules::priority(&data_dir(&cli)?, first, last, remove).await,
        Cmd::Explain { path, profile } => {
            rules::explain(&data_dir(&cli)?, open_store(&cli, profile.memory_profile)?, path).await
        }
        Cmd::Activity { path, limit, profile } => {
            activity::show(&open_store(&cli, profile.memory_profile)?, path.as_deref(), (*limit).max(1))
        }
        Cmd::Unsynced { cmd: None, profile } => oneup_app::unsynced::show(&open_store(&cli, profile.memory_profile)?),
        Cmd::Unsynced { cmd: Some(UnsyncedCmd::Resolve(r)), .. } => {
            let ask = unsynced_cmd::Ask {
                paths: &r.paths,
                all: r.all,
                expect: r.expect,
                choice: r.choice(),
                name: r.rename.as_deref(),
                yes: r.yes,
            };
            unsynced_cmd::resolve(&data_dir(&cli)?, open_store(&cli, r.profile.memory_profile)?, ask).await
        }
        Cmd::Verify(p) => verify(&cli, p.memory_profile),
        Cmd::Diagnose { top, profile } => diagnose(&cli, *top, profile.memory_profile),
        Cmd::Sync(args) => {
            let dir = data_dir(&cli)?;
            setup::require_test_account(&dir)?;
            let _lock = DirLock::acquire(&dir)?;
            let preset = exclusion_choice(&cli, args.exclude, args.profile.memory_profile)?;
            let mut store = open_store(&cli, args.profile.memory_profile)?;
            store.write(|o| o.meta_set("exclude_preset", preset.name()))?;
            let http = http()?;
            let auth = auth(&cli, &http)?;
            if !auth.is_signed_in()? {
                bail!("not signed in; run `oneup login --test-account` first");
            }
            let requests = oneup_app::config::load_agent(&dir).unwrap_or_default().requests();
            let graph = Arc::new(Graph::new(http, auth, requests));
            let opts = sync::Options {
                data_dir: dir.clone(),
                local: args.local.clone(),
                remote: args.remote.clone(),
                dry_run: args.dry_run,
                preview_limit: args.limit,
                pair_only: args.pair_only,
                on_demand: args.on_demand,
                max_rounds: args.max_rounds.max(1),
                answers: sync::Answers {
                    approve: args.approve.clone(),
                    keep: args.keep.clone(),
                    discard: args.discard.clone(),
                    ..sync::Answers::default()
                },
            };
            sync::run(store, graph, preset.rules(), &opts).await
        }
        Cmd::Versions {
            older_than_days,
            max_gib,
            yes,
            set_min_days,
            set_max_gib,
            set_max_percent,
            set_rotate,
            set_overflow,
            set_keep_replaced,
            profile,
        } => {
            let _lock = DirLock::acquire(&data_dir(&cli)?)?;
            let mut store = open_store(&cli, profile.memory_profile)?;
            let changes = versions::Changes {
                min_days: *set_min_days,
                max_gib: *set_max_gib,
                max_percent: *set_max_percent,
                rotate: set_rotate.map(Toggle::on),
                overflow: set_overflow.map(|c| match c {
                    OverflowChoice::Hold => oneup_gate::OverflowPolicy::Hold,
                    OverflowChoice::Keep => oneup_gate::OverflowPolicy::Keep,
                }),
                keep_replaced: set_keep_replaced.map(Toggle::on),
            };
            if !changes.is_empty() {
                versions::set(&mut store, &changes)?;
                println!("{}", versions::describe(&store)?);
                return Ok(());
            }
            versions::versions(&store, *older_than_days, *max_gib, *yes)
        }
        Cmd::Restore { id, to, profile } => {
            let _lock = DirLock::acquire(&data_dir(&cli)?)?;
            versions::restore(&open_store(&cli, profile.memory_profile)?, id, to.as_deref())
        }
        Cmd::CloudCheck | Cmd::DevRemote(_) | Cmd::UploadBench { .. } => {
            let dir = data_dir(&cli)?;
            if test_account::load(&dir)?.is_none() {
                bail!(
                    "this developer command needs a test-account data folder (--data-dir); it never runs on your own account"
                );
            }
            let http = http()?;
            let graph = Arc::new(Graph::new(http.clone(), auth(&cli, &http)?, Aimd::new(2, 4)));
            match &cli.cmd {
                Cmd::DevRemote(c) => dev_remote::run(graph, c).await,
                Cmd::UploadBench { session_mib, rounds, smart_mib } => {
                    upload_bench::run(&dir, graph, *session_mib, *rounds, *smart_mib).await
                }
                _ => cloud_check::run(&dir, graph).await,
            }
        }
    }
}

/// `oneup history PATH [--open N | --restore N]`.
async fn history(path: &std::path::Path, open: Option<usize>, restore: Option<usize>) -> Result<()> {
    let v = oneup_app::item::versions_json(path).await?;
    let list = v["versions"].as_array().cloned().unwrap_or_default();
    if v["folder"].as_bool() == Some(true) {
        bail!("folders have no versions; give a file");
    }
    let pick = |n: usize| -> Result<&serde_json::Value> {
        list.get(n.wrapping_sub(1)).with_context(|| format!("there's no version {n} (1 to {})", list.len()))
    };
    let when = |x: &serde_json::Value| {
        x["modified_ms"]
            .as_i64()
            .and_then(|ms| time::OffsetDateTime::from_unix_timestamp(ms / 1000).ok())
            .map(|t| {
                let t = time::UtcOffset::current_local_offset().map_or(t, |o| t.to_offset(o));
                format!("{:04}-{:02}-{:02} {:02}:{:02}", t.year(), u8::from(t.month()), t.day(), t.hour(), t.minute())
            })
            .unwrap_or_else(|| "?".into())
    };
    if let Some(n) = open {
        let x = pick(n)?;
        let to = oneup_app::item::open_version(path, x["id"].as_str().unwrap_or_default(), &when(x).replace(':', ""))
            .await?;
        println!("Saved a copy of version {n} as {}", to.display());
        std::process::Command::new("explorer.exe").arg(&to).spawn()?;
        return Ok(());
    }
    if let Some(n) = restore {
        if n == 1 {
            bail!("version 1 is the current one already");
        }
        let x = pick(n)?;
        oneup_app::item::restore_version(path, x["id"].as_str().unwrap_or_default()).await?;
        println!("Version {n} ({}) is the current version again; this PC gets it with the next sync.", when(x));
        return Ok(());
    }
    println!("{} in OneDrive:", v["path"].as_str().unwrap_or_default());
    for (i, x) in list.iter().enumerate() {
        println!(
            "  {:>3}  {}  {:>10}  {}{}",
            i + 1,
            when(x),
            oneup_app::versions::size(x["size"].as_u64().unwrap_or(0)),
            x["by"].as_str().unwrap_or(""),
            if i == 0 { "  (current)" } else { "" }
        );
    }
    println!("`--open N` saves a copy of one and opens it; `--restore N` makes one current again.");
    Ok(())
}

fn open_browser(url: &str) {
    println!("Opening your browser to sign in. If it doesn't open, visit:\n\n{url}\n");
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
}

async fn login(cli: &Cli, shared: bool) -> Result<()> {
    let dir = data_dir(cli)?;
    if test_account::load(&dir)?.is_some() {
        bail!("this data folder belongs to a test account; use `oneup login --test-account`");
    }
    // The marker decides which permission the sign-in (and every refresh)
    // asks for (`setup::auth`); it goes again if signing in fails.
    let marker = dir.join(setup::SHARED_MARKER);
    let had = marker.exists();
    if shared {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&marker, "Files.ReadWrite.All\n")?;
    }
    let http = http()?;
    let auth = auth(cli, &http)?;
    // Microsoft's account picker every time: otherwise the browser quietly
    // signs in whichever account it used last, which can put a test account
    // where someone's own belongs.
    if let Err(e) = auth.sign_in_with_browser(true, open_browser).await {
        if shared && !had {
            let _ = std::fs::remove_file(&marker);
        }
        return Err(e.into());
    }
    let drive = Graph::new(http, Arc::clone(&auth), Aimd::new(1, 1)).drive().await?;
    let name = drive.owner_name().unwrap_or("your Microsoft account").to_string();
    match oneup_app::accounts::record_sign_in(&dir, &drive.id, &name) {
        Err(e) => {
            auth.sign_out().await?;
            return Err(e.context("signed back out"));
        }
        Ok(oneup_app::accounts::Switched::Cleared) => {
            println!("This data folder listed another account's OneDrive; that listing was dropped.");
        }
        Ok(oneup_app::accounts::Switched::No) => {}
    }
    if oneup_app::setup::PRE_RELEASE {
        println!("Signed in as {name} (read-only access). Next: `oneup mirror`.");
    } else {
        println!("Signed in as {name}.");
    }
    Ok(())
}

/// Signs in a dedicated test account with write access, in its own data
/// folder. Refuses the default folder, and refuses the drive that the
/// default folder mirrors (that's the personal account).
async fn login_test_account(cli: &Cli, shared: bool) -> Result<()> {
    let default = test_account::default_dir()?;
    let Some(dir) = cli.data_dir.clone() else {
        bail!("a test account needs its own data folder: pass --data-dir (e.g. %LOCALAPPDATA%\\OneUpDrive-test)");
    };
    if test_account::same_dir(&dir, &default) {
        bail!("a test account can't use the default data folder; pick another --data-dir");
    }
    let client_id = cli
        .client_id
        .clone()
        .or(test_account::load(&dir)?.map(|t| t.client_id))
        .context("no client ID: set ONEUP_CLIENT_ID or pass --client-id")?;
    let http = http()?;
    let store = DpapiFileStore::new(dir.join("token.dpapi"));
    let scope = if shared { READ_WRITE_ALL_SCOPE } else { READ_WRITE_SCOPE };
    let auth = Arc::new(Auth::new(http.clone(), client_id.clone(), scope, Box::new(store)));
    println!("Pick the TEST account in Microsoft's account list (not your usual one).");
    auth.sign_in_with_browser(true, open_browser).await?;

    let graph = Graph::new(http, Arc::clone(&auth), Aimd::new(1, 1));
    let drive = graph.drive().await?;
    let refuse =
        |why: &str| -> Result<()> { bail!("{why}; nothing was changed, and this folder's sign-in has been removed") };
    if drive.drive_type.as_deref() != Some("personal") {
        auth.sign_out().await?;
        return refuse("that isn't a OneDrive Personal account");
    }
    if let Some(personal) = test_account::personal_drive(&default)
        && test_account::is_same_drive(&drive.id, &personal)
    {
        auth.sign_out().await?;
        return refuse("that is the account mirrored in your default data folder, not a test account");
    }
    test_account::save(&dir, &test_account::TestAccount { client_id, drive_id: drive.id.clone() })?;
    oneup_app::accounts::record_sign_in(&dir, &drive.id, drive.owner_name().unwrap_or("the test account"))?;
    // The sign-in's permission decides which one refreshing asks for.
    let marker = dir.join(setup::SHARED_MARKER);
    if shared {
        std::fs::write(&marker, "Files.ReadWrite.All\n")?;
    } else if marker.exists() {
        std::fs::remove_file(&marker)?;
    }
    let tail: String = drive.id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    println!(
        "Signed in the test account (drive …{tail}) with write access, for {} only.\n\
         Your usual account and the default data folder are unchanged (read-only).",
        dir.display()
    );
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ExcludePreset {
    None,
    Dev,
}

impl ExcludePreset {
    fn name(self) -> &'static str {
        match self {
            ExcludePreset::None => "none",
            ExcludePreset::Dev => "dev",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        match s {
            "none" => Some(ExcludePreset::None),
            "dev" => Some(ExcludePreset::Dev),
            _ => None,
        }
    }

    fn rules(self) -> ExcludeSet {
        match self {
            ExcludePreset::None => ExcludeSet::none(),
            ExcludePreset::Dev => ExcludeSet::dev(),
        }
    }
}

/// The exclusion choice: given now, or remembered from an earlier run.
/// Nothing is excluded unless the user chose it. `mirror` and `scan` share
/// one choice, because comparing a local folder with OneDrive only makes
/// sense if both sides leave out the same things.
fn parse_guard(s: &str) -> std::result::Result<usize, String> {
    match s {
        "off" => Ok(0),
        n => n.parse().map_err(|_| "a number, or `off`".to_string()),
    }
}

fn exclusion_choice(cli: &Cli, given: Option<ExcludePreset>, profile: MemoryProfile) -> Result<ExcludePreset> {
    if let Some(p) = given {
        return Ok(p);
    }
    if db_path(cli)?.exists() {
        let stored = open_store(cli, profile)?.read(|o| o.meta_get("exclude_preset"))?;
        if let Some(p) = stored.as_deref().and_then(ExcludePreset::from_name) {
            return Ok(p);
        }
    }
    bail!(
        "choose what to leave out (remembered for later runs of `mirror` and `scan`):\n  \
         --exclude dev    skip dependency/build folders (node_modules, .venv, __pycache__, target, ...)\n  \
         --exclude none   leave nothing out"
    )
}

async fn mirror(cli: &Cli, args: &MirrorArgs) -> Result<()> {
    // Read before a rebuild discards the database that remembers it.
    let preset = exclusion_choice(cli, args.exclude, args.profile.memory_profile)?;
    let http = http()?;
    let auth = auth(cli, &http)?;
    if !auth.is_signed_in()? {
        bail!("not signed in; run `oneup login` first");
    }
    let limiter = Aimd::new(4, args.max_concurrency.max(1));
    let graph = Arc::new(Graph::new(http, auth, Arc::clone(&limiter)));
    let rules = preset.rules();
    let fingerprint = rules.fingerprint();
    let mut store = open_store(cli, args.profile.memory_profile)?;
    // Where the previous mirror found excluded folders: worth keeping as a
    // hint before a rebuild forgets them (only if built with the same rules).
    let mut previous_excluded = Vec::new();
    if args.rebuild {
        if store.read(|o| o.meta_get("rules"))?.as_deref() == Some(fingerprint.as_str()) {
            previous_excluded = store.read(|o| o.remote_excluded_paths())?;
        }
        // Only the Remote tree: the last folder scan and settings stay.
        store.write(|o| o.clear_remote())?;
        println!("Local mirror cleared; rebuilding from scratch.");
    }
    store.write(|o| o.meta_set("exclude_preset", preset.name()))?;
    let fresh = store.read(|o| o.cursor())?.is_none();
    let hint = if fresh && !args.no_hints && !rules.is_empty() {
        enumeration_hint(&store, &fingerprint, previous_excluded)?
    } else {
        None
    };
    let cfg = Config {
        list_depth: args.depth,
        max_list_depth: args.max_depth.max(args.depth),
        split_bytes: args.split_size_mib.saturating_mul(1 << 20),
        rules,
        max_tasks: 32,
        hint: hint.map(Arc::new),
    };
    let m = Mirror::new(Arc::clone(&graph), store, cfg);

    let cancel = m.cancel_flag();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nStopping at the next page boundary… (progress is saved; run `oneup mirror` to resume)");
            cancel.store(true, Relaxed);
        }
    });

    let progress = m.progress();
    let (g2, l2) = (Arc::clone(&graph), Arc::clone(&limiter));
    let started = Instant::now();
    let ticker = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.tick().await;
        loop {
            tick.tick().await;
            let secs = started.elapsed().as_secs_f64().max(1e-9);
            let req = g2.stats.requests.load(Relaxed);
            let mem = oneup_sys::metrics::memory();
            let ls = l2.stats();
            eprintln!(
                "[{:>6.0}s] {:<16} units {}/{}  entries {} ({:.0}/s)  stored {}  excluded {}  pruned {}  requests {} (limit {}, throttled {}, paused {:.0}s{})  mem {:.0} MiB",
                secs,
                phase_name(progress.phase()),
                progress.units_done.load(Relaxed),
                progress.units_known.load(Relaxed),
                progress.entries.load(Relaxed),
                progress.entries.load(Relaxed) as f64 / secs,
                progress.stored.load(Relaxed),
                progress.excluded.load(Relaxed),
                progress.pruned.load(Relaxed),
                req,
                ls.limit,
                g2.stats.throttled.load(Relaxed),
                ls.paused_total.as_secs_f64(),
                if ls.paused_now { " NOW" } else { "" },
                mem.working_set as f64 / 1_048_576.0,
            );
        }
    });

    let result = m.run().await;
    ticker.abort();
    let summary = match result {
        Ok(s) => s,
        Err(oneup_remote::Error::ResyncRequired) => {
            bail!("OneDrive says the saved position is too old. Run `oneup mirror --rebuild`.")
        }
        Err(e) => return Err(e.into()),
    };
    let st = &summary.stats;
    let mem = oneup_sys::metrics::memory();
    println!("{}", if summary.fresh { "Initial mirror complete." } else { "Mirror up to date." });
    println!("  elapsed           {:.1?}", summary.elapsed);
    println!("  files             {}  ({:.1} GiB)", st.files, st.file_bytes as f64 / (1u64 << 30) as f64);
    println!(
        "  folders           {}  (+{} excluded, {} pruned inside them)",
        st.folders, st.excluded_folders, st.pruned_folders
    );
    println!("  packages          {}", st.packages);
    let ls = limiter.stats();
    println!(
        "  requests          {}  (retries {}, {:.1} MiB received, final concurrency limit {})",
        graph.stats.requests.load(Relaxed),
        graph.stats.retries.load(Relaxed),
        graph.stats.bytes.load(Relaxed) as f64 / 1_048_576.0,
        ls.limit
    );
    println!(
        "  throttling        {} responses, all requests paused {:.0} s in total (longest Retry-After {} s), {} early warnings",
        graph.stats.throttled.load(Relaxed),
        ls.paused_total.as_secs_f64(),
        graph.stats.retry_after_max_s.load(Relaxed),
        graph.stats.rate_limit_warnings.load(Relaxed)
    );
    println!("  peak memory       {:.1} MiB", mem.peak_working_set as f64 / 1_048_576.0);
    println!("  database          {:.1} MiB", st.db_bytes as f64 / 1_048_576.0);
    Ok(())
}

const GIB: f64 = (1u64 << 30) as f64;
const MIB: f64 = (1u64 << 20) as f64;

/// `oneup scan`: a read-only scan of a local folder into the Local tree.
async fn scan(cli: &Cli, args: &ScanArgs) -> Result<()> {
    let preset = exclusion_choice(cli, args.exclude, args.profile.memory_profile)?;
    let mut store = open_store(cli, args.profile.memory_profile)?;
    store.write(|o| o.meta_set("exclude_preset", preset.name()))?;
    let mut scan = oneup_local::Scan::open(store, &args.path, oneup_local::Config::new(preset.rules()))
        .with_context(|| format!("scanning {}", args.path.display()))?;

    use oneup_local::{Replaced, Start};
    match scan.start() {
        Start::Fresh => println!("Scanning {} (read-only)…", scan.root().path),
        Start::Resumed => println!("Resuming the unfinished scan of {}…", scan.root().path),
        Start::Replaced(why) => {
            let why = match why {
                Replaced::Finished => "rescanning from scratch",
                Replaced::OtherFolder => "the previous scan was of a different folder; starting over",
                Replaced::RulesChanged => "the exclusion choice changed; starting over",
                Replaced::OlderScanner => "the previous scan was made by an older version; starting over",
                Replaced::ManagerChanged => {
                    "another sync app started or stopped managing the folder since; starting over"
                }
            };
            println!("Scanning {} (read-only; {why})…", scan.root().path);
        }
    }
    if let Some(other) = scan.other_engine() {
        print_other_engine(other);
    }

    let cancel = scan.cancel_flag();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nStopping… (progress is saved; run the same `oneup scan` again to resume)");
            cancel.store(true, Relaxed);
        }
    });
    let progress = scan.progress();
    let started = Instant::now();
    let ticker = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.tick().await;
        loop {
            tick.tick().await;
            let secs = started.elapsed().as_secs_f64().max(1e-9);
            let entries = progress.entries.load(Relaxed);
            eprintln!(
                "[{:>6.0}s] folders {}  entries {} ({:.0}/s)  excluded {}  not downloaded {}  unreadable {}  links {}  mem {:.0} MiB",
                secs,
                progress.folders_listed.load(Relaxed),
                entries,
                entries as f64 / secs,
                progress.excluded.load(Relaxed),
                progress.unpopulated.load(Relaxed),
                progress.unreadable.load(Relaxed),
                progress.links.load(Relaxed),
                oneup_sys::metrics::memory().working_set as f64 / MIB,
            );
        }
    });

    // The scan is blocking file-system and SQLite work: keep it off the async runtime.
    let progress = scan.progress();
    let summary = tokio::task::spawn_blocking(move || scan.run()).await.context("scan task")??;
    ticker.abort();
    tracing::info!(
        list_s = progress.list_us.load(Relaxed) as f64 / 1e6,
        store_s = progress.store_us.load(Relaxed) as f64 / 1e6,
        "scan time split"
    );

    let st = &summary.stats;
    if summary.finished {
        println!("Scan complete.");
    } else {
        println!("Scan paused; run the same `oneup scan` command again to continue.");
    }
    println!("  elapsed           {:.1?}", summary.elapsed);
    println!("  files             {}  ({:.1} GiB)", st.files, st.file_bytes as f64 / GIB);
    if st.placeholders > 0 {
        println!(
            "    online-only     {}  ({:.1} GiB not on this PC; never downloaded by the scan)",
            st.online_only_files,
            st.online_only_bytes as f64 / GIB
        );
        println!("    always kept     {}  (\"Always keep on this device\")", st.pinned_files);
    }
    println!(
        "  folders           {}  (+{} excluded, contents not scanned)",
        st.folders - st.excluded_folders,
        st.excluded_folders
    );
    if st.unpopulated_folders > 0 {
        println!(
            "  online-only folders {}  (their file lists aren't on this PC; left alone so nothing gets downloaded)",
            st.unpopulated_folders
        );
    }
    if st.unreadable_folders > 0 {
        println!("  unreadable        {} folders (access denied or removed during the scan)", st.unreadable_folders);
    }
    if st.links > 0 {
        println!("  links             {}  (shortcuts to other places: recorded, not followed)", st.links);
    }
    if st.lossy_names > 0 {
        println!("  unusual names     {}  (not valid Unicode; can't be synced under that name)", st.lossy_names);
    }
    if summary.duplicates > 0 {
        println!(
            "  hard links        {}  (extra names of files already counted, or moved during the scan)",
            summary.duplicates
        );
    }
    let mem = oneup_sys::metrics::memory();
    println!("  peak memory       {:.1} MiB", mem.peak_working_set as f64 / MIB);
    Ok(())
}

/// `oneup observe`: replays the change journal since the last scan (or the
/// last observe) into the Local tree. Reads only metadata, like the scan.
async fn observe(cli: &Cli, profile: MemoryProfile) -> Result<()> {
    let preset = exclusion_choice(cli, None, profile)?;
    let store = open_store(cli, profile)?;
    let started = Instant::now();
    // Blocking file-system and SQLite work: keep it off the async runtime.
    let ch = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut obs = oneup_local::observe::Observer::open(store, oneup_local::Config::new(preset.rules()))?;
        Ok(obs.catch_up()?)
    })
    .await
    .context("observe task")??;
    println!("Local folder list is up to date.");
    println!("  elapsed           {:.1?}", started.elapsed());
    println!("  journal records   {}  (the whole drive's; most are about other folders)", ch.records);
    println!("  new               {}", ch.created);
    println!("  changed           {}  (content, attributes, name or place)", ch.updated);
    println!("  deleted           {}  ({} of them synced: recorded as deleted)", ch.deleted, ch.evidence);
    if ch.left > 0 {
        println!(
            "  moved away        {}  (out of the folder or into an excluded one; not treated as deleted)",
            ch.left
        );
    }
    if ch.unreadable > 0 {
        println!("  unreadable        {}  (left as they were)", ch.unreadable);
    }
    println!("  folders affected  {}", ch.dirty.len());
    Ok(())
}

/// Adapts `oneup-local`'s verifier to the planner's content check.
struct HashCheck(oneup_local::verify::Verifier);

impl oneup_plan::ContentCheck for HashCheck {
    fn check(&mut self, local: &oneup_store::LocalRow, expected: &[u8; 20]) -> oneup_plan::Verified {
        use oneup_local::verify::Checked;
        use oneup_plan::{Unchecked, Verified};
        match self.0.hash(local) {
            Checked::Hash(h) if &h == expected => Verified::Match,
            Checked::Hash(_) => Verified::Mismatch,
            Checked::NotOrdinary => Verified::Skipped(Unchecked::Placeholder),
            Checked::Changed => Verified::Skipped(Unchecked::ChangedSinceScan),
            Checked::Unreadable => Verified::Skipped(Unchecked::Unreadable),
        }
    }
}

/// "3.2 GiB", "12.0 MiB", "512 B".
fn size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// "5 min ago", "3 h ago", "2 days ago" for a Unix-milliseconds timestamp.
fn ago(ms: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let s = ((now - ms) / 1000).max(0);
    match s {
        0..120 => format!("{s} s ago"),
        120..7_200 => format!("{} min ago", s / 60),
        7_200..172_800 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// `oneup plan`: compares the last folder scan with the OneDrive mirror and
/// prints what a first sync would do. Writes nothing anywhere.
async fn plan(cli: &Cli, args: &PlanArgs) -> Result<()> {
    use oneup_plan::{Category, Error as PlanError, Unchecked};
    if !db_path(cli)?.exists() {
        println!("Nothing to compare yet: run `oneup mirror` and `oneup scan <folder>` first.");
        return Ok(());
    }
    let store = open_store(cli, args.profile.memory_profile)?;
    let basis = match store.read(|o| Ok(oneup_plan::basis(o)))? {
        Ok(b) => b,
        Err(PlanError::NotReady(m)) => bail!("{m}"),
        Err(e) => return Err(e.into()),
    };
    println!(
        "Comparing {} (scanned {}) with the top folder of your OneDrive (mirror as of {}).",
        basis.local_path,
        ago(basis.scanned_at_ms),
        ago(basis.mirror_at_ms)
    );
    println!("This only reports; nothing is changed on this PC or in OneDrive.");
    if let Some(p) = &basis.other_engine {
        println!("Note: this folder is synced by {p}. The report shows what switching to OneUpDrive would involve.");
    }
    if args.verify {
        if let Some(p) = &basis.other_engine {
            bail!(
                "--verify isn't possible here: {p} manages this folder, so its files are cloud files, and reading \
                 them could make {p} download them. Run `oneup plan` without --verify; content checks become \
                 possible once the folder is no longer managed by {p}."
            );
        }
        println!("Reading same-size files to compare their contents (cloud placeholders are skipped)…");
    }

    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let progress = Arc::new(oneup_plan::Progress::default());
    {
        let cancel = Arc::clone(&cancel);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nStopping…");
                cancel.store(true, Relaxed);
            }
        });
    }
    let ticker = {
        let progress = Arc::clone(&progress);
        let started = Instant::now();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                eprintln!(
                    "[{:>6.0}s] folders {}  files compared by content {} ({})",
                    started.elapsed().as_secs_f64(),
                    progress.folders.load(Relaxed),
                    progress.hashed_files.load(Relaxed),
                    size(progress.hashed_bytes.load(Relaxed)),
                );
            }
        })
    };

    // Store reads and (with --verify) file reads are blocking work.
    let (verify, examples, buffer) = (args.verify, args.examples, args.profile.memory_profile.hash_buffer());
    let (cancel2, progress2) = (Arc::clone(&cancel), Arc::clone(&progress));
    let outcome = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut check = match verify {
            true => {
                let root = store.read(|o| o.local_root())?.context("the folder scan disappeared")?;
                Some(HashCheck(oneup_local::verify::Verifier::open(&root, buffer)?))
            }
            false => None,
        };
        let check = check.as_mut().map(|c| c as &mut dyn oneup_plan::ContentCheck);
        Ok(store.read(|o| Ok(oneup_plan::observe(o, examples, check, &progress2, &cancel2)))?)
    })
    .await
    .context("plan task")??;
    ticker.abort();
    let (_, report) = match outcome {
        Ok(r) => r,
        Err(PlanError::Cancelled) => bail!("stopped before the comparison finished; nothing was reported"),
        Err(e) => return Err(e.into()),
    };

    // Category order in the report is the enum's order; group it for reading.
    let group = |c: Category| match c {
        Category::Identical
        | Category::SameSize(_)
        | Category::FolderInBoth
        | Category::OnlineOnly
        | Category::OnlineOnlyChanged => 0,
        Category::Download | Category::Upload => 1,
        Category::Conflict(_) | Category::Parked(_) => 2,
        Category::Excluded | Category::Package | Category::NotSynced(_) => 3,
    };
    let titles = ["Already in sync", "Would be transferred", "Needs your attention", "Left out"];
    for (g, title) in titles.iter().enumerate() {
        let rows: Vec<_> = report.tallies.iter().filter(|(c, _)| group(**c) == g).collect();
        if rows.is_empty() {
            continue;
        }
        println!("\n{title}:");
        for (c, t) in rows {
            let mut counts = Vec::new();
            if t.files > 0 {
                counts.push(format!("{} files, {}", t.files, size(t.bytes)));
            }
            if t.folders > 0 {
                counts.push(format!("{} folders", t.folders));
            }
            println!("  {}: {}", c.label(), counts.join("; "));
            for p in report.examples.get(c).into_iter().flatten() {
                println!("      e.g. {p}");
            }
        }
    }
    if report.hashed_files > 0 {
        println!("\nCompared the contents of {} files ({}).", report.hashed_files, size(report.hashed_bytes));
    }
    let unread = report.tally(Category::SameSize(Unchecked::NotRequested)).files;
    match (&basis.other_engine, unread) {
        (_, 0) => {}
        (None, _) => {
            println!("\nRun `oneup plan --verify` to compare the contents of same-size files (reads them from disk).")
        }
        // Ordinary files inside another app's folder are usually ones it
        // hasn't taken over yet (e.g. still waiting to upload them).
        (Some(p), n) => println!(
            "\n{n} same-size files here aren't {p}'s cloud files yet (often: still waiting for {p} to process \
             them). Their contents can be compared once {p} no longer manages this folder."
        ),
    }
    Ok(())
}

/// Explains, in plain words, that another sync app owns (part of) the folder.
fn print_other_engine(other: &oneup_local::coexist::OtherEngine) {
    use oneup_local::coexist::OtherEngine;
    let what = match other {
        OtherEngine::Inside { provider } => format!("This folder is synced by {provider}."),
        OtherEngine::Contains { provider, folder } => {
            format!("This folder contains {}, which is synced by {provider}.", folder.display())
        }
    };
    println!(
        "  Note: {what} Scanning only reads it, which is safe. OneUpDrive will not sync a folder\n  \
         \x20     that another sync app manages; see the migration guide before switching."
    );
}

/// Builds the first enumeration's hint from the sources that match the
/// current exclusion rules, and tells the user which ones are used.
fn enumeration_hint(store: &Store, fingerprint: &str, previous_excluded: Vec<String>) -> Result<Option<SplitHint>> {
    let mut hint = SplitHint::default();
    let mut used = Vec::new();
    if let Some(root) = store.read(|o| o.local_root())? {
        if root.finished_at_ms.is_none() {
            println!("The last folder scan is unfinished; not using it to speed up the mirror.");
        } else if root.rules != fingerprint {
            println!("The last folder scan used different exclusions; not using it to speed up the mirror.");
        } else {
            let paths = store.read(|o| o.local_excluded_paths())?;
            used.push(format!("{} excluded folders from the scan of {}", paths.len(), root.path));
            hint.add(paths);
        }
    }
    if !previous_excluded.is_empty() {
        used.push(format!("{} from the previous mirror", previous_excluded.len()));
        hint.add(previous_excluded);
    }
    if hint.is_empty() {
        return Ok(None);
    }
    println!(
        "Using {} to skip junk: the {} folders leading to them are listed, everything else is fetched in one go \
         (--no-hints to turn off).",
        used.join(" and "),
        hint.folders_to_list()
    );
    if hint.is_truncated() {
        println!("  (Too many to remember all of them; the rest are handled the usual way.)");
    }
    Ok(Some(hint))
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Starting => "starting",
        Phase::Enumerating => "enumerating",
        Phase::ResolvingOrphans => "placing orphans",
        Phase::CatchingUp => "catching up",
        Phase::Verifying => "verifying",
        Phase::Done => "done",
    }
}

fn status(cli: &Cli, profile: MemoryProfile) -> Result<()> {
    if !db_path(cli)?.exists() {
        println!("No local mirror yet. Run `oneup login`, then `oneup mirror`.");
        return Ok(());
    }
    let store = open_store(cli, profile)?;
    let (st, cursor, rules) = store.read(|o| Ok((o.stats()?, o.cursor()?, o.meta_get("rules")?)))?;
    println!("Local mirror: {}", db_path(cli)?.display());
    println!("  files             {}  ({:.1} GiB)", st.files, st.file_bytes as f64 / (1u64 << 30) as f64);
    println!(
        "  folders           {}  (+{} excluded, {} pruned inside them)",
        st.folders, st.excluded_folders, st.pruned_folders
    );
    println!("  packages          {}", st.packages);
    if st.units_pending > 0 || st.orphans > 0 {
        println!(
            "  unfinished        {} units pending, {} entries waiting for parents (run `oneup mirror`)",
            st.units_pending, st.orphans
        );
    }
    match cursor {
        None => println!("  state             not started"),
        Some(c) => {
            let age_s = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
                - c.taken_at_ms)
                / 1000;
            let state = match c.state {
                oneup_store::CursorState::Current => "up to date",
                oneup_store::CursorState::Pending => "initial mirror in progress",
            };
            println!("  state             {state} (last sync position {age_s} s ago)");
        }
    }
    println!("  exclusions        {}", rules.unwrap_or_else(|| "(none recorded)".into()));
    println!("  database          {:.1} MiB", st.db_bytes as f64 / 1_048_576.0);

    let (root, local) = store.read(|o| Ok((o.local_root()?, o.local_stats()?)))?;
    match root {
        None => println!("\nNo folder scanned yet (`oneup scan <folder>`)."),
        Some(r) => {
            println!("\nLast folder scan: {}", r.path);
            println!(
                "  state             {}",
                if r.finished_at_ms.is_some() { "complete" } else { "unfinished (run `oneup scan` again to resume)" }
            );
            println!("  files             {}  ({:.1} GiB)", local.files, local.file_bytes as f64 / GIB);
            println!(
                "  folders           {}  (+{} excluded)",
                local.folders - local.excluded_folders,
                local.excluded_folders
            );
            if let Some(p) = r.sync_root_provider {
                println!("  synced by         {p} (OneUpDrive won't manage this folder while that's the case)");
            }
        }
    }
    Ok(())
}

fn diagnose(cli: &Cli, top: usize, profile: MemoryProfile) -> Result<()> {
    let store = open_store(cli, profile)?;
    let (units, (list_total, delta_total), st) =
        store.read(|o| Ok((o.heaviest_units(top)?, o.unit_totals()?, o.stats()?)))?;
    let kept = st.files + st.folders + st.packages + st.excluded_folders;
    let downloaded = list_total + delta_total;
    println!("Initial enumeration downloads (from the last full mirror):");
    println!("  listing units     {list_total} entries");
    println!("  delta units       {delta_total} entries");
    println!(
        "  kept              {kept} ({:.0}% of downloads; the rest was junk dropped after download)",
        100.0 * kept as f64 / downloaded.max(1) as f64
    );
    println!("\nHeaviest units:");
    println!("  {:>9}  {:<5} {:>5}  path", "entries", "kind", "depth");
    for u in units {
        let kind = match u.kind {
            oneup_store::UnitKind::List => "list",
            oneup_store::UnitKind::Delta => "delta",
        };
        println!("  {:>9}  {:<5} {:>5}  {}", u.items, kind, u.depth, u.path);
    }
    Ok(())
}

fn verify(cli: &Cli, profile: MemoryProfile) -> Result<()> {
    let store = open_store(cli, profile)?;
    let problems = store.read(|o| o.check_invariants())?;
    if problems.is_empty() {
        println!("Mirror and folder scan are consistent.");
        Ok(())
    } else {
        for p in &problems {
            println!("  problem: {p}");
        }
        bail!(
            "{} consistency problem(s); `oneup mirror --rebuild` rebuilds the mirror, and running `oneup scan` \
             again rebuilds a folder scan (problems marked \"local:\")",
            problems.len()
        )
    }
}
