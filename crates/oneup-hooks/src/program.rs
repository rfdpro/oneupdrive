//! Where a hook's program is, and whether it may run from there.
//!
//! The safety rule ([`docs/architecture/data-safety.md`] invariant 17): a
//! change in the cloud must never become code that runs. Two ways it could:
//!
//! * the program itself (or a script an interpreter runs) living where a
//!   cloud change can replace it: inside a folder any pair of ours syncs, or
//!   one another sync app manages. [`cloud_place`] finds that; the program
//!   is resolved first (absolute path, or a bare name looked up on `PATH` by
//!   us, never by Windows), symlinks and junctions followed, compared as
//!   written and as resolved, case-insensitively.
//! * a value a cloud change controls (a shared file's name in `{path}`, an
//!   error in `{detail}`) landing where an interpreter treats it as code.
//!   [`check`] classifies known interpreters and loaders ([`interpreter`])
//!   and allows a template only as a data argument *after* a literal script
//!   file that is itself outside every synced folder; a program or shape it
//!   can't classify as safe is refused.
//!
//! The paths [`check`] returns (the program, and any script) are opened and
//! re-checked against a hard link and a swap immediately before the launch
//! (`oneup_sys::contained`), so nothing moved in between.

use std::path::{Path, PathBuf};

use oneup_local::coexist::{self, OtherEngine};

/// Why a hook's program may not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refusal(pub String);

/// The folders this PC syncs with OneUpDrive, each with what syncs it ("the
/// vault Taxes"), and a safe working folder for hook programs (OneUpDrive's
/// data folder, never synced). `complete` is false when the set of synced
/// folders couldn't be worked out in full: hooks are then refused, never
/// run against a folder we might have missed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Places {
    pub synced: Vec<(PathBuf, String)>,
    /// Whether [`Places::synced`] is the whole set.
    pub complete: bool,
    /// Where a hook runs (its current folder); must not be synced.
    pub cwd: PathBuf,
    /// A `PATH` with synced folders removed, for the hook's environment.
    pub safe_path: Option<std::ffi::OsString>,
}

impl Places {
    /// Adds each folder in its resolved form too, so a folder reached
    /// through a link is recognised either way.
    pub fn new(
        synced: Vec<(PathBuf, String)>,
        complete: bool,
        cwd: PathBuf,
        safe_path: Option<std::ffi::OsString>,
    ) -> Self {
        let mut all = Vec::with_capacity(synced.len() * 2);
        for (p, what) in synced {
            if let Ok(real) = std::fs::canonicalize(&p)
                && real != p
            {
                all.push((real, what.clone()));
            }
            all.push((p, what));
        }
        Self { synced: all, complete, cwd, safe_path }
    }
}

/// What a hook will start: the program and any scripts, by full resolved
/// paths, to open and guard at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub program: PathBuf,
    /// Scripts an interpreter runs (verified outside synced folders).
    pub scripts: Vec<PathBuf>,
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

/// A file (the program or a script), resolved and shown not to be anywhere
/// a cloud change can replace it. Returns its canonical path, to guard at
/// launch.
fn safe_file(given: &Path, what: &str, places: &Places) -> Result<PathBuf, Refusal> {
    let real = std::fs::canonicalize(given).map_err(|e| Refusal(format!("{what} {}: {e}", given.display())))?;
    if !real.is_file() {
        return Err(Refusal(format!("{what} {} isn't a file", given.display())));
    }
    for p in [given, real.as_path()] {
        if let Some(place) = cloud_place(p, places) {
            return Err(Refusal(format!(
                "{what} {} is in {place}: a change in the cloud could replace it, so it never runs as a hook. \
                 Move it to a folder that isn't synced",
                given.display()
            )));
        }
    }
    Ok(real)
}

