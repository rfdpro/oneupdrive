//! Thin wrappers over the Windows Cloud Files API (`cldapi.dll`), the
//! platform layer behind Files On-Demand.
//!
//! * [`provider`] is OneUpDrive's own provider side, under the
//!   rules in `data-safety.md` invariants 11–14; [`identity`] is what its
//!   placeholders carry.
//! * The rest only *inspects*: whether a path lies inside some cloud
//!   provider's sync root (the official OneDrive client, Dropbox, iCloud,
//!   ...), placeholder state from directory-listing metadata, and making sure
//!   Windows doesn't disguise placeholders from us. None of that opens a
//!   file, so none of it can trigger a download or a directory population.

pub mod closes;
pub mod identity;
pub mod ours;
#[cfg(windows)]
pub mod provider;
mod recent;
#[cfg(windows)]
pub mod shell;

/// Placeholder state bits, as returned by `CfGetPlaceholderStateFromAttributeTag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaceholderState(pub u32);

impl PlaceholderState {
    pub const PLACEHOLDER: u32 = 0x1;
    pub const SYNC_ROOT: u32 = 0x2;
    pub const IN_SYNC: u32 = 0x8;
    /// A file whose data isn't (fully) on disk, or a directory whose children
    /// haven't been (fully) populated. Listing a partial directory makes the
    /// provider fetch its children from the cloud (measured: a lazy
    /// directory reported `partial` until first listed).
    pub const PARTIAL: u32 = 0x10;
    pub const PARTIALLY_ON_DISK: u32 = 0x20;
    /// Returned by the API when the input isn't understood.
    pub const INVALID: u32 = u32::MAX;

    pub fn is_placeholder(self) -> bool {
        self.0 != Self::INVALID && self.0 & Self::PLACEHOLDER != 0
    }

    pub fn is_partial(self) -> bool {
        self.0 != Self::INVALID && self.0 & Self::PARTIAL != 0
    }

    pub fn is_in_sync(self) -> bool {
        self.0 != Self::INVALID && self.0 & Self::IN_SYNC != 0
    }
}

/// Decodes the placeholder state of a directory entry from its attributes and
/// reparse tag alone (no file is opened). Outside Windows, or for ordinary
/// files, this is "no state".
pub fn placeholder_state(attributes: u32, reparse_tag: u32) -> PlaceholderState {
    #[cfg(windows)]
    {
        // SAFETY: a pure function of its two integer arguments; it touches no memory of ours.
        let s = unsafe {
            windows::Win32::Storage::CloudFilters::CfGetPlaceholderStateFromAttributeTag(attributes, reparse_tag)
        };
        PlaceholderState(s.0)
    }
    #[cfg(not(windows))]
    {
        let _ = (attributes, reparse_tag);
        PlaceholderState::default()
    }
}

/// Makes Windows show this process cloud placeholders as what they are.
///
/// By default Windows may *disguise* placeholders for a program: the reparse
/// point attribute and tag (and the offline bit) are removed from directory
/// listings **and** from queries on open handles, so placeholders look like
/// ordinary files. That was measured for `oneup.exe` itself: a scan of a
/// OneDrive folder recorded no cloud tags at all, and a non-provider console
/// process started in "disguise" mode and saw tags only after switching to
/// "expose". Everything that decides
/// whether a file is a placeholder (the scanner, `--verify`) depends on
/// seeing them, so call this before any of it. It is process-wide and
/// idempotent; it changes only what this process is shown.
#[cfg(windows)]
pub fn expose_placeholders() -> Result<(), PlaceholderModeError> {
    /// `PHCM_EXPOSE_PLACEHOLDERS` (ntifs.h).
    const PHCM_EXPOSE_PLACEHOLDERS: i8 = 2;
    // SAFETY: a documented ntdll call (Windows 10 1803+) taking a constant;
    // it reads and writes only this process's own placeholder mode.
    let previous = unsafe {
        windows::Wdk::Storage::FileSystem::RtlSetProcessPlaceholderCompatibilityMode(PHCM_EXPOSE_PLACEHOLDERS)
    };
    // Negative values are errors (PHCM_ERROR_*); others are the previous mode.
    if previous < 0 { Err(PlaceholderModeError(previous)) } else { Ok(()) }
}

#[cfg(not(windows))]
pub fn expose_placeholders() -> Result<(), PlaceholderModeError> {
    Ok(())
}

