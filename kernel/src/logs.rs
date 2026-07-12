use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// today's date as YYYY-MM-DD (UTC), used as the budget window key
pub fn today_utc() -> String {
    let days = now_unix_secs() / 86_400;
    civil_from_days(days)
}

// days-since-epoch -> proleptic Gregorian date, Howard Hinnant's algorithm
// (std has no chrono; this avoids pulling in a date crate for one field).
// `pub(crate)` — `gateway.rs`'s maintenance skip-checks need to build the
// same `memory/notes/<day>/log.md` paths `agent_loop.rs` writes, for an
// arbitrary day in a range, not just today's.
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

/// Formats the whole line to a `String` *first*, then a single `write_all`
/// call — not `writeln!(f, "{line}")` directly, which can turn into several
/// separate `write()` syscalls as `Value`'s `Display` impl emits the object
/// piece by piece (braces, each field). `O_APPEND` only guarantees one
/// `write()` call is atomic against other appenders; multiple calls for one
/// logical line can interleave with a concurrent writer's own calls,
/// corrupting the file with a malformed merged line. Runs are per-session
/// now (not serialized by one global lock — see `gateway.rs`'s
/// `AppState::session_locks`), so two different sessions genuinely can
/// append to the same `.jsonl` (`usage.jsonl`, `egress.jsonl`, ...) at once.
pub fn append_jsonl(path: &Path, line: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut s = serde_json::to_string(line)?;
    s.push('\n');
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(s.as_bytes())?;
    Ok(())
}

pub fn notify(agent_home: &Path, message: &str) -> anyhow::Result<()> {
    println!("[notify] {message}");
    append_jsonl(
        &agent_home.join("logs/notifications.jsonl"),
        &serde_json::json!({"ts": now_unix_secs(), "message": message}),
    )
}

/// Same as [`notify`] but tags the line with where it came from (webui vs a
/// Discord channel/DM vs a scheduler-driven run) — used only by the
/// `notify` *syscall* (agent-triggered), not the ~10 internal
/// budget/rate-limit call sites elsewhere in the kernel, which have no
/// trigger/session to attribute to.
pub fn notify_with_source(agent_home: &Path, message: &str, source: &serde_json::Value) -> anyhow::Result<()> {
    println!("[notify] {message}");
    append_jsonl(
        &agent_home.join("logs/notifications.jsonl"),
        &serde_json::json!({"ts": now_unix_secs(), "message": message, "source": source}),
    )
}
