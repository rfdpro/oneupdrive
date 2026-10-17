//! Explorer's sync-status flyout for OneUpDrive's folders, and the "vault"
//! state on their items.
//!
//! * [`StatusFactory`] (`IStorageProviderStatusUISourceFactory`, Windows 11
//!   23H2 and later): the flyout behind the cloud button in Explorer's
//!   address bar. What it shows comes from the agent, which publishes a few
//!   `key=value` lines per sync root to `HKCU\Software\OneUpDrive\Status`
//!   (`oneup_app::shellstatus`); this side only reads them, and watches the
//!   key to tell Explorer to ask again. Its buttons start `oneup.exe` /
//!   `oneup-settings.exe`, as the context menu does.
//! * [`States`] (`IStorageProviderItemPropertySource`, the custom-state
//!   handler): a lock on items in an encrypted vault's folder.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::Foundation::{TypedEventHandler, Uri};
use windows::Storage::Provider::{
    IStorageProviderItemPropertySource, IStorageProviderItemPropertySource_Impl, IStorageProviderStatusUISource,
    IStorageProviderStatusUISource_Impl, IStorageProviderStatusUISourceFactory,
    IStorageProviderStatusUISourceFactory_Impl, IStorageProviderUICommand, IStorageProviderUICommand_Impl,
    StorageProviderItemProperty, StorageProviderQuotaUI, StorageProviderState, StorageProviderStatusUI,
    StorageProviderUICommandState,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, KEY_READ, REG_NOTIFY_CHANGE_LAST_SET, RRF_RT_REG_SZ, RegCloseKey,
    RegGetValueW, RegNotifyChangeKeyValue, RegOpenKeyExW,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{AgileReference, HSTRING, IInspectable, PCWSTR, Ref, Result, implement};
use windows_collections::IIterable;

/// Where the agents publish (see `oneup_app::shellstatus`).
const KEY: &str = r"Software\OneUpDrive\Status";
/// The vault state's ID (declared when a vault's folder is registered).
pub const STATE_VAULT: i32 = 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// The published lines for sync root `id`, as a map.
fn published(id: &str) -> Option<HashMap<String, String>> {
    let (key, name) = (wide(KEY), wide(id));
    let mut len = 0u32;
    // SAFETY: valid NUL-terminated strings; first call asks for the size.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut len),
        )
    };
    if r != ERROR_SUCCESS || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize / 2 + 1];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: `buf` is writable for `size` bytes.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let text = String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]);
    Some(text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
}

/// Every published root's lines (for the custom state, by path).
fn all_published() -> Vec<HashMap<String, String>> {
    let Ok(values) = registry_value_names() else { return Vec::new() };
    values.iter().filter_map(|n| published(n)).collect()
}

fn registry_value_names() -> std::result::Result<Vec<String>, ()> {
    use windows::Win32::System::Registry::RegEnumValueW;
    let key = wide(KEY);
    let mut h = HKEY::default();
    // SAFETY: opens our own key for reading; closed below.
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), None, KEY_READ, &mut h) } != ERROR_SUCCESS {
        return Err(());
    }
    let mut out = Vec::new();
    for i in 0.. {
        let mut name = vec![0u16; 512];
        let mut len = name.len() as u32;
        // SAFETY: `name` is writable for `len` characters.
        let r = unsafe {
            RegEnumValueW(h, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
        };
        if r != ERROR_SUCCESS {
            break;
        }
        out.push(String::from_utf16_lossy(&name[..len as usize]));
    }
    // SAFETY: `h` was opened above.
    let _ = unsafe { RegCloseKey(h) };
    Ok(out)
}

/// This DLL's folder, where the programs are.
fn programs() -> Option<PathBuf> {
    crate::folder()
}

fn start(exe: &str, args: &[&str]) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if let Some(dir) = programs() {
        let _ = crate::spawn_outside_job(std::process::Command::new(dir.join(exe)).args(args), CREATE_NO_WINDOW);
    }
}

