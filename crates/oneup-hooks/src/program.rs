//! Where a hook's program is, how it's launched, and whether a cloud value
//! can ever reach it as code.
//!
//! The rule ([`docs/architecture/data-safety.md`] invariant 17): **cloud
//! data never reaches a code position.** Two parts:
//!
//! * The program (and any script) must live outside every folder a cloud
//!   change can touch: a folder any pair of ours syncs, or one another sync
//!   app manages. The program is resolved (absolute, or a bare name looked
//!   up on `PATH` by us), symlinks and junctions followed, every hard link
//!   enumerated (a second name inside a synced folder is refused too), and
//!   compared case-insensitively.
//! * Template values (`{path}`, `{detail}`, …) carry text a cloud change
//!   controls. They are allowed in `args` **only** when the program is a
//!   real `.exe` that isn't a known shell, interpreter or loader
//!   ([`is_interpreter`]). For an interpreter, or any non-`.exe` program,
//!   templates in `args` are refused at load; the event reaches the program
//!   as data instead (environment variables and a JSON line on standard
//!   input, `oneup_hooks::runner`). Literal arguments (the user's own switches
//!   and script path) stay allowed, and a literal script file must itself be
//!   outside every synced folder.
//!
//! The paths [`check`] returns (the program and any script) are opened and
//! re-checked against a swap immediately before the launch
//! (`oneup_sys::contained`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use oneup_local::coexist::{self, OtherEngine};

/// Why a hook's program may not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refusal(pub String);

/// The folders this PC syncs with OneUpDrive, each with what syncs it ("the
/// vault Taxes"), a safe working folder for hook programs (the data folder,
/// never synced), and a `PATH` with synced folders removed. `complete` is
/// false when the set of synced folders couldn't be worked out in full:
/// hooks are then refused, never run against a folder we might have missed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Places {
    pub synced: Vec<(PathBuf, String)>,
    /// Whether [`Places::synced`] is the whole set.
    pub complete: bool,
    /// Where a hook runs (its current folder); must not be synced.
    pub cwd: PathBuf,
    /// A `PATH` with synced folders removed, for the hook's environment.
    pub safe_path: Option<OsString>,
}

