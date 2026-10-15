//! A window showing one HTML page in WebView2 (the Edge runtime built into
//! Windows 10 and 11), for OneUpDrive's settings: the page and the program
//! talk only through messages (`chrome.webview.postMessage` one way,
//! [`Poster::post`] the other).
//!
//! The page comes from the program itself (never from the network). The
//! runtime's own files (cache, cookies) go to `data_dir`, a folder of ours.
//! One window per process; [`run`] returns when it's closed.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_COLOR, CreateCoreWebView2EnvironmentWithOptions, ICoreWebView2, ICoreWebView2Controller,
    ICoreWebView2Controller2, ICoreWebView2Environment,
};
use webview2_com::{
    CoTaskMemPWSTR, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    WebMessageReceivedEventHandler,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW, LoadIconW, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SW_SHOW, SWP_NOMOVE, SWP_NOZORDER, SetForegroundWindow,
    SetWindowPos, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_COPYDATA, WM_DESTROY, WM_SIZE, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::{Error, Result};

/// Messages from the program to the page waiting for the window's thread.
const WM_POST: u32 = WM_APP + 1;

/// Marks the `WM_COPYDATA` that [`focus_existing`] sends to switch an open
/// window's page ("OUPG").
const GOTO_PAGE: usize = 0x4F55_5047;

/// The page message for `WM_COPYDATA`'s bytes: a page name, then
/// optionally a NUL and an item's path (UTF-8). The name keeps only ASCII
/// lowercase letters and the path is a JSON string, so another program
/// sending one can't put more than a page and a path before the page.
fn goto_message(bytes: &[u8]) -> Option<String> {
    let (page, item) = match bytes.iter().position(|&b| b == 0) {
        Some(i) => (&bytes[..i], std::str::from_utf8(&bytes[i + 1..]).ok()?),
        None => (bytes, ""),
    };
    if page.len() > 32 || item.len() > 32 * 1024 {
        return None;
    }
    let page: String = page.iter().map(|&b| b as char).filter(char::is_ascii_lowercase).collect();
    (!page.is_empty()).then(|| format!(r#"{{"event":"goto","page":"{page}","item":{}}}"#, json_string(item)))
}

/// `s` as a JSON string; `<` escaped too, so it can't end a script.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c < ' ' || c == '<' || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn win(op: &'static str, source: windows::core::Error) -> Error {
    Error::Win32 { op, source }
}

fn wv(op: &'static str, e: webview2_com::Error) -> Error {
    match e {
        webview2_com::Error::WindowsError(source) => Error::Win32 { op, source },
        other => Error::Win32 { op, source: windows::core::Error::new(windows::core::HRESULT(-1), format!("{other}")) },
    }
}

/// What the window needs.
pub struct Options<'a> {
    pub title: &'a str,
    /// In device-independent pixels.
    pub width: i32,
    pub height: i32,
    /// For the runtime's own files.
    pub data_dir: &'a Path,
    /// The whole page.
    pub html: &'a str,
    /// The page's background while it loads, and the title bar's theme.
    pub dark: bool,
}

/// Sends messages (JSON) to the page from any thread.
#[derive(Clone)]
pub struct Poster {
    hwnd: isize,
    queue: Arc<Mutex<VecDeque<String>>>,
}

impl Poster {
    pub fn post(&self, json: String) {
        self.queue.lock().unwrap_or_else(|p| p.into_inner()).push_back(json);
        // SAFETY: posting to a window that's gone just fails; the message
        // carries no pointers (the text waits in the queue).
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_POST, WPARAM(0), LPARAM(0)) };
    }

    /// Windows' folder picker over the window; `None` if cancelled. Blocks
    /// until it's closed (on a thread of its own: the dialog needs a
    /// single-threaded COM apartment).
    pub fn pick_folder(&self, title: &str) -> Result<Option<std::path::PathBuf>> {
        self.pick(title, true)
    }

    /// Windows' file picker over the window; `None` if cancelled.
    pub fn pick_file(&self, title: &str) -> Result<Option<std::path::PathBuf>> {
        self.pick(title, false)
    }

    fn pick(&self, title: &str, folders: bool) -> Result<Option<std::path::PathBuf>> {
        let (hwnd, title) = (self.hwnd, title.to_string());
        std::thread::spawn(move || pick(hwnd, &title, folders)).join().unwrap_or_else(|_| {
            Err(win("folder picker", windows::core::Error::from_hresult(windows::core::HRESULT(-1))))
        })
    }
}