/// Windows refused to switch the placeholder mode (the raw `PHCM_ERROR_*` code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("Windows couldn't be asked to show cloud placeholders (code {0}); refusing to continue without them")]
pub struct PlaceholderModeError(pub i8);

/// The name OneUpDrive registers its own sync roots under.
pub const PROVIDER_NAME: &str = "OneUpDrive";

/// The provider of the sync root containing `path`, unless it is
/// OneUpDrive's own (Files On-Demand): what the coexistence checks refuse.
/// Ours by the registered provider name or, failing that, by the shell's
/// record of our sync roots: Windows can leave a shell-registered root's
/// Cloud Files registration without its name and identity (seen after
/// re-registering the identity package with the agent stopped), and our
/// own folder must not then pass for another program's.
pub fn other_sync_root(path: &std::path::Path) -> Option<SyncRootInfo> {
    #[cfg(windows)]
    let ours_by_shell = || shell::is_ours(path);
    #[cfg(not(windows))]
    let ours_by_shell = || false;
    sync_root_containing(path).filter(|i| i.provider_name != PROVIDER_NAME && !ours_by_shell())
}

/// The cloud provider that owns a sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRootInfo {
    /// The provider's display name as it registered itself (e.g. "OneDrive").
    pub provider_name: String,
    pub provider_version: String,
}

/// Returns the provider whose registered sync root contains `path`, or `None`
/// if `path` isn't inside any cloud sync root (or can't be queried).
///
/// This works for *any* Cloud Files provider, not just OneDrive, which is what
/// we want: no other sync engine may be managing a folder we're asked to manage.
#[cfg(windows)]
pub fn sync_root_containing(path: &std::path::Path) -> Option<SyncRootInfo> {
    use windows::Win32::Storage::CloudFilters::{
        CF_SYNC_ROOT_INFO_STANDARD, CF_SYNC_ROOT_STANDARD_INFO, CfGetSyncRootInfoByPath,
    };
    use windows::core::HSTRING;

    // The registration (STANDARD), not the connected provider (PROVIDER):
    // with no provider connected, PROVIDER's name is left unset, and our
    // own folder, with the agent stopped, read as "managed by ì€€" (measured),
    // which made `oneup on-demand on` refuse it.
    let w = HSTRING::from(path.as_os_str());
    let mut buf = vec![0u8; size_of::<CF_SYNC_ROOT_STANDARD_INFO>() + 4096];
    let mut returned = 0u32;
    // SAFETY: `w` is NUL-terminated and outlives the call; `buf` is writable
    // for the length passed.
    let r = unsafe {
        CfGetSyncRootInfoByPath(
            &w,
            CF_SYNC_ROOT_INFO_STANDARD,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            Some(&mut returned),
        )
    };
    // Any failure (most commonly "not a cloud file") means "no provider we can
    // see". Callers treat this as advisory and combine it with other signals.
    r.ok()?;
    // SAFETY: the buffer begins with the structure, which may be unaligned.
    let info: CF_SYNC_ROOT_STANDARD_INFO = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    Some(SyncRootInfo {
        provider_name: wide_to_string(&info.ProviderName),
        provider_version: wide_to_string(&info.ProviderVersion),
    })
}

#[cfg(not(windows))]
pub fn sync_root_containing(_path: &std::path::Path) -> Option<SyncRootInfo> {
    None
}

/// Decodes a fixed-size, NUL-terminated UTF-16 buffer.
#[cfg_attr(not(windows), allow(dead_code))]
fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn ordinary_entries_are_not_placeholders() {
        // FILE_ATTRIBUTE_ARCHIVE, no reparse tag.
        let s = placeholder_state(0x20, 0);
        assert!(!s.is_placeholder() && !s.is_partial());
    }

    #[test]
    fn placeholders_can_be_exposed() {
        assert_eq!(expose_placeholders(), Ok(()));
        assert_eq!(expose_placeholders(), Ok(()), "idempotent");
    }

    #[test]
    fn temp_dir_is_not_in_a_sync_root() {
        assert_eq!(sync_root_containing(&std::env::temp_dir()), None);
    }

    #[test]
    fn wide_buffers_stop_at_nul() {
        let mut b = [0u16; 8];
        b[..3].copy_from_slice(&[b'a' as u16, b'b' as u16, b'c' as u16]);
        assert_eq!(wide_to_string(&b), "abc");
    }
}