/// The program to start for a hook (and any script it runs), by full
/// resolved paths; refused if anything can't be found, a change in a cloud
/// folder could replace it, or an argument could make a cloud value run as
/// code. `cwd` is where a relative script argument is resolved from (the
/// hook's working folder).
pub fn check(program: &str, args: &[String], cwd: &Path, places: &Places) -> Result<Checked, Refusal> {
    // The synced folders must be known in full, or a program could sit in
    // one we didn't see.
    if !places.complete {
        return Err(Refusal(
            "OneUpDrive can't list all the folders it syncs right now, so it won't run hooks (it can't be sure a \
             program isn't in one). Try again in a moment"
                .into(),
        ));
    }
    let given = resolve(program)?;
    let program_path = safe_file(&given, "the program", places)?;
    let scripts = check_args(&program_path, args, cwd, places)?;
    Ok(Checked { program: program_path, scripts })
}

/// What the interpreter treats its arguments as, so a cloud value never
/// lands where it's run as code.
struct Interpreter {
    /// Flags whose value, or whose presence, runs arguments as code: never
    /// allowed for a hook (compared whole, case-insensitively).
    code_flags: &'static [&'static str],
    /// The script file is the value of one of these flags (e.g. `-File`),
    /// or, when empty, the first argument that isn't a switch.
    script_flags: &'static [&'static str],
    /// Switches (value-less) allowed before the script for a first-positional
    /// interpreter; an unrecognised token before the script is refused.
    /// Empty with `script_flags` set (that form finds the script by flag).
    safe_switches: &'static [&'static str],
}

/// A program whose arguments it reads as a command line, so a cloud value
/// anywhere in them could run: allowed only with no templates at all.
fn is_command_shell(stem: &str) -> bool {
    matches!(
        stem,
        "cmd"
            | "wsl"
            | "wslconfig"
            | "rundll32"
            | "regsvr32"
            | "regsvcs"
            | "installutil"
            | "mshta"
            | "msiexec"
            | "wmic"
            | "forfiles"
            | "hh"
            | "powershell_ise"
            | "scriptrunner"
            | "dllhost"
            | "conhost"
    )
}

/// The known interpreter `stem` is, if it's one with a safe script-file
/// shape (`None`: not a known script interpreter).
fn interpreter(stem: &str) -> Option<Interpreter> {
    Some(match stem {
        "powershell" | "pwsh" => Interpreter {
            code_flags: &["-command", "-c", "-e", "-ec", "-enc", "-encodedcommand", "/command", "/c"],
            script_flags: &["-file", "-f"],
            safe_switches: &[],
        },
        "wscript" | "cscript" => Interpreter { code_flags: &[], script_flags: &[], safe_switches: &[] },
        "python" | "python3" | "py" | "pythonw" => Interpreter {
            code_flags: &["-c", "-m"],
            script_flags: &[],
            safe_switches: &["-b", "-bb", "-e", "-s", "-i", "-o", "-oo", "-q", "-u", "-v", "-x", "-s", "-ss"],
        },
        "node" | "nodejs" => Interpreter {
            code_flags: &["-e", "--eval", "-p", "--print", "--input-type"],
            script_flags: &[],
            safe_switches: &[],
        },
        "ruby" => Interpreter { code_flags: &["-e"], script_flags: &[], safe_switches: &["-w", "-v"] },
        "perl" => Interpreter { code_flags: &["-e", "-e"], script_flags: &[], safe_switches: &["-w"] },
        "php" => Interpreter { code_flags: &["-r", "-a"], script_flags: &["-f"], safe_switches: &[] },
        "bash" | "sh" | "zsh" | "dash" => {
            Interpreter { code_flags: &["-c"], script_flags: &[], safe_switches: &["-l", "-i", "-e", "-x"] }
        }
        _ => return None,
    })
}