fn pick(owner: isize, title: &str, folders: bool) -> Result<Option<std::path::PathBuf>> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, CoUninitialize};
    use windows::Win32::UI::Shell::{
        FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
    };
    // SAFETY: a fresh thread joins a single-threaded apartment, left below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok().map_err(|e| win("CoInitializeEx", e))?;
    // SAFETY: plain COM calls on interfaces this function owns; the path
    // string is copied, then freed with the allocator COM used.
    let picked = unsafe {
        (|| -> windows::core::Result<Option<std::path::PathBuf>> {
            let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
            let kind = if folders { FOS_PICKFOLDERS } else { Default::default() };
            dialog.SetOptions(dialog.GetOptions()? | kind | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)?;
            dialog.SetTitle(&HSTRING::from(title))?;
            if dialog.Show(Some(HWND(owner as *mut _))).is_err() {
                return Ok(None); // cancelled
            }
            let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = name.to_string().map_err(|_| windows::core::Error::from_hresult(windows::core::HRESULT(-1)));
            CoTaskMemFree(Some(name.0 as *const _));
            Ok(Some(std::path::PathBuf::from(path?)))
        })()
    };
    // SAFETY: pairs with the CoInitializeEx above, on the same thread.
    unsafe { CoUninitialize() };
    picked.map_err(|e| win("folder picker", e))
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

struct State {
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
    queue: Arc<Mutex<VecDeque<String>>>,
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_SIZE => {
            STATE.with(|s| {
                if let Some(s) = s.borrow().as_ref() {
                    let mut r = RECT::default();
                    // SAFETY: a live window and a local rectangle.
                    if unsafe { GetClientRect(hwnd, &mut r) }.is_ok() {
                        // SAFETY: the controller belongs to this window.
                        let _ = unsafe { s.controller.SetBounds(r) };
                    }
                }
            });
            LRESULT(0)
        }
        WM_POST => {
            STATE.with(|s| {
                if let Some(s) = s.borrow().as_ref() {
                    let pending: Vec<String> = s.queue.lock().unwrap_or_else(|p| p.into_inner()).drain(..).collect();
                    for json in pending {
                        let text = HSTRING::from(json.as_str());
                        // SAFETY: the string outlives the synchronous call.
                        let _ = unsafe { s.webview.PostWebMessageAsJson(&text) };
                    }
                }
            });
            LRESULT(0)
        }
        WM_COPYDATA => {
            use windows::Win32::System::DataExchange::COPYDATASTRUCT;
            // SAFETY: for WM_COPYDATA, Windows passes a valid COPYDATASTRUCT
            // whose buffer of `cbData` bytes stays readable during the call.
            let bytes = unsafe {
                let cds = &*(lp.0 as *const COPYDATASTRUCT);
                if cds.dwData != GOTO_PAGE || cds.lpData.is_null() {
                    return LRESULT(0);
                }
                std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize).to_vec()
            };
            let Some(json) = goto_message(&bytes) else { return LRESULT(0) };
            STATE.with(|s| {
                if let Some(s) = s.borrow().as_ref() {
                    s.queue.lock().unwrap_or_else(|p| p.into_inner()).push_back(json);
                }
            });
            // SAFETY: our own window; the text waits in the queue.
            let _ = unsafe { PostMessageW(Some(hwnd), WM_POST, WPARAM(0), LPARAM(0)) };
            LRESULT(1)
        }
        WM_DESTROY => {
            STATE.with(|s| {
                if let Some(s) = s.borrow_mut().take() {
                    // SAFETY: closing our own controller once.
                    let _ = unsafe { s.controller.Close() };
                }
            });
            // SAFETY: plain call.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: the arguments are the ones Windows passed us.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Brings an open settings window (of any process) to the front, on `page`
/// if it isn't empty, about `item` if that isn't (the page gets
/// `{"event":"goto","page":…,"item":…}`); false if there is none.
pub fn focus_existing(page: &str, item: &str) -> bool {
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SW_RESTORE, SendMessageW};
    // SAFETY: plain calls with a static class name; the page's bytes
    // outlive the synchronous SendMessageW, which only reads them.
    unsafe {
        match FindWindowW(w!("OneUpDriveSettings"), PCWSTR::null()) {
            Ok(h) if !h.is_invalid() => {
                let _ = ShowWindow(h, SW_RESTORE);
                let _ = SetForegroundWindow(h);
                if !page.is_empty() {
                    let data = format!("{page}\0{item}");
                    let cds = COPYDATASTRUCT {
                        dwData: GOTO_PAGE,
                        cbData: data.len() as u32,
                        lpData: data.as_ptr() as *mut _,
                    };
                    let _ = SendMessageW(h, WM_COPYDATA, None, Some(LPARAM(&cds as *const _ as isize)));
                }
                true
            }
            _ => false,
        }
    }
}

