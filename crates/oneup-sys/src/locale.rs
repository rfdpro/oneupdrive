//! The user's languages and formats, as Windows has them: the display
//! languages they prefer (Settings > Time & language > Language) and their
//! regional format (numbers, dates). Read only.

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{
    DATE_SHORTDATE, GetDateFormatEx, GetLocaleInfoEx, GetTimeFormatEx, GetUserDefaultLocaleName,
    GetUserPreferredUILanguages, LOCALE_SDECIMAL, LOCALE_SGROUPING, LOCALE_STHOUSAND, MUI_LANGUAGE_NAME,
    TIME_NOSECONDS,
};
use windows::core::{HSTRING, PCWSTR, PWSTR};

/// The display languages the user prefers, most preferred first, as
/// language tags ("de-DE", "en-US"). Empty if Windows can't say.
pub fn ui_languages() -> Vec<String> {
    let (mut count, mut len) = (0u32, 0u32);
    // SAFETY: asks only for the size; the pointers are to our locals.
    if unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut len) }.is_err() || len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds the `len` characters Windows asked for.
    let ok =
        unsafe { GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, Some(PWSTR(buf.as_mut_ptr())), &mut len) };
    if ok.is_err() {
        return Vec::new();
    }
    // NUL-separated, ending in two NULs.
    buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

/// The user's regional format ("en-GB", "de-DE"), which numbers and dates
/// follow, whatever the display language.
pub fn format_locale() -> Option<String> {
    let mut buf = [0u16; 85];
    // SAFETY: a buffer of LOCALE_NAME_MAX_LENGTH characters.
    let n = unsafe { GetUserDefaultLocaleName(&mut buf) };
    let n = usize::try_from(n).ok().filter(|&n| n > 1)?;
    Some(String::from_utf16_lossy(&buf[..n - 1]))
}

/// How a locale writes numbers: its decimal and thousands separators, and
/// its digit grouping ("3;0": threes; "3;2;0": three, then twos).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberSymbols {
    pub decimal: String,
    pub group: String,
    pub grouping: String,
}

fn info(locale: &HSTRING, what: u32) -> Option<String> {
    let mut buf = [0u16; 32];
    // SAFETY: a NUL-terminated locale name and a buffer of its length.
    let n = unsafe { GetLocaleInfoEx(PCWSTR(locale.as_ptr()), what, Some(&mut buf)) };
    let n = usize::try_from(n).ok().filter(|&n| n > 0)?;
    Some(String::from_utf16_lossy(&buf[..n - 1]))
}

/// The number symbols of `locale` (a name like "de-DE"), with the user's
/// own changes when it's their format locale.
pub fn number_symbols(locale: &str) -> Option<NumberSymbols> {
    let l = HSTRING::from(locale);
    Some(NumberSymbols {
        decimal: info(&l, LOCALE_SDECIMAL)?,
        group: info(&l, LOCALE_STHOUSAND)?,
        grouping: info(&l, LOCALE_SGROUPING)?,
    })
}

/// A local date and time (to the minute) in `locale`'s short date and
/// time formats, like "20/10/2026 14:05".
pub fn date_time(locale: &str, year: u16, month: u8, day: u8, hour: u8, minute: u8) -> Option<String> {
    let l = HSTRING::from(locale);
    let t = SYSTEMTIME {
        wYear: year,
        wMonth: u16::from(month),
        wDay: u16::from(day),
        wHour: u16::from(hour),
        wMinute: u16::from(minute),
        ..Default::default()
    };
    let mut date = [0u16; 80];
    let mut time = [0u16; 80];
    // SAFETY: a NUL-terminated locale name, a valid SYSTEMTIME, no custom
    // format or calendar, and buffers of their stated length.
    let d = unsafe {
        GetDateFormatEx(PCWSTR(l.as_ptr()), DATE_SHORTDATE, Some(&t), PCWSTR::null(), Some(&mut date), PCWSTR::null())
    };
    // SAFETY: as above.
    let h = unsafe { GetTimeFormatEx(PCWSTR(l.as_ptr()), TIME_NOSECONDS, Some(&t), PCWSTR::null(), Some(&mut time)) };
    let (d, h) = (usize::try_from(d).ok().filter(|&n| n > 0)?, usize::try_from(h).ok().filter(|&n| n > 0)?);
    Some(format!("{} {}", String::from_utf16_lossy(&date[..d - 1]), String::from_utf16_lossy(&time[..h - 1])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_names_the_users_languages_and_formats() {
        assert!(!ui_languages().is_empty(), "no display language");
        let f = format_locale().expect("a format locale");
        assert!(f.contains('-') || f.len() >= 2, "{f}");
    }

    #[test]
    fn number_symbols_and_dates_follow_the_locale() {
        let us = number_symbols("en-US").expect("en-US");
        assert_eq!((us.decimal.as_str(), us.group.as_str()), (".", ","));
        assert_eq!(us.grouping, "3;0");
        let de = number_symbols("de-DE").expect("de-DE");
        assert_eq!((de.decimal.as_str(), de.group.as_str()), (",", "."));
        assert_eq!(date_time("en-US", 2026, 3, 4, 14, 5).as_deref(), Some("3/4/2026 2:05 PM"));
        assert_eq!(date_time("de-DE", 2026, 3, 4, 14, 5).as_deref(), Some("04.03.2026 14:05"));
    }
}