/// Checks a hook's arguments and returns the scripts an interpreter runs,
/// to guard at launch. `program` is the resolved program.
fn check_args(program: &Path, args: &[String], cwd: &Path, places: &Places) -> Result<Vec<PathBuf>, Refusal> {
    let has_template = |a: &str| a.contains('{');
    let stem = program
        .file_name()
        .map(|f| f.to_string_lossy().to_ascii_lowercase())
        .map(|f| f.trim_end_matches(".exe").to_string())
        .unwrap_or_default();

    // A command shell: a cloud value anywhere in the line could run, so only
    // a line with no templates is allowed (the user's own literal command).
    if is_command_shell(&stem) {
        if let Some(a) = args.iter().find(|a| has_template(a)) {
            return Err(Refusal(format!(
                "{stem} reads its arguments as a command, so a value from the cloud in `{a}` could run. \
                 Run a program directly, or an interpreter with a script file (e.g. powershell.exe -File script.ps1)"
            )));
        }
        // A literal file it names, if any, still mustn't be in a synced
        // folder (its contents would be cloud-controlled).
        return literal_scripts_outside(args, cwd, places);
    }

    let Some(interp) = interpreter(&stem) else {
        // An ordinary program: it gets templates as data arguments (argv),
        // never as code. A literal absolute path it reads is data, not code,
        // so it isn't restricted; only interpreters and shells are.
        return Ok(Vec::new());
    };

    // No code flag, ever (abbreviations of powershell's -Command included via
    // its own set; we also refuse an unrecognised switch before the script).
    for a in args {
        let low = a.to_ascii_lowercase();
        if interp.code_flags.iter().any(|f| low == *f) {
            return Err(Refusal(format!(
                "{stem} runs the value of {a} as code, which a cloud value could reach. Put your code in a script \
                 file and run that file instead"
            )));
        }
    }

    // Find the script argument.
    let script_index = if !interp.script_flags.is_empty() {
        // The token after the script flag.
        let flag = args.iter().position(|a| {
            let low = a.to_ascii_lowercase();
            interp.script_flags.iter().any(|f| low == *f)
        });
        let Some(flag) = flag else {
            return Err(Refusal(format!(
                "a {stem} hook must run a script file: put {} before it (e.g. {stem} {} script.ps1 {{path}})",
                interp.script_flags[0], interp.script_flags[0]
            )));
        };
        // Nothing templated before or at the flag's value.
        if args[..flag].iter().any(|a| has_template(a)) {
            return Err(Refusal(format!("a template before {} could choose what {stem} runs", interp.script_flags[0])));
        }
        flag + 1
    } else {
        // The first token that isn't a safe switch is the script.
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if a.starts_with('-') || a.starts_with('/') {
                let low = a.to_ascii_lowercase();
                if !interp.safe_switches.iter().any(|s| low == *s) {
                    return Err(Refusal(format!(
                        "{stem}: OneUpDrive doesn't recognise {a} as safe before the script. Run a script file with \
                         only plain switches before it"
                    )));
                }
                if has_template(a) {
                    return Err(Refusal(format!("a template in {a} could reach {stem} as code")));
                }
                i += 1;
                continue;
            }
            break;
        }
        i
    };

    let Some(script) = args.get(script_index) else {
        return Err(Refusal(format!(
            "a {stem} hook must name a script file to run (OneUpDrive won't let it read code from its input or a \
             cloud value)"
        )));
    };
    if has_template(script) {
        return Err(Refusal(format!(
            "the script {stem} runs can't be a template like {script}: a cloud value could choose the file. Name a \
             fixed script file; pass {{path}} after it as an argument"
        )));
    }
    // The script must be a real file outside every synced folder.
    let path = resolve_literal(script, cwd);
    let real = safe_file(&path, "the script", places)?;
    // Everything after the script is data to it (templates allowed).
    Ok(vec![real])
}

/// A literal argument as a path: absolute as given, or relative to `cwd`.
fn resolve_literal(arg: &str, cwd: &Path) -> PathBuf {
    let a = arg.trim().trim_matches('"');
    let p = Path::new(a);
    if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
}

