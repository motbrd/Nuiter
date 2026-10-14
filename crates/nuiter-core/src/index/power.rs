//! When the backlog waits: Energy Saver, battery, and "index only at set hours".

use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

use crate::settings::Settings;

fn status() -> Option<SYSTEM_POWER_STATUS> {
    let mut s = SYSTEM_POWER_STATUS::default();
    // SAFETY: valid out pointer.
    unsafe { GetSystemPowerStatus(&mut s) }.ok().map(|_| s)
}

pub fn is_on_battery() -> bool {
    status().is_some_and(|s| s.ACLineStatus == 0)
}

/// Windows' Energy Saver (Battery Saver) is on.
pub fn is_energy_saver_on() -> bool {
    status().is_some_and(|s| s.SystemStatusFlag == 1)
}

/// Why the backlog has to wait, or None: Energy Saver is on, the PC is on battery with "pause on battery" set, or it's
/// outside the hours set for indexing. New files the watcher reports are indexed anyway.
pub fn wait_reason(s: &Settings) -> Option<String> {
    if s.pause_on_energy_saver && is_energy_saver_on() {
        return Some("Energy Saver is on".into());
    }
    if s.pause_on_battery && is_on_battery() {
        return Some("on battery".into());
    }
    if s.index_only_at_hours && !window_contains(&s.index_from, &s.index_to, crate::win::local_minutes()) {
        return Some(format!("indexing runs {}–{}", s.index_from, s.index_to));
    }
    None
}

/// Minutes since midnight for "HH:mm" (or "H:mm", "HH:mm:ss").
pub fn parse_time(text: &str) -> Option<u32> {
    let mut parts = text.trim().split(':');
    let h: u32 = parts.next()?.trim().parse().ok()?;
    let m: u32 = parts.next()?.trim().parse().ok()?;
    if let Some(s) = parts.next() {
        let s: u32 = s.trim().parse().ok()?;
        if s >= 60 {
            return None;
        }
    }
    if parts.next().is_some() || h >= 24 || m >= 60 {
        return None;
    }
    Some(h * 60 + m)
}

/// Whether `now` (minutes since midnight) is in from–to, across midnight when to comes first; the same time twice, or
/// an unreadable one, means all day.
pub fn window_contains(from: &str, to: &str, now: u32) -> bool {
    let (Some(f), Some(t)) = (parse_time(from), parse_time(to)) else { return true };
    if f == t {
        return true;
    }
    if f < t { now >= f && now < t } else { now >= f || now < t }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_hours_wrap_around_midnight() {
        for (from, to, now, inside) in [
            ("01:00", "07:00", "03:30", true),
            ("01:00", "07:00", "07:00", false),
            ("01:00", "07:00", "12:00", false),
            ("22:00", "06:00", "23:15", true),
            ("22:00", "06:00", "05:59", true),
            ("22:00", "06:00", "14:00", false),
            ("08:00", "08:00", "14:00", true),
            ("soon", "07:00", "14:00", true),
        ] {
            assert_eq!(window_contains(from, to, parse_time(now).unwrap()), inside, "{from}-{to} at {now}");
        }
    }
}
