//! Where a hook's program is, and whether it may run from there.
//!
//! A program in a synced folder, or anywhere a cloud sync app manages, can
//! be replaced by a change in the cloud: a hook running it would turn that
//! change into code running on this PC. So a hook's program must be
//! outside every folder any of the user's pairs syncs (main, vault and
//! extra pairs, of every account), outside every folder another sync app
//! registered with Windows, and outside the OneDrive app's folders. The
//! path is resolved first (symbolic links and junctions followed) and
//! compared both as written and as resolved, case-insensitively.
//!
//! Relative paths are refused; a bare name (`powershell.exe`) is looked up
//! on `PATH` here, never by Windows, and the file found must pass the same
//! check. The agent starts the file this returns, by its full path.

use std::path::{Path, PathBuf};

use oneup_local::coexist::{self, OtherEngine};

/// Why a hook's program may not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refusal(pub String);

/// The folders this PC syncs with OneUpDrive, each with what syncs it ("the
/// vault Taxes"), as the caller found them (every data folder's pairs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Places {
    pub synced: Vec<(PathBuf, String)>,
}

impl Places {
    /// Adds each folder in its resolved form too, so a folder reached
    /// through a link is recognised either way.
    pub fn new(synced: Vec<(PathBuf, String)>) -> Self {
        let mut all = Vec::with_capacity(synced.len() * 2);
        for (p, what) in synced {
            if let Ok(real) = std::fs::canonicalize(&p)
                && real != p
            {
                all.push((real, what.clone()));
            }
            all.push((p, what));
        }
        Self { synced: all }
    }
}

