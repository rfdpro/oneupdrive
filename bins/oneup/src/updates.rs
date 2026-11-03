//! OneUpDrive updating itself, with Velopack (MIT): releases on
//! GitHub, installed per user, downloaded in the background and applied
//! when nothing is syncing.
//!
//! * The release feed is the GitHub repository this build was made for
//!   (`ONEUP_UPDATE_REPO` at build time), or `[updates] repo`. `stable` sees
//!   releases, `preview` pre-releases too; with no choice made, a copy follows
//!   the channel its package was built for. Not installed with the installer
//!   (a developer build), or no repository: no updates.
//! * **An update is installed only if its programs are signed by the same
//!   publisher as the running one** (Authenticode, `oneup_sys::
//!   authenticode`), checked on the downloaded package before anything is
//!   replaced. The feed comes over HTTPS from GitHub; the signature check
//!   means a compromised feed alone can't install anything. An unsigned
//!   installation is never updated by itself.
//! * The tray (`oneup-tray`, the installed program) asks for it; applying
//!   stops the agents first and starts them again afterwards
//!   (`oneup-tray --start`).

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use velopack::bundle::Manifest;
use velopack::locator::{self, LocationContext};
use velopack::sources::{GithubSource, UpdateSource};
use velopack::{Error as VelopackError, UpdateCheck, UpdateInfo, UpdateManager, VelopackAsset, VelopackAssetFeed};

/// The repository releases come from, set when the release is built.
pub const BUILT_FOR: Option<&str> = option_env!("ONEUP_UPDATE_REPO");

const STABLE: &str = "stable";
const PREVIEW: &str = "preview";

/// Every release publishes one feed file, `releases.<channel>.json`, while
/// Velopack reads only the file of the one channel it's given. This source
/// reads the channels this copy wants and merges them, so a preview copy
/// also sees stable releases.
struct ChannelFeeds<S> {
    source: S,
    previews: bool,
}

impl<S: UpdateSource> UpdateSource for ChannelFeeds<S> {
    fn get_release_feed(
        &self,
        _channel: &str,
        app: &Manifest,
        staged_user_id: &str,
    ) -> Result<VelopackAssetFeed, VelopackError> {
        let channels: &[&str] = if self.previews { &[STABLE, PREVIEW] } else { &[STABLE] };
        let mut merged = Vec::new();
        for channel in channels {
            let feed = self.source.get_release_feed(channel, app, staged_user_id)?;
            merged.extend(usable_from(channel, &app.channel, feed.Assets));
        }
        Ok(VelopackAssetFeed { Assets: merged })
    }

    fn download_release_entry(
        &self,
        asset: &VelopackAsset,
        local_file: &Path,
        progress_sender: Option<std::sync::mpsc::Sender<i16>>,
    ) -> Result<(), VelopackError> {
        self.source.download_release_entry(asset, local_file, progress_sender)
    }
}

/// What a copy of `installed` channel can use from `channel`'s feed. A delta
/// is built against the previous release of its own channel: from any other
/// channel it wouldn't apply to what's installed, and Velopack chooses deltas
/// by version alone. Without one it takes the full package.
fn usable_from(channel: &str, installed: &str, assets: Vec<VelopackAsset>) -> Vec<VelopackAsset> {
    assets.into_iter().filter(|a| channel == installed || !a.Type.eq_ignore_ascii_case("Delta")).collect()
}

/// Whether this copy wants pre-releases: the choice in `[updates]`, else
/// what it was installed from.
pub fn previews_wanted(cfg: &crate::config::Updates) -> bool {
    wants_previews(cfg.channel.as_deref(), installed_channel().as_deref())
}

fn wants_previews(chosen: Option<&str>, installed: Option<&str>) -> bool {
    chosen.or(installed) == Some(PREVIEW)
}

/// The channel the running copy's package was built for; `None` for a
/// developer build.
fn installed_channel() -> Option<String> {
    locator::auto_locate_app_manifest(LocationContext::FromCurrentExe).ok().map(|l| l.get_manifest_channel())
}

/// An update downloaded and checked, ready to apply.
pub struct Ready {
    pub version: String,
    info: UpdateInfo,
    um: UpdateManager,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ready").field("version", &self.version).finish_non_exhaustive()
    }
}

