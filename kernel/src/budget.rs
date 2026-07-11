use crate::filelock::FileLock;
use crate::logs::today_utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize)]
struct BudgetState {
    date: String,
    tokens_used: u64,
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
        let _lock = FileLock::acquire(self.path.with_extension("lock"), Duration::from_secs(5)).map_err(|e| anyhow::anyhow!(e))?;
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
