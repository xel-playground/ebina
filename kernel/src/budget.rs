use crate::logs::today_utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Serialize, Deserialize)]
struct BudgetState {
    date: String,
    tokens_used: u64,
}

/// Plain `create_new`-based advisory lock (atomic file creation, no `libc`/
/// `flock` dependency needed) — `record()` used to be a bare read-modify-
/// write with no locking at all, safe only because a single global
/// `run_lock` guaranteed just one run ever touched a given budget file at
/// once. Runs are per-session now (see `gateway.rs`'s `AppState::
/// session_locks`), so two concurrent runs (different sessions, or a
/// background trigger alongside a chat reply) can genuinely call `record`
/// on the *same* budget file at the same time — without this, the second
/// writer's `std::fs::write` clobbers the first's increment outright
/// (a lost update, not just a stale read), silently undercounting real
/// usage and letting the daily cap be bypassed under concurrent load.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Spin-waits for the lock file to not exist, then atomically creates
    /// it. A lock file older than 5s is treated as abandoned (a crashed
    /// holder) and force-removed rather than deadlocking every future
    /// budget check forever — the critical section here is always just one
    /// file read + write, never a network call, so 5s is generous, not tight.
    fn acquire(path: PathBuf) -> Self {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(_) => return FileLock { path },
                Err(_) if Instant::now() >= deadline => {
                    let _ = std::fs::remove_file(&path); // stale lock from a crashed holder
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct BudgetTracker {
    path: PathBuf,
    cap: u64,
    state: BudgetState,
}

impl BudgetTracker {
    pub fn load(agent_home: &Path, cap: u64) -> Self {
        Self::load_named(agent_home, cap, "logs/budget-state.json")
    }

    /// Same daily-cap-with-rollover tracker, under a different state file —
    /// used for `http_get`'s `daily_request_cap` (counting requests, not
    /// tokens) so it doesn't share a counter with the LLM token budget.
    pub fn load_named(agent_home: &Path, cap: u64, filename: &str) -> Self {
        let path = agent_home.join(filename);
        let state = read_state(&path);
        BudgetTracker { path, cap, state }
    }

    /// Re-reads the on-disk state — this run's own in-memory copy (loaded
    /// once at `load`/`load_named`) can be stale if a *different*
    /// concurrent run has since called `record` on the same file (see
    /// `FileLock`'s doc comment). Doesn't close the check-then-act race
    /// entirely (two concurrent calls can each pass `has_headroom` before
    /// either records its usage — that would need reserving budget
    /// up-front, a bigger behavior change), but keeps the check itself
    /// current as of the moment it runs, and — critically — `record` below
    /// always re-reads immediately before its own locked write, so no
    /// increment is ever silently lost even under real concurrency.
    fn roll_if_new_day(&mut self) {
        self.state = read_state(&self.path);
    }

    /// true if there's headroom left in today's budget
    pub fn has_headroom(&mut self) -> bool {
        self.roll_if_new_day();
        self.state.tokens_used < self.cap
    }

    pub fn remaining(&mut self) -> u64 {
        self.roll_if_new_day();
        self.cap.saturating_sub(self.state.tokens_used)
    }

    /// Locked read-modify-write: re-reads the file *under the lock*
    /// (not this tracker's possibly-stale `self.state`) so a concurrent
    /// run's own `record` call in between can never get silently
    /// overwritten — see `FileLock`'s doc comment.
    pub fn record(&mut self, tokens: u64) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _lock = FileLock::acquire(self.path.with_extension("lock"));
        let mut state = read_state(&self.path);
        state.tokens_used += tokens;
        std::fs::write(&self.path, serde_json::to_string(&state)?)?;
        self.state = state;
        Ok(())
    }
}

fn read_state(path: &Path) -> BudgetState {
    let today = today_utc();
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<BudgetState>(&s).ok())
        .filter(|s| s.date == today)
        .unwrap_or(BudgetState { date: today, tokens_used: 0 })
}