/// A small picture for the flyout, written once into the per-user cache
/// (Explorer wants a file address for each).
fn icon(name: &str) -> Option<Uri> {
    const STROKE: &str = "#3a86d9";
    let body = match name {
        "insync" => {
            r#"<path d="M5 12.5l4.5 4.5L19 7.5" fill="none" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"/>"#
        }
        "syncing" => {
            r#"<path d="M19 12a7 7 0 1 1-2.05-4.95M19 4v4h-4" fill="none" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#
        }
        "paused" => r#"<path d="M9 6v12M15 6v12" fill="none" stroke-width="2.4" stroke-linecap="round"/>"#,
        "attention" | "error" => {
            r#"<path d="M12 4l9 16H3zM12 10v4M12 17.2v.1" fill="none" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/>"#
        }
        "settings" => {
            r#"<circle cx="12" cy="12" r="3.2" fill="none" stroke-width="2"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3M5.3 5.3l2.1 2.1M16.6 16.6l2.1 2.1M5.3 18.7l2.1-2.1M16.6 7.4l2.1-2.1" stroke-width="2" stroke-linecap="round"/>"#
        }
        _ => {
            r#"<path d="M7 18h10a4 4 0 0 0 .5-7.97A6 6 0 0 0 6.1 9.1 4.5 4.5 0 0 0 7 18z" fill="none" stroke-width="2" stroke-linejoin="round"/>"#
        }
    };
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("OneUpDrive").join("flyout");
    let file = dir.join(format!("{name}.svg"));
    if !file.exists() {
        std::fs::create_dir_all(&dir).ok()?;
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" stroke="{STROKE}">{body}</svg>"#
        );
        std::fs::write(&file, svg).ok()?;
    }
    let url = format!("file:///{}", file.to_string_lossy().replace('\\', "/"));
    Uri::CreateUri(&HSTRING::from(url)).ok()
}

// ------------------------------------------------------------- commands

/// What a flyout button does.
#[derive(Clone)]
enum Action {
    /// The settings window of a data folder, on a page.
    Settings { dir: String, page: &'static str },
    /// The folder's page on onedrive.com.
    Online { local: String },
}

#[implement(IStorageProviderUICommand)]
struct Command {
    label: String,
    description: String,
    icon: &'static str,
    action: Action,
}

impl IStorageProviderUICommand_Impl for Command_Impl {
    fn Label(&self) -> Result<HSTRING> {
        Ok(HSTRING::from(&self.label))
    }
    fn Description(&self) -> Result<HSTRING> {
        Ok(HSTRING::from(&self.description))
    }
    fn Icon(&self) -> Result<Uri> {
        icon(self.icon).ok_or_else(|| windows::Win32::Foundation::E_FAIL.into())
    }
    fn State(&self) -> Result<StorageProviderUICommandState> {
        Ok(StorageProviderUICommandState::Enabled)
    }
    fn Invoke(&self) -> Result<()> {
        match &self.action {
            Action::Settings { dir, page } => start("oneup-settings.exe", &["--data-dir", dir, "--page", page]),
            Action::Online { local } => start("oneup.exe", &["shell-verb", "online", local]),
        }
        Ok(())
    }
}

// --------------------------------------------------------- the flyout

/// Handlers Explorer registered for a change of the status, by token.
type Handlers = Mutex<Vec<(i64, AgileReference<TypedEventHandler<IStorageProviderStatusUISource, IInspectable>>)>>;

fn handlers() -> &'static Handlers {
    static H: OnceLock<Handlers> = OnceLock::new();
    H.get_or_init(|| Mutex::new(Vec::new()))
}

/// Watches the key on a thread of its own (started once): every change
/// tells every registered handler.
fn watch() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new().name("oneup-status-watch".into()).spawn(|| {
            // SAFETY: COM for this thread (the handlers are agile references).
            let _ = unsafe {
                windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED)
            };
            let key = wide(KEY);
            let mut h = HKEY::default();
            // SAFETY: opens our own key; the event and key live for the
            // thread's life (the surrogate process's).
            unsafe {
                if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), None, KEY_NOTIFY, &mut h) != ERROR_SUCCESS {
                    return;
                }
                let Ok(event) = CreateEventW(None, false, false, None) else { return };
                loop {
                    if RegNotifyChangeKeyValue(h, false, REG_NOTIFY_CHANGE_LAST_SET, Some(event), true) != ERROR_SUCCESS
                    {
                        return;
                    }
                    WaitForSingleObject(HANDLE(event.0), INFINITE);
                    let list: Vec<_> = handlers()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .iter()
                        .filter_map(|(_, a)| a.resolve().ok())
                        .collect();
                    for handler in list {
                        // Explorer asks for the status again.
                        let _ = handler.Invoke(None, None);
                    }
                }
            }
        });
    });
}

#[implement(IStorageProviderStatusUISource)]
struct Source {
    root: String,
}