impl Places {
    /// Adds each folder in its resolved form too, so a folder reached
    /// through a link is recognised either way.
    pub fn new(synced: Vec<(PathBuf, String)>, complete: bool, cwd: PathBuf, safe_path: Option<OsString>) -> Self {
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

/// What a hook will start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// The real executable to run (the program, or `cmd.exe` when it wraps a
    /// `.bat`/`.cmd`).
    pub program: PathBuf,
    /// Literal arguments before the user's (e.g. `["/c", script]`).
    pub prefix: Vec<OsString>,
    /// Scripts (and a non-`.exe` program) to open and verify at launch.
    pub scripts: Vec<PathBuf>,
    /// Whether the user's `args` may hold templates (a real non-interpreter
    /// `.exe`); false means the event reaches it as data only.
    pub templates_ok: bool,
}

/// Whether `stem` (a program's file name without extension, lower-cased) is
/// a known shell, interpreter or loader: its arguments can become code, so
/// a cloud value must never be one of them. Matched generously by family —
/// when in doubt, an interpreter.
pub fn is_interpreter(stem: &str) -> bool {
    const EXACT: &[&str] = &[
        "cmd",
        "powershell",
        "powershell_ise",
        "pwsh",
        "wscript",
        "cscript",
        "mshta",
        "rundll32",
        "regsvr32",
        "regsvcs",
        "installutil",
        "msiexec",
        "wmic",
        "forfiles",
        "hh",
        "wsl",
        "wslconfig",
        "nodejs",
        "deno",
        "bun",
        "dotnet",
        "scriptrunner",
        "dllhost",
        "conhost",
        "cmd.exe",
    ];
    if EXACT.contains(&stem) {
        return true;
    }
    // Families named by a prefix: python / python3 / python3.12 / pythonw /
    // py / pyw, node, ruby, perl, php, java / javaw, (ba)sh, zsh.
    const PREFIX: &[&str] = &["python", "pyw", "py", "node", "ruby", "perl", "php", "java"];
    if PREFIX.iter().any(|p| stem.starts_with(p)) {
        return true;
    }
    matches!(stem, "sh" | "bash" | "zsh" | "dash" | "ksh")
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
/// replace it, and if so, where. Uses the known synced folders and the OS's
/// record of every cloud provider's sync roots.
pub fn cloud_place(path: &Path, synced: &[(PathBuf, String)]) -> Option<String> {
    for (folder, what) in synced {
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

/// A file (the program or a script), resolved and shown not to be anywhere a
/// cloud change can replace it: the resolved path, every hard link, and the
/// path as written must all be outside the synced folders. Returns the
/// canonical path, to guard at launch.
fn safe_file(given: &Path, what: &str, places: &Places) -> Result<PathBuf, Refusal> {
    let real = std::fs::canonicalize(given).map_err(|e| Refusal(format!("{what} {}: {e}", given.display())))?;
    if !real.is_file() {
        return Err(Refusal(format!("{what} {} isn't a file", given.display())));
    }
    // Every name the file has: a second one inside a synced folder could be
    // replaced by a cloud change too. (System interpreters have several
    // links, all under %SystemRoot%, which pass.)
    let links = oneup_sys::fs_links::all_links(&real)
        .map_err(|e| Refusal(format!("{what} {}: can't check its names: {e}", given.display())))?;
    for p in links.iter().map(PathBuf::as_path).chain([given, real.as_path()]) {
        if let Some(place) = cloud_place(p, &places.synced) {
            return Err(Refusal(format!(
                "{what} {} is in {place}: a change in the cloud could replace it, so it never runs as a hook. \
                 Move it to a folder that isn't synced",
                given.display()
            )));
        }
    }
    Ok(real)
}

/// The program to start for a hook (and any script it runs), and whether its
/// arguments may carry templates. `cwd` is where a relative literal script
/// argument is resolved.
pub fn check(program: &str, args: &[String], cwd: &Path, places: &Places) -> Result<Checked, Refusal> {
    if !places.complete {
        return Err(Refusal(
            "OneUpDrive can't list all the folders it syncs right now, so it won't run hooks (it can't be sure a \
             program isn't in one). Try again in a moment"
                .into(),
        ));
    }
    let given = resolve(program)?;
    let real = safe_file(&given, "the program", places)?;
    let ext = real.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let stem = real.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    // The name it has now, and the names its version resource gives (a
    // renamed copy of an interpreter keeps its OriginalFilename).
    let renamed_interpreter = oneup_sys::version_info::original_names(&real).iter().any(|n| {
        let n = n.trim().to_ascii_lowercase();
        let n = n.strip_suffix(".exe").or_else(|| n.strip_suffix(".mui")).unwrap_or(&n).to_string();
        is_interpreter(&n)
    });
    let templates_ok = ext == "exe" && !is_interpreter(&stem) && !renamed_interpreter;

    if !templates_ok && let Some(a) = args.iter().find(|a| has_template(a)) {
        let kind = if ext == "exe" { "an interpreter" } else { "a script, not a plain .exe" };
        return Err(Refusal(format!(
            "{program} is {kind}, so a value from the cloud in `{a}` could run as code. OneUpDrive passes the event \
             to it instead: as environment variables (ONEUPHOOK_PATH, ONEUPHOOK_EVENT, ...) and a JSON line on \
             standard input. Remove the template; keep only fixed arguments (switches, a script path)"
        )));
    }

    // For an interpreter or a script, every path its literal arguments name
    // (a script, or one inside inline code) must be outside every synced
    // folder. A plain .exe's arguments are data to it, so they're left alone.
    let mut scripts = if templates_ok { Vec::new() } else { literal_scripts(args, cwd, places)? };

    // How it actually launches.
    let (launch, prefix) = match ext.as_str() {
        "exe" => (real.clone(), Vec::new()),
        "bat" | "cmd" => {
            // Run the batch through cmd, with only the literal (verified)
            // script path on the line; the event is data (env + stdin). cmd
            // doesn't understand an extended `\\?\` path, so the argument is
            // the plain form; the guard still holds the canonical file.
            // `/d`: no AutoRun commands from the registry; `call`: cmd keeps
            // the quoted path as one argument, whatever parentheses or
            // spaces it has.
            scripts.insert(0, real.clone());
            let plain = coexist::plain(&real).into_os_string();
            (cmd_exe()?, vec![OsString::from("/d"), OsString::from("/c"), OsString::from("call"), plain])
        }
        _ => {
            return Err(Refusal(format!(
                "{program} is a {ext} script; run it through its interpreter instead (for example powershell.exe \
                 -File your-script.ps1, or python.exe your-script.py)"
            )));
        }
    };
    Ok(Checked { program: launch, prefix, scripts, templates_ok })
}

/// `System32\cmd.exe`, to wrap a `.bat`/`.cmd` program.
fn cmd_exe() -> Result<PathBuf, Refusal> {
    let root = std::env::var_os("SystemRoot").ok_or_else(|| Refusal("SystemRoot isn't set".into()))?;
    let p = Path::new(&root).join("System32").join("cmd.exe");
    std::fs::canonicalize(&p).map_err(|e| Refusal(format!("{}: {e}", p.display())))
}

/// Whether `arg` holds a template (`{...}`, but not an escaped `{{`).
fn has_template(arg: &str) -> bool {
    let mut rest = arg;
    while let Some(i) = rest.find('{') {
        let tail = &rest[i..];
        if let Some(after) = tail.strip_prefix("{{") {
            rest = after;
            continue;
        }
        return true;
    }
    false
}

/// Literal arguments that name a file, for an interpreter or a script: each
/// path in them (whole, after `=`, after a leading `/c`/`/k`, and every token
/// of inline code split on quotes, white space and `; | & ( ) , < >`) must be
/// outside every synced folder, whether or not the file exists yet (the path
/// and its deepest existing folder are checked), across every hard link of a
/// file that exists. Drive-relative paths (`C:script.py`) are refused. Script
/// files found are returned to guard at launch.
fn literal_scripts(args: &[String], cwd: &Path, places: &Places) -> Result<Vec<PathBuf>, Refusal> {
    let mut scripts = Vec::new();
    for a in args {
        if has_template(a) {
            continue;
        }
        let mut cands = arg_paths(a);
        cands.extend(inline_tokens(a));
        for cand in cands {
            let p = Path::new(&cand);
            // A drive-relative path (`C:foo`, no separator after the colon)
            // resolves against that drive's current directory, which we
            // can't vouch for.
            if is_drive_relative(&cand) {
                return Err(Refusal(format!(
                    "{a}: `{cand}` is a drive-relative path; give a full path (`C:\\folder\\file`) so it can't \
                     resolve into a synced folder"
                )));
            }
            if !p.is_absolute() && !cand.contains(['\\', '/']) {
                // A bare switch or word, not a path.
                continue;
            }
            let full = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
            if let Some(place) = place_of_path(&full, places) {
                return Err(Refusal(format!(
                    "{} is in {place}: a change in the cloud could replace what the hook runs. Move it to a folder \
                     that isn't synced",
                    full.display()
                )));
            }
            let Ok(real) = std::fs::canonicalize(&full) else { continue };
            if !real.is_file() {
                continue;
            }
            // Every name the file has must be outside the synced folders too.
            let real = safe_file(&full, "the script", places)?;
            if is_script_name(&real) && !scripts.contains(&real) {
                scripts.push(real);
            }
        }
    }
    Ok(scripts)
}

/// Where `path` lies in a cloud folder, if it does, even when it doesn't
/// exist yet: the path as written, and its deepest existing folder resolved
/// (links followed) with the rest of the path added back.
fn place_of_path(path: &Path, places: &Places) -> Option<String> {
    if let Some(place) = cloud_place(path, &places.synced) {
        return Some(place);
    }
    let mut ancestor = path;
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(ancestor) {
            let mut full = real;
            for part in rest.iter().rev() {
                full.push(part);
            }
            return cloud_place(&full, &places.synced);
        }
        rest.push(ancestor.file_name()?);
        ancestor = ancestor.parent()?;
    }
}

/// The tokens of inline code in a literal argument (`& 'C:\x.ps1'`,
/// `call C:\x.bat`), split on quotes, white space and command separators.
fn inline_tokens(arg: &str) -> Vec<String> {
    arg.split(|c: char| c.is_whitespace() || "\"'`;|&(),<>".contains(c))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// The path-like candidates in a literal argument: the whole thing, the part
/// after `=` (`--file=x`), and the part after a leading `/c`/`/k` switch.
fn arg_paths(arg: &str) -> Vec<String> {
    let a = arg.trim().trim_matches('"');
    let mut out = vec![a.to_string()];
    if let Some((_, v)) = a.split_once('=') {
        out.push(v.trim_matches('"').to_string());
    }
    let low = a.to_ascii_lowercase();
    if let Some(rest) = low.strip_prefix("/c").or_else(|| low.strip_prefix("/k")) {
        let rest = rest.trim_start();
        if !rest.is_empty() {
            // Keep the original case by slicing the tail of `a`.
            out.push(a[a.len() - rest.len()..].to_string());
        }
    }
    out
}

/// A drive-relative path like `C:script.py` (a colon after one letter, not
/// followed by a separator).
fn is_drive_relative(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && !matches!(b.get(2), Some(b'\\' | b'/'))
}

/// Whether a file's extension marks it as a script an interpreter runs
/// (case-insensitive).
fn is_script_name(path: &Path) -> bool {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    matches!(
        ext.as_str(),
        "bat"
            | "cmd"
            | "ps1"
            | "psm1"
            | "vbs"
            | "vbe"
            | "js"
            | "jse"
            | "wsf"
            | "wsh"
            | "py"
            | "pyw"
            | "rb"
            | "pl"
            | "php"
            | "sh"
    )
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

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
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

    #[test]
    fn interpreter_families_are_recognised_generously() {
        for s in [
            "cmd",
            "powershell",
            "pwsh",
            "python",
            "python3",
            "python3.12",
            "pythonw",
            "py",
            "pyw",
            "node",
            "ruby",
            "perl",
            "php",
            "java",
            "javaw",
            "wscript",
            "cscript",
            "mshta",
            "rundll32",
            "regsvr32",
            "msiexec",
            "wsl",
            "bash",
            "sh",
            "deno",
            "bun",
        ] {
            assert!(is_interpreter(s), "{s} should be an interpreter");
        }
        for s in ["thumb", "mytool", "ffmpeg", "notify", "7z"] {
            assert!(!is_interpreter(s), "{s} should not be an interpreter");
        }
    }

    /// A program or script in a synced folder is refused, however reached.
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
    }

    /// A program with a second hard link inside a synced folder is refused;
    /// one with extra links all outside is fine.
    #[test]
    fn a_hard_link_into_a_synced_folder_is_refused() {
        let t = tmp("links");
        let synced = t.0.join("Synced");
        let tools = t.0.join("Tools");
        std::fs::create_dir_all(&synced).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let exe = tools.join("run.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let pl = places(vec![(synced.clone(), "the folder pair Work".into())]);
        // Outside only: fine.
        let also = tools.join("also.exe");
        std::fs::hard_link(&exe, &also).unwrap();
        assert!(check(&exe.to_string_lossy(), &[], &t.0, &pl).is_ok(), "extra link, still outside");
        // A link inside the synced folder: refused.
        let inside = synced.join("run.exe");
        std::fs::hard_link(&exe, &inside).unwrap();
        let e = check(&exe.to_string_lossy(), &[], &t.0, &pl).unwrap_err();
        assert!(e.0.contains("the folder pair Work"), "{e}");
    }

    /// Templates are allowed only for a real, non-interpreter .exe.
    #[test]
    fn templates_are_only_for_a_plain_exe() {
        let t = tmp("templates");
        std::fs::create_dir_all(&t.0).unwrap();
        let exe = t.0.join("thumb.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let bat = t.0.join("hook.bat");
        std::fs::write(&bat, b"@exit /b 0").unwrap();
        let pl = places(vec![]);
        // Plain exe: templates fine.
        let c = check(&exe.to_string_lossy(), &args(&["{path}"]), &t.0, &pl).unwrap();
        assert!(c.templates_ok && c.prefix.is_empty() && c.scripts.is_empty());
        // Interpreter: a template is refused with guidance.
        let e = check("powershell.exe", &args(&["-File", "x.ps1", "{path}"]), &t.0, &pl).unwrap_err();
        assert!(e.0.contains("ONEUPHOOK_PATH") && e.0.contains("interpreter"), "{e}");
        assert!(check("cmd.exe", &args(&["/c", "echo {relpath}"]), &t.0, &pl).is_err());
        // A .bat program: templates refused, wrapped in cmd /c, guarded.
        let e = check(&bat.to_string_lossy(), &args(&["{detail}"]), &t.0, &pl).unwrap_err();
        assert!(e.0.contains("script, not a plain .exe"), "{e}");
        let c = check(&bat.to_string_lossy(), &[], &t.0, &pl).unwrap();
        assert!(!c.templates_ok);
        assert!(c.program.to_string_lossy().to_lowercase().ends_with(r"\cmd.exe"));
        let plain_bat = coexist::plain(&std::fs::canonicalize(&bat).unwrap()).into_os_string();
        assert_eq!(c.prefix, vec![OsString::from("/d"), OsString::from("/c"), OsString::from("call"), plain_bat]);
        assert_eq!(c.scripts, vec![std::fs::canonicalize(&bat).unwrap()]);
        // A .ps1 program (not .bat): refused, pointed at an interpreter.
        let ps = t.0.join("x.ps1");
        std::fs::write(&ps, b"x").unwrap();
        assert!(check(&ps.to_string_lossy(), &[], &t.0, &pl).unwrap_err().0.contains("run it through its interpreter"));
    }

    /// A literal script an interpreter runs must be outside synced folders;
    /// drive-relative paths and a `/c`-attached script are caught.
    #[test]
    fn literal_scripts_are_checked() {
        let t = tmp("literal");
        let synced = t.0.join("Synced");
        let tools = t.0.join("Tools");
        std::fs::create_dir_all(&synced).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(synced.join("evil.ps1"), b"x").unwrap();
        let good = tools.join("good.ps1");
        std::fs::write(&good, b"x").unwrap();
        let pl = places(vec![(synced.clone(), "the main folder".into())]);
        // Script inside the synced folder: refused.
        let e = check("powershell.exe", &args(&["-File", &synced.join("evil.ps1").to_string_lossy()]), &tools, &pl)
            .unwrap_err();
        assert!(e.0.contains("could replace what the hook runs"), "{e}");
        // Outside: fine, and guarded.
        let c = check("powershell.exe", &args(&["-NoProfile", "-File", &good.to_string_lossy()]), &tools, &pl).unwrap();
        assert_eq!(c.scripts, vec![std::fs::canonicalize(&good).unwrap()]);
        // A drive-relative path is refused.
        assert!(check("cmd.exe", &args(&["/c", "C:evil.bat"]), &tools, &pl).unwrap_err().0.contains("drive-relative"));
        // A script attached to /c, inside the synced folder, is caught.
        let attached = format!("/c{}", synced.join("evil.ps1").to_string_lossy());
        assert!(check("cmd.exe", &args(&[&attached]), &tools, &pl).unwrap_err().0.contains("could replace"));
    }

    /// However the template is positioned, an interpreter refuses it — the
    /// old switch-by-switch allow-list is gone.
    #[test]
    fn interpreters_refuse_a_template_in_any_position() {
        let t = tmp("positions");
        std::fs::create_dir_all(&t.0).unwrap();
        let pl = places(vec![]);
        let refused = |prog: &str, a: &[&str]| check(prog, &args(a), &t.0, &pl).is_err();
        // PowerShell: abbreviated/positional/after other switches.
        assert!(refused("powershell.exe", &["-co", "{path}"]));
        assert!(refused("powershell.exe", &["{path}"]));
        assert!(refused("powershell.exe", &["-ExecutionPolicy", "Bypass", "-File", "x.ps1", "{path}"]));
        // php -B (run code before), python-family variants, pyw -c, node -e.
        assert!(refused("php.exe", &["-B", "{detail}"]));
        assert!(refused("php.exe", &["-d", "auto_prepend_file={path}"]));
        assert!(refused("pyw.exe", &["-c", "{path}"]));
        assert!(refused("python3.12.exe", &["{relpath}"]));
        assert!(refused("node.exe", &["-e", "{detail}"]));
        // A literal interpreter command with no template is fine (the user's
        // own code), e.g. cmd echoing a fixed string.
        assert!(check("cmd.exe", &args(&["/c", "echo done"]), &t.0, &pl).is_ok());
    }

    /// Paths inside inline code, paths that don't exist yet, and a script
    /// with a second hard link into a synced folder are all caught.
    #[test]
    fn literal_paths_are_checked_in_inline_code_unborn_and_across_links() {
        let t = tmp("inline");
        let synced = t.0.join("Synced");
        let tools = t.0.join("Tools");
        std::fs::create_dir_all(&synced).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let pl = places(vec![(synced.clone(), "the main folder".into())]);
        let evil = synced.join("evil.ps1");
        let evil_s = evil.to_string_lossy().to_string();
        // Inline code naming a script in the synced folder (it needn't exist).
        let cmdline = format!("& '{evil_s}'");
        assert!(check("powershell.exe", &args(&["-Command", &cmdline]), &tools, &pl).is_err(), "inline &");
        let call = format!("call {}", synced.join("evil.bat").display());
        assert!(check("cmd.exe", &args(&["/c", &call]), &tools, &pl).is_err(), "inline call");
        // A path that doesn't exist yet, under a synced folder.
        let unborn = synced.join("later").join("x.py").to_string_lossy().to_string();
        assert!(check("python.exe", &args(&[&unborn]), &tools, &pl).is_err(), "unborn path");
        // A script outside with a second hard link inside the synced folder.
        let ok = tools.join("ok.ps1");
        std::fs::write(&ok, b"x").unwrap();
        assert!(check("powershell.exe", &args(&["-File", &ok.to_string_lossy()]), &tools, &pl).is_ok());
        std::fs::hard_link(&ok, synced.join("ok.ps1")).unwrap();
        let e = check("powershell.exe", &args(&["-File", &ok.to_string_lossy()]), &tools, &pl).unwrap_err();
        assert!(e.0.contains("the main folder"), "{e}");
        // A plain .exe's literal data path is its own business.
        let exe = tools.join("tool.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        assert!(check(&exe.to_string_lossy(), &args(&[&evil_s]), &tools, &pl).is_ok());
    }

    /// A renamed copy of an interpreter is still an interpreter (its version
    /// resource's original name), so templates are refused for it.
    #[test]
    fn a_renamed_interpreter_is_still_an_interpreter() {
        let t = tmp("renamed");
        std::fs::create_dir_all(&t.0).unwrap();
        let cmd = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32").join("cmd.exe");
        let copy = t.0.join("innocent.exe");
        std::fs::copy(&cmd, &copy).unwrap();
        let e = check(&copy.to_string_lossy(), &args(&["/c", "echo {path}"]), &t.0, &places(vec![])).unwrap_err();
        assert!(e.0.contains("interpreter"), "{e}");
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

    #[test]
    fn drive_relative_and_escaped_templates() {
        assert!(is_drive_relative("C:evil.bat") && is_drive_relative("c:x"));
        assert!(!is_drive_relative(r"C:\ok") && !is_drive_relative("plain") && !is_drive_relative("/c"));
        assert!(has_template("{path}") && !has_template("{{literal}}") && !has_template("plain"));
    }
}
