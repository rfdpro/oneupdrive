//! Where Windows keeps a user's own folders (Desktop, Documents, ...), for
//! PC folder backup. Read only: OneUpDrive never moves or redirects them.

use std::path::PathBuf;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_Videos, KF_FLAG_DEFAULT,
    SHGetKnownFolderPath,
};
use windows::core::GUID;

/// The folders PC folder backup offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Known {
    Desktop,
    Documents,
    Pictures,
    Music,
    Videos,
}

impl Known {
    pub const ALL: [Known; 5] = [Known::Desktop, Known::Documents, Known::Pictures, Known::Music, Known::Videos];

    /// The English name, which is also the OneDrive folder's name.
    pub fn name(self) -> &'static str {
        match self {
            Known::Desktop => "Desktop",
            Known::Documents => "Documents",
            Known::Pictures => "Pictures",
            Known::Music => "Music",
            Known::Videos => "Videos",
        }
    }

    pub fn parse(s: &str) -> Option<Known> {
        Known::ALL.into_iter().find(|k| k.name().eq_ignore_ascii_case(s))
    }

    fn id(self) -> GUID {
        match self {
            Known::Desktop => FOLDERID_Desktop,
            Known::Documents => FOLDERID_Documents,
            Known::Pictures => FOLDERID_Pictures,
            Known::Music => FOLDERID_Music,
            Known::Videos => FOLDERID_Videos,
        }
    }

    /// Where the folder is now (it may have been moved, e.g. into the
    /// OneDrive app's folder by its own backup).
    pub fn path(self) -> crate::Result<PathBuf> {
        // SAFETY: the GUID is a valid reference for the call; on success the
        // returned string is a valid NUL-terminated wide string that we own
        // and free with CoTaskMemFree once copied.
        unsafe {
            let p = SHGetKnownFolderPath(&self.id(), KF_FLAG_DEFAULT, None)
                .map_err(|source| crate::Error::Win32 { op: "SHGetKnownFolderPath", source })?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s.map(PathBuf::from).ok_or(crate::Error::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn known_folders_resolve_to_existing_paths() {
        let docs = Known::Documents.path().unwrap();
        assert!(docs.is_absolute());
        assert_eq!(Known::parse("pictures"), Some(Known::Pictures));
        assert_eq!(Known::parse("Downloads"), None);
    }
}