fn command(label: &str, description: &str, icon: &'static str, action: Action) -> IStorageProviderUICommand {
    Command { label: label.into(), description: description.into(), icon, action }.into()
}

impl IStorageProviderStatusUISource_Impl for Source_Impl {
    fn GetStatusUI(&self) -> Result<StorageProviderStatusUI> {
        let p = published(&self.root).unwrap_or_default();
        let get = |k: &str| p.get(k).cloned().unwrap_or_default();
        let ui = StorageProviderStatusUI::new()?;
        let state = get("state");
        ui.SetProviderState(match state.as_str() {
            "insync" => StorageProviderState::InSync,
            "syncing" => StorageProviderState::Syncing,
            "paused" => StorageProviderState::Paused,
            "attention" => StorageProviderState::Warning,
            "error" => StorageProviderState::Error,
            _ => StorageProviderState::Offline,
        })?;
        let label = if p.is_empty() { "Not running".to_string() } else { get("label") };
        ui.SetProviderStateLabel(&HSTRING::from(&label))?;
        if let Some(i) = icon(if state.is_empty() { "cloud" } else { &state }) {
            ui.SetProviderStateIcon(&i)?;
        }
        let dir = get("dir");
        if !dir.is_empty() {
            let detail = match get("detail") {
                _ if p.is_empty() => "OneUpDrive isn't running for this folder.".into(),
                d if d.is_empty() => "Open settings".into(),
                d => d,
            };
            ui.SetSyncStatusCommand(&command(
                &label,
                &detail,
                if state.is_empty() { "cloud" } else { "insync" },
                Action::Settings { dir: dir.clone(), page: "status" },
            ))?;
            if let (Ok(used), Ok(total)) = (get("quota_used").parse::<u64>(), get("quota_total").parse::<u64>())
                && total > 0
            {
                let q = StorageProviderQuotaUI::new()?;
                q.SetQuotaTotalInBytes(total)?;
                q.SetQuotaUsedInBytes(used)?;
                let gb = |b: u64| b as f64 / 1_073_741_824.0;
                q.SetQuotaUsedLabel(&HSTRING::from(format!("{:.1} GB of {:.0} GB used", gb(used), gb(total))))?;
                ui.SetQuotaUI(&q)?;
            }
            // The status line opens the settings; the button, OneDrive's
            // website (a vault's encrypted files mean nothing there, so its
            // button opens the vault's settings instead).
            let local = get("local");
            let primary = if get("kind") != "vault" && !local.is_empty() {
                command("View online", "Open this folder on OneDrive's website", "cloud", Action::Online { local })
            } else {
                command("Settings", "OneUpDrive's settings", "settings", Action::Settings { dir, page: "vaults" })
            };
            ui.SetProviderPrimaryCommand(&primary)?;
        }
        Ok(ui)
    }

    fn StatusUIChanged(
        &self,
        handler: Ref<TypedEventHandler<IStorageProviderStatusUISource, IInspectable>>,
    ) -> Result<i64> {
        static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
        let Some(h) = handler.as_ref() else { return Ok(0) };
        let token = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        handlers().lock().unwrap_or_else(|p| p.into_inner()).push((token, AgileReference::new(h)?));
        watch();
        Ok(token)
    }

    fn RemoveStatusUIChanged(&self, token: i64) -> Result<()> {
        handlers().lock().unwrap_or_else(|p| p.into_inner()).retain(|(t, _)| *t != token);
        Ok(())
    }
}

#[implement(IStorageProviderStatusUISourceFactory)]
pub struct StatusFactory;

impl IStorageProviderStatusUISourceFactory_Impl for StatusFactory_Impl {
    fn GetStatusUISource(&self, root: &HSTRING) -> Result<IStorageProviderStatusUISource> {
        Ok(Source { root: root.to_string() }.into())
    }
}

// ------------------------------------------------------ custom states

#[implement(IStorageProviderItemPropertySource)]
pub struct States;

impl IStorageProviderItemPropertySource_Impl for States_Impl {
    fn GetItemProperties(&self, path: &HSTRING) -> Result<IIterable<StorageProviderItemProperty>> {
        let path = path.to_string();
        let in_vault = all_published()
            .iter()
            .any(|p| p.get("kind").is_some_and(|k| k == "vault") && p.get("local").is_some_and(|l| below(&path, l)));
        let mut out = Vec::new();
        if in_vault {
            let p = StorageProviderItemProperty::new()?;
            p.SetId(STATE_VAULT)?;
            p.SetValue(&HSTRING::from("Encrypted in OneDrive (vault)"))?;
            p.SetIconResource(&HSTRING::from(r"%SystemRoot%\system32\imageres.dll,-1304"))?;
            out.push(Some(p));
        }
        Ok(IIterable::from(out))
    }
}

