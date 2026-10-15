//! A notification-area ("tray") icon with a menu and notifications, for
//! `oneup-tray`. One [`Tray`] per thread: it owns a hidden window and must
//! be used on the thread that created it, which runs [`Tray::next`] as its
//! message loop. Other threads ask it to look again with a [`Waker`].
//!
//! The menu follows the taskbar's light or dark mode, with small pictures
//! beside its entries (`art`).
//!
//! The icon is identified by a number, not a GUID: Windows ties an icon's
//! GUID to the executable's path, which breaks when the program moves.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_RESPECT_QUIET_TIME,
    NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_BALLOONUSERCLICK, NIN_SELECT, NINF_KEY,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetMessageW, GetSystemMetrics, HICON, HMENU, IDCANCEL, IDNO, IDYES, MB_ICONERROR,
    MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_YESNO, MB_YESNOCANCEL, MENUINFO,
    MENUITEMINFOW, MESSAGEBOX_STYLE, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_BITMAP, MIM_STYLE,
    MNS_CHECKORBMP, MSG, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SM_CXSMICON, SetForegroundWindow, SetMenuInfo, SetMenuItemInfoW, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU,
    WM_ENDSESSION, WM_NULL, WNDCLASSW, WS_OVERLAPPED,
};
use windows::core::{HSTRING, PCWSTR, w};

mod art;

/// Shell notifications about the icon arrive as this message.
const WM_TRAY: u32 = WM_APP + 1;
/// [`Waker::wake`] posts this.
const WM_WAKE: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;
/// Selected with the keyboard (not in the bindings by name).
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

fn io(op: &'static str, e: windows::core::Error) -> std::io::Error {
    std::io::Error::other(format!("{op}: {e}"))
}

/// What happened, as [`Tray::next`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A [`Waker`] asked the tray to look again.
    Wake,
    /// The icon was clicked or chosen with the keyboard (left button,
    /// Enter, Space).
    Select { x: i32, y: i32 },
    /// The icon's context menu was asked for (right button, Shift+F10).
    Menu { x: i32, y: i32 },
    /// The user clicked the last notification.
    NotificationClicked,
    /// Time to go: Windows is signing out or shutting down, or another
    /// program asked the window to close (`taskkill`, an installer).
    Exit,
}

/// A menu entry. IDs must not be 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Action { id: u32, label: String, enabled: bool, glyph: Option<Glyph> },
    Separator,
    Submenu { label: String, items: Vec<Item>, glyph: Option<Glyph> },
}

/// The small picture beside a menu entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// A dot of this colour (RGB): how things are.
    Dot([u8; 3]),
    /// A symbol from Windows' icon font (Segoe Fluent Icons, or Segoe MDL2
    /// Assets on Windows 10), in the menu's text colour.
    Symbol(char),
}

/// How a notification or message presents itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Info,
    /// A question (shown as information in notifications).
    Question,
    Warning,
    Error,
}

/// The size, in pixels, of a notification-area icon on this display.
pub fn icon_size() -> u32 {
    // SAFETY: a query with no pointers.
    u32::try_from(unsafe { GetSystemMetrics(SM_CXSMICON) }).ok().filter(|&s| s > 0).unwrap_or(16)
}

/// An icon, from pixels.
pub struct Icon(HICON);

impl Icon {
    /// `rgba`: `size`×`size` pixels, top row first, 4 bytes each
    /// (straight, not premultiplied, alpha).
    pub fn from_rgba(rgba: &[u8], size: u32) -> std::io::Result<Icon> {
        let n = size as usize;
        if size == 0 || rgba.len() != n * n * 4 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "icon pixels don't match its size"));
        }
        let bgra: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();
        // The 1-bit mask is all zeros: the colour's alpha does the masking.
        let mask = vec![0u8; n.div_ceil(16) * 2 * n];
        let side = i32::try_from(size).map_err(std::io::Error::other)?;
        // SAFETY: both buffers are the sizes Windows reads for a
        // `size`×`size` icon (32 bits a pixel; a word-aligned 1-bit mask),
        // and live through the call, which copies them.
        let h = unsafe { CreateIcon(None, side, side, 1, 32, mask.as_ptr(), bgra.as_ptr()) }
            .map_err(|e| io("CreateIcon", e))?;
        Ok(Icon(h))
    }
}