/// For a command shell with no templates: any literal argument that is a
/// file in a synced folder is refused (its contents are cloud-controlled),
/// and such files are returned to guard. Arguments that aren't paths are
/// left alone.
fn literal_scripts_outside(args: &[String], cwd: &Path, places: &Places) -> Result<Vec<PathBuf>, Refusal> {
    let mut scripts = Vec::new();
    for a in args {
        let candidates = [Some(a.as_str()), a.split_once('=').map(|(_, v)| v), a.split_once(':').map(|(_, v)| v)];
        for c in candidates.into_iter().flatten() {
            let p = resolve_literal(c, cwd);
            if !p.is_absolute() {
                continue;
            }
            if let Ok(real) = std::fs::canonicalize(&p) {
                if real.is_file()
                    && let Some(place) = cloud_place(&real, places).or_else(|| cloud_place(&p, places))
                {
                    return Err(Refusal(format!(
                        "{} is in {place}: a change in the cloud could replace what the hook runs. Move it to a \
                         folder that isn't synced",
                        p.display()
                    )));
                }
                if real.is_file() && (c.ends_with(".bat") || c.ends_with(".cmd") || c.ends_with(".ps1")) {
                    scripts.push(real);
                }
            }
        }
    }
    Ok(scripts)
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

    fn places(synced: Vec<(PathBuf, String)>) -> Places {
        Places::new(synced, true, std::env::temp_dir(), None)
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

    /// A program in a synced folder is refused, however it's reached: by its
    /// path, in another case, through a junction or a symbolic link, or as a
    /// script an interpreter runs.
    #[test]
    fn a_program_or_script_in_a_synced_folder_is_refused() {
        let t = tmp("refused");
        let synced = t.0.join("Synced");
        let tools = t.0.join("Tools");
        std::fs::create_dir_all(synced.join("bin")).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let exe = synced.join("bin").join("run.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        std::fs::write(tools.join("ok.exe"), b"MZ").unwrap();
        let pl = places(vec![(synced.clone(), "your main synced folder".into())]);

        let e = check(&exe.to_string_lossy(), &[], &t.0, &pl).unwrap_err();
        assert!(e.0.contains("your main synced folder") && e.0.contains("never runs as a hook"), "{e}");
        assert!(check(&exe.to_string_lossy().to_uppercase(), &[], &t.0, &pl).is_err(), "case doesn't matter");
        assert_eq!(
            check(&tools.join("ok.exe").to_string_lossy(), &[], &t.0, &pl).unwrap().program,
            std::fs::canonicalize(tools.join("ok.exe")).unwrap()
        );

        // A junction outside, into the synced folder.
        let junction = t.0.join("Linked");
        mklink(&["/J".as_ref(), junction.as_os_str(), synced.join("bin").as_os_str()]);
        assert!(check(&junction.join("run.exe").to_string_lossy(), &[], &t.0, &pl).is_err(), "through a junction");

        // A PowerShell script in the synced folder.
        let ps = synced.join("x.ps1");
        std::fs::write(&ps, b"echo hi").unwrap();
        let args = vec!["-File".to_string(), ps.to_string_lossy().into_owned(), "{path}".into()];
        let e = check("powershell.exe", &args, &tools, &pl).unwrap_err();
        assert!(e.0.contains("could replace it"), "{e}");
        // The same script outside is fine, and returned to guard.
        let ps2 = tools.join("x.ps1");
        std::fs::write(&ps2, b"echo hi").unwrap();
        let ok = check(
            "powershell.exe",
            &["-File".into(), ps2.to_string_lossy().into_owned(), "{path}".into()],
            &tools,
            &pl,
        )
        .unwrap();
        assert_eq!(ok.scripts, vec![std::fs::canonicalize(&ps2).unwrap()]);
    }

    #[test]
    fn interpreters_refuse_code_from_the_cloud_but_allow_a_script_with_data() {
        let t = tmp("interp");
        let tools = t.0.join("Tools");
        std::fs::create_dir_all(&tools).unwrap();
        let script = |name: &str| {
            let p = tools.join(name);
            std::fs::write(&p, b"x").unwrap();
            p.to_string_lossy().into_owned()
        };
        let synced = t.0.join("Synced");
        std::fs::create_dir_all(&synced).unwrap();
        std::fs::write(synced.join("evil.py"), b"x").unwrap();
        let pl = places(vec![(synced.clone(), "the folder pair Work".into())]);
        let hook =
            |prog: &str, a: &[&str]| check(prog, &a.iter().map(|s| s.to_string()).collect::<Vec<_>>(), &tools, &pl);

        // cmd / shells: a template anywhere is refused; a plain command is fine.
        assert!(hook("cmd.exe", &["/c", "echo", "{path}"]).unwrap_err().0.contains("reads its arguments as a command"));
        assert!(hook("wsl.exe", &["{path}"]).is_err());
        assert!(hook("mshta.exe", &["{path}"]).is_err());
        assert!(hook("rundll32.exe", &["{path}"]).is_err());
        assert!(hook("regsvr32.exe", &["/s", "{path}"]).is_err());

        // powershell: -Command refused; -File <literal outside> ok; templated script refused.
        assert!(hook("powershell.exe", &["-Command", "Write-Host {path}"]).unwrap_err().0.contains("runs the value"));
        assert!(hook("powershell.exe", &["-EncodedCommand", "{path}"]).is_err());
        assert!(hook("powershell.exe", &["-NoProfile", "-File", &script("a.ps1"), "{path}"]).is_ok());
        assert!(hook("powershell.exe", &["-File", "{path}"]).unwrap_err().0.contains("can't be a template"));
        // A template standing in for a switch before -File is refused.
        assert!(hook("powershell.exe", &["{path}", "-File", &script("b.ps1")]).is_err());

        // python: first positional is the script; -c and -m refused; data after ok.
        assert!(hook("python.exe", &["-c", "import os"]).is_err());
        assert!(hook("python.exe", &["-m", "{path}"]).is_err());
        assert!(hook("python.exe", &[&script("s.py"), "{path}", "{event}"]).is_ok());
        assert!(hook("python.exe", &["{path}"]).unwrap_err().0.contains("can't be a template"));
        // An unknown switch before the script is refused (can't classify).
        assert!(hook("python.exe", &["-W", "ignore", &script("s2.py")]).is_err());

        // node -e refused; node script ok.
        assert!(hook("node.exe", &["-e", "{path}"]).is_err());
        assert!(hook("node.exe", &["--eval", "x"]).is_err());
        assert!(hook("node.exe", &[&script("s.js"), "{detail}"]).is_ok());

        // wscript/cscript: first positional script.
        assert!(hook("wscript.exe", &["{path}"]).unwrap_err().0.contains("can't be a template"));
        assert!(hook("cscript.exe", &["//nologo", &script("s.vbs"), "{path}"]).is_err(), "// switch not recognised");
        assert!(hook("cscript.exe", &[&script("s2.vbs"), "{path}"]).is_ok());

        // A script in a synced folder, even literal, is refused.
        assert!(
            hook("python.exe", &[&synced.join("evil.py").to_string_lossy()])
                .unwrap_err()
                .0
                .contains("could replace it")
        );

        // An ordinary program gets {path} as data: allowed.
        let tool = tools.join("thumb.exe");
        std::fs::write(&tool, b"MZ").unwrap();
        assert!(check(&tool.to_string_lossy(), &["{path}".into()], &tools, &pl).unwrap().scripts.is_empty());
    }

    #[test]
    fn a_relative_script_resolves_against_the_working_folder() {
        let t = tmp("relscript");
        let synced = t.0.join("Synced");
        std::fs::create_dir_all(&synced).unwrap();
        std::fs::write(synced.join("s.py"), b"x").unwrap();
        // cwd is the synced folder: a relative script resolves into it and is refused.
        let pl = places(vec![(synced.clone(), "the main folder".into())]);
        assert!(check("python.exe", &["s.py".into()], &synced, &pl).unwrap_err().0.contains("could replace it"));
    }

    #[test]
    fn incomplete_discovery_refuses_every_hook() {
        let mut pl = places(vec![]);
        pl.complete = false;
        assert!(
            check(r"C:\Windows\System32\cmd.exe", &[], Path::new(r"C:\"), &pl)
                .unwrap_err()
                .0
                .contains("can't list all the folders")
        );
    }

    #[test]
    fn relative_programs_are_refused_and_bare_names_found_on_path() {
        assert!(resolve(r"tools\x.exe").unwrap_err().0.contains("full path"));
        assert!(resolve(r".\x.exe").is_err());
        assert!(resolve("").is_err());
        let found = resolve("cmd").unwrap();
        assert!(found.to_string_lossy().to_ascii_lowercase().ends_with(r"\cmd.exe"), "{found:?}");
        assert!(resolve("no-such-program-oneup").unwrap_err().0.contains("isn't on PATH"));
    }
}
