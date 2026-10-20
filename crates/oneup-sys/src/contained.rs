//! Running a program in the background, contained: the event hooks
//! (`oneup_hooks`). The program and everything it starts run in a job
//! object, at below-normal priority, with EcoQoS asked for, without a
//! window, and end together: when the program does, when it runs past its
//! time, or when this process ends.
//!
//! The process is created in the job (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), so
//! nothing it starts can get out of the job first, and nothing can leave it
//! later (breaking away isn't allowed). It inherits exactly two handles,
//! its standard input (empty) and output (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`):
//! none of this process's, whatever else is inheritable.
//!
//! Right before the process starts, the program (and any script handed to
//! an interpreter) is reopened denying other writers and deleters, and its
//! final path and link count are checked, so nothing that passed the
//! caller's where-is-it checks can be swapped or hard-linked into a synced
//! folder between the check and the launch (the handles stay open until the
//! process has ended).
//!
//! Each run's job is registered in a shared [`HookJobs`] while it runs, so
//! the control pipe can refuse any client that is one of these processes or
//! their descendants ([`HookJobs::contains`]): a hook can't reach the pipe
//! calls (approving a safety stop, say) by any path another of the user's
//! programs doesn't also have.

use std::ffi::{OsStr, OsString, c_void};
use std::io::Read;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::FromRawHandle as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_NAME_NORMALIZED, FILE_SHARE_READ,
    GetFileInformationByHandle, GetFinalPathNameByHandleW, OPEN_EXISTING,
};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PRIORITY_CLASS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION,
    ProcessPowerThrottling, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, SetProcessInformation,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

/// What to run.
pub struct Spec<'a> {
    /// The program, by a full path the caller has resolved and checked.
    pub program: &'a Path,
    pub args: &'a [OsString],
    /// The working folder: a safe one (OneUpDrive's data folder), never a
    /// synced folder (a DLL search there would reach cloud content).
    pub cwd: &'a Path,
    /// Environment variables the program doesn't get (by name); it gets
    /// the rest of this process's.
    pub drop_env: &'a dyn Fn(&OsStr) -> bool,
    /// Variables set after the rest (e.g. a `PATH` with synced folders
    /// removed).
    pub env_overrides: &'a [(OsString, OsString)],
    /// Files, besides the program, to open denying other writers/deleters
    /// and check (final path and a single hard link) before the launch: a
    /// script handed to an interpreter. The caller has checked these are
    /// outside every synced folder.
    pub guard: &'a [PathBuf],
    pub timeout: Duration,
    /// How much of its output (standard output and error together) to keep;
    /// the rest is read and dropped.
    pub keep_output: usize,
}

/// How it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// `None` if it was ended (its time was up) or couldn't be read.
    pub exit_code: Option<i32>,
    /// It ran past its time and was ended (with everything it started).
    pub timed_out: bool,
    pub output: Vec<u8>,
    /// More output came than was kept.
    pub truncated: bool,
}

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            // SAFETY: a handle this module opened and nothing else closes.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

impl Owned {
    /// Gives the handle up (to a `File` that closes it).
    fn take(mut self) -> HANDLE {
        std::mem::take(&mut self.0)
    }
}

fn io(op: &'static str, e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(format!("{op}: {e}"))
}

/// The jobs of the hook runs under way, so the control pipe can tell its
/// own hook processes (and their descendants) apart from the user's other
/// programs ([`HookJobs::contains`]). A run registers its job while it runs
/// and removes it before the job handle is closed.
#[derive(Default)]
pub struct HookJobs {
    /// Raw job handles by a token; the handle stays open (its run's `Owned`)
    /// while it's listed here.
    jobs: Mutex<Vec<(u64, isize)>>,
    next: AtomicU64,
}

impl HookJobs {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, job: HANDLE) -> u64 {
        let token = self.next.fetch_add(1, Relaxed);
        self.jobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push((token, job.0 as isize));
        token
    }

    fn unregister(&self, token: u64) {
        self.jobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).retain(|(t, _)| *t != token);
    }

    /// Whether process `pid` is one of the hooks' processes, or started by
    /// one (a job holds a whole tree, and breaking away isn't allowed). A
    /// process that can't be opened (gone, or not ours) is not a hook.
    pub fn contains(&self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        // SAFETY: opening a process by id for a query only; the handle is
        // owned and closed by `Owned`.
        let Ok(proc) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
            return false;
        };
        let proc = Owned(proc);
        let jobs = self.jobs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        jobs.iter().any(|&(_, job)| {
            let mut in_job = windows::core::BOOL(0);
            // SAFETY: `proc` is a valid handle we hold; `job` is open while
            // it's listed here (its run keeps it); `in_job` is a valid out.
            unsafe { IsProcessInJob(proc.0, Some(HANDLE(job as *mut c_void)), &mut in_job).is_ok() && in_job.as_bool() }
        })
    }
}

