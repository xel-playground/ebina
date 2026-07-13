use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// `YYYY-MM-DD`, used as both the notes-file-per-day name and the date half
/// of `human_timestamp`.
pub fn today_utc() -> String {
    civil_from_days((now_unix() / 86_400) as i64)
}

/// `YYYY-MM-DD` for an arbitrary unix timestamp, not just "now" — used to
/// find which `logs/run_log/<date>/log.md` a given timestamp's entry lives
/// in when scanning a range of days (daily_maintenance's since-last-run scan).
pub fn date_from_unix(secs: u64) -> String {
    civil_from_days((secs / 86_400) as i64)
}

/// `YYYY-MM-DD_HHMM` — a filename-safe, still-sorts-chronologically id for
/// one maintenance run, now that `daily_maintenance` fires every 6h instead
/// of once/day (see `agent_loop.rs`'s `build_system_prompt`), so a bare date
/// is no longer a unique report filename.
pub fn maintenance_run_id(secs: u64) -> String {
    let date = date_from_unix(secs);
    let time_of_day = secs % 86_400;
    let (h, m) = (time_of_day / 3600, (time_of_day % 3600) / 60);
    format!("{date}_{h:02}{m:02}")
}

/// `YYYY-MM-DD HH:MM:SS UTC` — human-readable stand-in for a raw unix
/// timestamp in things a human (or an LLM reading its own notes) reads back.
pub fn human_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let (h, m, s) = (time_of_day / 3600, (time_of_day % 3600) / 60, time_of_day % 60);
    format!("{} {h:02}:{m:02}:{s:02} UTC", civil_from_days(days))
}

// days-since-epoch -> proleptic Gregorian date, Howard Hinnant's algorithm
// (no chrono in the guest; mirrors kernel/src/logs.rs's host-side version)
pub(crate) fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
