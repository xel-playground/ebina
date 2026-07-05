/// Minimal 5-field cron matcher — "minute hour day month weekday", always
/// UTC (no timezone support, no chrono dependency — same call as
/// `logs::today_utc`'s hand-rolled date math). Each field is `*`, a single
/// number, a comma list, or `*/step`.
pub fn validate(spec: &str) -> Result<(), String> {
    let fields: Vec<&str> = spec.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!("expected 5 fields (minute hour day month weekday), got {}: `{spec}`", fields.len()));
    }
    fields.iter().try_for_each(|f| validate_field(f))
}

fn validate_field(field: &str) -> Result<(), String> {
    if field == "*" {
        return Ok(());
    }
    if let Some(step) = field.strip_prefix("*/") {
        return step.parse::<u32>().map(|_| ()).map_err(|_| format!("bad step field: `{field}`"));
    }
    for part in field.split(',') {
        part.parse::<u32>().map_err(|_| format!("bad cron field: `{field}`"))?;
    }
    Ok(())
}

/// Whether `spec` matches the UTC minute containing `ts`.
pub fn matches(spec: &str, ts: i64) -> bool {
    let fields: Vec<&str> = spec.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    let (minute, hour, day, month, weekday) = fields_at(ts);
    field_matches(fields[0], minute)
        && field_matches(fields[1], hour)
        && field_matches(fields[2], day)
        && field_matches(fields[3], month)
        && field_matches(fields[4], weekday)
}

fn field_matches(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(step) = field.strip_prefix("*/") {
        return step.parse::<u32>().is_ok_and(|s| s > 0 && value % s == 0);
    }
    field.split(',').any(|part| part.parse::<u32>() == Ok(value))
}

/// (minute, hour, day-of-month, month, weekday) at `ts`, UTC. `weekday` is
/// 0=Sunday..6=Saturday (standard cron convention) — Jan 1 1970 was a
/// Thursday (weekday 4), so `(days + 4) % 7` gives weekday from the same
/// days-since-epoch `logs::today_utc` already computes.
fn fields_at(ts: i64) -> (u32, u32, u32, u32, u32) {
    let days = ts.div_euclid(86_400);
    let secs_of_day = ts.rem_euclid(86_400);
    let minute = (secs_of_day / 60 % 60) as u32;
    let hour = (secs_of_day / 3600 % 24) as u32;
    let weekday = (days + 4).rem_euclid(7) as u32;
    let (_, month, day) = civil_from_days(days);
    (minute, hour, day, month, weekday)
}

/// same Howard Hinnant algorithm as `logs::civil_from_days`, returning the
/// numeric (year, month, day) instead of a formatted string
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_time() {
        // 2026-07-06 09:00:00 UTC
        let ts = 1783328400;
        assert!(matches("0 9 * * *", ts));
        assert!(!matches("0 10 * * *", ts));
    }

    #[test]
    fn matches_step() {
        let ts = 1783328400; // minute 0
        assert!(matches("*/15 * * * *", ts));
        let ts_off = ts + 60 * 7; // minute 7
        assert!(!matches("*/15 * * * *", ts_off));
    }

    #[test]
    fn rejects_bad_field_count() {
        assert!(validate("0 9 * *").is_err());
    }

    #[test]
    fn rejects_non_numeric_field() {
        assert!(validate("abc 9 * * *").is_err());
    }
}
