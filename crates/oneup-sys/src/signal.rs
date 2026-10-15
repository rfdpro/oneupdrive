//! A named event one program waits on and another raises, in the user's
//! session (`Local\`): how the settings window asks the tray to look for
//! an update now, without either polling.

use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject};
use windows::core::HSTRING;

fn full(name: &str) -> HSTRING {
    HSTRING::from(format!("Local\\{name}"))
}

/// The waiting side.
pub struct Signal(HANDLE);

// SAFETY: an event handle may be used and closed from any thread.
unsafe impl Send for Signal {}

impl Signal {
    /// Creates (or opens) the event `name`; it resets as a wait returns.
    pub fn new(name: &str) -> std::io::Result<Signal> {
        // SAFETY: the name is NUL-terminated and lives through the call.
        let h = unsafe { CreateEventW(None, false, false, &full(name)) }
            .map_err(|e| std::io::Error::other(format!("CreateEventW: {e}")))?;
        Ok(Signal(h))
    }

    /// Waits until the event is raised (`true`) or `limit` has passed.
    pub fn wait(&self, limit: Duration) -> bool {
        let ms = u32::try_from(limit.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: our own handle, open until drop.
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        // SAFETY: closing our own handle, once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Raises the event `name`; `false` if nobody is waiting on it (no such
/// event).
pub fn raise(name: &str) -> std::io::Result<bool> {
    // SAFETY: the name is NUL-terminated and lives through the call.
    let h = match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &full(name)) } {
        Ok(h) => h,
        Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(false),
        Err(e) => return Err(std::io::Error::other(format!("OpenEventW: {e}"))),
    };
    // SAFETY: the handle was just opened with the right to set it, and is
    // closed once.
    let r = unsafe {
        let r = SetEvent(h);
        let _ = CloseHandle(h);
        r
    };
    r.map(|()| true).map_err(|e| std::io::Error::other(format!("SetEvent: {e}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_raised_signal_wakes_the_waiter_once() {
        let name = format!("OneUpDrive-test-signal-{}", std::process::id());
        assert!(!super::raise(&name).unwrap(), "nobody waits yet");
        let s = super::Signal::new(&name).unwrap();
        assert!(!s.wait(std::time::Duration::from_millis(10)));
        assert!(super::raise(&name).unwrap());
        assert!(s.wait(std::time::Duration::from_secs(5)));
        assert!(!s.wait(std::time::Duration::from_millis(10)), "it resets");
    }
}
