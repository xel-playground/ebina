//! `/workspace/memory/` — short-term items (reminders, things to watch,
//! pending human-only actions) the hourly `daily_maintenance` pass works
//! through. Each item is one file whose first non-empty line may set a
//! deadline:
//!
//!   expires: 2026-10-25
//!
//! - **active** — has a valid `expires` date that hasn't passed: left alone,
//!   and doesn't force an hourly run unless it was modified.
//! - **expired** — the date has passed (an item expiring on the 25th is
//!   still active all of the 25th): forces the next hourly run, which wraps
//!   it up (or pushes the date out if it genuinely still needs watching).
//! - **one-shot** — no valid `expires` line at all: handled once by the
//!   next hourly run, then deleted.
//!
//! Deletion is the host's job, not the model's: `removal_candidates` is
//! taken right before a `daily_maintenance` run starts, and
//! `remove_resolved` runs after it *succeeds* — so an item is only ever
//! deleted after a run actually had the chance to read it, and an item
//! written during that run isn't touched.

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum ItemState {
    Active,
    Expired,
    OneShot,
}

pub fn dir(agent_home: &Path) -> PathBuf {
    agent_home.join("workspace/memory")
}

/// The `YYYY-MM-DD` from an item's first non-empty line, if it's a valid
/// `expires: YYYY-MM-DD`.
pub fn expires(text: &str) -> Option<&str> {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let date = first.strip_prefix("expires:")?.trim();
    let b = date.as_bytes();
    let well_formed = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    well_formed.then_some(date)
}

/// `today` is `YYYY-MM-DD` (`logs::today_utc`) — plain string comparison
/// orders dates correctly in that format.
pub fn state(text: &str, today: &str) -> ItemState {
    match expires(text) {
        None => ItemState::OneShot,
        Some(date) if date < today => ItemState::Expired,
        Some(_) => ItemState::Active,
    }
}

fn items(agent_home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir(agent_home)) else { return Vec::new() };
    entries.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect()
}

fn modified_after(path: &Path, since_ts: i64) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|d| d.as_secs() as i64 > since_ts)
}

/// Whether the hourly pass has anything here to do: an item changed since
/// `since_ts`, or one is expired / one-shot. An active, untouched item —
/// however long it's been sitting there — doesn't count; its deadline is
/// what brings it back.
pub fn needs_maintenance(agent_home: &Path, since_ts: i64, today: &str) -> bool {
    items(agent_home).iter().any(|path| {
        modified_after(path, since_ts)
            || std::fs::read_to_string(path).map_or(true, |text| state(&text, today) != ItemState::Active)
    })
}

/// Items due for removal once the upcoming run finishes — snapshot this
/// *before* the run starts.
pub fn removal_candidates(agent_home: &Path, today: &str) -> Vec<PathBuf> {
    items(agent_home)
        .into_iter()
        .filter(|path| std::fs::read_to_string(path).map_or(true, |text| state(&text, today) != ItemState::Active))
        .collect()
}

/// After a successful run: deletes each candidate that's still not active —
/// one the run extended (rewrote with a future `expires`) or already
/// deleted itself is left alone. Returns the file names removed.
pub fn remove_resolved(candidates: &[PathBuf], today: &str) -> Vec<String> {
    let mut removed = Vec::new();
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        if state(&text, today) != ItemState::Active && std::fs::remove_file(path).is_ok() {
            removed.push(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_expires_from_first_non_empty_line_only() {
        assert_eq!(expires("expires: 2026-10-25\n\n# x"), Some("2026-10-25"));
        assert_eq!(expires("\n  expires:2026-10-25  \nbody"), Some("2026-10-25"));
        assert_eq!(expires("# title\nexpires: 2026-10-25"), None);
        assert_eq!(expires("expires: soon"), None);
        assert_eq!(expires("expires: 2026-1-25"), None);
        assert_eq!(expires(""), None);
    }

    #[test]
    fn classifies_items_against_today() {
        assert_eq!(state("expires: 2026-10-25\n", "2026-10-24"), ItemState::Active);
        assert_eq!(state("expires: 2026-10-25\n", "2026-10-25"), ItemState::Active);
        assert_eq!(state("expires: 2026-10-25\n", "2026-10-26"), ItemState::Expired);
        assert_eq!(state("just a note", "2026-10-24"), ItemState::OneShot);
    }

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("ebina-wsmem-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(dir(&home)).unwrap();
        home
    }

    #[test]
    fn untouched_active_items_do_not_force_a_run() {
        let home = temp_home("active");
        std::fs::write(dir(&home).join("watch.md"), "expires: 2026-10-30\nwatch it").unwrap();
        let future = crate::logs::now_unix_secs() + 3600;
        assert!(!needs_maintenance(&home, future, "2026-10-24"));
        assert!(needs_maintenance(&home, future, "2026-10-31"), "expired forces a run");
        assert!(needs_maintenance(&home, 0, "2026-10-24"), "modified since checkpoint forces a run");
    }

    #[test]
    fn removes_one_shot_and_expired_but_keeps_extended_and_new_items() {
        let home = temp_home("remove");
        let d = dir(&home);
        std::fs::write(d.join("oneshot.md"), "remind me").unwrap();
        std::fs::write(d.join("expired.md"), "expires: 2026-10-20\nold").unwrap();
        std::fs::write(d.join("extended.md"), "expires: 2026-10-20\nstill going").unwrap();
        std::fs::write(d.join("active.md"), "expires: 2026-11-01\nlater").unwrap();
        let today = "2026-10-24";
        let candidates = removal_candidates(&home, today);
        assert_eq!(candidates.len(), 3);

        // during the run: the agent extends one item and writes a new one-shot
        std::fs::write(d.join("extended.md"), "expires: 2026-10-27\nstill going").unwrap();
        std::fs::write(d.join("new.md"), "written mid-run").unwrap();

        let mut removed = remove_resolved(&candidates, today);
        removed.sort();
        assert_eq!(removed, vec!["expired.md", "oneshot.md"]);
        assert!(d.join("extended.md").exists());
        assert!(d.join("active.md").exists());
        assert!(d.join("new.md").exists(), "not in the pre-run snapshot");
    }
}