/// Removes a run's job from the registry when the run ends.
struct Registered<'a> {
    jobs: Option<&'a HookJobs>,
    token: u64,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        if let Some(j) = self.jobs {
            j.unregister(self.token);
        }
    }
}

/// A job whose processes run below normal priority, can't show the crash
/// dialog (which would wait for a click no one makes), and end when it's
/// closed. Breaking away isn't allowed (no breakaway limit flag), so a
/// descendant can't escape the job.
fn job() -> std::io::Result<Owned> {
    // SAFETY: no name and default security; the handle is owned below.
    let job = Owned(unsafe { CreateJobObjectW(None, None) }.map_err(|e| io("CreateJobObjectW", e))?);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_PRIORITY_CLASS
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    limits.BasicLimitInformation.PriorityClass = BELOW_NORMAL_PRIORITY_CLASS.0;
    // SAFETY: `limits` is a valid structure of the class's type and size,
    // alive for the call; the job handle is valid.
    unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast::<c_void>(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|e| io("SetInformationJobObject", e))?;
    Ok(job)
}

/// A pipe: the end the program gets (it reads it if `child_reads`) is
/// inheritable, ours isn't. Returns (the program's end, ours).
fn pipe(child_reads: bool) -> std::io::Result<(Owned, Owned)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: true.into(),
        ..Default::default()
    };
    let (mut r, mut w) = (HANDLE::default(), HANDLE::default());
    // SAFETY: valid out locations and attributes, alive for the call.
    unsafe { CreatePipe(&mut r, &mut w, Some(&sa), 0) }.map_err(|e| io("CreatePipe", e))?;
    let (r, w) = (Owned(r), Owned(w));
    let (child, ours) = if child_reads { (r, w) } else { (w, r) };
    // SAFETY: a valid handle we own.
    unsafe { SetHandleInformation(ours.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        .map_err(|e| io("SetHandleInformation", e))?;
    Ok((child, ours))
}

/// Opens `path` denying other writers and deleters, and checks it is the
/// one file the caller checked: a single hard link (so no second name in a
/// synced folder points at the same bytes) and the expected final path (so
/// it wasn't replaced since). The handle stays open (returned) until the
/// process has run, so it can't be swapped meanwhile.
fn guard_file(path: &Path, check_links: bool) -> std::io::Result<Owned> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call; no security
    // attributes, no template.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| std::io::Error::other(format!("open {}: {e}", path.display())))?;
    let h = Owned(h);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `h` is valid; `info` is a valid, writable out structure.
    unsafe { GetFileInformationByHandle(h.0, &mut info) }.map_err(|e| io("GetFileInformationByHandle", e))?;
    // A script must be a single name: a second hard link, possibly in a
    // synced folder, could point at the same bytes. System interpreters
    // (cmd.exe, powershell.exe) ship with several links by servicing, so the
    // program itself is only checked for being swapped, not for links.
    if check_links && info.nNumberOfLinks != 1 {
        return Err(std::io::Error::other(format!(
            "{} has {} hard links; another name could point at the same bytes from a synced folder",
            path.display(),
            info.nNumberOfLinks
        )));
    }
    let mut buf = vec![0u16; 1024];
    // SAFETY: `h` is valid; `buf` is writable for its length.
    let n = unsafe { GetFinalPathNameByHandleW(h.0, &mut buf, FILE_NAME_NORMALIZED) } as usize;
    if n == 0 || n > buf.len() {
        return Err(std::io::Error::other(format!("couldn't read the final path of {}", path.display())));
    }
    let final_path = PathBuf::from(OsString::from_wide(&buf[..n]));
    // The caller's path is already the canonical (`\\?\`) form.
    if !same_path(&final_path, path) {
        return Err(std::io::Error::other(format!(
            "{} now resolves to {}; it was moved or replaced",
            path.display(),
            final_path.display()
        )));
    }
    Ok(h)
}