/// The event the settings window raises to make `data_dir`'s tray look
/// for an update now (`oneup_sys::signal`).
pub fn check_signal(data_dir: &Path) -> String {
    crate::tray::instance_name(data_dir).replace("-tray-", "-update-check-")
}

/// The update manager for this installation; `None` when this isn't an
/// installed copy or there's nowhere to update from.
pub fn manager(data_dir: &Path) -> Result<Option<UpdateManager>> {
    let cfg = crate::config::load_updates(data_dir)?;
    let repo = if cfg.repo.is_empty() { BUILT_FOR.unwrap_or_default() } else { cfg.repo.as_str() };
    if repo.is_empty() || url_parts(repo).is_none() {
        return Ok(None);
    }
    let previews = previews_wanted(&cfg);
    let source = ChannelFeeds { source: GithubSource::new(repo, None, previews), previews };
    // Fails when this copy wasn't installed (a developer build).
    Ok(UpdateManager::new(source, None, None).ok())
}

/// `https://github.com/owner/repo` only.
fn url_parts(repo: &str) -> Option<(&str, &str)> {
    let rest = repo.strip_prefix("https://github.com/")?.trim_end_matches('/');
    let (owner, name) = rest.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then_some((owner, name))
}

/// Whether the tray may look for and download an update by itself (the
/// daily check): not on a metered network, a network with a data budget,
/// or while working offline, as for any background transfer. A check the
/// user asks for always goes.
pub fn may_download_by_itself(metered: bool, budgeted: bool, offline: bool) -> bool {
    !metered && !budgeted && !offline
}

/// [`may_download_by_itself`] for data folder `data_dir` now: the network
/// Windows says this PC is on, its profile's budget, and the agent's
/// offline mode.
#[cfg(windows)]
pub fn may_download_by_itself_now(data_dir: &Path) -> bool {
    let agent = crate::config::load_agent(data_dir).unwrap_or_default();
    let (net, profile) = crate::network::situation(data_dir, &agent);
    let offline = data_dir.join(crate::control::OFFLINE_FILE).exists();
    may_download_by_itself(net.metered, profile.budget_bytes().is_some(), offline)
}

/// How a download that may be stopped ended ([`run_stoppable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ran {
    /// It ended by itself; whether it succeeded.
    Finished(bool),
    /// A change made `stop` say no: it was ended.
    Stopped,
}

/// What [`run_stoppable`] hears: the child ended, or something changed
/// (the network) that `stop` judges.
#[derive(Debug)]
pub enum Heard {
    Ended(bool),
    Changed,
}

/// Runs `cmd` (the automatic update download, in a process of its own)
/// until it ends, or until a change sent on `events` makes `stop()` true:
/// then it's ended at once. A partial download is left as velopack's
/// `.partial` file, which its next download clears. Waits on the channel:
/// nothing is polled.
#[cfg(windows)]
pub fn run_stoppable(
    mut cmd: std::process::Command,
    events: std::sync::mpsc::Sender<Heard>,
    heard: std::sync::mpsc::Receiver<Heard>,
    stop: impl Fn() -> bool,
) -> std::io::Result<Ran> {
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let exe = oneup_sys::processes::image_of(pid);
    std::thread::Builder::new().name("update-download".into()).spawn(move || {
        let ok = child.wait().is_ok_and(|s| s.success());
        let _ = events.send(Heard::Ended(ok));
    })?;
    let mut stopping = false;
    loop {
        match heard.recv() {
            Ok(Heard::Ended(ok)) => return Ok(if stopping { Ran::Stopped } else { Ran::Finished(ok) }),
            Ok(Heard::Changed) if !stopping && stop() => {
                stopping = true;
                if let Some(exe) = exe.clone() {
                    let p = oneup_sys::processes::Process { pid, exe };
                    oneup_sys::processes::terminate(&p, 10_000).map_err(std::io::Error::other)?;
                }
            }
            Ok(Heard::Changed) => {}
            Err(_) => return Ok(Ran::Stopped),
        }
    }
}

