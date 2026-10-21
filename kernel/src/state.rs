use crate::budget::BudgetTracker;
use crate::config::Config;
use crate::secrets::Secrets;
use rusqlite::Connection;
use std::path::PathBuf;

/// Everything a syscall needs for one agent run. All fields are touched
/// only from `abi::serve`, which handles the agent's syscalls one at a time
/// on the single thread driving that run — no locking needed.
/// Rate limiting is *not* here — `llm_bucket`/`embed_bucket`/`http_bucket`/
/// `http_domain_buckets` used to be built fresh per run, which meant the
/// configured per-minute caps were really "per run", not process-wide, once
/// runs stopped being serialized by one global lock (see `gateway.rs`'s
/// `AppState::session_locks`). Syscalls now go through
/// `crate::ratelimit::global(&state.config.ratelimit)` instead, a
/// process-wide singleton shared by every concurrent run.
pub struct AgentState {
    pub agent_home: PathBuf,
    pub config: Config,
    pub secrets: Secrets,
    pub budget: BudgetTracker,
    pub embed_budget: BudgetTracker,
    pub http_daily: BudgetTracker,
    pub search_daily: BudgetTracker,
    /// set by the `sleep_until` syscall; read by the host after the agent exits
    pub sleep_until: Option<i64>,
    db: Option<Connection>,
}

impl AgentState {
    pub fn new(agent_home: PathBuf, config: Config, secrets: Secrets) -> Self {
        let budget = BudgetTracker::load(&agent_home, config.budget.daily_token_cap);
        let embed_budget =
            BudgetTracker::load_named(&agent_home, config.budget.embed_daily_token_cap, "logs/embed-budget-state.json");
        let http_daily = BudgetTracker::load_named(&agent_home, config.network.daily_request_cap, "logs/http-request-count.json");
        let search_daily = BudgetTracker::load_named(&agent_home, config.search.daily_request_cap, "logs/search-request-count.json");
        AgentState {
            agent_home,
            config,
            secrets,
            budget,
            embed_budget,
            http_daily,
            search_daily,
            sleep_until: None,
            db: None,
        }
    }

    /// Lazily opens (once per run) `agent_home/memory/index.db` with the
    /// hardening required by PROJECT.md 3/db_exec: ATTACH/DETACH denied via
    /// authorizer, load_extension compiled out entirely (feature not
    /// enabled), and a wall-clock query timeout via progress_handler.
    pub fn db(&mut self) -> rusqlite::Result<&mut Connection> {
        if self.db.is_none() {
            // sqlite-vec's vec0 module, statically linked in (build.rs compiles
            // sqlite-vec.c with SQLITE_CORE defined) and registered via
            // sqlite3_auto_extension — this is *not* the dynamic load_extension()
            // path the authorizer below denies: it's a single C entrypoint baked
            // into this binary at compile time, invoked for every connection
            // opened from here on. No new "load arbitrary .so" surface is added.
            static VEC_EXTENSION_REGISTERED: std::sync::Once = std::sync::Once::new();
            VEC_EXTENSION_REGISTERED.call_once(|| unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ())));
            });

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
            // Without this, two writers racing `index.db` (two concurrent
            // `db_exec` write statements from different sessions, or one
            // colliding with `agent/src/memory.rs`'s `reindex_file`'s own
            // `BEGIN IMMEDIATE`) get an immediate `SQLITE_BUSY` — SQLite's
            // own locking already keeps the file itself correct either way,
            // this isn't a correctness fix, but a bare `SQLITE_BUSY` failure
            // surfacing as a user-visible `db_error` is a new failure mode
            // that plain didn't happen back when one global lock meant only
            // one run — and so only one writer — was ever active at a time.
            // 5s comfortably covers a normal single-statement write/commit
            // without masking a genuinely stuck connection for long.
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            crate::syscalls::db_exec::harden(&conn, self.config.db.query_timeout_secs)?;
            self.db = Some(conn);
        }
        Ok(self.db.as_mut().unwrap())
    }
}

#[cfg(test)]
mod vec_extension_tests {
    use rusqlite::Connection;

    /// Confirms sqlite-vec's static registration + vec0 KNN query syntax
    /// actually works end to end (compile, auto_extension registration,
    /// `distance_metric=cosine`, JSON-text vector literals, bound `LIMIT ?`
    /// param for k) rather than trusting docs alone.
    #[test]
    fn vec0_registers_and_answers_knn_query() {
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ())));
        }
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE VIRTUAL TABLE t USING vec0(embedding float[3] distance_metric=cosine)", []).unwrap();
        conn.execute("INSERT INTO t(rowid, embedding) VALUES (1, '[1.0,0.0,0.0]')", []).unwrap();
        conn.execute("INSERT INTO t(rowid, embedding) VALUES (2, '[0.0,1.0,0.0]')", []).unwrap();
        conn.execute("INSERT INTO t(rowid, embedding) VALUES (3, '[0.9,0.1,0.0]')", []).unwrap();

        let k: i64 = 2;
        let mut stmt = conn.prepare("SELECT rowid FROM t WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2").unwrap();
        let rows: Vec<i64> = stmt
            .query_map(rusqlite::params!["[1.0,0.0,0.0]", k], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows, vec![1, 3]);
    }
}