/// Whether two paths are the same, ignoring case (Windows paths) and a
/// `\\?\` prefix on either.
fn same_path(a: &Path, b: &Path) -> bool {
    let strip = |p: &Path| {
        let s = p.to_string_lossy().to_lowercase();
        s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
    };
    strip(a) == strip(b)
}

/// Asks Windows to run the process power-efficiently (EcoQoS). Best effort.
fn eco_qos(process: HANDLE) {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    };
    // SAFETY: `process` is a valid handle with full access (we created it);
    // `state` is valid, correctly sized and outlives the call.
    let _ = unsafe {
        SetProcessInformation(
            process,
            ProcessPowerThrottling,
            (&raw const state).cast::<c_void>(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
}

/// Appends `arg` as the C runtime's command-line parser reads it back: in
/// quotes if it has white space or quotes (or is empty), with backslashes
/// doubled before a quote and at the end.
fn push_arg(cmd: &mut Vec<u16>, arg: &OsStr) {
    let chars: Vec<u16> = arg.encode_wide().collect();
    let plain = !chars.is_empty() && !chars.iter().any(|&c| matches!(c, 0x20 | 0x09 | 0x0A | 0x0B | 0x22));
    if !cmd.is_empty() {
        cmd.push(u16::from(b' '));
    }
    if plain {
        cmd.extend(chars);
        return;
    }
    cmd.push(u16::from(b'"'));
    let mut backslashes = 0;
    for c in chars {
        if c == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        let n = if c == u16::from(b'"') { backslashes * 2 + 1 } else { backslashes };
        cmd.extend(std::iter::repeat_n(u16::from(b'\\'), n));
        cmd.push(c);
        backslashes = 0;
    }
    cmd.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    cmd.push(u16::from(b'"'));
}

/// The command line: the program in quotes (a path has no quotes), then
/// the arguments; NUL-terminated.
fn command_line(program: &Path, args: &[OsString]) -> Vec<u16> {
    let mut cmd: Vec<u16> = vec![u16::from(b'"')];
    cmd.extend(program.as_os_str().encode_wide());
    cmd.push(u16::from(b'"'));
    for a in args {
        push_arg(&mut cmd, a);
    }
    cmd.push(0);
    cmd
}

/// This process's environment without the variables `drop` names, with
/// `overrides` set on top, as a block (sorted by name without case, as
/// Windows keeps it).
fn environment(drop: &dyn Fn(&OsStr) -> bool, overrides: &[(OsString, OsString)]) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os().filter(|(k, _)| !drop(k)).collect();
    for (k, v) in overrides {
        let fold = |s: &OsStr| s.to_string_lossy().to_uppercase();
        vars.retain(|(ek, _)| fold(ek) != fold(k));
        vars.push((k.clone(), v.clone()));
    }
    vars.sort_by_key(|(k, _)| k.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push(u16::from(b'='));
        block.extend(v.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn wide(p: &OsStr) -> Vec<u16> {
    p.encode_wide().chain(std::iter::once(0)).collect()
}

/// Output kept from both streams, in the order it came.
#[derive(Default)]
struct Kept {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_into(mut r: impl Read, kept: &Mutex<Kept>, limit: usize) {
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut k = kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let room = limit.saturating_sub(k.bytes.len());
                k.bytes.extend_from_slice(&buf[..n.min(room)]);
                k.truncated |= n > room;
            }
        }
    }
}

/// The attribute list: inherit only `handles`, and start in `job`.
struct Attributes {
    buf: Vec<u64>,
}

impl Attributes {
    fn new(handles: &[HANDLE; 2], job: &[HANDLE; 1]) -> std::io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: a size query (no list); it fails with "insufficient
        // buffer" and sets `size`, which is all that's wanted from it.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 2, None, &mut size) };
        // u64 words keep the list aligned.
        let mut a = Self { buf: vec![0u64; size.div_ceil(8).max(1)] };
        // SAFETY: `buf` is writable and at least `size` bytes.
        unsafe { InitializeProcThreadAttributeList(Some(a.list()), 2, None, &mut size) }
            .map_err(|e| io("InitializeProcThreadAttributeList", e))?;
        // SAFETY: the values are valid for their sizes; the caller keeps them
        // alive until the list's last use (CreateProcessW).
        unsafe {
            UpdateProcThreadAttribute(
                a.list(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                size_of_val(handles),
                None,
                None,
            )
            .map_err(|e| io("UpdateProcThreadAttribute", e))?;
            UpdateProcThreadAttribute(
                a.list(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(job.as_ptr().cast()),
                size_of_val(job),
                None,
                None,
            )
            .map_err(|e| io("UpdateProcThreadAttribute", e))?;
        }
        Ok(a)
    }

    fn list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buf.as_mut_ptr().cast())
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: initialized in `new` (a failed initialization leaves
        // nothing to delete, and deleting a zeroed list is harmless).
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

/// Creates the process, suspended, in `job`, with `stdin` and `stdout`
/// (also its error output) as its only inherited handles.
fn create(spec: &Spec<'_>, job: &Owned, stdin: &Owned, stdout: &Owned) -> std::io::Result<PROCESS_INFORMATION> {
    let handles = [stdin.0, stdout.0];
    let jobs = [job.0];
    let mut attributes = Attributes::new(&handles, &jobs)?;
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = stdin.0;
    si.StartupInfo.hStdOutput = stdout.0;
    si.StartupInfo.hStdError = stdout.0;
    si.lpAttributeList = attributes.list();
    let program = wide(spec.program.as_os_str());
    let mut cmd = command_line(spec.program, spec.args);
    let env = environment(spec.drop_env, spec.env_overrides);
    let cwd = wide(spec.cwd.as_os_str());
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: every pointer is valid, NUL-terminated where required, and
    // outlives the call (`handles` and `jobs` too, which the attribute list
    // points to); `cmd` is writable, as CreateProcessW requires; `si` is an
    // extended start-up structure, as the flag says.
    unsafe {
        CreateProcessW(
            PCWSTR(program.as_ptr()),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_NO_WINDOW
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | BELOW_NORMAL_PRIORITY_CLASS,
            Some(env.as_ptr().cast()),
            PCWSTR(cwd.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        )
    }
    .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
    Ok(pi)
}

/// Runs the program until it ends or its time is up, and returns how it
/// ended. Everything it started ends with it. `jobs`: the shared registry
/// its job joins while it runs (`None` in tests that don't need it).
pub fn run(spec: &Spec<'_>, jobs: Option<&HookJobs>) -> std::io::Result<Finished> {
    let job = job()?;
    let _registered = Registered { jobs, token: jobs.map(|j| j.register(job.0)).unwrap_or(0) };
    // Open and verify the program and any script, denying other writers and
    // deleters, and keep the handles open until the run has ended.
    let mut guards = Vec::with_capacity(1 + spec.guard.len());
    guards.push(guard_file(spec.program, false)?);
    for p in spec.guard {
        guards.push(guard_file(p, true)?);
    }
    let (child_in, ours_in) = pipe(true)?;
    // Its input is empty: our end closes now.
    drop(ours_in);
    let (child_out, ours_out) = pipe(false)?;
    let pi = create(spec, &job, &child_in, &child_out)?;
    let (process, thread) = (Owned(pi.hProcess), Owned(pi.hThread));
    // Only the program holds its ends now, so its output ends when it (and
    // all it started) has.
    drop((child_in, child_out));
    eco_qos(process.0);
    // SAFETY: the first thread of the process we created, suspended.
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        let e = std::io::Error::last_os_error();
        // SAFETY: a valid job handle; ends the suspended process.
        let _ = unsafe { TerminateJobObject(job.0, 1) };
        return Err(e);
    }
    drop(thread);
    let kept = Arc::new(Mutex::new(Kept::default()));
    // SAFETY: our read end, owned by the file from here on.
    let out = unsafe { std::fs::File::from_raw_handle(ours_out.take().0) };
    let reader = {
        let (kept, limit) = (Arc::clone(&kept), spec.keep_output);
        std::thread::Builder::new().name("hook-output".into()).spawn(move || read_into(out, &kept, limit))
    };
    let ms = u32::try_from(spec.timeout.as_millis()).unwrap_or(INFINITE - 1).min(INFINITE - 1);
    // SAFETY: a valid process handle, waited on for at most `ms`.
    let timed_out = unsafe { WaitForSingleObject(process.0, ms) } == WAIT_TIMEOUT;
    // Its time is up, or it ended: whatever it left running ends too, which
    // also closes their copies of the output pipe.
    // SAFETY: valid handles; the wait ends once the process has.
    let code = unsafe {
        let _ = TerminateJobObject(job.0, 1);
        WaitForSingleObject(process.0, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(process.0, &mut code).ok().map(|()| code as i32)
    };
    if let Ok(r) = reader {
        let _ = r.join();
    }
    // The program has ended: the guard handles have done their job.
    drop(guards);
    let k = std::mem::take(&mut *kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    Ok(Finished { exit_code: if timed_out { None } else { code }, timed_out, output: k.bytes, truncated: k.truncated })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cmd() -> std::path::PathBuf {
        let p = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32").join("cmd.exe");
        std::fs::canonicalize(p).unwrap()
    }

    fn spec<'a>(
        program: &'a Path,
        args: &'a [OsString],
        timeout: Duration,
        drop_env: &'a dyn Fn(&OsStr) -> bool,
    ) -> Spec<'a> {
        Spec {
            program,
            args,
            cwd: Path::new(r"C:\Windows"),
            drop_env,
            env_overrides: &[],
            guard: &[],
            timeout,
            keep_output: 64,
        }
    }

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_program_runs_and_its_exit_code_and_output_come_back() {
        let a = args(&["/c", "echo hello& exit 3"]);
        let f = run(&spec(&cmd(), &a, Duration::from_secs(30), &|_| false), None).unwrap();
        assert_eq!((f.exit_code, f.timed_out), (Some(3), false));
        assert_eq!(String::from_utf8_lossy(&f.output).trim(), "hello");
        // Only so much output is kept.
        let a = args(&["/c", "for /l %i in (1,1,50) do @echo 0123456789"]);
        let f = run(&spec(&cmd(), &a, Duration::from_secs(30), &|_| false), None).unwrap();
        assert!(f.truncated && f.output.len() == 64, "{f:?}");
        // A file that isn't a program.
        let not = std::env::temp_dir().join(format!("oneup-contained-{}.exe", std::process::id()));
        std::fs::write(&not, b"not a program").unwrap();
        let not = std::fs::canonicalize(&not).unwrap();
        assert!(run(&spec(&not, &[], Duration::from_secs(5), &|_| false), None).is_err());
        let _ = std::fs::remove_file(&not);
    }

    #[test]
    fn arguments_arrive_as_given() {
        let cases = ["plain", "two words", "", r"C:\a b\", r#"say "hi""#, r"back\\slash", "a&b|c"];
        let mut line: Vec<u16> = Vec::new();
        for c in cases {
            push_arg(&mut line, OsStr::new(c));
        }
        let line = String::from_utf16(&line).unwrap();
        assert_eq!(line, r#"plain "two words" "" "C:\a b\\" "say \"hi\"" back\\slash a&b|c"#);
        // As a program reads them back.
        let a = args(&["/c", "echo", "two words"]);
        let f = run(&spec(&cmd(), &a, Duration::from_secs(30), &|_| false), None).unwrap();
        assert_eq!(String::from_utf8_lossy(&f.output).trim(), "\"two words\"");
    }

    #[test]
    fn variables_it_mustnt_see_are_left_out_and_overrides_win() {
        // SAFETY: a variable only this test reads, in this process.
        unsafe { std::env::set_var("ONEUP_TEST_SECRET_CONTAINED", "s3cret") };
        let a = args(&["/c", "set ONEUP_TEST_PATHISH& set ONEUP_TEST_SECRET_CONTAINED"]);
        let drop = |n: &OsStr| n.to_string_lossy().to_ascii_uppercase().starts_with("ONEUP_");
        let overrides = [(OsString::from("ONEUP_TEST_PATHISH"), OsString::from("clean"))];
        let exe = cmd();
        let mut sp = spec(&exe, &a, Duration::from_secs(30), &drop);
        sp.env_overrides = &overrides;
        let f = run(&sp, None).unwrap();
        let out = String::from_utf8_lossy(&f.output);
        assert!(!out.contains("s3cret"), "{out}");
        // An override is set even though the drop would have removed it.
        assert!(out.contains("ONEUP_TEST_PATHISH=clean"), "{out}");
    }

    /// A program past its time ends, and so does what it started (here
    /// `ping`, a child of `cmd`, which would hold the output pipe open and
    /// keep running).
    #[test]
    fn a_program_past_its_time_ends_with_everything_it_started() {
        let a = args(&["/c", "ping -n 30 127.0.0.1"]);
        let t = std::time::Instant::now();
        let f = run(&spec(&cmd(), &a, Duration::from_millis(500), &|_| false), None).unwrap();
        assert!(f.timed_out && f.exit_code.is_none(), "{f:?}");
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
        // A program that ends by itself takes what it started along too.
        let a = args(&["/c", "start /b ping -n 30 127.0.0.1 & exit 0"]);
        let t = std::time::Instant::now();
        let f = run(&spec(&cmd(), &a, Duration::from_secs(20), &|_| false), None).unwrap();
        assert_eq!(f.exit_code, Some(0));
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    }

    /// Our own inheritable handles stay ours: the program gets only its
    /// input and output.
    #[test]
    fn the_program_inherits_only_its_input_and_output() {
        // An inheritable handle of ours: a pipe's write end. Had the program
        // inherited it, reading the pipe would only end with the program.
        let (inheritable, ours) = pipe(false).unwrap();
        let a = args(&["/c", "ping -n 4 127.0.0.1 >nul"]);
        let runner =
            std::thread::spawn(move || run(&spec(&cmd(), &a, Duration::from_secs(30), &|_| false), None).unwrap());
        std::thread::sleep(Duration::from_millis(500));
        let t = std::time::Instant::now();
        drop(inheritable);
        // SAFETY: our read end, owned by the file from here on.
        let mut f = unsafe { std::fs::File::from_raw_handle(ours.take().0) };
        let _ = f.read_to_end(&mut Vec::new());
        assert!(t.elapsed() < Duration::from_millis(1000), "{:?}", t.elapsed());
        assert_eq!(runner.join().unwrap().exit_code, Some(0));
    }

    /// A guarded script with a second hard link is refused: a second name,
    /// in a synced folder, could point at the same bytes. The program (a
    /// system binary with several servicing links) is only checked for a
    /// swap, never for links.
    #[test]
    fn a_hard_linked_script_is_refused() {
        let dir = std::env::temp_dir().join(format!("oneup-contained-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("s.bat");
        std::fs::write(&script, b"@exit /b 0").unwrap();
        let link = dir.join("also.bat");
        std::fs::hard_link(&script, &link).unwrap();
        let script = std::fs::canonicalize(&script).unwrap();
        let guard = [script.clone()];
        let a = args(&["/c", "exit 0"]);
        let exe = cmd();
        let mut sp = spec(&exe, &a, Duration::from_secs(10), &|_| false);
        sp.guard = &guard;
        let e = run(&sp, None).unwrap_err();
        assert!(e.to_string().contains("hard link"), "{e}");
        std::fs::remove_file(&link).unwrap();
        // One link again: it runs (cmd.exe itself has several links, and is
        // not refused for that).
        assert!(run(&sp, None).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A hook process (and its children) are in the shared registry; this
    /// test process, which isn't one, is not.
    #[test]
    fn hook_processes_and_their_children_are_in_the_registry() {
        let jobs = Arc::new(HookJobs::new());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // A hook that starts a child (ping) and waits: while it runs, both
        // it and the child are in the job.
        let j = Arc::clone(&jobs);
        let d = Arc::clone(&done);
        let child_pid = Arc::new(Mutex::new(0u32));
        let cp = Arc::clone(&child_pid);
        let run_thread = std::thread::spawn(move || {
            let a = args(&["/c", "ping -n 20 127.0.0.1 >nul"]);
            let _ = run(&spec(&cmd(), &a, Duration::from_secs(30), &|_| false), Some(&j));
            d.store(true, Relaxed);
            let _ = cp;
        });
        // Wait for a descendant that is in our job (other tests may run
        // their own cmd children under the shared test-binary process, so a
        // bare "any descendant" could be someone else's).
        let mut hook_pid = 0u32;
        for _ in 0..400 {
            if done.load(Relaxed) {
                break;
            }
            if let Some(pid) = crate::processes::any_descendant(std::process::id())
                && jobs.contains(pid)
            {
                hook_pid = pid;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(hook_pid != 0, "a hook process (or its child) should be in the registry");
        assert!(!jobs.contains(std::process::id()), "this process is not a hook");
        // Once the run ends, its job leaves the registry.
        run_thread.join().unwrap();
        assert!(!jobs.contains(hook_pid));
        let _ = child_pid;
    }
}