/// Looks for a newer release; downloads and checks it if there is one.
/// Blocking (network): run it off the UI thread.
pub fn check(um: UpdateManager) -> Result<Option<Ready>> {
    let info = match um.check_for_updates().context("checking for updates")? {
        UpdateCheck::UpdateAvailable(info) => *info,
        _ => return Ok(None),
    };
    um.download_updates(&info, None).context("downloading the update")?;
    let package = packages_dir()?.join(local_package_name(&info.TargetFullRelease.FileName));
    if let Err(e) = verify(&package) {
        // Ours, just downloaded: never left to be applied by anyone.
        let _ = std::fs::remove_file(&package);
        return Err(e);
    }
    Ok(Some(Ready { version: info.TargetFullRelease.Version.clone(), info, um }))
}

/// Where Velopack keeps downloaded packages: `<install>\packages`, next to
/// `<install>\current` where the programs are.
fn packages_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let root = exe.parent().and_then(Path::parent).context("not an installed copy")?;
    Ok(root.join("packages"))
}

/// The name Velopack stores a feed entry under: the last path segment, with
/// characters Windows forbids replaced. Verifying any other path would check
/// a different file from the one applied. Velopack's own function is private.
fn local_package_name(feed_name: &str) -> String {
    let last = feed_name.rsplit(['/', '\\']).next().unwrap_or("");
    last.chars()
        .map(|c| if matches!(c, '"' | '<' | '>' | '|' | ':' | '*' | '?') || c < ' ' { '_' } else { c })
        .collect()
}

/// The package's `oneupd.exe` must be signed by the running program's
/// publisher.
fn verify(package: &Path) -> Result<()> {
    let mine = oneup_sys::authenticode::signer(&std::env::current_exe()?)?
        .context("this copy of OneUpDrive isn't signed, so it doesn't install updates by itself")?;
    let tmp = package.with_extension("verify.exe");
    copy_oneupd(package, &tmp)?;
    let theirs = oneup_sys::authenticode::signer(&tmp);
    let _ = std::fs::remove_file(&tmp);
    match theirs? {
        Some(s) if s == mine => Ok(()),
        Some(s) => bail!("the update is signed by {s}, not by {mine}; it wasn't installed"),
        None => bail!("the update isn't signed; it wasn't installed"),
    }
}

