//! Automatic pauses (`rules.md` § User controls): on a network whose profile pauses
//! sync (a metered connection, by default; `crate::network`), with battery
//! saver on, and in quiet hours, all as set in `config.toml`.
//! Pure: the agent passes in what the PC is doing.

use anyhow::{Result, bail};

use crate::config::Agent;
use crate::l10n::{Arg, Locale};
use crate::t_key;

/// Why the agent pauses by itself: a fixed English message (the key a
/// catalogue translates it by, and what the status carries) and, for
/// quiet hours, the hours it fills in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub key: &'static str,
    pub hours: Option<String>,
}

impl Reason {
    pub fn fixed(key: &'static str) -> Reason {
        Reason { key, hours: None }
    }

    /// In `loc`'s language.
    pub fn said(&self, loc: &Locale) -> String {
        said(self.key, self.hours.as_deref(), loc)
    }

    /// In English, for the agent's own log.
    pub fn english(&self) -> String {
        self.said(&Locale::english())
    }
}

/// A status's `auto_pause` and `auto_pause_hours` in `loc`'s language.
pub fn said(key: &str, hours: Option<&str>, loc: &Locale) -> String {
    match hours {
        Some(h) => loc.message(key, &[("hours", Arg::from(h))]),
        None => loc.message(key, &[]),
    }
}

/// What the PC is doing now.
#[derive(Debug, Clone)]
pub struct Conditions {
    /// Why the network's profile pauses sync, if it does
    /// (`crate::network::pause_reason`).
    pub network: Option<Reason>,
    pub battery_saver: bool,
    /// What has the whole screen (a game or presentation, a video).
    pub full_screen: oneup_sys::conditions::FullScreen,
    /// Local time, minutes since midnight.
    pub minute_of_day: u32,
}

/// Why the agent pauses by itself now, if it does.
pub fn reason(a: &Agent, c: &Conditions) -> Option<Reason> {
    if let Some(why) = &c.network {
        return Some(why.clone());
    }
    if a.pause_on_battery_saver && c.battery_saver {
        return Some(Reason::fixed(t_key!("battery saver is on")));
    }
    use oneup_sys::conditions::FullScreen;
    if a.pause_on_full_screen && c.full_screen == FullScreen::Program {
        return Some(Reason::fixed(t_key!("a game, presentation or other full-screen program is running")));
    }
    if a.pause_on_full_screen_video && c.full_screen == FullScreen::Video {
        return Some(Reason::fixed(t_key!("a video is playing full screen")));
    }
    match &a.quiet_hours {
        Some(q) if parse_quiet_hours(q).is_ok_and(|(from, to)| within(from, to, c.minute_of_day)) => {
            Some(Reason { key: t_key!("quiet hours ({hours})"), hours: Some(q.clone()) })
        }
        _ => None,
    }
}

/// Whether the bandwidth caps apply now (`cap_hours`; always when unset).
pub fn caps_apply(a: &Agent, minute_of_day: u32) -> bool {
    match a.cap_hours.as_deref().map(parse_quiet_hours) {
        Some(Ok((from, to))) => within(from, to, minute_of_day),
        _ => true,
    }
}

/// Local time as minutes since midnight (UTC if the offset can't be told).
pub fn local_minute_of_day() -> u32 {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let now = time::OffsetDateTime::now_utc().to_offset(offset);
    u32::from(now.hour()) * 60 + u32::from(now.minute())
}

/// Minutes until quiet hours end (the agent wakes then), if in them.
pub fn quiet_until(a: &Agent, minute_of_day: u32) -> Option<u32> {
    let (from, to) = parse_quiet_hours(a.quiet_hours.as_deref()?).ok()?;
    within(from, to, minute_of_day).then(|| (to + 24 * 60 - minute_of_day) % (24 * 60))
}