/// Gives the window an app ID of its own (so the taskbar groups and
/// activates it as this window, not as the identity package's app) and the
/// program's icon at the taskbar's and title bar's sizes.
fn own_taskbar_button(hwnd: HWND) {
    use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SM_CXICON, SM_CXSMICON, SendMessageW, WM_SETICON,
    };
    // SAFETY: our own window; the property store is released when dropped;
    // the icon resource is ours (resource 1) and stays loaded for the
    // window's life.
    unsafe {
        if let Ok(store) = SHGetPropertyStoreForWindow::<IPropertyStore>(hwnd) {
            let _ = store.SetValue(&PKEY_AppUserModel_ID, &PROPVARIANT::from("OneUpDrive.Settings"));
            let _ = store.Commit();
        }
        let Ok(instance) = GetModuleHandleW(None) else { return };
        let dpi = GetDpiForWindow(hwnd).max(96);
        for (which, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
            let side = GetSystemMetricsForDpi(metric, dpi);
            if let Ok(icon) = LoadImageW(
                Some(instance.into()),
                PCWSTR(std::ptr::without_provenance(1)),
                IMAGE_ICON,
                side,
                side,
                LR_DEFAULTCOLOR,
            ) {
                let _ = SendMessageW(hwnd, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(icon.0 as isize)));
            }
        }
    }
}

