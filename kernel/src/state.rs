use crate::budget::BudgetTracker;
use crate::config::Config;
use crate::secrets::Secrets;
use rusqlite::Connection;
use std::path::PathBuf;
use wasmtime::StoreLimits;
use wasmtime_wasi::p1::WasiP1Ctx;

/// Everything a syscall needs, held as the wasmtime `Store<T>` data. All
/// fields are touched only from within host-import closures, which run
/// synchronously on the single thread driving `_start` — no locking needed.
/// Rate limiting is *not* here — `llm_bucket`/`embed_bucket`/`http_bucket`/
/// `http_domain_buckets` used to be built fresh per run, which meant the
/// configured per-minute caps were really "per run", not process-wide, once
/// runs stopped being serialized by one global lock (see `gateway.rs`'s
/// `AppState::session_locks`). Syscalls now go through
/// `crate::ratelimit::global(&state.config.ratelimit)` instead, a
/// process-wide singleton shared by every concurrent run.
pub struct AgentState {
    pub wasi: WasiP1Ctx,
    pub agent_home: PathBuf,
    pub config: Config,
    pub secrets: Secrets,
    pub budget: BudgetTracker,
    pub embed_budget: BudgetTracker,
    pub http_daily: BudgetTracker,
    pub search_daily: BudgetTracker,
    /// set by the `sleep_until` syscall; read by the host after `_start` returns
    pub sleep_until: Option<i64>,
    /// wired to `Store::limiter` — enforces the 512MB memory cap (PROJECT.md 4.1)
    pub limits: StoreLimits,
    db: Option<Connection>,
}

impl AgentState {
    pub fn new(agent_home: PathBuf, config: Config, secrets: Secrets, wasi: WasiP1Ctx, limits: StoreLimits) -> Self {
        let budget = BudgetTracker::load(&agent_home, config.budget.daily_token_cap);
        let embed_budget =
            BudgetTracker::load_named(&agent_home, config.budget.embed_daily_token_cap, "logs/embed-budget-state.json");
        let http_daily = BudgetTracker::load_named(&agent_home, config.network.daily_request_cap, "logs/http-request-count.json");
        let search_daily = BudgetTracker::load_named(&agent_home, config.search.daily_request_cap, "logs/search-request-count.json");
        AgentState {
            wasi,
            agent_home,
            config,
            secrets,
            budget,
            embed_budget,
            http_daily,
            search_daily,
            sleep_until: None,
            limits,
            db: None,
        }
    }

    /// Lazily opens (once per run) `agent_home/memory/index.db` with the
    /// hardening required by PROJECT.md 3/db_exec: ATTACH/DETACH denied via
    /// authorizer, load_extension compiled out entirely (feature not
    /// enabled), and a wall-clock query timeout via progress_handler.
    pub fn db(&mut self) -> rusqlite::Result<&mut Connection> {
        if self.db.is_none() {
            let path = self.agent_home.join("memory/index.db");
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let conn = Connection::open(path)?;
            // WAL: readers (e.g. DB Browser for SQLite opened externally for
            // debugging) don't block a concurrent writer here and vice versa
            // — default rollback journal takes an exclusive lock on write,
            // which "database is locked" against any external tool that has
            // the file open read-write at the same time.
            conn.pragma_update(None, "journal_mode", "WAL")?;
            crate::syscalls::db_exec::harden(&conn, self.config.db.query_timeout_secs)?;
            self.db = Some(conn);
        }
        Ok(self.db.as_mut().unwrap())
    }
}
