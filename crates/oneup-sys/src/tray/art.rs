//! How the tray's menu looks: light or dark like the taskbar it opens
//! from, and the small pictures beside its entries.

use std::sync::OnceLock;

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, CreateCompatibleDC,
    CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    DeleteDC, DeleteObject, DrawTextW, FW_NORMAL, GdiFlush, GetTextFaceW, HBITMAP, HDC, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST, MonitorFromPoint, OUT_DEFAULT_PRECIS, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::core::{BOOL, HSTRING, PCSTR, w};

use super::Glyph;

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// The taskbar is dark (the "default Windows mode", not the apps' one).
fn taskbar_dark() -> bool {
    crate::user_registry::get_dword(PERSONALIZE, "SystemUsesLightTheme").ok().flatten() == Some(0)
}

/// uxtheme's dark-mode switches. Windows has no documented way to make a
/// popup menu dark; these (by ordinal, as Chromium, Electron and Firefox
/// use them) are what Explorer's own menus use. Only looked up from
/// Windows 10 1903 on: on 1809 ordinal 135 is a different function.
struct DarkMode {
    /// `SetPreferredAppMode`: 2 forces dark, 3 forces light.
    set_mode: unsafe extern "system" fn(i32) -> i32,
    /// `AllowDarkModeForWindow`.
    allow: unsafe extern "system" fn(HWND, BOOL) -> BOOL,
    /// `FlushMenuThemes`: menus pick the mode up at once.
    flush: unsafe extern "system" fn(),
}

fn dark_mode() -> Option<&'static DarkMode> {
    static FOUND: OnceLock<Option<DarkMode>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let build: u32 = crate::user_registry::machine_values(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
                .ok()?
                .into_iter()
                .find(|(k, _)| k == "CurrentBuildNumber")?
                .1
                .parse()
                .ok()?;
            if build < 18362 {
                return None;
            }
            // SAFETY: a system DLL by name; functions looked up by ordinal
            // (a PCSTR whose value is the ordinal) and given the signatures
            // they have had from 1903 on.
            unsafe {
                let ux = LoadLibraryW(w!("uxtheme.dll")).ok()?;
                let ordinal = |n: usize| GetProcAddress(ux, PCSTR(n as *const u8));
                type Found = unsafe extern "system" fn() -> isize;
                Some(DarkMode {
                    set_mode: std::mem::transmute::<Found, unsafe extern "system" fn(i32) -> i32>(ordinal(135)?),
                    allow: std::mem::transmute::<Found, unsafe extern "system" fn(HWND, BOOL) -> BOOL>(ordinal(133)?),
                    flush: std::mem::transmute::<Found, unsafe extern "system" fn()>(ordinal(136)?),
                })
            }
        })
        .as_ref()
}

/// Makes the menus about to open light or dark, like the taskbar.
/// Returns whether they're dark.
pub(super) fn follow_taskbar(hwnd: HWND) -> bool {
    let dark = taskbar_dark();
    if let Some(d) = dark_mode() {
        // SAFETY: the functions found above, with plain values and our
        // own window.
        unsafe {
            (d.set_mode)(if dark { 2 } else { 3 });
            let _ = (d.allow)(hwnd, dark.into());
            (d.flush)();
        }
        dark
    } else {
        false
    }
}

/// The pixel size of a menu picture on the monitor at (`x`, `y`).
pub(super) fn size_at(x: i32, y: i32) -> i32 {
    let (mut dx, mut dy) = (96u32, 96u32);
    // SAFETY: plain values in, two numbers out.
    unsafe {
        let m = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    }
    (16 * dx.max(96) / 96) as i32
}

/// A picture beside a menu entry: `size`×`size`, premultiplied alpha
/// (what menus draw blended). `ink`: the colour of symbols. `None` when
/// it can't be drawn (no icon font): the entry goes without.
pub(super) fn bitmap(g: Glyph, size: i32, ink: [u8; 3]) -> Option<HBITMAP> {
    let mut canvas = Canvas::new(size)?;
    let px = match g {
        Glyph::Dot(rgb) => dot_pixels(rgb, size),
        Glyph::Symbol(ch) => canvas.symbol(ch)?.into_iter().map(|a| (ink, a)).collect(),
    };
    canvas.fill(&px);
    Some(canvas.into_bitmap())
}

