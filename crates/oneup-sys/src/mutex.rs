//! A lock shared by every process of this Windows user, in any session
//! (they share the user's registry): a named mutex in the global
//! namespace, its name ending in the user's SID. Windows lets go of it when
//! the process holding it ends.

use std::marker::PhantomData;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::HSTRING;

/// The lock, held until dropped. Windows ties a mutex to the thread that
/// took it, so this stays on that thread (it isn't `Send`), and that thread
/// taking it again succeeds.
pub struct Held {
    h: HANDLE,
    _thread: PhantomData<*const ()>,
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex (taken in `try_lock`, and
        // `Held` never leaves the thread); the handle is ours, closed once.
        unsafe {
            let _ = ReleaseMutex(self.h);
            let _ = CloseHandle(self.h);
        }
    }
}

/// Takes the lock called `name` (letters, digits and `-`) for this user,
/// without waiting: `None` if another thread or process holds it.
pub fn try_lock(name: &str) -> std::io::Result<Option<Held>> {
    let sid = crate::pipe::current_user_sid()?;
    let full = HSTRING::from(format!(r"Global\{name}-{sid}"));
    // SAFETY: the name outlives the call; no security attributes (the
    // default DACL: this user); the handle is owned below.
    let h =
        unsafe { CreateMutexW(None, false, &full) }.map_err(|e| std::io::Error::other(format!("CreateMutexW: {e}")))?;
    // SAFETY: `h` is the mutex just opened; a zero timeout only asks.
    let r = unsafe { WaitForSingleObject(h, 0) };
    if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
        // Abandoned: its last holder ended without letting go; ours now.
        return Ok(Some(Held { h, _thread: PhantomData }));
    }
    // SAFETY: the handle is ours and isn't used again.
    let _ = unsafe { CloseHandle(h) };
    if r == WAIT_TIMEOUT { Ok(None) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn one_holder_at_a_time_and_free_once_dropped() {
        let name = format!("OneUpDrive-test-lock-{}", std::process::id());
        let (took, wait) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let other = {
            let name = name.clone();
            std::thread::spawn(move || {
                let held = try_lock(&name).unwrap().unwrap();
                took.send(()).unwrap();
                released.recv().unwrap();
                drop(held);
            })
        };
        wait.recv().unwrap();
        assert!(try_lock(&name).unwrap().is_none(), "held by the other thread");
        release.send(()).unwrap();
        other.join().unwrap();
        let mine = try_lock(&name).unwrap();
        assert!(mine.is_some(), "free again");
        // Another name is another lock.
        assert!(try_lock(&format!("{name}-other")).unwrap().is_some());
    }
}