/// The file `program` names: a full path as given, or a bare name found on
/// `PATH` (with `.exe` added when it has no extension).
pub fn resolve(program: &str) -> Result<PathBuf, Refusal> {
    let program = program.trim();
    let p = Path::new(program);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    if program.contains(['\\', '/', ':']) || program.is_empty() {
        return Err(Refusal(format!("{program}: give the program's full path (relative paths are refused)")));
    }
    let names: Vec<String> = if p.extension().is_some() {
        vec![program.to_string()]
    } else {
        vec![format!("{program}.exe"), program.to_string()]
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        // A relative entry would depend on the agent's current folder.
        if !dir.is_absolute() {
            continue;
        }
        for n in &names {
            let candidate = dir.join(n);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(Refusal(format!("{program} isn't on PATH; give its full path")))
}

/// Whether `path` (as written or resolved) is somewhere a cloud change can
/// replace it, and if so, where.
fn cloud_place(path: &Path, places: &Places) -> Option<String> {
    for (folder, what) in &places.synced {
        if coexist::is_within(path, folder) {
            return Some(format!("{} ({what})", coexist::plain(folder).display()));
        }
    }
    if let Some(info) = oneup_cfapi::sync_root_containing(path) {
        let who = if info.provider_name.is_empty() { "a cloud sync app".into() } else { info.provider_name };
        return Some(format!("a folder {who} syncs"));
    }
    match coexist::other_engine(path) {
        Some(OtherEngine::Inside { provider }) => Some(format!("a folder {provider} syncs")),
        _ => None,
    }
}

/// The program to start for a hook, by its full, resolved path; refused if
/// it can't be found or a change in a cloud folder could replace it.
pub fn check(program: &str, places: &Places) -> Result<PathBuf, Refusal> {
    let given = resolve(program)?;
    let real = std::fs::canonicalize(&given).map_err(|e| Refusal(format!("{}: {e}", given.display())))?;
    if !real.is_file() {
        return Err(Refusal(format!("{} isn't a program file", given.display())));
    }
    for p in [&given, &real] {
        if let Some(place) = cloud_place(p, places) {
            return Err(Refusal(format!(
                "{} is in {place}: a change in the cloud could replace it, so it never runs as a hook. \
                 Move it to a folder that isn't synced",
                given.display()
            )));
        }
    }
    Ok(coexist::plain(&real))
}

/// Arguments that name a file in a synced folder (a script for the
/// program to run, as in `powershell.exe -File C:\OneDrive\x.ps1`): refused
/// like a program there. Only literal text is checked; templates are the
/// event's data.
pub fn check_args(args: &[String], places: &Places) -> Result<(), Refusal> {
    for a in args {
        if a.contains('{') {
            continue;
        }
        let candidates = [Some(a.as_str()), a.split_once('=').map(|(_, v)| v)];
        for c in candidates.into_iter().flatten() {
            let p = Path::new(c.trim().trim_matches('"'));
            if !p.is_absolute() {
                continue;
            }
            let real = std::fs::canonicalize(p).ok();
            for q in std::iter::once(p).chain(real.as_deref()) {
                if let Some(place) = cloud_place(q, places) {
                    return Err(Refusal(format!(
                        "{} is in {place}: a change in the cloud could replace what the hook runs. \
                         Move it to a folder that isn't synced",
                        p.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("oneup-hooks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    fn mklink(args: &[&std::ffi::OsStr]) {
        let ok = std::process::Command::new("cmd.exe")
            .arg("/c")
            .arg("mklink")
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "mklink {args:?}");
    }

    /// A program in a synced folder is refused, however it's reached: by
    /// its path, in another case, through a junction or a symbolic link
    /// from outside, or as an argument (a script).
    #[test]
    fn a_program_in_a_synced_folder_is_refused_with_a_clear_message() {
        let t = tmp("refused");
        let synced = t.0.join("Synced");
        let tools = t.0.join("Tools");
        std::fs::create_dir_all(synced.join("bin")).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let exe = synced.join("bin").join("run.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        std::fs::write(tools.join("ok.exe"), b"MZ").unwrap();
        let places = Places::new(vec![(synced.clone(), "your main synced folder".into())]);

        let e = check(&exe.to_string_lossy(), &places).unwrap_err();
        assert!(e.0.contains("is in") && e.0.contains("your main synced folder"), "{e}");
        assert!(e.0.contains("never runs as a hook"), "{e}");
        let upper = exe.to_string_lossy().to_uppercase();
        assert!(check(&upper, &places).is_err(), "case doesn't matter");
        assert_eq!(check(&tools.join("ok.exe").to_string_lossy(), &places).unwrap(), tools.join("ok.exe"));

        // A junction outside, into the synced folder.
        let junction = t.0.join("Linked");
        mklink(&["/J".as_ref(), junction.as_os_str(), synced.join("bin").as_os_str()]);
        assert!(check(&junction.join("run.exe").to_string_lossy(), &places).is_err(), "through a junction");
        // A symbolic link to the file, if this account may make one
        // (developer mode or admin); otherwise the junction covers links.
        let link = tools.join("link.exe");
        let made = std::process::Command::new("cmd.exe")
            .arg("/c")
            .arg("mklink")
            .arg(&link)
            .arg(&exe)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if made {
            assert!(check(&link.to_string_lossy(), &places).is_err(), "through a symbolic link");
        }

        // A script in the synced folder, run by a program outside it.
        let script = synced.join("x.ps1");
        let args = vec!["-File".to_string(), script.to_string_lossy().into_owned(), "{path}".into()];
        assert!(check_args(&args, &places).unwrap_err().0.contains("could replace what the hook runs"));
        let args = vec![format!("--script={}", script.display())];
        assert!(check_args(&args, &places).is_err());
        assert!(check_args(&["-File".into(), tools.join("x.ps1").to_string_lossy().into_owned()], &places).is_ok());
    }

    #[test]
    fn relative_paths_are_refused_and_bare_names_found_on_path() {
        assert!(resolve(r"tools\x.exe").unwrap_err().0.contains("full path"));
        assert!(resolve(r".\x.exe").is_err());
        assert!(resolve("").is_err());
        let found = resolve("cmd").unwrap();
        assert!(found.to_string_lossy().to_ascii_lowercase().ends_with(r"\cmd.exe"), "{found:?}");
        assert!(resolve("no-such-program-oneup").unwrap_err().0.contains("isn't on PATH"));
        assert!(check(r"C:\no\such\program.exe", &Places::default()).is_err());
    }
}