/// Shows the window and runs it until it's closed. `on_message` gets each
/// message the page sends (its JSON text) on the window's thread; it
/// should hand slow work elsewhere and answer through the [`Poster`].
pub fn run(opts: &Options<'_>, mut on_message: impl FnMut(String, &Poster) + 'static) -> Result<()> {
    // SAFETY: plain calls with static wide strings; the window procedure
    // lives for the whole program.
    let hwnd = unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().map_err(|e| win("CoInitializeEx", e))?;
        let instance = GetModuleHandleW(None).map_err(|e| win("GetModuleHandleW", e))?;
        let class = w!("OneUpDriveSettings");
        // The program's own icon (resource 1, embedded by the build), for
        // the title bar and the taskbar.
        // MAKEINTRESOURCE(1): the resource number in the pointer's place.
        let icon = LoadIconW(Some(instance.into()), PCWSTR(std::ptr::without_provenance(1))).unwrap_or_default();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hIcon: icon,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let title = HSTRING::from(opts.title);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &title,
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .map_err(|e| win("CreateWindowExW", e))?
    };
    // SAFETY: our own window; sizes scaled to its monitor's DPI.
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let _ =
            SetWindowPos(hwnd, None, 0, 0, opts.width * dpi / 96, opts.height * dpi / 96, SWP_NOMOVE | SWP_NOZORDER);
        let dark: windows::core::BOOL = opts.dark.into();
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            std::ptr::from_ref(&dark).cast(),
            size_of::<windows::core::BOOL>() as u32,
        );
        // Its own taskbar identity and icons: with the package's identity the
        // taskbar showed the package's logo on a plain plate and treated a
        // click as launching the package's app instead of bringing this
        // window forward.
        own_taskbar_button(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }

    // ---- the runtime, then the browser control in the window
    let data = HSTRING::from(opts.data_dir.as_os_str());
    let (tx, rx) = std::sync::mpsc::channel();
    CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            // SAFETY: the folder string outlives the call; the handler is
            // a live COM object.
            unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &data, None, &handler) }
                .map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, env| {
            code?;
            let _ = tx.send(env);
            Ok(())
        }),
    )
    .map_err(|e| wv("CreateCoreWebView2Environment", e))?;
    let env: ICoreWebView2Environment = rx
        .recv()
        .ok()
        .flatten()
        .ok_or_else(|| win("CreateCoreWebView2Environment", windows::core::Error::from_thread()))?;
    let (tx, rx) = std::sync::mpsc::channel();
    let env2 = env.clone();
    CreateCoreWebView2ControllerCompletedHandler::wait_for_async_operation(
        Box::new(move |handler| {
            // SAFETY: a live window and handler.
            unsafe { env2.CreateCoreWebView2Controller(hwnd, &handler) }.map_err(webview2_com::Error::WindowsError)
        }),
        Box::new(move |code, controller| {
            code?;
            let _ = tx.send(controller);
            Ok(())
        }),
    )
    .map_err(|e| wv("CreateCoreWebView2Controller", e))?;
    let controller: ICoreWebView2Controller = rx
        .recv()
        .ok()
        .flatten()
        .ok_or_else(|| win("CreateCoreWebView2Controller", windows::core::Error::from_thread()))?;

    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let poster = Poster { hwnd: hwnd.0 as isize, queue: Arc::clone(&queue) };
    // SAFETY: COM calls on live objects owned by this thread; strings
    // outlive the calls.
    let webview = unsafe {
        // No flash of white in dark mode while the page loads.
        if let Ok(c2) = windows::core::Interface::cast::<ICoreWebView2Controller2>(&controller) {
            let bg = if opts.dark {
                COREWEBVIEW2_COLOR { A: 255, R: 32, G: 32, B: 32 }
            } else {
                COREWEBVIEW2_COLOR { A: 255, R: 243, G: 243, B: 243 }
            };
            let _ = c2.SetDefaultBackgroundColor(bg);
        }
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        controller.SetBounds(r).map_err(|e| win("SetBounds", e))?;
        let webview = controller.CoreWebView2().map_err(|e| win("CoreWebView2", e))?;
        let settings = webview.Settings().map_err(|e| win("Settings", e))?;
        // A settings window, not a browser.
        let _ = settings.SetAreDevToolsEnabled(false);
        let _ = settings.SetIsStatusBarEnabled(false);
        let _ = settings.SetAreDefaultContextMenusEnabled(false);
        let _ = settings.SetIsZoomControlEnabled(false);
        let mut token = 0i64;
        let p = poster.clone();
        webview
            .add_WebMessageReceived(
                &WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                    if let Some(args) = args {
                        let mut raw = windows::core::PWSTR::null();
                        if args.TryGetWebMessageAsString(&mut raw).is_ok() {
                            // Freed when dropped.
                            let text = CoTaskMemPWSTR::from(raw);
                            on_message(text.to_string(), &p);
                        }
                    }
                    Ok(())
                })),
                &mut token,
            )
            .map_err(|e| win("add_WebMessageReceived", e))?;
        let html = HSTRING::from(opts.html);
        webview.NavigateToString(&html).map_err(|e| win("NavigateToString", e))?;
        webview
    };
    STATE.with(|s| *s.borrow_mut() = Some(State { controller, webview, queue }));

    // ---- until it's closed
    let mut msg = MSG::default();
    // SAFETY: the standard message loop on this thread.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    drop(env);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::goto_message;

    #[test]
    fn a_page_switch_carries_a_page_name_and_a_path_and_nothing_else() {
        assert_eq!(goto_message(b"activity").unwrap(), r#"{"event":"goto","page":"activity","item":""}"#);
        assert_eq!(goto_message(br#"x","op":"sign_out"#).unwrap(), r#"{"event":"goto","page":"xopsignout","item":""}"#);
        assert_eq!(
            goto_message("itemwhy\0C:\\a \"b\"</script>\n".as_bytes()).unwrap(),
            r#"{"event":"goto","page":"itemwhy","item":"C:\\a \"b\"\u003c/script>\u000a"}"#
        );
        assert_eq!(goto_message(b"\"<>"), None);
        assert_eq!(goto_message(&[b'a'; 33]), None);
        assert_eq!(goto_message(b"item\0\xff"), None);
    }
}