/// Whether `path` is inside `folder`, names compared as Windows compares
/// them (`oneup_sync::fold`).
fn below(path: &str, folder: &str) -> bool {
    oneup_sync::fold(path).strip_prefix(&oneup_sync::fold(folder)).is_some_and(|rest| rest.starts_with('\\'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn paths_compare_as_windows_compares_them() {
        assert!(below(r"C:\Users\Me\ΟΔΟΣ\x.txt", r"c:\users\me\οδος"));
        assert!(!below(r"C:\Users\Me\ΟΔΟΣ", r"C:\Users\Me\ΟΔΟΣ"), "the folder itself");
        assert!(!below(r"C:\Users\Me\ΟΔΟΣ2\x", r"C:\Users\Me\ΟΔΟΣ"));
        assert!(!below("C:\\\u{212A}it\\x", r"C:\kit"), "Kelvin sign");
    }

    #[test]
    fn an_unpublished_root_shows_as_not_running_and_items_have_no_state() {
        let src: IStorageProviderStatusUISource = Source { root: "OneUpDrive!test!none".into() }.into();
        let ui = src.GetStatusUI().unwrap();
        assert_eq!(ui.ProviderStateLabel().unwrap(), "Not running");
        let states: IStorageProviderItemPropertySource = States.into();
        let got = states.GetItemProperties(&HSTRING::from(r"C:\nowhere\x.txt")).unwrap();
        assert!(!got.First().unwrap().HasCurrent().unwrap());
    }

    /// Writes one value under our own key (removed again).
    fn put(name: &str, text: &str) {
        use windows::Win32::System::Registry::{
            KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCreateKeyExW, RegSetValueExW,
        };
        let (key, n, v) = (wide(KEY), wide(name), wide(text));
        let mut h = HKEY::default();
        // SAFETY: our own test key; NUL-terminated strings that outlive the calls.
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut h,
                None,
            )
            .ok()
            .unwrap();
            let bytes = std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 2);
            RegSetValueExW(h, PCWSTR(n.as_ptr()), None, REG_SZ, Some(bytes)).ok().unwrap();
            let _ = RegCloseKey(h);
        }
    }

    fn remove(name: &str) {
        use windows::Win32::System::Registry::RegDeleteKeyValueW;
        let (key, n) = (wide(KEY), wide(name));
        // SAFETY: our own test key; NUL-terminated strings that outlive the calls.
        unsafe {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(n.as_ptr()));
        }
    }

    #[test]
    fn the_flyout_shows_what_the_agent_published_and_vault_items_get_a_lock() {
        let name = format!("OneUpDrive!test!flyout{}", std::process::id());
        let local = format!(r"C:\Vault-test-{}", std::process::id());
        let text = format!(
            "state=syncing\nlabel=Syncing\ndetail=Busy\ndir=C:\\Data\nkind=vault\nlocal={local}\n\
             quota_used=5368709120\nquota_total=10737418240\n"
        );
        put(&name, &text);
        let src: IStorageProviderStatusUISource = Source { root: name.clone() }.into();
        let ui = src.GetStatusUI().unwrap();
        assert_eq!(ui.ProviderStateLabel().unwrap(), "Syncing");
        assert_eq!(ui.ProviderState().unwrap(), StorageProviderState::Syncing);
        assert_eq!(ui.QuotaUI().unwrap().QuotaUsedInBytes().unwrap(), 5_368_709_120);
        assert_eq!(
            ui.ProviderPrimaryCommand().unwrap().Label().unwrap(),
            "Settings",
            "a vault has no web page worth opening"
        );
        let states: IStorageProviderItemPropertySource = States.into();
        let inside = states.GetItemProperties(&HSTRING::from(format!(r"{local}\Sub.txt"))).unwrap().First().unwrap();
        let next = states.GetItemProperties(&HSTRING::from(format!(r"{local}2.txt"))).unwrap().First().unwrap();
        remove(&name);
        assert_eq!(inside.Current().unwrap().Id().unwrap(), STATE_VAULT);
        assert!(!next.HasCurrent().unwrap(), "a folder with a longer name isn't inside");
    }
}