/// A dot of colour `rgb`: each pixel's colour and alpha, top row first.
/// About half the box, like the status dots in Windows' own menus, and
/// smoothed by 4×4 samples a pixel.
pub(super) fn dot_pixels(rgb: [u8; 3], size: i32) -> Vec<([u8; 3], u8)> {
    let n = size.max(0) as usize;
    let (c, r) = (size as f32 / 2.0, size as f32 * 0.28);
    (0..n * n)
        .map(|i| {
            let (x, y) = ((i % n) as f32, (i / n) as f32);
            let mut hits = 0u32;
            for s in 0..16 {
                let sx = x + (s % 4) as f32 / 4.0 + 0.125 - c;
                let sy = y + (s / 4) as f32 / 4.0 + 0.125 - c;
                hits += u32::from(sx * sx + sy * sy <= r * r);
            }
            (rgb, (hits * 255 / 16) as u8)
        })
        .collect()
}

/// A 32-bit, top-down bitmap and a device context drawing into it.
struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    size: i32,
}

impl Canvas {
    fn new(size: i32) -> Option<Canvas> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: `info` describes the bitmap; Windows allocates the pixels
        // and hands back their address. Each handle is freed once (`Drop`
        // or `into_bitmap`).
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(dc);
                return None;
            };
            let old = SelectObject(dc, bitmap.into());
            let mut c = Canvas { dc, bitmap, old, bits: bits.cast(), size };
            c.pixels().fill(0);
            Some(c)
        }
    }

    fn pixels(&mut self) -> &mut [u8] {
        let n = self.size as usize;
        // SAFETY: the section's `n`×`n` 4-byte pixels, alive as long as the
        // bitmap; only this thread draws into it.
        unsafe { std::slice::from_raw_parts_mut(self.bits, n * n * 4) }
    }

    /// Draws `ch` from the system's icon font; its coverage of each pixel.
    fn symbol(&mut self, ch: char) -> Option<Vec<u8>> {
        // Windows 11's icons, else Windows 10's (the same code points).
        let faces = ["Segoe Fluent Icons", "Segoe MDL2 Assets"];
        // SAFETY: our device context; each font is selected, used and
        // deleted here, the old one put back.
        unsafe {
            for face in faces {
                let font = CreateFontW(
                    -self.size,
                    0,
                    0,
                    0,
                    FW_NORMAL.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    // Grey levels, not ClearType's coloured fringes: they
                    // become the alpha.
                    ANTIALIASED_QUALITY,
                    0,
                    &HSTRING::from(face),
                );
                if font.is_invalid() {
                    continue;
                }
                let before = SelectObject(self.dc, font.into());
                // Windows substitutes a font that isn't there: check.
                let mut got = [0u16; 64];
                let len = GetTextFaceW(self.dc, Some(&mut got)).clamp(0, 64) as usize;
                if String::from_utf16_lossy(&got[..len]).trim_end_matches('\0') == face {
                    SetTextColor(self.dc, COLORREF(0x00FF_FFFF));
                    SetBkMode(self.dc, TRANSPARENT);
                    let mut buf = [0u16; 2];
                    let mut text = ch.encode_utf16(&mut buf).to_vec();
                    let mut rect = RECT { left: 0, top: 0, right: self.size, bottom: self.size };
                    DrawTextW(self.dc, &mut text, &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
                    let _ = GdiFlush();
                    SelectObject(self.dc, before);
                    let _ = DeleteObject(font.into());
                    // White on black: any channel is the coverage.
                    return Some(self.pixels().as_chunks::<4>().0.iter().map(|p| p[1]).collect());
                }
                SelectObject(self.dc, before);
                let _ = DeleteObject(font.into());
            }
        }
        None
    }

    /// Sets every pixel to a colour and an alpha (premultiplied, BGRA).
    fn fill(&mut self, px: &[([u8; 3], u8)]) {
        let pre = |c: u8, a: u8| (u16::from(c) * u16::from(a) / 255) as u8;
        for (out, ([r, g, b], a)) in self.pixels().as_chunks_mut::<4>().0.iter_mut().zip(px) {
            out.copy_from_slice(&[pre(*b, *a), pre(*g, *a), pre(*r, *a), *a]);
        }
    }

    fn into_bitmap(self) -> HBITMAP {
        let bitmap = self.bitmap;
        // SAFETY: our device context, deleted once, after the bitmap is
        // let go so it survives.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteDC(self.dc);
        }
        std::mem::forget(self);
        bitmap
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: ours, each freed once.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bitmap.into());
        }
    }
}
