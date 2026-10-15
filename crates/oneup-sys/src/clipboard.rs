//! Putting text on the clipboard (a sharing link).

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

fn err(op: &'static str) -> impl Fn(windows::core::Error) -> crate::Error {
    move |source| crate::Error::Win32 { op, source }
}

/// Replaces the clipboard's content with `text`.
pub fn set_text(text: &str) -> crate::Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let bytes = wide.len() * 2;
    // SAFETY: the memory is allocated for `bytes`, filled while locked, and
    // either handed to the clipboard (which then owns it) or freed here.
    // The clipboard is opened for this thread and closed on every path.
    unsafe {
        let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(err("GlobalAlloc"))?;
        let p = GlobalLock(mem).cast::<u16>();
        if p.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err(crate::Error::Unsupported);
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
        let _ = GlobalUnlock(mem);
        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(mem));
            return Err(err("OpenClipboard")(e));
        }
        let done = EmptyClipboard().and_then(|()| SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(mem.0))));
        let _ = CloseClipboard();
        if let Err(e) = done {
            let _ = GlobalFree(Some(mem));
            return Err(err("SetClipboardData")(e));
        }
    }
    Ok(())
}