/// `HH:MM-HH:MM` (local time; may cross midnight) as minutes of the day.
pub fn parse_quiet_hours(s: &str) -> Result<(u32, u32)> {
    let time = |t: &str| -> Option<u32> {
        let (h, m) = t.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let Some((a, b)) = s.split_once('-') else { bail!("quiet hours look like 22:00-07:00") };
    match (time(a), time(b)) {
        (Some(a), Some(b)) if a != b => Ok((a, b)),
        _ => bail!("quiet hours look like 22:00-07:00 (two different times)"),
    }
}

impl Default for Conditions {
    fn default() -> Self {
        Self {
            network: None,
            battery_saver: false,
            full_screen: oneup_sys::conditions::FullScreen::None,
            minute_of_day: 0,
        }
    }
}

fn within(from: u32, to: u32, m: u32) -> bool {
    if from < to { from <= m && m < to } else { m >= from || m < to }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn each_condition_pauses_only_when_its_setting_is_on() {
        use oneup_sys::conditions::FullScreen::{self, Program, Video};
        let on = Agent::default();
        let c = |metered: bool, battery_saver, full_screen: FullScreen| Conditions {
            network: metered.then(|| Reason::fixed(t_key!("on a metered connection"))),
            battery_saver,
            full_screen,
            minute_of_day: 600,
        };
        let none = FullScreen::None;
        assert_eq!(reason(&on, &c(false, false, none)), None);
        assert!(reason(&on, &c(true, false, none)).unwrap().english().contains("metered"));
        assert!(reason(&on, &c(false, true, none)).unwrap().english().contains("battery"));
        assert!(reason(&on, &c(false, false, Program)).unwrap().english().contains("game"));
        assert!(reason(&on, &c(false, false, Video)).unwrap().english().contains("video"));
        // Games but not videos.
        let games = Agent { pause_on_full_screen_video: false, ..Agent::default() };
        assert!(reason(&games, &c(false, false, Program)).is_some());
        assert_eq!(reason(&games, &c(false, false, Video)), None);
        let off = Agent {
            pause_on_metered: false,
            pause_on_battery_saver: false,
            pause_on_full_screen: false,
            pause_on_full_screen_video: false,
            ..Agent::default()
        };
        assert_eq!(reason(&off, &c(false, true, Program)), None);
        assert_eq!(reason(&off, &c(false, false, Video)), None);
    }

    #[test]
    fn quiet_hours_are_a_fixed_message_with_the_hours_put_in() {
        let quiet = Agent { quiet_hours: Some("22:00-07:00".into()), ..Agent::default() };
        let night = Conditions {
            network: None,
            battery_saver: false,
            full_screen: oneup_sys::conditions::FullScreen::None,
            minute_of_day: 23 * 60,
        };
        let r = reason(&quiet, &night).unwrap();
        assert_eq!(r, Reason { key: "quiet hours ({hours})", hours: Some("22:00-07:00".into()) });
        assert_eq!(r.english(), "quiet hours (22:00-07:00)");
        let de = r#"{"quiet hours ({hours})": "Ruhezeit ({hours})", "battery saver is on": "Energiesparmodus ist an"}"#;
        let de = Locale::with_catalogue("de", de, "de-DE", crate::l10n::Numbers::new(",", ".", "3;0")).unwrap();
        assert_eq!(r.said(&de), "Ruhezeit (22:00-07:00)");
        // As a status carries it.
        assert_eq!(said(r.key, r.hours.as_deref(), &de), "Ruhezeit (22:00-07:00)");
        assert_eq!(said("battery saver is on", None, &de), "Energiesparmodus ist an");
    }

    #[test]
    fn caps_apply_in_their_hours_or_always() {
        assert!(caps_apply(&Agent::default(), 3 * 60));
        let a = Agent { cap_hours: Some("09:00-18:00".into()), ..Agent::default() };
        assert!(caps_apply(&a, 10 * 60) && !caps_apply(&a, 20 * 60));
    }

    #[test]
    fn quiet_hours_may_cross_midnight() {
        let a = Agent { quiet_hours: Some("22:00-07:30".into()), ..Agent::default() };
        let at = |h: u32, m: u32| Conditions { minute_of_day: h * 60 + m, ..Conditions::default() };
        assert!(reason(&a, &at(23, 0)).is_some() && reason(&a, &at(3, 0)).is_some());
        assert!(reason(&a, &at(7, 30)).is_none() && reason(&a, &at(12, 0)).is_none());
        assert_eq!(quiet_until(&a, 23 * 60), Some(8 * 60 + 30));
        assert_eq!(quiet_until(&a, 12 * 60), None);
        let day = Agent { quiet_hours: Some("09:00-17:00".into()), ..Agent::default() };
        assert!(reason(&day, &at(10, 0)).is_some() && reason(&day, &at(18, 0)).is_none());
        for bad in ["22-07", "25:00-07:00", "07:00-07:00", "evening"] {
            assert!(parse_quiet_hours(bad).is_err(), "{bad}");
        }
    }
}