impl Drop for Icon {
    fn drop(&mut self) {
        // SAFETY: we created the icon and destroy it once.
        let _ = unsafe { DestroyIcon(self.0) };
    }
}

thread_local! {
    /// Events the window procedure saw, for [`Tray::next`] to hand out.
    static EVENTS: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    /// Explorer restarted: the icon must be added again.
    static RECREATE: Cell<bool> = const { Cell::new(false) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

fn push(hwnd: HWND, e: Event) {
    EVENTS.with(|q| q.borrow_mut().push_back(e));
    look_again(hwnd);
}

/// Makes [`Tray::next`]'s `GetMessageW` return. Explorer *sends* the icon's
/// notifications (as Windows sends `WM_ENDSESSION` and "TaskbarCreated"):
/// `GetMessageW` runs the window procedure for those itself and goes on
/// waiting for a posted message. Without this, a click waited for the next
/// status change, often half a minute.
fn look_again(hwnd: HWND) {
    // SAFETY: our own window; no pointers. Failing (a full queue) means
    // there are messages to return anyway.
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let push = |e| push(hwnd, e);
    match msg {
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event in the low word of lParam,
            // the anchor point (screen coordinates) in wParam.
            let ev = (lp.0 & 0xFFFF) as u32;
            let x = i32::from((wp.0 & 0xFFFF) as u16 as i16);
            let y = i32::from(((wp.0 >> 16) & 0xFFFF) as u16 as i16);
            match ev {
                WM_CONTEXTMENU => push(Event::Menu { x, y }),
                NIN_SELECT | NIN_KEYSELECT => push(Event::Select { x, y }),
                NIN_BALLOONUSERCLICK => push(Event::NotificationClicked),
                _ => {}
            }
            LRESULT(0)
        }
        WM_WAKE => {
            push(Event::Wake);
            LRESULT(0)
        }
        WM_ENDSESSION => {
            if wp.0 != 0 {
                push(Event::Exit);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            push(Event::Exit);
            LRESULT(0)
        }
        m if m != 0 && m == TASKBAR_CREATED.get() => {
            RECREATE.set(true);
            look_again(hwnd);
            LRESULT(0)
        }
        // SAFETY: the arguments are the ones Windows passed us.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Copies `s` into a fixed, NUL-terminated buffer, cut short if needed.
fn fill(buf: &mut [u16], s: &str) {
    let max = buf.len().saturating_sub(1);
    let mut n = 0;
    for c in s.encode_utf16().take(max) {
        buf[n] = c;
        n += 1;
    }
    // Don't leave half a surrogate pair at the end.
    if n > 0 && (0xD800..0xDC00).contains(&buf[n - 1]) {
        n -= 1;
    }
    buf[n] = 0;
}

/// Asks a [`Tray`] on another thread to report [`Event::Wake`].
///
/// Wakes that come before the tray took the last one add nothing: the
/// window's queue holds at most one. (A message per agent status change
/// queued thousands while syncing, and a click on the icon waited its turn
/// behind them all, up to half a minute.)
#[derive(Clone)]
pub struct Waker {
    hwnd: isize,
    pending: Arc<AtomicBool>,
}

impl Waker {
    pub fn wake(&self) {
        if self.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        // SAFETY: posting to a window that's gone just fails; the message
        // carries no pointers.
        let posted = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_WAKE, WPARAM(0), LPARAM(0)) };
        if posted.is_err() {
            self.pending.store(false, Ordering::Release);
        }
    }
}

/// The icon (see the module docs). Removed when dropped.
pub struct Tray {
    hwnd: HWND,
    icon: RefCell<Option<Icon>>,
    tip: RefCell<String>,
    added: Cell<bool>,
    /// A wake is queued (see [`Waker`]).
    wake_pending: Arc<AtomicBool>,
}

impl Tray {
    /// Creates the hidden window; the icon appears with the first [`Tray::set`].
    pub fn new() -> std::io::Result<Tray> {
        // SAFETY: plain calls; the class and window names are static wide
        // strings, and the window procedure lives for the whole program.
        unsafe {
            // Sharp icons on high-DPI displays; failing only blurs them.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(None).map_err(|e| io("GetModuleHandleW", e))?;
            let class = w!("OneUpDriveTray");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            // A second registration (a second tray on this thread) fails
            // harmlessly; the window creation below is what counts.
            RegisterClassW(&wc);
            TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
            // A hidden top-level window (not message-only): the menu needs
            // a window that can come to the foreground.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!("OneUpDrive"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| io("CreateWindowExW", e))?;
            Ok(Tray {
                hwnd,
                icon: RefCell::new(None),
                tip: RefCell::new(String::new()),
                added: Cell::new(false),
                wake_pending: Arc::new(AtomicBool::new(false)),
            })
        }
    }

    pub fn waker(&self) -> Waker {
        Waker { hwnd: self.hwnd.0 as isize, pending: Arc::clone(&self.wake_pending) }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        }
    }

    /// Adds the icon, or updates it to the current picture and tooltip.
    fn show(&self) -> std::io::Result<()> {
        let mut d = self.data();
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = self.icon.borrow().as_ref().map(|i| i.0).unwrap_or_default();
        fill(&mut d.szTip, &self.tip.borrow());
        // SAFETY: `d` is a valid, fully initialised structure for the call.
        unsafe {
            if self.added.get() && Shell_NotifyIconW(NIM_MODIFY, &d).as_bool() {
                return Ok(());
            }
            if !Shell_NotifyIconW(NIM_ADD, &d).as_bool() {
                // No notification area yet (Explorer starting): it's added
                // when Explorer announces itself.
                self.added.set(false);
                return Err(std::io::Error::other("the notification area isn't available"));
            }
            d.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &d);
        }
        self.added.set(true);
        Ok(())
    }

    /// Sets the icon's picture and tooltip (cut to 127 characters).
    pub fn set(&self, icon: Icon, tip: &str) -> std::io::Result<()> {
        *self.icon.borrow_mut() = Some(icon);
        *self.tip.borrow_mut() = tip.to_string();
        self.show()
    }

    /// Shows a notification from the icon (Windows shows it as a toast,
    /// and holds it while the user doesn't want to be disturbed).
    pub fn notify(&self, title: &str, text: &str, kind: Kind) -> std::io::Result<()> {
        let mut d = self.data();
        d.uFlags = NIF_INFO;
        fill(&mut d.szInfoTitle, title);
        fill(&mut d.szInfo, text);
        d.dwInfoFlags = NIIF_RESPECT_QUIET_TIME
            | match kind {
                Kind::Info | Kind::Question => NIIF_INFO,
                Kind::Warning => NIIF_WARNING,
                Kind::Error => NIIF_ERROR,
            };
        // SAFETY: as in `show`.
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) }.as_bool() {
            Ok(())
        } else {
            Err(std::io::Error::other("the notification couldn't be shown"))
        }
    }

    /// Shows `items` as a menu at (`x`, `y`); the chosen entry's ID, if
    /// any.
    pub fn menu(&self, x: i32, y: i32, items: &[Item]) -> std::io::Result<Option<u32>> {
        let dark = art::follow_taskbar(self.hwnd);
        // The menu's text colour (Windows 11's; near enough on 10).
        let ink = if dark { [255, 255, 255] } else { [26, 26, 26] };
        let art = Art { size: art::size_at(x, y), ink, made: RefCell::new(Vec::new()) };
        let menu = build(items, &art)?;
        // SAFETY: `menu` is ours, destroyed once (with its submenus) below,
        // before the pictures in `art`; the window is ours.
        unsafe {
            // Without this the menu doesn't close when clicking elsewhere.
            // Windows allows it for a moment after the click; answered later
            // (a busy PC), it refuses, and the menu would stay up: then share
            // the foreground window's input for the call.
            if !SetForegroundWindow(self.hwnd).as_bool() {
                use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
                use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
                let theirs = GetWindowThreadProcessId(GetForegroundWindow(), None);
                let mine = GetCurrentThreadId();
                if theirs != 0 && theirs != mine && AttachThreadInput(mine, theirs, true).as_bool() {
                    let _ = SetForegroundWindow(self.hwnd);
                    let _ = AttachThreadInput(mine, theirs, false);
                }
            }
            let chosen = TrackPopupMenuEx(
                menu,
                (TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0,
                x,
                y,
                self.hwnd,
                None,
            );
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            drop(art);
            Ok(u32::try_from(chosen.0).ok().filter(|&id| id != 0))
        }
    }

    /// Shows a message in front of everything and waits for the answer.
    pub fn ask(&self, title: &str, text: &str, kind: Kind, buttons: Buttons) -> Reply {
        let style = MB_SETFOREGROUND
            | match kind {
                Kind::Info => MB_ICONINFORMATION,
                Kind::Question => MB_ICONQUESTION,
                Kind::Warning => MB_ICONWARNING,
                Kind::Error => MB_ICONERROR,
            }
            | match buttons {
                Buttons::Ok => MB_OK,
                Buttons::YesNo => MB_YESNO,
                Buttons::YesNoCancel => MB_YESNOCANCEL,
            };
        message_box(Some(self.hwnd), title, text, style)
    }

    /// Like [`Tray::ask`], with buttons of our own: `choices` are ids and
    /// labels (a label's second line, after `\n`, explains it); `default`
    /// is the id Enter chooses. Returns the chosen id, or `None` when closed.
    pub fn choose(&self, title: &str, text: &str, kind: Kind, choices: &[(i32, &str)], default: i32) -> Option<i32> {
        choose(self.hwnd, title, text, kind, choices, default)
    }

    /// Waits for the next event. `None`: the loop is over ([`Tray::quit`]).
    pub fn next(&self) -> Option<Event> {
        loop {
            if RECREATE.replace(false) {
                self.added.set(false);
                let _ = self.show();
            }
            if let Some(e) = EVENTS.with(|q| q.borrow_mut().pop_front()) {
                if e == Event::Wake {
                    // Before the caller looks: a change after this wakes again.
                    self.wake_pending.store(false, Ordering::Release);
                }
                return Some(e);
            }
            let mut msg = MSG::default();
            // SAFETY: `msg` is a valid out-structure; messages go to the
            // window procedure above.
            unsafe {
                if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                    return None;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Ends the loop: [`Tray::next`] returns `None` from now on.
    pub fn quit(&self) {
        // SAFETY: no pointers.
        unsafe { PostQuitMessage(0) };
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let d = self.data();
        // SAFETY: removes our own icon and destroys our own window, once.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &d);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// One menu's pictures: drawn while it's built, freed after it's gone.
struct Art {
    size: i32,
    ink: [u8; 3],
    made: RefCell<Vec<HBITMAP>>,
}

impl Art {
    fn bitmap(&self, g: Glyph) -> Option<HBITMAP> {
        let b = art::bitmap(g, self.size, self.ink)?;
        self.made.borrow_mut().push(b);
        Some(b)
    }
}

impl Drop for Art {
    fn drop(&mut self) {
        for b in self.made.take() {
            // SAFETY: ours, in no menu any more, freed once.
            let _ = unsafe { DeleteObject(b.into()) };
        }
    }
}

/// Builds a popup menu; the caller destroys it (submenus go with it), then
/// `art`.
fn build(items: &[Item], art: &Art) -> std::io::Result<HMENU> {
    // SAFETY: creates a menu we own; on failure below it's destroyed.
    let menu = unsafe { CreatePopupMenu() }.map_err(|e| io("CreatePopupMenu", e))?;
    // Pictures go where check marks would: no second, empty column.
    let info = MENUINFO {
        cbSize: size_of::<MENUINFO>() as u32,
        fMask: MIM_STYLE,
        dwStyle: MNS_CHECKORBMP,
        ..Default::default()
    };
    // SAFETY: our menu and a valid structure; failing only widens the menu.
    let _ = unsafe { SetMenuInfo(menu, &info) };
    let add = || -> std::io::Result<()> {
        for (pos, item) in items.iter().enumerate() {
            // SAFETY: labels are NUL-terminated and live through each call,
            // which copies them; a submenu becomes part of `menu`.
            let glyph = unsafe {
                match item {
                    Item::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, None).map(|()| None),
                    Item::Action { id, label, enabled, glyph } => {
                        let flags = if *enabled { MF_STRING } else { MF_STRING | MF_GRAYED };
                        AppendMenuW(menu, flags, *id as usize, &HSTRING::from(label.as_str())).map(|()| *glyph)
                    }
                    Item::Submenu { label, items, glyph } => {
                        let sub = build(items, art)?;
                        AppendMenuW(menu, MF_STRING | MF_POPUP, sub.0 as usize, &HSTRING::from(label.as_str()))
                            .map(|()| *glyph)
                    }
                }
            }
            .map_err(|e| io("AppendMenuW", e))?;
            if let Some(b) = glyph.and_then(|g| art.bitmap(g)) {
                let mii = MENUITEMINFOW {
                    cbSize: size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_BITMAP,
                    hbmpItem: b,
                    ..Default::default()
                };
                // SAFETY: our menu, an entry it has, a valid structure; the
                // bitmap outlives the menu (`Art`). Failing leaves the entry
                // without its picture.
                let _ = unsafe { SetMenuItemInfoW(menu, pos as u32, true, &mii) };
            }
        }
        Ok(())
    };
    if let Err(e) = add() {
        // SAFETY: ours, destroyed once.
        let _ = unsafe { DestroyMenu(menu) };
        return Err(e);
    }
    Ok(menu)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buttons {
    Ok,
    YesNo,
    YesNoCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Ok,
    Yes,
    No,
    Cancel,
}

fn message_box(owner: Option<HWND>, title: &str, text: &str, style: MESSAGEBOX_STYLE) -> Reply {
    // SAFETY: the strings are NUL-terminated and live through the call.
    let r = unsafe { MessageBoxW(owner, &HSTRING::from(text), &HSTRING::from(title), style) };
    match r {
        IDYES => Reply::Yes,
        IDNO => Reply::No,
        IDCANCEL => Reply::Cancel,
        _ => Reply::Ok,
    }
}

/// `TaskDialogIndirect` (comctl32).
type TaskDialogIndirectFn = unsafe extern "system" fn(
    *const windows::Win32::UI::Controls::TASKDIALOGCONFIG,
    *mut i32,
    *mut i32,
    *mut windows::core::BOOL,
) -> windows::core::HRESULT;

/// A task dialog with command-link buttons. Looked up when it's shown:
/// only the common controls 6 have it, which the programs' manifest asks
/// for and a test binary (no manifest) doesn't get. Without it, a message
/// box with Yes, No and Cancel standing for the first three choices.
fn choose(owner: HWND, title: &str, text: &str, kind: Kind, choices: &[(i32, &str)], default: i32) -> Option<i32> {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::Controls::{
        TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TD_ERROR_ICON, TD_INFORMATION_ICON, TD_WARNING_ICON,
        TDF_ALLOW_DIALOG_CANCELLATION, TDF_USE_COMMAND_LINKS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{MB_DEFBUTTON1, MB_DEFBUTTON2, MB_DEFBUTTON3};
    // SAFETY: a system DLL by name, and a function looked up in it by name.
    let found = unsafe { LoadLibraryW(w!("comctl32.dll")) }
        .ok()
        .and_then(|m| unsafe { GetProcAddress(m, windows::core::s!("TaskDialogIndirect")) });
    let Some(found) = found else {
        let keys = ["Yes", "No", "Cancel"];
        let how: Vec<String> = choices
            .iter()
            .zip(keys)
            .map(|((_, label), key)| format!("{key}: {}", label.split('\n').next().unwrap_or_default()))
            .collect();
        let style = MB_SETFOREGROUND
            | MB_ICONWARNING
            | if choices.len() > 2 { MB_YESNOCANCEL } else { MB_YESNO }
            | match choices.iter().position(|c| c.0 == default) {
                Some(1) => MB_DEFBUTTON2,
                Some(2) => MB_DEFBUTTON3,
                _ => MB_DEFBUTTON1,
            };
        let r = message_box(Some(owner), title, &format!("{text}\n\n{}", how.join("\n")), style);
        let i = match r {
            Reply::Yes => 0,
            Reply::No => 1,
            _ if choices.len() > 2 => 2,
            _ => return None,
        };
        return choices.get(i).map(|c| c.0);
    };
    // SAFETY: the documented signature of `TaskDialogIndirect`.
    let dialog: TaskDialogIndirectFn = unsafe { std::mem::transmute(found) };
    let title = HSTRING::from(title);
    let text = HSTRING::from(text);
    let labels: Vec<HSTRING> = choices.iter().map(|(_, l)| HSTRING::from(*l)).collect();
    let buttons: Vec<TASKDIALOG_BUTTON> = choices
        .iter()
        .zip(&labels)
        .map(|((id, _), l)| TASKDIALOG_BUTTON { nButtonID: *id, pszButtonText: PCWSTR(l.as_ptr()) })
        .collect();
    let mut config = TASKDIALOGCONFIG {
        cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner,
        dwFlags: TDF_USE_COMMAND_LINKS | TDF_ALLOW_DIALOG_CANCELLATION,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszContent: PCWSTR(text.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: default,
        ..Default::default()
    };
    config.Anonymous1.pszMainIcon = match kind {
        Kind::Warning => TD_WARNING_ICON,
        Kind::Error => TD_ERROR_ICON,
        Kind::Info | Kind::Question => TD_INFORMATION_ICON,
    };
    // In front, as `ask`'s message boxes are.
    // SAFETY: our own window.
    unsafe {
        let _ = SetForegroundWindow(owner);
    }
    let mut chosen = 0i32;
    // SAFETY: `config` and everything it points to (titles, labels,
    // buttons) live through the call; `chosen` receives the button.
    let r = unsafe { dialog(&config, &mut chosen, std::ptr::null_mut(), std::ptr::null_mut()) };
    // Closed (Esc, the title bar's X) reports IDCANCEL.
    (r.is_ok() && choices.iter().any(|c| c.0 == chosen)).then_some(chosen)
}

/// Shows an error without a tray (it couldn't start).
pub fn error_box(title: &str, text: &str) {
    message_box(None, title, text, MB_OK | MB_ICONERROR | MB_SETFOREGROUND);
}

/// Held while this is the only instance named `name` in this sign-in
/// session.
pub struct Instance(HANDLE);

impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: our own handle, closed once.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// `None` if another process already holds `name` (letters, digits and
/// dashes).
pub fn single_instance(name: &str) -> std::io::Result<Option<Instance>> {
    let full = HSTRING::from(format!("Local\\{name}"));
    // SAFETY: the name is NUL-terminated and lives through the call; the
    // last error is read straight after it.
    unsafe {
        let h = CreateMutexW(None, false, PCWSTR(full.as_ptr())).map_err(|e| io("CreateMutexW", e))?;
        let exists = GetLastError() == ERROR_ALREADY_EXISTS;
        let held = Instance(h);
        Ok((!exists).then_some(held))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn text_is_cut_to_fit_without_splitting_a_character() {
        let mut buf = [1u16; 4];
        fill(&mut buf, "ab");
        assert_eq!(buf[..3], [b'a' as u16, b'b' as u16, 0]);
        fill(&mut buf, "abcdef");
        assert_eq!(buf, [b'a' as u16, b'b' as u16, b'c' as u16, 0]);
        // "ab😀": the emoji's two halves don't fit; neither is kept.
        fill(&mut buf, "ab\u{1F600}");
        assert_eq!(buf[..3], [b'a' as u16, b'b' as u16, 0]);
    }

    #[test]
    fn icons_are_made_from_pixels_of_the_right_size() {
        let px = vec![200u8; 16 * 16 * 4];
        assert!(Icon::from_rgba(&px, 16).is_ok());
        assert!(Icon::from_rgba(&px, 15).is_err());
        assert!(icon_size() >= 16);
    }

    #[test]
    fn only_one_instance_holds_a_name() {
        let name = format!("oneup-sys-test-{}", std::process::id());
        let a = single_instance(&name).unwrap();
        assert!(a.is_some());
        assert!(single_instance(&name).unwrap().is_none());
        drop(a);
        assert!(single_instance(&name).unwrap().is_some());
    }

    #[test]
    fn menus_build_with_submenus() {
        let items = vec![
            Item::Action { id: 1, label: "Up to date".into(), enabled: false, glyph: Some(Glyph::Dot([0, 200, 0])) },
            Item::Separator,
            Item::Submenu {
                label: "Pause".into(),
                items: vec![Item::Action { id: 2, label: "1 hour".into(), enabled: true, glyph: None }],
                glyph: Some(Glyph::Symbol('\u{E769}')),
            },
        ];
        let art = Art { size: 16, ink: [0, 0, 0], made: RefCell::new(Vec::new()) };
        let m = build(&items, &art).unwrap();
        // SAFETY: ours, destroyed once.
        unsafe { DestroyMenu(m).unwrap() };
        // The dot always; the symbol too where the icon font is installed.
        assert!(!art.made.borrow().is_empty());
    }

    #[test]
    fn wakes_queue_at_most_once() {
        use windows::Win32::UI::WindowsAndMessaging::{PM_NOREMOVE, PeekMessageW};
        let tray = Tray::new().unwrap();
        let w = tray.waker();
        for _ in 0..1000 {
            w.wake();
        }
        assert_eq!(tray.next(), Some(Event::Wake));
        let mut msg = MSG::default();
        // SAFETY: a valid out-structure; nothing is taken off the queue.
        let more = unsafe { PeekMessageW(&mut msg, Some(tray.hwnd), WM_WAKE, WM_WAKE, PM_NOREMOVE) };
        assert!(!more.as_bool(), "one wake for a thousand calls");
        // Taken: the next change wakes again.
        w.wake();
        assert_eq!(tray.next(), Some(Event::Wake));
    }

    #[test]
    fn sent_notifications_are_reported_at_once() {
        use windows::Win32::UI::WindowsAndMessaging::SendNotifyMessageW;
        let tray = Tray::new().unwrap();
        let (hwnd, w) = (tray.hwnd.0 as isize, tray.waker());
        let other = std::thread::spawn(move || {
            // Explorer sends a click this way (from its own thread, without
            // waiting); nothing is posted after it for a while.
            // SAFETY: a window that outlives this thread; no pointers.
            let sent = unsafe {
                SendNotifyMessageW(HWND(hwnd as *mut _), WM_TRAY, WPARAM(0x0064_0032), LPARAM(WM_CONTEXTMENU as isize))
            };
            assert!(sent.is_ok());
            std::thread::sleep(std::time::Duration::from_secs(3));
            w.wake();
        });
        let start = std::time::Instant::now();
        assert_eq!(tray.next(), Some(Event::Menu { x: 50, y: 100 }));
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "reported after {:?}", start.elapsed());
        other.join().unwrap();
    }

    #[test]
    fn menu_dots_are_drawn_with_alpha() {
        let px = art::dot_pixels([200, 100, 50], 16);
        // A clear corner, a solid middle, premultiplied.
        assert_eq!(px[0], ([200, 100, 50], 0));
        assert_eq!(px[8 * 16 + 8].1, 255);
        assert!(art::bitmap(Glyph::Dot([200, 100, 50]), 16, [0, 0, 0]).is_some_and(|b| {
            // SAFETY: ours, freed once.
            unsafe { DeleteObject(b.into()) }.as_bool()
        }));
    }
}