/// Copies the package's `oneupd.exe` to `to`. An entry whose name can't be
/// decoded is never the one we look for.
fn copy_oneupd(package: &Path, to: &Path) -> Result<()> {
    let file = std::fs::File::open(package).with_context(|| format!("opening {}", package.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("reading the update package")?;
    let name = (0..zip.len())
        .filter_map(|i| zip.name_for_index(i).and_then(Result::ok).map(std::borrow::Cow::into_owned))
        .find(|n| n.replace('\\', "/").to_lowercase().ends_with("/oneupd.exe"))
        .context("the update package has no oneupd.exe")?;
    let mut entry = zip.by_name(&name)?;
    let mut out = std::fs::File::create(to)?;
    // At most 256 MiB (it's a few MiB).
    std::io::copy(&mut (&mut entry).take(256 << 20), &mut out)?;
    Ok(())
}

/// Applies `ready` and restarts the tray with `args` (this process exits).
/// The caller stops the agents first.
pub fn apply(ready: Ready, args: Vec<String>) -> Result<()> {
    ready.um.apply_updates_and_restart_with_args(&ready.info, args).context("installing the update")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use velopack::locator::VelopackLocatorConfig;
    use velopack::sources::UpdateSource;
    use velopack::{UpdateCheck, UpdateManager, VelopackAsset, VelopackAssetFeed};

    use super::ChannelFeeds;

    fn asset(version: &str, kind: &str, channel: &str) -> VelopackAsset {
        VelopackAsset {
            PackageId: "OneUpDrive.App".into(),
            Version: version.into(),
            Type: kind.into(),
            FileName: format!("OneUpDrive.App-{version}-{channel}-{}.nupkg", kind.to_lowercase()),
            ..Default::default()
        }
    }

    /// Releases as GitHub serves them: a channel's file, and only if the
    /// source's pre-release filter lets its release through.
    #[derive(Clone)]
    struct Releases {
        previews: bool,
        stable: Vec<VelopackAsset>,
        preview: Vec<VelopackAsset>,
    }

    impl UpdateSource for Releases {
        fn get_release_feed(
            &self,
            channel: &str,
            _app: &velopack::bundle::Manifest,
            _staged_user_id: &str,
        ) -> Result<VelopackAssetFeed, velopack::Error> {
            let assets = match channel {
                "stable" => self.stable.clone(),
                "preview" if self.previews => self.preview.clone(),
                _ => Vec::new(),
            };
            Ok(VelopackAssetFeed { Assets: assets })
        }

        fn download_release_entry(
            &self,
            _asset: &VelopackAsset,
            _local_file: &std::path::Path,
            _progress_sender: Option<std::sync::mpsc::Sender<i16>>,
        ) -> Result<(), velopack::Error> {
            Ok(())
        }
    }

    /// What Velopack itself offers a copy of `version` installed from
    /// `installed`, with `chosen` in `[updates]`.
    fn offered(
        chosen: Option<&str>,
        installed: &str,
        version: &str,
        stable: &[VelopackAsset],
        preview: &[VelopackAsset],
    ) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("current")).unwrap();
        std::fs::create_dir_all(root.join("packages")).unwrap();
        std::fs::write(root.join("Update.exe"), b"").unwrap();
        std::fs::write(
            root.join("current").join("sq.version"),
            format!(
                "<?xml version=\"1.0\"?><package><metadata><id>OneUpDrive.App</id>\
                 <version>{version}</version><channel>{installed}</channel>\
                 <mainExe>oneup-tray.exe</mainExe></metadata></package>"
            ),
        )
        .unwrap();
        let config = VelopackLocatorConfig {
            RootAppDir: root.into(),
            UpdateExePath: root.join("Update.exe"),
            PackagesDir: root.join("packages"),
            ManifestPath: root.join("current").join("sq.version"),
            CurrentBinaryDir: root.join("current"),
            IsPortable: false,
        };
        let previews = super::wants_previews(chosen, Some(installed));
        let source = ChannelFeeds {
            source: Releases { previews, stable: stable.to_vec(), preview: preview.to_vec() },
            previews,
        };
        let um = UpdateManager::new(source, None, Some(config)).unwrap();
        match um.check_for_updates().unwrap() {
            UpdateCheck::UpdateAvailable(info) => Some(info.TargetFullRelease.Version.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_copy_follows_its_channel_until_a_choice_is_made() {
        use super::wants_previews as wants;
        assert!(wants(None, Some("preview")));
        assert!(!wants(None, Some("stable")));
        assert!(!wants(None, None));
        assert!(wants(Some("preview"), Some("stable")));
        assert!(!wants(Some("stable"), Some("preview")));
    }

    #[test]
    fn a_preview_copy_gets_previews_with_no_choice_made() {
        let preview = [asset("0.2.0-beta.2", "Full", "preview")];
        let got = offered(None, "preview", "0.2.0-beta.1", &[], &preview);
        assert_eq!(got.as_deref(), Some("0.2.0-beta.2"));
    }

    #[test]
    fn a_preview_copy_moves_on_to_the_stable_release() {
        let stable = [asset("0.2.0", "Full", "stable")];
        let preview = [asset("0.2.0-beta.2", "Full", "preview")];
        let got = offered(None, "preview", "0.2.0-beta.2", &stable, &preview);
        assert_eq!(got.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn a_stable_copy_gets_previews_only_when_chosen() {
        let stable = [asset("0.2.0", "Full", "stable")];
        let preview = [asset("0.3.0-beta.1", "Full", "preview")];
        assert_eq!(offered(None, "stable", "0.2.0", &stable, &preview), None);
        let got = offered(Some("preview"), "stable", "0.2.0", &stable, &preview);
        assert_eq!(got.as_deref(), Some("0.3.0-beta.1"));
    }

    #[test]
    fn a_preview_copy_set_to_stable_ignores_previews_and_never_goes_back() {
        let stable = [asset("0.1.9", "Full", "stable")];
        let preview = [asset("0.2.0-beta.2", "Full", "preview")];
        assert_eq!(offered(Some("stable"), "preview", "0.2.0-beta.1", &stable, &preview), None);
    }

    #[test]
    fn saving_other_settings_makes_no_channel_choice() {
        let dir = tempfile::tempdir().unwrap();
        let mut u = crate::config::Updates { auto: false, ..Default::default() };
        crate::config::save_updates(dir.path(), &u).unwrap();
        assert_eq!(crate::config::load_updates(dir.path()).unwrap().channel, None);

        u.channel = Some("stable".into());
        crate::config::save_updates(dir.path(), &u).unwrap();
        assert_eq!(crate::config::load_updates(dir.path()).unwrap().channel.as_deref(), Some("stable"));
    }

    #[test]
    fn deltas_only_come_from_the_installed_channel() {
        use super::usable_from;
        let assets = vec![asset("0.2.0", "Full", "stable"), asset("0.2.0", "Delta", "stable")];
        assert_eq!(usable_from("stable", "stable", assets.clone()).len(), 2);
        let other = usable_from("stable", "preview", assets);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].Type, "Full");
    }

    #[test]
    fn the_package_checked_is_the_one_velopack_applies() {
        use super::local_package_name as name;
        assert_eq!(name("App-1.0.0-full.nupkg"), "App-1.0.0-full.nupkg");
        assert_eq!(name("../../App-1.0.0-full.nupkg"), "App-1.0.0-full.nupkg");
        assert_eq!(name("..\\..\\App-1.0.0-full.nupkg"), "App-1.0.0-full.nupkg");
        assert_eq!(name("https://example.com/r/App-1.0.0-full.nupkg"), "App-1.0.0-full.nupkg");
        assert_eq!(name("App:1.0?.nupkg"), "App_1.0_.nupkg");
    }

    /// Writes a package with these entries; returns the folder that holds it.
    fn package(entries: &[(&str, &[u8])]) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("App-full.nupkg");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, deflated).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        (dir, path)
    }

    #[test]
    fn oneupd_is_read_from_the_update_package() {
        let (dir, path) = package(&[("lib/app/oneup.exe", b"other"), ("lib/app/OneUpd.exe", b"the agent")]);
        let out = dir.path().join("verify.exe");
        super::copy_oneupd(&path, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"the agent");
    }

    #[test]
    fn a_package_without_oneupd_is_refused() {
        let (dir, path) = package(&[("lib/app/oneup.exe", b"other"), ("notoneupd.exe", b"no")]);
        let out = dir.path().join("verify.exe");
        let err = super::copy_oneupd(&path, &out).unwrap_err();
        assert!(err.to_string().contains("no oneupd.exe"), "{err:#}");
        assert!(!out.exists());
    }

    /// The daily check holds back like background work: not on a metered
    /// network, with a data budget, or offline.
    #[test]
    fn a_change_that_says_stop_ends_the_download_at_once() {
        use super::{Heard, Ran, run_stoppable};
        let slow = || {
            let mut c = std::process::Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null());
            c
        };
        // A change that doesn't say stop leaves it; one that does ends it.
        let (tx, rx) = std::sync::mpsc::channel();
        let say = tx.clone();
        let started = std::time::Instant::now();
        let flips = std::sync::atomic::AtomicU32::new(0);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = say.send(Heard::Changed);
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = say.send(Heard::Changed);
        });
        let ran =
            run_stoppable(slow(), tx, rx, || flips.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 1).unwrap();
        assert_eq!(ran, Ran::Stopped);
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        // Left alone, it finishes by itself.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut quick = std::process::Command::new("cmd");
        quick.args(["/C", "exit 0"]);
        assert_eq!(run_stoppable(quick, tx, rx, || true).unwrap(), Ran::Finished(true));
    }

    #[test]
    fn updates_download_by_themselves_only_where_background_work_may() {
        use super::may_download_by_itself as may;
        assert!(may(false, false, false));
        assert!(!may(true, false, false) && !may(false, true, false) && !may(false, false, true));
    }
    #[test]
    fn only_github_repository_urls_are_used() {
        assert_eq!(super::url_parts("https://github.com/someone/oneupdrive"), Some(("someone", "oneupdrive")));
        assert_eq!(super::url_parts("https://github.com/someone/oneupdrive/"), Some(("someone", "oneupdrive")));
        assert_eq!(super::url_parts("http://github.com/someone/oneupdrive"), None);
        assert_eq!(super::url_parts("https://example.com/someone/oneupdrive"), None);
        assert_eq!(super::url_parts("https://github.com/someone"), None);
    }
}
