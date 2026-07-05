use crate::logs::today_utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
    /// used for `http_fetch`'s `daily_request_cap` (counting requests, not
    /// tokens) so it doesn't share a counter with the LLM token budget.
    pub fn load_named(agent_home: &Path, cap: u64, filename: &str) -> Self {
        let path = agent_home.join(filename);
        let today = today_utc();
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<BudgetState>(&s).ok())
            .filter(|s| s.date == today)
            .unwrap_or(BudgetState {
                date: today,
                tokens_used: 0,
            });
        BudgetTracker { path, cap, state }
    }

    /// roll over to a fresh window if the date changed since `load`
    fn roll_if_new_day(&mut self) {
        let today = today_utc();
        if self.state.date != today {
            self.state = BudgetState {
                date: today,
                tokens_used: 0,
            };
        }
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

    pub fn record(&mut self, tokens: u64) -> anyhow::Result<()> {
        self.roll_if_new_day();
        self.state.tokens_used += tokens;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, serde_json::to_string(&self.state)?)?;
        Ok(())
    }
}
