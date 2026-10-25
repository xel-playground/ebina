use crate::config::Config;
use crate::filelock::FileLock;
use crate::secrets::Secrets;
use axum::extract::{Path as AxumPath, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Kernel serves the API only — it has no idea `webui/` exists, doesn't
/// read its files, doesn't serve them. Fully decoupled at the user's
/// request; a separate wrapper (not built yet) is what runs both together.
pub struct GatewayConfig {
    pub agent_home: PathBuf,
    /// the native agent binary spawned for every run
    pub agent_path: PathBuf,
    /// single bearer token gating every `/api/*` route (PROJECT.md 4.4)
    pub token: String,
    pub port: u16,
}

/// A table of on-demand per-key locks (session keys, scheduled-task ids)
/// that would otherwise grow one entry per distinct key forever — a
/// long-running gateway that's seen thousands of Discord channels or
/// scheduled tasks over its life would hold that many stale `Arc<Mutex<()>>`
/// entries hostage in memory even though almost all of them are idle.
/// `sweep` drops any entry that's both unheld right now (`strong_count ==
/// 1`, i.e. only this table's own clone exists) and hasn't been touched
/// recently — called once per `scheduler_loop` tick (every 30s), cheap
/// since it's just a `HashMap::retain` over two small maps.
struct KeyedLocks {
    map: tokio::sync::Mutex<std::collections::HashMap<String, (Arc<tokio::sync::Mutex<()>>, std::time::Instant)>>,
}

impl KeyedLocks {
    fn new() -> Self {
        KeyedLocks { map: tokio::sync::Mutex::new(std::collections::HashMap::new()) }
    }

    async fn get(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.map.lock().await;
        let now = std::time::Instant::now();
        let entry = map.entry(key.to_string()).or_insert_with(|| (Arc::new(tokio::sync::Mutex::new(())), now));
        entry.1 = now;
        entry.0.clone()
    }

    async fn sweep(&self, idle_after: Duration) {
        let mut map = self.map.lock().await;
        let now = std::time::Instant::now();
        map.retain(|_, (lock, last_used)| Arc::strong_count(lock) > 1 || now.duration_since(*last_used) < idle_after);
    }
}

pub(crate) struct AppState {
    pub(crate) agent_home: PathBuf,
    agent_path: PathBuf,
    token: String,
    /// Only `message`-type runs (a real conversation) get serialized, and
    /// only against *their own session* — same session's turns must land in
    /// strict order (`session.json` is a read-modify-write per turn; two
    /// concurrent turns on the same session would race that) and a human
    /// doesn't want two replies interleaving one conversation. Every other
    /// trigger type (`daily_maintenance`/`scheduled_task`/`manual`)
    /// has no session to protect this way and runs fully concurrently with
    /// everything else — safe now that `write_file`/`append_file`'s target
    /// directories no longer overlap between trigger types (see
    /// `agent_loop.rs`'s `write_action_denial`): a `message` run can only
    /// touch `/workspace/`, curated `memory/notes/` is `daily_maintenance`-
    /// only, and only one `daily_maintenance` (same for `core_distillation`)
    /// is ever in flight at a time (`schedule_log` runs at most one occurrence per job,
    /// not just gating — 2026-07-12: `scheduler_loop` used to `.await` each
    /// fired trigger inline, one giant sequential loop body, which
    /// accidentally *serialized* everything this comment already claimed was
    /// safe to run concurrently; a multi-minute `daily_maintenance` LLM call
    /// blocked the same tick's `scheduled_task` due-check from ever running,
    /// silently missing an exact-minute cron task's whole day with no error.
    /// `tokio::spawn` now actually exploits the concurrency this comment
    /// describes instead of only asserting it's safe). The residual risk
    /// this doesn't close — two *background* runs (e.g. two different
    /// `scheduled_task`s, or a `scheduled_task` racing `daily_maintenance`)
    /// hitting `.git/index.lock` on autocommit at once, or both touching
    /// some workspace file the human configured them to share — is accepted
    /// rather than solved with finer-grained locking; both fail soft (a
    /// skipped commit that a later run's autocommit picks up, a last-write-
    /// wins on the shared file) rather than corrupting anything.
    session_locks: KeyedLocks,
    /// Same shape as `session_locks`, but keyed by `scheduled_task` id — two
    /// runs of *the same task* (a cron tick and a manual `/api/wake
    /// type=scheduled_task task_id=X` landing at once, say) used to be able
    /// to genuinely execute concurrently with zero coordination, each
    /// reading/writing that task's `data_path` independently. `mark_run`
    /// already had its own file lock, but that only protected the
    /// checkpoint write, not the run itself.
    task_run_locks: KeyedLocks,
    /// how many `run_trigger` calls are currently in flight, of any trigger
    /// type — `GET /api/status`'s `busy` field used to be "is the one
    /// global lock held", which stopped meaning anything once locking went
    /// per-session; this is trigger-type-agnostic so the webui still gets a
    /// meaningful "something's running right now" signal.
    active_runs: std::sync::atomic::AtomicUsize,
    /// every scheduler-driven occurrence's event-sourced state — see
    /// `crate::schedule_log`
    schedule_log: Arc<crate::schedule_log::ScheduleLog>,
}

impl AppState {
    /// Returns (creating if needed) the dedicated lock for one session key.
    async fn session_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.session_locks.get(key).await
    }

    /// Returns (creating if needed) the dedicated lock for one scheduled
    /// task id.
    async fn task_run_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.task_run_locks.get(id).await
    }

    /// Drops idle entries from both keyed-lock tables — see `KeyedLocks`'s
    /// doc comment. An hour of idleness is arbitrary but generous: even a
    /// gateway with hundreds of Discord channels/scheduled tasks re-touches
    /// each active one far more often than that, so this only ever prunes
    /// keys that are genuinely gone (a deleted task, a channel nobody's
    /// posted in for a while).
    async fn sweep_idle_locks(&self) {
        self.session_locks.sweep(Duration::from_secs(3600)).await;
        self.task_run_locks.sweep(Duration::from_secs(3600)).await;
    }
}

/// Kernel space: this is the only piece of the system that knows agent-home
/// exists as a directory on disk and that an agent binary needs running — the
/// agent itself never sees the gateway (PROJECT.md 4.4).
pub async fn serve(cfg: GatewayConfig) -> anyhow::Result<()> {
    // once-at-startup, not lazily inside `pairing_seed()` — an
    // already-paired instance never calls that again (the pairing-code path
    // only runs while `load_owner` is still `None`), so a lazy migration
    // would silently never fire for exactly the installs that most need it
    crate::discord::migrate_legacy_seed_path(&cfg.agent_home);

    // checked once, here, rather than on first run — a bad agent_path
    // should fail gateway startup loudly, not surface as a mysterious first
    // request failure
    if !cfg.agent_path.is_file() {
        anyhow::bail!("agent binary {} doesn't exist", cfg.agent_path.display());
    }

    let schedule_log = Arc::new(crate::schedule_log::ScheduleLog::load(crate::schedule_log::ScheduleLog::path_for(&cfg.agent_home)));
    let state = Arc::new(AppState {
        agent_home: cfg.agent_home,
        agent_path: cfg.agent_path,
        token: cfg.token,
        session_locks: KeyedLocks::new(),
        task_run_locks: KeyedLocks::new(),
        active_runs: std::sync::atomic::AtomicUsize::new(0),
        schedule_log,
    });

    let api = Router::new()
        .route("/message", post(post_message))
        .route("/upload", post(post_upload))
        .route("/attachment", get(get_attachment))
        .route("/wake", post(post_wake))
        .route("/status", get(get_status))
        .route("/memory/notes", get(get_notes))
        .route("/reports", get(get_reports))
        .route("/config", get(get_config).post(post_config))
        .route("/soul", get(get_soul).post(post_soul))
        .route("/core", get(get_core))
        .route("/core/history", get(get_core_history))
        .route("/core/history/{hash}", get(get_core_at_commit))
        .route("/secrets", post(post_secret))
        .route("/logs", get(get_logs_sse))
        .route("/logs/tree", get(get_logs_tree))
        .route("/logs/file", get(get_logs_file))
        .route("/workspace/tree", get(get_workspace_tree))
        .route("/workspace/file", get(get_workspace_file))
        .route("/thinking", get(get_thinking_sse))
        .route("/thinking/snapshot", get(get_thinking_snapshot))
        .route("/abort", post(post_abort))
        .route("/session", get(get_session))
        .route("/session/reset", post(post_session_reset))
        .route("/session/compact", post(post_session_compact))
        .route("/grants", get(get_grants))
        .route("/grants/{id}/approve", post(post_grant_approve))
        .route("/grants/{id}/deny", post(post_grant_deny))
        .route("/egress", get(get_egress))
        .route("/discord/pairing", get(get_discord_pairing))
        .route("/llm/logs", get(get_llm_logs))
        .route("/scheduler/occurrences", get(get_scheduler_occurrences))
        .route("/scheduler/occurrences/{id}", get(get_scheduler_occurrence))
        .route("/scheduler/occurrences/{id}/retry", post(post_scheduler_occurrence_retry))
        .route("/todos", get(get_todos))
        .route("/todos/{name}", axum::routing::put(put_todo).delete(delete_todo))
        .route("/scheduler/tasks", get(get_scheduler_tasks).post(post_scheduler_task))
        .route("/scheduler/tasks/{id}", axum::routing::put(put_scheduler_task).delete(delete_scheduler_task))
        .route("/scheduler/task_file", get(get_task_file).put(put_task_file))
        .route("/skills", get(get_skills).post(post_skill))
        .route("/skills/{name}", axum::routing::delete(delete_skill))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone());

    tokio::spawn(scheduler_loop(state.clone()));
    tokio::spawn(crate::discord::run(state.clone()));

    let app = Router::new().nest("/api", api);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    println!("[gateway] listening on http://0.0.0.0:{}", cfg.port);
    axum::serve(listener, app).await?;
    Ok(())
}

/// PROJECT.md 4.5: "tokio loop,管 next-wake + daily_maintenance cron" — until
/// now nothing drove this; the agent only ever ran in response to a human
/// hitting `/api/message` or `/api/wake`. Ticks every 30s and fires several
/// kinds of self-driven wake, none carrying chat `history` (so each is
/// structurally a fresh session — `agent_loop.rs` only continues a
/// conversation when `trigger.history` is present):
///
/// - `daily_maintenance`, every `MAINTENANCE_INTERVAL_SECS` (1h — despite
///   the name, a deliberately *light*, frequent pass now, not a deep
///   review) since the last one, tracked by `last_maintenance_marker_path`
///   rather than "does today's report exist" — a bare day-existence check
///   would only ever fire once per calendar day. `since_ts` (the checkpoint
///   being advanced) rides along in the trigger so the agent only reviews
///   what's new since then (`agent_loop.rs`'s `recent_log_entries`), not
///   everything. 2026-07-12: this used to be the *only* distillation pass,
///   at 6h — a fact could sit unfolded into memory/notes/ for up to 6h, and
///   worse, a fact that got noted as "needs attention" one cycle but wasn't
///   acted on just got silently re-noted every cycle after with nothing
///   ever escalating (a real incident: a "please update the bot's avatar"
///   reminder got rediscovered and reported on for over a day, never
///   distilled, never escalated). Shrinking the light pass to 1h reduces
///   how long anything sits unprocessed. Items in `/workspace/memory/` carry
///   their own deadline now (`crate::workspace_memory`) — the run after it
///   passes reports the outcome and the host deletes the item, which is
///   what replaced the old "re-noted forever, never escalated" failure.
///   Gated by
///   `MAINTENANCE_WALL_CLOCK_SPEC` to only ever actually fire at minute 13
///   of the hour, not on the hour — see that const's doc comment.
/// - `core_distillation`, every `CORE_DISTILLATION_INTERVAL_SECS` (24h, its
///   own checkpoint) — the deep pass above the hourly one. Reads
///   `maintenance_reports/hourly/` since the last core run (the hourly
///   tier's *product*, not raw logs — bounded at ~24 reports), and from
///   that: merges/dedupes fragmented notes and distills oversized ones,
///   runs the skill quality gate
///   (adr/002-skill-v2.md §3), and decides what's durable enough to
///   promote/demote in `/core.md` (capped, `agent/src/agent_loop.rs`'s
///   `CORE_MD_MAX_CHARS`). This used to be split across a separate 6h
///   `maintenance_summary` tier; folded in here to keep just two layers —
///   hourly writes notes, daily writes core.
///   `/core.md` itself can only ever be written during this trigger type
///   (`agent_loop.rs`'s `write_action_denial`) — no exception path, by
///   design (staleness gets fixed by the next distillation cycle rereading
///   fresh evidence, not by a live-chat shortcut).
/// - one `scheduled_task` wake per enabled, cron-matching entry under
///   `crate::scheduler_tasks` — user/agent-defined jobs (`data_path` points
///   the woken session at its own instructions), independent of the two
///   built-in wakes above.
/// - host-only housekeeping every `HOUSEKEEPING_INTERVAL_SECS` (6h, no LLM):
///   `sweep_idle_sessions` and `verify_recent_distillation`.
///
/// Every one of these goes through `crate::schedule_log` — the loop only
/// ever *schedules* an occurrence (`<job>@<slot>`, idempotent: noticing the
/// same due slot on several ticks or across a restart schedules it once),
/// then claims whatever is ready and spawns it. Pending/running/succeeded/
/// failed state, retries (`schedule_log::RETRY_BACKOFF_SECS`, up to
/// `schedule_log::MAX_RETRIES`) and restart recovery all live in that
/// append-only event log, not in this loop's memory — see the 2026-07-12
/// Moonshot content-filter incident (a once-daily task that silently missed
/// its fire) and the 2026-07-15 one (one retry spawned four times) for why
/// each of those used to need its own ad-hoc fix here.
const MAINTENANCE_INTERVAL_SECS: i64 = 3600;
/// Idle-session sweep + distillation sanity check cadence — and the
/// idle threshold `sweep_idle_sessions` uses.
const HOUSEKEEPING_INTERVAL_SECS: i64 = 6 * 3600;
/// Fires at :18 on every 6th hour; deduped per slot by `schedule_log`.
const HOUSEKEEPING_SPEC: &str = "18 */6 * * *";
/// Both maintenance tiers only actually fire at minute 13 of the hour, never
/// on the hour itself — deliberately offset from the common `:00` mark other
/// cron-style jobs tend to cluster on (e.g. `morning_report`'s `0 2 * * *`),
/// so a maintenance run's LLM call doesn't contend with them for the same
/// wall-clock instant. Checked with the same hand-rolled matcher
/// `scheduled_task`'s cron strings use (`cron::matches`) — the interval
/// checks above already gate *whether* a run is due; this just constrains
/// *when within the hour* a due run is allowed to actually execute.
const MAINTENANCE_WALL_CLOCK_SPEC: &str = "13 * * * *";
/// Daily deep tier — see `scheduler_loop`'s doc comment for what it does.
/// 24h interval, own checkpoint (`last_core_marker_path`) — same "fire hourly
/// at a fixed minute, checkpoint gates whether it's actually due" pattern
/// as the other two tiers, not a once-a-day-only cron match, so a missed
/// tick still self-heals on the next one rather than waiting a full day.
const CORE_DISTILLATION_INTERVAL_SECS: i64 = 24 * 3600;
/// 10 minutes after `MAINTENANCE_WALL_CLOCK_SPEC` — `core_distillation`
/// reads `daily_maintenance`'s reports, so this gives that hour's hourly
/// pass a normal run's length of head start before core distillation
/// starts reading `maintenance_reports/hourly/`. Lock-free insurance for
/// the common case, not a guarantee; the mtime checkpoint picks up a late
/// report next cycle anyway.
const CORE_DISTILLATION_WALL_CLOCK_SPEC: &str = "23 * * * *";
/// `run_trigger`'s outcome shape: `{"ok":true,"result":{"summary":..},...}`
/// on a completed run, `{"ok":false,"error":..}` only on a panic/spawn
/// failure — a plain `llm_call` failure chain (`agent_loop.rs` aborts the
/// whole run rather than continuing with a broken turn) still comes back
/// `ok:true` with `result.summary` starting `"run aborted"`, so both need
/// checking to catch every way a self-driven run can fail to actually
/// finish its job. Decides whether `schedule_log` records the attempt as
/// succeeded or failed (and so whether a maintenance tier's checkpoint
/// advances).
fn outcome_aborted(outcome: &Value) -> bool {
    !outcome.get("ok").and_then(Value::as_bool).unwrap_or(false)
        || outcome.get("result").and_then(|r| r.get("summary")).and_then(Value::as_str).is_some_and(|s| s.starts_with("run aborted"))
}

fn scheduler_tick_marker_path(agent_home: &Path) -> PathBuf {
    agent_home.join("scheduler/.last_tick")
}

/// 0 is the sentinel for "this scheduler has never ticked before" — the
/// caller treats that as "no backfill, just establish a baseline", not
/// "everything since the Unix epoch is a gap" (unlike `read_last_maintenance`,
/// where 0 deliberately *does* mean "review everything so far").
fn read_scheduler_last_tick(agent_home: &Path) -> i64 {
    std::fs::read_to_string(scheduler_tick_marker_path(agent_home)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn write_scheduler_last_tick(agent_home: &Path, now: i64) {
    let path = scheduler_tick_marker_path(agent_home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, now.to_string());
}

/// Whether `cron_spec` matched at least one whole minute strictly between
/// `gap_start` and `now`'s own current minute (which `due_now` already
/// covers, at the call site — not this function's job). In the ordinary
/// case `gap_start` is ~30s ago (last tick) and this loop finds nothing;
/// it only matters after a real gap — kernel downtime spanning a restart,
/// or (before 2026-07-12's `tokio::spawn` fix) the tick loop itself stuck
/// inside a long `.await` — during which `scheduled_task`'s own `due_now`
/// check, tied to *this* tick's current minute alone, never got a chance to
/// see an exact-minute cron match that fell inside the gap and is gone
/// forever once the minute passes. Returns the *latest* matching minute
/// (as that occurrence's slot) so the task fires once to catch up rather
/// than once per missed minute in the gap — a long outage spanning several
/// of a daily task's matching minutes should bring it current, not replay
/// a backlog of stale runs.
fn missed_in_gap(cron_spec: &str, gap_start: i64, now: i64) -> Option<i64> {
    let mut t = gap_start + 60;
    let cutoff = now - 60; // this tick's own current minute is due_now's job, not this
    let mut latest = None;
    while t < cutoff {
        if crate::cron::matches(cron_spec, t) {
            latest = Some(t - t.rem_euclid(60));
        }
        t += 60;
    }
    latest
}

fn last_maintenance_marker_path(agent_home: &Path) -> PathBuf {
    agent_home.join("maintenance_reports/.last_run")
}

fn read_last_maintenance(agent_home: &Path) -> i64 {
    std::fs::read_to_string(last_maintenance_marker_path(agent_home)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn write_last_maintenance(agent_home: &Path, now: i64) {
    let path = last_maintenance_marker_path(agent_home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, now.to_string());
}

fn last_core_marker_path(agent_home: &Path) -> PathBuf {
    agent_home.join("maintenance_reports/.last_core_run")
}

fn read_last_core_run(agent_home: &Path) -> i64 {
    std::fs::read_to_string(last_core_marker_path(agent_home)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn write_last_core_run(agent_home: &Path, now: i64) {
    let path = last_core_marker_path(agent_home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, now.to_string());
}

fn maintenance_run_counts_path(agent_home: &Path) -> PathBuf {
    agent_home.join("logs/maintenance_run_counts.json")
}

#[derive(Default, Serialize, Deserialize)]
struct TierRunCounts {
    #[serde(default)]
    ran: u64,
    #[serde(default)]
    skipped: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct MaintenanceRunCounts {
    #[serde(default)]
    daily_maintenance: TierRunCounts,
    #[serde(default)]
    core_distillation: TierRunCounts,
}

/// Visibility into whether the short-circuits above are actually engaging —
/// `scheduler_loop` is the only writer (one tick at a time, sequential, no
/// concurrent callers), so unlike `BudgetTracker` this needs no file lock.
/// `ran` counts a tier actually spawning an LLM pass; `skipped` counts the
/// host-only "nothing new" path. A `daily_maintenance.skipped` that never
/// climbs despite quiet hours is the signal `has_new_log_entries` isn't
/// excluding something it should.
fn record_maintenance_tier(agent_home: &Path, pick: impl Fn(&mut MaintenanceRunCounts) -> &mut TierRunCounts, ran: bool) {
    let path = maintenance_run_counts_path(agent_home);
    let mut counts: MaintenanceRunCounts =
        std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let tier = pick(&mut counts);
    if ran {
        tier.ran += 1;
    } else {
        tier.skipped += 1;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serde_json::to_string(&counts).unwrap_or_default());
}

/// Pure host-side check, no LLM involved — `memory/notes/` should show at
/// least one real git commit within `window_secs` if the hourly
/// `daily_maintenance` passes during that window are actually distilling
/// anything, not just self-reporting that they did. Gated on
/// `has_new_log_entries` first: a genuinely quiet window (nothing logged at
/// all — `daily_maintenance` itself would have skipped it, see
/// `scheduler_loop`) produces no commits for a completely unremarkable
/// reason, not a bug; only worth a `notify()` if there *was* something to
/// react to and nothing landed on disk anyway.
fn verify_recent_distillation(agent_home: &Path, window_secs: i64) {
    let git_dir = crate::autocommit::git_dir_path(agent_home);
    if !git_dir.exists() {
        return; // no git history yet at all — nothing to check
    }
    let since_ts = crate::logs::now_unix_secs() - window_secs;
    if !has_new_log_entries(agent_home, since_ts) {
        return;
    }
    let since = format!("{window_secs} seconds ago");
    let output = std::process::Command::new("git")
        .args(["--git-dir", &git_dir.to_string_lossy(), "--work-tree", &agent_home.to_string_lossy(), "log", "--since", &since, "--oneline", "--", "memory/notes"])
        .output();
    let has_commits = output.map(|o| o.status.success() && !o.stdout.is_empty()).unwrap_or(false);
    if !has_commits {
        let _ = crate::logs::notify(
            agent_home,
            &format!(
                "distillation check: memory/notes/ has had no git commits in the last {}h despite new activity logged in that window — daily_maintenance may not actually be writing anything",
                window_secs / 3600,
            ),
        );
    }
}

/// Host-side mirror of `agent_loop.rs`'s `recent_log_entries` — same day-log
/// files, same `(ts=N)` block markers — but boolean-only. Lets
/// `scheduler_loop` decide *whether* a `daily_maintenance` run has anything
/// to review at all before paying for the LLM call that would otherwise
/// just come back saying "nothing new since the last maintenance run".
/// A tier's own run logs itself unconditionally (`agent_loop.rs`'s
/// `write_memory_note` is called at the end of *every* `run()`, `trigger`
/// type included) — without this exclusion, `daily_maintenance`'s own
/// completion entry would itself count as "new log activity" on the very
/// next check, making the short-circuit below never actually engage: a
/// tier that ran once would keep re-triggering itself forever purely from
/// its own bookkeeping, independent of whether anything real ever happened.
const MAINTENANCE_TRIGGER_TYPES: &[&str] = &["daily_maintenance", "core_distillation"];

fn has_new_log_entries(agent_home: &Path, since_ts: i64) -> bool {
    let today_day = crate::logs::now_unix_secs() / 86_400;
    let since_day = if since_ts <= 0 { today_day } else { since_ts / 86_400 };
    for day in since_day..=today_day {
        let path = agent_home.join("logs/run_log").join(crate::logs::civil_from_days(day)).join("log.md");
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for block in text.split("\n## run at ") {
            let Some(start) = block.find("(ts=") else { continue };
            let rest = &block[start + 4..];
            let Some(end) = rest.find(')') else { continue };
            let Ok(ts) = rest[..end].parse::<i64>() else { continue };
            if ts <= since_ts {
                continue;
            }
            let trigger_type = block.split("trigger: ").nth(1).and_then(|s| s.split_whitespace().next()).unwrap_or("");
            if MAINTENANCE_TRIGGER_TYPES.contains(&trigger_type) {
                continue;
            }
            return true;
        }
    }
    false
}

/// Whether `maintenance_reports/hourly/` has any `daily_maintenance`
/// report written since `since_ts` — that's `core_distillation`'s primary
/// input; with none, there's nothing new to consolidate or promote.
fn has_new_maintenance_reports(agent_home: &Path, since_ts: i64) -> bool {
    let Ok(entries) = std::fs::read_dir(agent_home.join("maintenance_reports/hourly")) else { return false };
    entries.flatten().any(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| d.as_secs() as i64 > since_ts)
    })
}

/// A session (webui or any Discord DM/channel) nobody's touched in over
/// `idle_after` gets reset the same way `!reset`/the webui button would —
/// unlike `maybe_auto_compact` (which only ever runs on the *next* turn a
/// session gets, so a truly abandoned one never triggers it), this is
/// piggybacked on the same 6h cadence `daily_maintenance` already ticks on
/// rather than its own timer, so a conversation nobody's returned to
/// doesn't just sit there accumulating context/tokens forever waiting for
/// a turn that may never come. Checked independently of whether that
/// run's own LLM call actually completes — this is a pure mechanical
/// file-timestamp comparison, nothing to retry/back off on.
async fn sweep_idle_sessions(state: &Arc<AppState>, idle_after: Duration) {
    let sessions_dir = state.agent_home.join("logs/chat_sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else { return };
    let now = crate::logs::now_unix_secs();
    let idle_secs = idle_after.as_secs() as i64;
    for entry in entries.flatten() {
        let Some(key) = entry.file_name().to_str().map(str::to_string) else { continue };
        let turns = load_session(&state.agent_home, &key);
        // no turns at all — already empty/never used, nothing to reset
        let Some(last_ts) = turns.last().map(|t| t.ts) else { continue };
        if now - last_ts >= idle_secs {
            let outcome = reset_session_key(state, &key).await;
            if outcome.get("ok").and_then(Value::as_bool) == Some(true) {
                let _ = crate::logs::notify(&state.agent_home, &format!("session {key} idle for over {}h — auto-reset", idle_secs / 3600));
            }
        }
    }
}

const JOB_DAILY_MAINTENANCE: &str = "daily_maintenance";
const JOB_HOUSEKEEPING: &str = "housekeeping";
const JOB_CORE_DISTILLATION: &str = "core_distillation";
const TASK_JOB_PREFIX: &str = "task:";

/// The minute `ts` falls in — every slot is minute-aligned, so the two
/// 30s ticks inside one cron minute agree on the same occurrence id.
fn minute_slot(ts: i64) -> i64 {
    ts - ts.rem_euclid(60)
}

async fn scheduler_loop(state: Arc<AppState>) {
    let log = state.schedule_log.clone();
    let recovered = log.recover_interrupted(crate::logs::now_unix_secs());
    if !recovered.is_empty() {
        let _ = crate::logs::notify(
            &state.agent_home,
            &format!("scheduler: {} run(s) were cut off by a kernel restart, retrying: {}", recovered.len(), recovered.join(", ")),
        );
    }
    // Persisted (not a local `Option`) — a plain local would reset to "no
    // gap" on every kernel restart, exactly the case (downtime spanning an
    // exact-minute cron match) this is meant to catch. 0 sentinel means
    // "never ticked before" — see `read_scheduler_last_tick`'s doc comment.
    let mut scheduler_last_tick = read_scheduler_last_tick(&state.agent_home);
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let now = crate::logs::now_unix_secs();
        state.sweep_idle_locks().await;

        let gap_start = if scheduler_last_tick == 0 { now } else { scheduler_last_tick };
        write_scheduler_last_tick(&state.agent_home, now);
        scheduler_last_tick = now;

        schedule_due_occurrences(&state, gap_start, now).await;

        // spawned, not awaited — a real LLM-driven pass has taken several
        // minutes in practice, and this loop is also what notices every
        // *other* job's exact-minute slot; blocking on one run here is how
        // `rss_tech_report` silently missed a whole day on 2026-07-12.
        for occ in log.claim_ready(now) {
            let spawn_state = state.clone();
            tokio::spawn(async move { execute_occurrence(spawn_state, occ).await });
        }
    }
}

/// Decides which slots are due this tick and records them in the schedule
/// log. Pure bookkeeping — nothing runs here; a slot that's already been
/// scheduled (earlier tick, before a restart) is a no-op inside
/// `schedule`/`skip`.
async fn schedule_due_occurrences(state: &Arc<AppState>, gap_start: i64, now: i64) {
    let log = &state.schedule_log;
    let slot = minute_slot(now);

    let last_maintenance = read_last_maintenance(&state.agent_home);
    if now - last_maintenance >= MAINTENANCE_INTERVAL_SECS && crate::cron::matches(MAINTENANCE_WALL_CLOCK_SPEC, now) {
        if !has_new_log_entries(&state.agent_home, last_maintenance)
            && !crate::workspace_memory::needs_maintenance(&state.agent_home, last_maintenance, &crate::logs::today_utc())
        {
            // nothing logged, and nothing under workspace/memory/ changed,
            // expired or is a one-shot item — an LLM pass here would just come back
            // saying so. The check itself *is* a full, deterministic review
            // of [last_maintenance, now), so it's safe to advance the
            // checkpoint same as a real completed run would.
            if log.skip(JOB_DAILY_MAINTENANCE, slot, now, "no new log entries or workspace memory items to handle") {
                write_last_maintenance(&state.agent_home, slot);
                record_maintenance_tier(&state.agent_home, |c| &mut c.daily_maintenance, false);
            }
        } else if log.schedule(JOB_DAILY_MAINTENANCE, slot, now) {
            record_maintenance_tier(&state.agent_home, |c| &mut c.daily_maintenance, true);
        }
    }

    // host-only, no LLM — recorded as an already-finished occurrence so
    // the two ticks inside the :18 minute (or a restart) run it once
    if crate::cron::matches(HOUSEKEEPING_SPEC, now) && log.complete_inline(JOB_HOUSEKEEPING, slot, now) {
        sweep_idle_sessions(state, Duration::from_secs(HOUSEKEEPING_INTERVAL_SECS as u64)).await;
        verify_recent_distillation(&state.agent_home, HOUSEKEEPING_INTERVAL_SECS);
    }

    let last_core_run = read_last_core_run(&state.agent_home);
    if now - last_core_run >= CORE_DISTILLATION_INTERVAL_SECS && crate::cron::matches(CORE_DISTILLATION_WALL_CLOCK_SPEC, now) {
        if !has_new_maintenance_reports(&state.agent_home, last_core_run) {
            // no hourly reports since last core run — every hour in this
            // window was itself skipped for lack of activity; nothing new to
            // consolidate or promote, same "the check itself is a full
            // review" reasoning as the hourly tier.
            if log.skip(JOB_CORE_DISTILLATION, slot, now, "no new hourly maintenance reports") {
                write_last_core_run(&state.agent_home, slot);
                record_maintenance_tier(&state.agent_home, |c| &mut c.core_distillation, false);
            }
        } else if log.schedule(JOB_CORE_DISTILLATION, slot, now) {
            record_maintenance_tier(&state.agent_home, |c| &mut c.core_distillation, true);
        }
    }


    for task in crate::scheduler_tasks::load_tasks(&state.agent_home) {
        if !task.enabled {
            continue;
        }
        // the current minute if it matches, else (after a real gap —
        // kernel downtime spanning the match) the latest matching minute
        // inside the gap, which would otherwise be gone forever. See
        // `missed_in_gap`.
        let due_slot = if crate::cron::matches(&task.cron, now) { Some(slot) } else { missed_in_gap(&task.cron, gap_start, now) };
        if let Some(due_slot) = due_slot {
            log.schedule(&format!("{TASK_JOB_PREFIX}{}", task.id), due_slot, now);
        }
    }
}

/// Runs one claimed occurrence (already `running` in the schedule log) and
/// records how it ended. The trigger is built here, at run time, not when
/// the slot was scheduled — a retry 15 minutes later should see the
/// current checkpoint/chat context/task file, not a stale snapshot.
async fn execute_occurrence(state: Arc<AppState>, occ: crate::schedule_log::Occurrence) {
    let log = state.schedule_log.clone();
    let trigger = match build_occurrence_trigger(&state, &occ) {
        Ok(trigger) => trigger,
        Err(e) => {
            log.fail(&occ.id, occ.attempts, crate::logs::now_unix_secs(), &e, None);
            return;
        }
    };
    // one-shot and expired `/workspace/memory/` items this run is about to
    // read — removed below only if the run succeeds (see
    // `crate::workspace_memory`)
    let today = crate::logs::today_utc();
    let removal_candidates = if occ.job == JOB_DAILY_MAINTENANCE {
        crate::workspace_memory::removal_candidates(&state.agent_home, &today)
    } else {
        Vec::new()
    };
    let (outcome, run_file) = run_scheduled(state.clone(), trigger, Some(&occ)).await;
    let now = crate::logs::now_unix_secs();
    if outcome_aborted(&outcome) {
        let error = outcome
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| outcome.get("result").and_then(|r| r.get("summary")).and_then(Value::as_str))
            .unwrap_or("run failed")
            .to_string();
        let retry_at = log.fail(&occ.id, occ.attempts, now, &error, run_file);
        if retry_at.is_none() {
            let _ = crate::logs::notify(
                &state.agent_home,
                &format!("scheduler: {} failed {} times, giving up until its next slot: {error}", occ.id, occ.attempts),
            );
        }
        return;
    }
    // only advance a maintenance checkpoint on a real completion — an
    // aborted run that advanced it would silently skip whatever happened in
    // that window forever (the 2026-07-08 Moonshot outage did exactly
    // this). The slot, not `now`: anything logged while a slow/retried run
    // was in flight still lands in the next window.
    let removed = crate::workspace_memory::remove_resolved(&removal_candidates, &today);
    if !removed.is_empty() {
        let _ = crate::logs::notify(
            &state.agent_home,
            &format!("daily_maintenance: removed handled workspace/memory items (one-shot or expired): {}", removed.join(", ")),
        );
    }
    match occ.job.as_str() {
        JOB_DAILY_MAINTENANCE => write_last_maintenance(&state.agent_home, occ.slot),
        JOB_CORE_DISTILLATION => write_last_core_run(&state.agent_home, occ.slot),
        _ => {}
    }
    log.succeed(&occ.id, occ.attempts, now, run_file);
}

fn build_occurrence_trigger(state: &Arc<AppState>, occ: &crate::schedule_log::Occurrence) -> Result<Value, String> {
    let home = &state.agent_home;
    match occ.job.as_str() {
        JOB_DAILY_MAINTENANCE => Ok(json!({"type": JOB_DAILY_MAINTENANCE, "since_ts": read_last_maintenance(home)})),
        JOB_CORE_DISTILLATION => Ok(json!({"type": JOB_CORE_DISTILLATION, "since_ts": read_last_core_run(home)})),
        job => {
            let task_id = job.strip_prefix(TASK_JOB_PREFIX).ok_or_else(|| format!("unknown scheduler job `{job}`"))?;
            let task = crate::scheduler_tasks::load_task(home, task_id).ok_or_else(|| format!("task {task_id} no longer exists"))?;
            crate::scheduler_tasks::mark_run(home, task_id, crate::logs::now_unix_secs());
            Ok(json!({"type": "scheduled_task", "task_id": task.id, "data_path": task.data_path}))
        }
    }
}

/// Runs a self-driven trigger (as opposed to a human hitting `/api/message`/
/// `/api/wake`) and, since nothing else logs these, records `{trigger,
/// outcome}` to `logs/scheduled_runs/<ts>-<type>.json` — "要有 session 紀錄"
/// for scheduler-driven wakes. Returns that file's name alongside the
/// outcome so the schedule log can point at it — `GET
/// /api/scheduler/occurrences/{id}` serves it from there.
async fn run_scheduled(state: Arc<AppState>, trigger: Value, occ: Option<&crate::schedule_log::Occurrence>) -> (Value, Option<String>) {
    let ts = crate::logs::now_unix_secs();
    let trigger_type = trigger.get("type").and_then(|t| t.as_str()).unwrap_or("scheduled").to_string();
    let outcome = run_trigger(state.clone(), trigger.clone()).await;
    let dir = state.agent_home.join("logs/scheduled_runs");
    let mut run_file = None;
    if std::fs::create_dir_all(&dir).is_ok() {
        let mut record = json!({"ts": ts, "trigger": trigger, "outcome": outcome});
        if let Some(occ) = occ {
            record["occurrence"] = json!(occ.id);
            record["attempt"] = json!(occ.attempts);
        }
        // filename keyed by nanos, not `ts` (whole seconds) — background
        // triggers aren't serialized by any lock (only message/
        // compact_session are, see `AppState::session_locks`), so two of
        // them starting in the same scheduler_loop tick can share the same
        // *second*; nanosecond precision here only has to be unique, not
        // human-readable — files are found through the schedule log's
        // `run_file`, never by listing or sorting this directory
        let file_name = format!("{}-{trigger_type}.json", crate::logs::now_unix_nanos());
        if std::fs::write(dir.join(&file_name), serde_json::to_vec_pretty(&record).unwrap_or_default()).is_ok() {
            run_file = Some(file_name);
        }
    }
    (outcome, run_file)
}

async fn get_scheduler_occurrences(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"occurrences": state.schedule_log.list()}))
}

/// One occurrence's full story for the webui's detail view: its current
/// state, every event in its log, and — for each attempt that got as far as
/// running — the recorded trigger and outcome from `logs/scheduled_runs/`
/// (stdout trimmed to its tail; the summary/error is what matters there).
async fn get_scheduler_occurrence(State(state): State<Arc<AppState>>, AxumPath(id): AxumPath<String>) -> Json<Value> {
    let Some(occurrence) = state.schedule_log.get(&id) else {
        return Json(json!({"ok": false, "error": "no such occurrence"}));
    };
    let events = state.schedule_log.events_for(&id);
    let runs_dir = state.agent_home.join("logs/scheduled_runs");
    let runs: Vec<Value> = events
        .iter()
        .filter_map(|e| e.get("run_file").and_then(Value::as_str))
        // our own log wrote these, but they still end up in a path
        .filter(|f| !f.contains('/') && !f.contains(".."))
        .filter_map(|f| {
            let mut record: Value = serde_json::from_str(&std::fs::read_to_string(runs_dir.join(f)).ok()?).ok()?;
            if let Some(stdout) = record.pointer_mut("/outcome/stdout") {
                if let Some(text) = stdout.as_str() {
                    let tail: String = text.chars().rev().take(4000).collect::<Vec<_>>().into_iter().rev().collect();
                    *stdout = Value::String(tail);
                }
            }
            record["file"] = json!(f);
            Some(record)
        })
        .collect();
    Json(json!({"ok": true, "occurrence": occurrence, "events": events, "runs": runs}))
}

/// Re-queues a terminally failed occurrence — the scheduler's next tick
/// claims and runs it like any other pending one, so it still goes through
/// the same one-running-per-job rule and records its outcome in the log.
async fn post_scheduler_occurrence_retry(State(state): State<Arc<AppState>>, AxumPath(id): AxumPath<String>) -> Json<Value> {
    match state.schedule_log.request_retry(&id, crate::logs::now_unix_secs()) {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

// ---- todos (`/workspace/memory/` items, see `crate::workspace_memory`) ----

async fn get_todos(State(state): State<Arc<AppState>>) -> Json<Value> {
    use crate::workspace_memory as wm;
    let today = crate::logs::today_utc();
    let mut todos: Vec<Value> = wm::items(&state.agent_home)
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&path).ok()?;
            let item_state = match wm::state(&text, &today) {
                wm::ItemState::Active => "active",
                wm::ItemState::Expired => "expired",
                wm::ItemState::OneShot => "one_shot",
            };
            let modified = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            Some(json!({
                "name": name,
                "expires": wm::expires(&text),
                "state": item_state,
                "body": wm::body(&text),
                "modified": modified,
            }))
        })
        .collect();
    // what the next hourly pass will act on first, then by deadline
    todos.sort_by_key(|t| (t["state"] == "active", t["expires"].as_str().unwrap_or("").to_string(), t["name"].as_str().unwrap_or("").to_string()));
    Json(json!({"today": today, "todos": todos}))
}

#[derive(Deserialize)]
struct TodoBody {
    body: String,
    /// `YYYY-MM-DD`; omitted/null makes the item one-shot
    expires: Option<String>,
}

async fn put_todo(State(state): State<Arc<AppState>>, AxumPath(name): AxumPath<String>, Json(todo): Json<TodoBody>) -> Json<Value> {
    use crate::workspace_memory as wm;
    if !wm::valid_name(&name) {
        return Json(json!({"ok": false, "error": "name must be a plain `something.md` file name"}));
    }
    let expires = todo.expires.as_deref().map(str::trim).filter(|d| !d.is_empty());
    if let Some(date) = expires {
        if !wm::valid_date(date) {
            return Json(json!({"ok": false, "error": "expires must be YYYY-MM-DD"}));
        }
    }
    let dir = wm::dir(&state.agent_home);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Json(json!({"ok": false, "error": e.to_string()}));
    }
    match std::fs::write(dir.join(&name), wm::compose(expires, &todo.body)) {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn delete_todo(State(state): State<Arc<AppState>>, AxumPath(name): AxumPath<String>) -> Json<Value> {
    use crate::workspace_memory as wm;
    if !wm::valid_name(&name) {
        return Json(json!({"ok": false, "error": "bad name"}));
    }
    match std::fs::remove_file(wm::dir(&state.agent_home).join(&name)) {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

// ---- scheduler tasks (user/agent-defined recurring jobs, CRUD) ----

async fn get_scheduler_tasks(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"tasks": crate::scheduler_tasks::load_tasks(&state.agent_home)}))
}

#[derive(Deserialize)]
struct AddTaskBody {
    cron: String,
    data_path: String,
    #[serde(default)]
    description: String,
}

async fn post_scheduler_task(State(state): State<Arc<AppState>>, Json(body): Json<AddTaskBody>) -> impl IntoResponse {
    match crate::scheduler_tasks::add_task(&state.agent_home, &body.cron, &body.data_path, &body.description) {
        Ok(task) => (StatusCode::OK, Json(json!({"ok": true, "task": task}))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": e}))),
    }
}

#[derive(Deserialize, Default)]
struct UpdateTaskBody {
    cron: Option<String>,
    data_path: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
}

/// Full edit — any subset of fields — for a task viewed in the UI (or by
/// the agent via `read_file` on its own `scheduler/<id>.json`) and then
/// changed. `PUT` rather than `POST` since it's idempotent replace-by-field.
async fn put_scheduler_task(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<UpdateTaskBody>,
) -> impl IntoResponse {
    match crate::scheduler_tasks::update_task(
        &state.agent_home,
        &id,
        body.cron.as_deref(),
        body.data_path.as_deref(),
        body.description.as_deref(),
        body.enabled,
    ) {
        Ok(Some(task)) => (StatusCode::OK, Json(json!({"ok": true, "task": task}))),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": "no such task"}))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": e}))),
    }
}

async fn delete_scheduler_task(State(state): State<Arc<AppState>>, AxumPath(id): AxumPath<String>) -> Json<Value> {
    match crate::scheduler_tasks::remove_task(&state.agent_home, &id) {
        Ok(true) => Json(json!({"ok": true})),
        Ok(false) => Json(json!({"ok": false, "error": "no such task"})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct TaskFileQuery {
    /// a task's `data_path` verbatim — guest-absolute (e.g.
    /// `/workspace/tasks/x.md`), same root the agent's paths resolve against
    path: String,
}

/// `data_path` is guest-absolute; the agent's filesystem root is exactly
/// `agent_home`, so the host-side file lives at `agent_home/<path minus
/// leading '/'>`. Rejects any `..` component so this can't be steered
/// outside agent_home (same shape check as `get_attachment`'s
/// `workspace/uploads/` scoping, just not fixed to one subdirectory since a
/// task's `data_path` can legitimately point anywhere under agent_home).
fn resolve_guest_path(agent_home: &Path, guest_path: &str) -> Result<PathBuf, String> {
    let rel = Path::new(guest_path.trim_start_matches('/'));
    if rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err("path must not contain ..".to_string());
    }
    Ok(agent_home.join(rel))
}

/// `GET /api/scheduler/task_file?path=...` — lets the Scheduler panel show a
/// task's `data_path` contents next to its cron/description, instead of the
/// human needing to go find the file some other way. Missing file reads as
/// empty (a brand-new task's `data_path` usually doesn't exist yet) rather
/// than an error.
async fn get_task_file(State(state): State<Arc<AppState>>, Query(q): Query<TaskFileQuery>) -> impl IntoResponse {
    let full = match resolve_guest_path(&state.agent_home, &q.path) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e),
    };
    match std::fs::read_to_string(&full) {
        Ok(content) => (StatusCode::OK, content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (StatusCode::OK, String::new()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `PUT /api/scheduler/task_file?path=... <raw body>` — same disk-quota
/// enforcement as the guest's own `write_file` (agent_loop.rs) and
/// `save_attachment` above, so editing a task's instructions from the UI
/// can't bypass the cap the guest itself is held to.
async fn put_task_file(State(state): State<Arc<AppState>>, Query(q): Query<TaskFileQuery>, body: String) -> impl IntoResponse {
    let full = match resolve_guest_path(&state.agent_home, &q.path) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e),
    };
    let quota = Config::load(&state.agent_home).map(|c| c.disk.quota_bytes).unwrap_or_else(|_| crate::config::DiskConfig::default().quota_bytes);
    let existing_len = std::fs::metadata(&full).map(|m| m.len()).unwrap_or(0);
    let projected = agent_home_disk_usage(&state.agent_home) - existing_len + body.len() as u64;
    if projected > quota {
        return (StatusCode::BAD_REQUEST, format!("disk quota exceeded: writing would bring agent-home to {projected} bytes, over the {quota}-byte cap"));
    }
    if let Some(parent) = full.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }
    match std::fs::write(&full, &body) {
        Ok(()) => (StatusCode::OK, "file updated".to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}")),
    }
}

async fn auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let header_ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {}", state.token));
    // applied to every route (not just this one): browsers' native
    // EventSource can't set headers (used by /api/logs, /api/thinking), and
    // neither can a plain `<img src>` (used by /api/attachment) — both need
    // the token in the URL instead
    let query_ok = req
        .uri()
        .query()
        .is_some_and(|q| q.split('&').any(|kv| kv == format!("token={}", state.token)));
    if !header_ok && !query_ok {
        return (StatusCode::UNAUTHORIZED, "missing or wrong bearer token").into_response();
    }
    next.run(req).await
}

// ---- message / wake ----

#[derive(Deserialize)]
struct MessageBody {
    text: String,
    /// agent_home-relative paths from `/api/upload` (e.g.
    /// `workspace/uploads/172-cat.png`) — attached to this turn, see
    /// `turn_to_message`
    #[serde(default)]
    attachments: Vec<String>,
}

const DEFAULT_SESSION_KEY: &str = "webui";

/// Chat turns are host-tracked (not just an in-browser illusion of memory):
/// loads the given session, appends the human's turn, hands the *whole*
/// history to the guest as `trigger.history` so it's real conversational
/// context (not just RAG over `memory/notes/`), then appends the agent's
/// reply and saves. Shared by `/api/message` (webui, session key
/// `"webui"`) and the Discord adapter (one session key per channel/DM —
/// see `discord.rs`) so the two don't bleed into each other's history.
// owned `String`/`Option<String>` params (not `&str`) so `post_message` can
// `tokio::spawn` this whole function body — a spawned task keeps running to
// completion independent of whether its caller's future is still alive, so
// a client disconnecting mid-run (tab closed/reloaded) no longer cancels
// this before it reaches `save_session` below and silently drops the turn.
// Borrowed params would tie the spawned future's lifetime to the (dropped)
// caller's stack frame, so ownership has to move in.

/// Raw facts about who sent a chat message — not Discord-specific despite
/// most callers being `discord.rs` (webui passes a fixed one too, see
/// `post_message`, since the bearer token already confirms it's the
/// operator before this ever runs). `id` is namespaced per source
/// (`discord-<user id>`, `webui-owner`) so two different surfaces can never
/// collide on the same identity string. `is_owner` for Discord is checked
/// against Discord's own signed gateway payload (`msg.author.id`), never
/// anything conversation content could talk the model into believing.
/// `handle_chat_message` is the one place that turns this into actual
/// wording (`sender_note`, `SessionTurn::sender`), so there's a single
/// definition of how sender identity gets phrased instead of every call
/// site drifting apart.
pub(crate) struct MessageSender {
    pub name: String,
    pub id: String,
    pub is_owner: bool,
}

pub(crate) async fn handle_chat_message(
    state: Arc<AppState>,
    session_key: String,
    text: String,
    attachments: Vec<String>,
    channel: Option<String>,
    // `None` skips the sender-identity machinery entirely (no wording,
    // no `SessionTurn::sender` tag) — reserved for a caller that has no
    // sender concept at all. `post_message` (webui) still passes a fixed
    // `Some(..., is_owner: true)` rather than `None`, since the bearer
    // token already confirms it's the operator before this ever runs, and
    // it's cheap to keep every stored turn consistently tagged rather than
    // mixing tagged (Discord) and untagged (webui) turns.
    sender: Option<MessageSender>,
) -> Value {
    // an empty text block inside a multimodal content array (attachments
    // with no caption) trips the same "must not be empty" rejection the
    // all-empty-reply guard further down exists for — give it a placeholder
    // instead of ever sending "" as a block's text
    let display_text = if text.trim().is_empty() && !attachments.is_empty() { "(附件)".to_string() } else { text.clone() };
    // Short, persisted per-turn — distinct from the longer advisory
    // `sender_note` below, which is only ever attached to the *current*
    // trigger, not stored. This is what lets a later turn's `history` show
    // which past turn came from whom on a shared multi-person surface (see
    // `SessionTurn::sender`'s doc comment for the incident that motivated
    // this).
    let sender_label = sender.as_ref().map(|s| format!("{} (id {}, {})", s.name, s.id, if s.is_owner { "owner" } else { "not owner" }));
    let user_turn =
        SessionTurn { role: "user".to_string(), content: display_text, attachments, ts: crate::logs::now_unix_secs(), sender: sender_label };

    // Persisted immediately, not batched with `assistant_turn` after the run
    // — for a session key nobody's ever written to before (a Discord
    // channel's first-ever message), `discord.rs`'s `session_watch_loop`
    // baselines on whatever it sees the *first* time it polls a new key
    // (meant to skip replaying old history after a restart). If user+
    // assistant landed together in one write, that first sighting already
    // contains the never-sent assistant reply — the loop treats it as "old
    // history" and silently swallows it instead of delivering it. Writing
    // the user turn alone right away gives the loop something harmless to
    // baseline on (it only ever sends `assistant`-role turns, so this is
    // never itself delivered anywhere), so the assistant turn appended below
    // after the run is a genuine, detectable append.
    {
        let agent_home = state.agent_home.clone();
        let key = session_key.clone();
        let user_turn_for_save = user_turn.clone();
        if let Ok(Err(e)) = tokio::task::spawn_blocking(move || append_turns_locked(&agent_home, &key, vec![user_turn_for_save])).await {
            let _ = crate::logs::notify(&state.agent_home, &format!("failed to save this turn to session {session_key}: {e}"));
        }
    }

    // Snapshot: this specific message's view of the conversation, frozen to
    // its own real file the moment it arrives — *before* it ever waits on
    // `run_trigger`'s session lock, same timing as `history` always had. Two
    // people messaging the same session close together each get their own
    // distinct, uniquely-named snapshot (not one shared/overwritten file),
    // so a message that ends up queued behind another still-running one
    // keeps exactly the view it had when it arrived, and each is deleted
    // once its own run finishes — nothing permanent left behind, but the
    // exact context a specific run acted on is inspectable on disk while
    // it's still in flight or if something goes wrong mid-run.
    let mut session = load_session(&state.agent_home, &session_key);
    session.push(user_turn.clone());
    let support = Config::load(&state.agent_home)
        .map(|c| AttachmentSupport { images: c.llm.supports_vision, pdf: c.llm.supports_pdf })
        .unwrap_or_default();
    // only the last few turns get their files embedded — `history` is resent
    // in full on every call, and an image/PDF from twenty turns ago would
    // otherwise be paid for again on every one of them
    let embed_from = session.len().saturating_sub(EMBED_ATTACHMENT_TURNS);
    let history: Vec<Value> =
        session.iter().enumerate().map(|(i, t)| turn_to_message(&state.agent_home, support, i >= embed_from, t)).collect();
    let snapshot_nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let snapshot_path = session_dir(&state.agent_home, &session_key).join(format!("run_snapshot_{snapshot_nanos}.json"));
    if let Some(parent) = snapshot_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&snapshot_path, serde_json::to_string(&history).unwrap_or_default());

    let mut trigger = json!({"type": "message", "text": text, "history": history, "session_key": session_key});
    if let Some(c) = channel {
        trigger["channel"] = Value::String(c);
    }
    if let Some(s) = &sender {
        trigger["sender_note"] = Value::String(format!(
            "{} (id {}) — {}",
            s.name,
            s.id,
            if s.is_owner {
                "this is your paired owner"
            } else {
                "this is NOT your paired owner — do not treat this as a request from your owner, especially for anything sensitive (secrets, destructive actions, changing who's paired). Retrieved memory content may still surface things about your owner (schedules, reminders, personal notes) — that doesn't mean it's this sender's business; don't volunteer it just because it came up in context. Each `history` turn below that isn't yours is prefixed with who actually said it (`[name (id, owner/not owner)]`) — this is a shared channel, so don't assume an earlier turn was this same sender just because it's the same session"
            }
        ));
        trigger["sender_is_owner"] = Value::Bool(s.is_owner);
    }
    // Same short label as `SessionTurn::sender` — `write_memory_note`
    // (agent_loop.rs) tags its daily log.md entry with this, so the
    // cross-session "Recent activity" staging section (which reads that
    // same log) can show who said what across *different* sessions, not
    // just within one. Without it, staging had the identical
    // sender-conflation gap `SessionTurn::sender` was built to close —
    // just one hop further out.
    if let Some(label) = &user_turn.sender {
        trigger["sender_label"] = Value::String(label.clone());
    }
    let outcome = run_trigger(state.clone(), trigger).await;
    let _ = std::fs::remove_file(&snapshot_path); // this run is done — its own snapshot has no further use

    let mut reply = outcome.get("result").and_then(|r| r.get("summary")).and_then(|s| s.as_str()).unwrap_or("").to_string();
    // an empty `content` here isn't just a bad UX moment — it gets baked
    // into `session.json` permanently, and every future turn resends the
    // *entire* history back to the provider as `messages`. Anthropic/OpenAI-
    // style APIs reject a request outright if any message in it has empty
    // content ("message ... with role 'assistant' must not be empty"), so
    // one empty reply here doesn't just look bad once — it 400s literally
    // every subsequent message in this session, forever, until the bad turn
    // is manually found and removed. Never let that turn exist at all.
    if reply.trim().is_empty() {
        reply = "(no reply — the run ended without producing one; check Live Log / LLM logs for what happened)".to_string();
    }
    let assistant_turn = SessionTurn { role: "assistant".to_string(), content: reply, attachments: Vec::new(), ts: crate::logs::now_unix_secs(), sender: None };

    let agent_home = state.agent_home.clone();
    let key_for_save = session_key.clone();
    // nothing left to return this to — the HTTP response already carries
    // `outcome` by the time this resolves — so a lock failure here (rare:
    // only if `chat_send` or another turn on this session is somehow still
    // holding session.json.lock past its own 5s deadline) at least lands in
    // Live Log instead of silently vanishing along with the turn itself.
    // Only `assistant_turn` here — `user_turn` was already persisted above,
    // before `run_trigger`, see that comment for why.
    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || append_turns_locked(&agent_home, &key_for_save, vec![assistant_turn])).await {
        let _ = crate::logs::notify(&state.agent_home, &format!("failed to save this turn to session {session_key}: {e}"));
    }

    maybe_auto_compact(&state, &session_key);

    outcome
}

/// Appends this turn's new messages onto whatever's *currently* on disk,
/// rather than the snapshot `session` was loaded into at the top of
/// `handle_chat_message` — `run_trigger`'s `.await` above is a whole LLM
/// round trip (seconds, sometimes much longer), and `session_locks` (the
/// per-session tokio lock) only ever covers *this* run's own session_key;
/// it has no idea about `chat_send` (kernel/src/syscalls/chat_send.rs),
/// which runs from a completely unrelated, unlocked background trigger and
/// can append a proactive turn to this exact `session.json` at any point
/// during that window. Blindly overwriting with the stale start-of-turn
/// snapshot (the old behavior) would silently erase that turn forever —
/// not a rare timing coincidence, guaranteed to happen whenever the two
/// overlap. Locked with the same `session.json.lock` convention
/// `chat_send` uses, so the two can't race this reload-then-save against
/// each other either. Called via `spawn_blocking` since `FileLock::acquire`
/// spin-waits with `std::thread::sleep` — fine off the async executor
/// thread, not fine on it.
fn append_turns_locked(agent_home: &Path, session_key: &str, new_turns: Vec<SessionTurn>) -> Result<(), String> {
    let path = session_path(agent_home, session_key);
    let _lock = FileLock::acquire(path.with_extension("json.lock"), Duration::from_secs(5))?;
    let mut turns = load_session(agent_home, session_key);
    turns.extend(new_turns);
    save_session(agent_home, session_key, &turns).map_err(|e| e.to_string())
}

/// Fires a background compact once this session's last-measured context
/// crosses `config.chat.auto_compact_tokens` — mainly for Discord threads,
/// which (unlike webui) have no manual reset button; left alone they'd grow
/// the context window forever. Runs after the reply is already sent so it
/// never adds latency to the turn that tripped it.
fn maybe_auto_compact(state: &Arc<AppState>, session_key: &str) {
    let Some(tokens) = last_chat_context_tokens(&state.agent_home, session_key) else { return };
    let threshold = Config::load(&state.agent_home)
        .map(|c| c.chat.auto_compact_tokens)
        .unwrap_or(crate::config::ChatConfig::default().auto_compact_tokens);
    if tokens < threshold {
        return;
    }
    println!("[chat] {session_key} hit {tokens} context tokens (>= {threshold}) — auto-compacting");
    let state = state.clone();
    let key = session_key.to_string();
    tokio::spawn(async move {
        compact_session_key(state, &key).await;
    });
}

async fn post_message(State(state): State<Arc<AppState>>, Json(body): Json<MessageBody>) -> Json<Value> {
    // spawned rather than awaited inline: if the client disconnects mid-run
    // (tab closed/reloaded, network drop) axum drops this handler's future,
    // but a `tokio::spawn`ed task keeps running on its own regardless of
    // whether anything is still awaiting it — so the turn still reaches
    // `save_session` at the end of `handle_chat_message` instead of being
    // silently lost (the run itself always completed; only the session.json
    // write was getting cancelled along with the dropped HTTP response)
    // gated by the bearer token itself (see `auth`) before this handler
    // ever runs — a fixed, always-owner identity rather than `None`, so
    // every stored turn ends up consistently tagged instead of mixing
    // tagged (Discord) and untagged (webui) turns
    let sender = Some(MessageSender { name: "webui".to_string(), id: "webui-owner".to_string(), is_owner: true });
    let handle = tokio::spawn(handle_chat_message(state, DEFAULT_SESSION_KEY.to_string(), body.text, body.attachments, None, sender));
    match handle.await {
        Ok(outcome) => Json(outcome),
        Err(e) => Json(json!({"ok": false, "error": format!("run task panicked: {e}")})),
    }
}

#[derive(Deserialize)]
struct UploadBody {
    filename: String,
    /// raw base64 (no `data:...;base64,` prefix — the frontend strips that
    /// before sending)
    data_base64: String,
}

/// `POST /api/upload {filename, data_base64} -> {ok, path}` — webui's side
/// of `save_attachment` (Discord goes straight to that function itself
/// after downloading its own attachment bytes, see `discord.rs`).
async fn post_upload(State(state): State<Arc<AppState>>, Json(body): Json<UploadBody>) -> Json<Value> {
    use base64::Engine;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(&body.data_base64) {
        Ok(b) => b,
        Err(e) => return Json(json!({"ok": false, "error": format!("invalid base64: {e}")})),
    };
    match save_attachment(&state.agent_home, &body.filename, &bytes) {
        Ok(path) => Json(json!({"ok": true, "path": path})),
        Err(e) => Json(json!({"ok": false, "error": e})),
    }
}

/// Saves a chat attachment (webui upload or downloaded Discord attachment)
/// into `agent_home/workspace/uploads/` — a normal quota-counted,
/// guest-visible file like anything else `write_file` creates. The returned
/// agent_home-relative path gets passed back in `MessageBody::attachments`
/// / `handle_chat_message`'s `attachments` param. Same quota enforcement as
/// the guest's own `write_file` (agent_loop.rs) — uploads count against the
/// same cap, not a separate unbounded channel.
pub(crate) fn save_attachment(agent_home: &Path, filename: &str, bytes: &[u8]) -> Result<String, String> {
    let quota = Config::load(agent_home).map(|c| c.disk.quota_bytes).unwrap_or_else(|_| crate::config::DiskConfig::default().quota_bytes);
    let projected = agent_home_disk_usage(agent_home) + bytes.len() as u64;
    if projected > quota {
        return Err(format!("disk quota exceeded: uploading would bring agent-home to {projected} bytes, over the {quota}-byte cap"));
    }

    // basename only — strips any directory components the client sent, so
    // this can't be steered outside workspace/uploads/
    let safe_name = Path::new(filename).file_name().and_then(|n| n.to_str()).unwrap_or("upload").to_string();
    let rel_path = format!("workspace/uploads/{}-{safe_name}", crate::logs::now_unix_secs());
    let full_path = agent_home.join(&rel_path);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&full_path, bytes).map_err(|e| e.to_string())?;
    Ok(rel_path)
}

#[derive(Deserialize)]
struct AttachmentQuery {
    path: String,
}

/// `GET /api/attachment?path=workspace/uploads/...` — the webui's own
/// `<img>` tags need *something* to point at to show a user their attached
/// image back (kernel otherwise serves API only, no static file serving —
/// see `GatewayConfig` doc comment); deliberately scoped to
/// `workspace/uploads/` only so this can't become a general
/// read-any-file-in-agent-home endpoint.
async fn get_attachment(State(state): State<Arc<AppState>>, Query(q): Query<AttachmentQuery>) -> Response {
    let rel = Path::new(&q.path);
    let in_uploads = rel.starts_with("workspace/uploads") && !rel.components().any(|c| matches!(c, std::path::Component::ParentDir));
    if !in_uploads {
        return (StatusCode::FORBIDDEN, "attachment path must be under workspace/uploads/").into_response();
    }
    match std::fs::read(state.agent_home.join(rel)) {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, mime_for_path(&q.path))], bytes).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Host-side equivalent of agent_loop.rs's `agent_home_size` (that one runs
/// *inside* the guest against its own preopened `/`; this runs on the real
/// path from the kernel process) — same best-effort walk, same
/// skip-unreadable-entries behavior.
fn agent_home_disk_usage(agent_home: &Path) -> u64 {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        let mut total = 0u64;
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += walk(&entry.path());
            } else {
                total += meta.len();
            }
        }
        total
    }
    walk(agent_home)
}

/// dev/debug: fire an arbitrary trigger JSON immediately (PROJECT.md 4.4
/// `POST /api/wake` — "手動立即喚醒,開發調試常用")
async fn post_wake(State(state): State<Arc<AppState>>, Json(trigger): Json<Value>) -> Json<Value> {
    Json(run_trigger(state, trigger).await)
}

/// Runs one trigger as a fresh agent process (`spawn_blocking`, since the
/// syscall loop and reqwest's blocking client both block / panic if driven
/// directly on a tokio runtime thread). Locking is per-session, for any trigger that actually touches a
/// session — `message` (a live turn) and `compact_session` (reads +
/// rewrites the same `session.json`, see `compact_session_key`) — see
/// `AppState::session_locks`'s doc comment for why everything else runs
/// fully concurrently with no lock at all. `scheduled_task` additionally
/// locks per task id (`AppState::task_run_locks`), separately from the
/// session lock above, since a `scheduled_task` run has no `session_key` of
/// its own to begin with.
async fn run_trigger(state: Arc<AppState>, trigger: Value) -> Value {
    let trigger_type = trigger.get("type").and_then(|t| t.as_str());
    let touches_session = matches!(trigger_type, Some("message") | Some("compact_session"));
    let session_key = trigger.get("session_key").and_then(|s| s.as_str()).unwrap_or(DEFAULT_SESSION_KEY).to_string();
    let task_id = (trigger_type == Some("scheduled_task")).then(|| trigger.get("task_id").and_then(|t| t.as_str()).map(str::to_string)).flatten();

    let _session_guard = if touches_session { Some(state.session_lock(&session_key).await.lock_owned().await) } else { None };
    let _task_guard = match &task_id {
        Some(id) => Some(state.task_run_lock(id).await.lock_owned().await),
        None => None,
    };
    state.active_runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let outcome = run_trigger_inner(&state, trigger).await;
    state.active_runs.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    outcome
}

async fn run_trigger_inner(state: &Arc<AppState>, trigger: Value) -> Value {
    let agent_home = state.agent_home.clone();
    let agent_path = state.agent_path.clone();
    let trigger_str = trigger.to_string();

    let outcome = tokio::task::spawn_blocking(move || {
        let agent_home = agent_home.to_string_lossy().into_owned();
        let agent_path = agent_path.to_string_lossy().into_owned();
        crate::run_agent(&agent_home, &agent_path, &["run", &trigger_str])
    })
    .await;

    match outcome {
        Ok(Ok(outcome)) => {
            // `outcome.trapped` means the agent never reached its own
            // `RESULT:` line (almost always the run-timeout kill mid-turn
            // — see `run_agent_with_epoch_timeout`'s doc comment) — without
            // this, `result` stays `null` and every caller (the webui's
            // Schedule history panel included) has no way to tell "it
            // failed silently" apart from "it genuinely had nothing to
            // say", both of which otherwise render as the same unhelpful
            // "(no summary)".
            let result = if outcome.trapped {
                json!({"summary": "(run timed out — killed mid-turn with no result, see notifications log)"})
            } else {
                extract_result(&outcome.stdout)
            };
            let _ = persist_last_run(&state.agent_home, &result);
            json!({"ok": true, "result": result, "stdout": outcome.stdout})
        }
        Ok(Err(e)) => json!({"ok": false, "error": e.to_string()}),
        Err(e) => json!({"ok": false, "error": format!("run panicked: {e}")}),
    }
}

/// pulls the JSON after `RESULT:` (the guest's own convention, see
/// agent/src/agent_loop.rs) out of raw stdout for a clean API response
fn extract_result(stdout: &str) -> Value {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("RESULT:") {
            return serde_json::from_str(rest).unwrap_or_else(|_| Value::String(rest.to_string()));
        }
    }
    Value::Null
}

/// Called at the end of *every* `run_trigger`, including fully-concurrent
/// background triggers (see `AppState::session_locks`'s doc comment) — a
/// plain `std::fs::write` here (open-truncate-write, not atomic) meant two
/// runs finishing close together could interleave writes to the same
/// `last_run.json` into a torn/corrupt file, same defect class the
/// `http_get` cache write race was fixed for. Temp file + `rename` instead:
/// `rename` is atomic on POSIX same-filesystem, so a reader (`GET
/// /api/status`) always sees either the old or the new complete payload,
/// never a mix.
fn persist_last_run(agent_home: &Path, result: &Value) -> anyhow::Result<()> {
    let path = agent_home.join("logs/last_run.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = json!({"ts": crate::logs::now_unix_secs(), "result": result});
    let tmp_path = path.with_extension(format!("json.{}.tmp", crate::logs::now_unix_nanos()));
    std::fs::write(&tmp_path, serde_json::to_vec_pretty(&payload)?)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

// ---- status ----

async fn get_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    let read_json = |p: PathBuf| std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
    // `active_runs` counts every in-flight `run_trigger` call regardless of
    // trigger type or which session (if any) it's locked against — lets a
    // freshly loaded/reloaded webui tab find out *something's* running
    // right now (e.g. a long `ssh_exec` chain) instead of having no idea
    // one exists just because *this* page load didn't start it itself.
    let busy = state.active_runs.load(std::sync::atomic::Ordering::Relaxed) > 0;
    Json(json!({
        "budget": read_json(state.agent_home.join("logs/budget-state.json")),
        "last_run": read_json(state.agent_home.join("logs/last_run.json")),
        "busy": busy,
    }))
}

// ---- memory browser ----

async fn get_notes(State(state): State<Arc<AppState>>) -> Json<Value> {
    let base = state.agent_home.join("memory/notes");
    Json(json!({"notes": collect_notes(&base, &base)}))
}

/// `path` in the response is relative to `memory/notes/` (e.g. `"pet.md"`,
/// `"2026-07-05/log.md"`) — never the host's absolute path, which the
/// frontend has no business knowing about.
fn collect_notes(dir: &Path, base: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_notes(&path, base));
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().replace('\\', "/");
                out.push(json!({"path": rel, "content": content}));
            }
        }
    }
    out
}

/// One report per maintenance run — `hourly/` (`daily_maintenance`, every
/// 1h) and `core/` (`core_distillation`, every 24h) — plus `summary/` from
/// the since-removed 6h tier, still listed if present — are separate
/// subfolders, not one flat directory distinguished only by a filename
/// suffix. `date` is `"<kind>/<filename stem>"` (e.g.
/// `"hourly/2026-07-05_1830"`) — the kind prefix keeps every tier's
/// keys unique even on the rare tick where more than one fires with the same
/// `maintenance_run_id`, at the cost of sorting the tiers as separate
/// newest-first blocks rather than fully interleaved by time; `.last_run`/
/// `.last_summary_run`/`.last_core_run` (checkpoint markers, no `.md`
/// extension) are filtered out by the extension check regardless.
async fn get_reports(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut reports = Vec::new();
    for kind in ["hourly", "summary", "core"] {
        let dir = state.agent_home.join("maintenance_reports").join(kind);
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            if let Ok(content) = std::fs::read_to_string(&path) {
                reports.push(json!({"date": format!("{kind}/{stem}"), "kind": kind, "content": content}));
            }
        }
    }
    reports.sort_by(|a, b| b["date"].as_str().cmp(&a["date"].as_str())); // newest first (within each kind block)
    Json(json!({"reports": reports}))
}

// ---- generic file browser (logs/ and workspace/) ----
//
// Every existing log view (`LlmLogsPanel`, `ScheduleHistoryPanel`, the old
// `LogsPanel`) is purpose-built for one specific on-disk shape and adds real
// value beyond raw file content — `LlmLogsPanel` parses three different
// providers' transcript shapes into one summary line, `ScheduleHistoryPanel`
// shows each scheduler occurrence's state, events and recorded runs.
// Neither is what this is for. This is the fallback for everything *not*
// already purpose-built: `logs/run_log/`, `logs/chat_sessions/`,
// `logs/notifications.jsonl`, `stdout.jsonl`, `performance.jsonl`,
// `usage.jsonl`, `llm_circuit_breaker.json`, `workspace/`'s whole tree
// (uploads, per-task scratch files, `workspace/memory/` staging notes),
// ... — a plain directory tree + raw file viewer, one level at a time
// (lazy, not a full recursive dump — `logs/transcripts/` alone can hold
// hundreds of files). Parametrized over which top-level directory
// (`root`, `"logs"` or `"workspace"`) rather than two near-identical
// copies of the same three functions.

/// Rejects anything that could escape `agent_home/<root>/` — `..`
/// components, a leading `/` (would be absolute, ignoring `base.join`), or a
/// NUL byte. Same threat model as `SKILLS_DIR`'s name check elsewhere in
/// this file: single-owner tool behind the bearer token, but a path built
/// from a query param is still worth validating component-wise rather than
/// trusting it.
fn safe_browse_path(agent_home: &Path, root: &str, rel: &str) -> Option<PathBuf> {
    if rel.contains('\0') {
        return None;
    }
    for component in rel.split('/') {
        if component == ".." {
            return None;
        }
    }
    let base = agent_home.join(root);
    Some(if rel.is_empty() { base } else { base.join(rel.trim_start_matches('/')) })
}

#[derive(Deserialize)]
struct BrowsePathQuery {
    #[serde(default)]
    path: String,
}

/// Lists one directory level under `agent_home/<root>/` — `?path=` relative
/// to `<root>/` itself, empty for the root. Directories sort first, then
/// newest-modified first within each group (most log/workspace directories
/// are naturally chronological — `transcripts/`, `scheduled_runs/`, dated
/// `run_log/` subfolders, `workspace/uploads/` — so this needs no
/// per-shape special-casing to be useful).
async fn get_browse_tree(state: &Arc<AppState>, root: &str, q: &BrowsePathQuery) -> Json<Value> {
    let Some(dir) = safe_browse_path(&state.agent_home, root, &q.path) else {
        return Json(json!({"ok": false, "error": "invalid path"}));
    };
    let mut entries: Vec<Value> = match std::fs::read_dir(&dir) {
        Ok(read) => read
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
                Some(json!({
                    "name": e.file_name().to_string_lossy(),
                    "is_dir": meta.is_dir(),
                    "size": meta.len(),
                    "mtime": mtime,
                }))
            })
            .collect(),
        Err(e) => return Json(json!({"ok": false, "error": e.to_string()})),
    };
    entries.sort_by(|a, b| {
        let dir_rank = |v: &Value| !v["is_dir"].as_bool().unwrap_or(false); // dirs (false) sort before files (true)
        dir_rank(a).cmp(&dir_rank(b)).then_with(|| b["mtime"].as_u64().cmp(&a["mtime"].as_u64()))
    });
    Json(json!({"ok": true, "path": q.path, "entries": entries}))
}

async fn get_logs_tree(State(state): State<Arc<AppState>>, Query(q): Query<BrowsePathQuery>) -> Json<Value> {
    get_browse_tree(&state, "logs", &q).await
}

async fn get_workspace_tree(State(state): State<Arc<AppState>>, Query(q): Query<BrowsePathQuery>) -> Json<Value> {
    get_browse_tree(&state, "workspace", &q).await
}

/// `tail -n 100`, not a byte cap — every log format here is append-only
/// chronological, one entry per line (or per few lines), so the last 100
/// *lines* is the meaningful unit, not the last N bytes (which can cut a
/// JSON line in half and land mid-entry). `BROWSE_FILE_READ_CAP_BYTES` is a
/// separate safety net underneath that, not the truncation itself — it just
/// stops a pathologically huge file (a `logs/transcripts/*.json` can run
/// 900KB+) from being read fully into memory just to throw away everything
/// but the last 100 lines.
const BROWSE_FILE_TAIL_LINES: usize = 100;
const BROWSE_FILE_READ_CAP_BYTES: u64 = 2_000_000;

async fn get_browse_file(state: &Arc<AppState>, root: &str, q: &BrowsePathQuery) -> Json<Value> {
    let Some(path) = safe_browse_path(&state.agent_home, root, &q.path) else {
        return Json(json!({"ok": false, "error": "invalid path"}));
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return Json(json!({"ok": false, "error": e.to_string()})),
    };
    if meta.is_dir() {
        return Json(json!({"ok": false, "error": "is a directory, not a file"}));
    }
    let total_bytes = meta.len();
    let read_from = total_bytes.saturating_sub(BROWSE_FILE_READ_CAP_BYTES);
    let raw = if read_from > 0 {
        use std::io::{Read, Seek, SeekFrom};
        match std::fs::File::open(&path) {
            Ok(mut f) => {
                let _ = f.seek(SeekFrom::Start(read_from));
                let mut buf = Vec::new();
                let _ = f.read_to_end(&mut buf);
                buf
            }
            Err(e) => return Json(json!({"ok": false, "error": e.to_string()})),
        }
    } else {
        match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => return Json(json!({"ok": false, "error": e.to_string()})),
        }
    };
    let text = String::from_utf8_lossy(&raw);
    let lines: Vec<&str> = text.lines().collect();
    let line_count = lines.len();
    let tail_start = line_count.saturating_sub(BROWSE_FILE_TAIL_LINES);
    let content = lines[tail_start..].join("\n");
    let truncated = read_from > 0 || tail_start > 0;
    Json(json!({"ok": true, "content": content, "total_bytes": total_bytes, "truncated": truncated}))
}

async fn get_logs_file(State(state): State<Arc<AppState>>, Query(q): Query<BrowsePathQuery>) -> Json<Value> {
    get_browse_file(&state, "logs", &q).await
}

async fn get_workspace_file(State(state): State<Arc<AppState>>, Query(q): Query<BrowsePathQuery>) -> Json<Value> {
    get_browse_file(&state, "workspace", &q).await
}

// ---- config (read/write) ----

async fn get_config(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (StatusCode::OK, std::fs::read_to_string(state.agent_home.join("config.toml")).unwrap_or_default())
}

/// Validates against the real `Config` schema before writing — a typo'd
/// `config.toml` fails here with a useful error instead of silently falling
/// back to defaults on the agent's next wake.
async fn post_config(State(state): State<Arc<AppState>>, body: String) -> impl IntoResponse {
    if let Err(e) = toml::from_str::<Config>(&body) {
        return (StatusCode::BAD_REQUEST, format!("invalid config: {e}"));
    }
    match std::fs::write(state.agent_home.join("config.toml"), &body) {
        Ok(()) => (StatusCode::OK, "config updated".to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}")),
    }
}

// ---- soul (persona/identity — free-form markdown, no schema to validate) ----

async fn get_soul(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (StatusCode::OK, std::fs::read_to_string(state.agent_home.join("SOUL.md")).unwrap_or_default())
}

async fn post_soul(State(state): State<Arc<AppState>>, body: String) -> impl IntoResponse {
    match std::fs::write(state.agent_home.join("SOUL.md"), &body) {
        Ok(()) => (StatusCode::OK, "soul updated".to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("write failed: {e}")),
    }
}

// ---- core.md (read-only here — adr/001-memory-v2.md §2.3: only a
// core_distillation run ever writes it, no exception path, not even a
// human editing it via webui the way SOUL.md above allows) ----

async fn get_core(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (StatusCode::OK, std::fs::read_to_string(state.agent_home.join("core.md")).unwrap_or_default())
}

/// `git log` for `core.md` specifically — `commit_run` (autocommit.rs)
/// commits the whole agent-home tree every run, so this filters to just the
/// commits that actually touched this one file (adr/001-memory-v2.md §9
/// TODO: "core 單獨顯示 + git 演化史" — this is the "演化史" half). `%x1f`
/// (unit separator) as the field delimiter rather than something printable
/// — a commit message could otherwise contain the delimiter itself and
/// desync the split.
async fn get_core_history(State(state): State<Arc<AppState>>) -> Json<Value> {
    let git_dir = crate::autocommit::git_dir_path(&state.agent_home);
    if !git_dir.exists() {
        return Json(json!({"ok": true, "commits": []}));
    }
    let output = std::process::Command::new("git")
        .args([
            "--git-dir",
            &git_dir.to_string_lossy(),
            "--work-tree",
            &state.agent_home.to_string_lossy(),
            "log",
            "--follow",
            "--pretty=format:%H%x1f%ct%x1f%s",
            "--",
            "core.md",
        ])
        .output();
    let Ok(output) = output else {
        return Json(json!({"ok": false, "error": "git log failed to run"}));
    };
    if !output.status.success() {
        return Json(json!({"ok": false, "error": String::from_utf8_lossy(&output.stderr).into_owned()}));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let commits: Vec<Value> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\u{1f}');
            let hash = parts.next()?;
            let ts: i64 = parts.next()?.parse().ok()?;
            let message = parts.next().unwrap_or("");
            Some(json!({"hash": hash, "ts": ts, "message": message}))
        })
        .collect();
    Json(json!({"ok": true, "commits": commits}))
}

/// `core.md`'s content as of one specific past commit — `git show
/// <hash>:core.md`. `hash` comes straight from `get_core_history`'s own
/// output (never free-typed by a caller), but validated as hex anyway
/// before ever reaching a shelled-out `git` argument.
async fn get_core_at_commit(State(state): State<Arc<AppState>>, AxumPath(hash): AxumPath<String>) -> impl IntoResponse {
    if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return (StatusCode::BAD_REQUEST, "invalid commit hash".to_string());
    }
    let git_dir = crate::autocommit::git_dir_path(&state.agent_home);
    let output = std::process::Command::new("git")
        .args(["--git-dir", &git_dir.to_string_lossy(), "--work-tree", &state.agent_home.to_string_lossy(), "show", &format!("{hash}:core.md")])
        .output();
    match output {
        Ok(o) if o.status.success() => (StatusCode::OK, String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => (StatusCode::NOT_FOUND, String::from_utf8_lossy(&o.stderr).into_owned()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---- secrets (write-only — no endpoint ever returns a value) ----

#[derive(Deserialize)]
struct SetSecretBody {
    name: String,
    value: String,
}

/// Sets one secret in the vault. Response echoes back only the *names*
/// present in the vault, never values, as confirmation — there is no GET
/// endpoint for secrets at all.
///
/// Locked (`secrets.toml.lock`) — same lost-update class the `grants.rs`
/// fix closed: a bare load-mutate-save here meant two concurrent
/// `POST /api/secrets` calls (setting two *different* names) could each
/// load the same snapshot and each save their own version back, the second
/// silently discarding the first secret entirely rather than just
/// duplicating it.
async fn post_secret(State(state): State<Arc<AppState>>, Json(body): Json<SetSecretBody>) -> impl IntoResponse {
    let path = crate::secrets_path(&state.agent_home);
    let _lock = match FileLock::acquire(path.with_extension("toml.lock"), Duration::from_secs(5)) {
        Ok(lock) => lock,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e}))),
    };
    let mut secrets = Secrets::load(&path);
    secrets.set(&body.name, &body.value);
    match secrets.save(&path) {
        Ok(()) => (StatusCode::OK, Json(json!({"ok": true, "names": secrets.names()}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))),
    }
}

// ---- chat session (compact / reset, archived on both) ----
//
// Keyed per conversation source — `"webui"` for the browser UI, one
// `discord-dm-<user>`/`discord-channel-<channel>` per Discord source (see
// discord.rs) — so they don't bleed into each other's history/context.
// Everything *else* (memory/notes/ RAG, SOUL, skills, scheduled tasks) stays
// global: one agent, one long-term brain, many separate conversation
// threads with it.

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SessionTurn {
    role: String,
    content: String,
    /// agent_home-relative paths (see `MessageBody::attachments`) — old
    /// sessions predate this field, hence the default for back-compat
    #[serde(default)]
    attachments: Vec<String>,
    ts: i64,
    /// Short human-readable identity for a `role: "user"` turn on a shared
    /// multi-person surface (a Discord *channel* session — every message
    /// in it lands in the same `session_key` regardless of who actually
    /// sent it, unlike a DM or webui). `None` for webui (single-operator
    /// surface, no comparable ambiguity), for `role: "assistant"` turns,
    /// and for anything predating this field (`#[serde(default)]`).
    ///
    /// Without this, a channel session's `history` was every past turn
    /// flattened into anonymous "user" text with no way to tell which one
    /// came from the paired owner vs a different guild member — the model
    /// only ever saw the *current* turn's sender (`trigger.sender_note`),
    /// never who said what historically. A real incident: the owner spent
    /// several turns setting up a skill, a different guild member then
    /// asked one question in the same channel, and the model answered it
    /// using context built with the owner moments earlier — worse, when
    /// confronted, it confidently claimed the whole conversation had been
    /// with the owner, because from its side there was no way to tell
    /// otherwise. `turn_to_message` prefixes this onto the turn's content
    /// for every historical turn that has one, not just the newest.
    #[serde(default)]
    sender: Option<String>,
}

/// How many of the most recent chat turns get their attachments embedded.
const EMBED_ATTACHMENT_TURNS: usize = 3;
/// Larger files are only named in the text — well under the providers' own
/// per-file limits, and a sane ceiling on what one turn resends.
const MAX_EMBED_BYTES: usize = 15 * 1024 * 1024;

/// Which attachment types `[llm]` says the model can take directly.
#[derive(Clone, Copy, Default)]
struct AttachmentSupport {
    images: bool,
    pdf: bool,
}

/// Builds the OpenAI/Ollama-style `{role, content}` message `agent_loop.rs`
/// clones straight into its `messages` array (PROJECT.md: the guest treats
/// `content` as an opaque JSON value, string or block-array alike, so
/// nothing on the guest side needs to know attachments exist).
///
/// No attachments → plain string content, unchanged from before. With
/// attachments, on a turn where `embed` is set: an image is embedded as a
/// base64 `image_url` block if `[llm] supports_vision` is on, a PDF as a
/// base64 `file` block if `[llm] supports_pdf` is on (no reliable way to
/// detect either automatically for an arbitrary OpenAI-compatible
/// endpoint). Anything else (capability off, other file type, too big,
/// unreadable, or an older turn) just gets named in the text so the agent
/// can `read_file`/`view_file` it itself instead of the model silently
/// never learning it exists. `llm_call.rs` reshapes these blocks for
/// Anthropic/Ollama.
fn turn_to_message(agent_home: &Path, support: AttachmentSupport, embed: bool, turn: &SessionTurn) -> Value {
    // `[timestamp]` first, then `[sender]` — `sender` only ever set on a
    // `role: "user"` turn (see its doc comment), prefixed onto every
    // historical turn that has one, not just the newest, so a shared
    // multi-person channel session's `history` lets the model tell past
    // turns apart by who actually said them. The timestamp is unconditional
    // (every turn has one) — without it the model has no way to tell "this
    // was 5 turns ago" from "this was 3 days ago" in a resumed session, and
    // (2026-07-21) no way to notice a reply landed on a stale view of a
    // fast-moving multi-person conversation. `turn.ts` is fixed once a turn
    // is written, unlike `## Current time` (agent_loop.rs's `volatile`
    // block) which is deliberately recomputed every run — so, unlike that
    // one, prefixing it here doesn't defeat prefix caching turn over turn
    // (see kernel/src/logs.rs's `human_timestamp` doc comment).
    let content = format!(
        "[{}]{} {}",
        crate::logs::human_timestamp(turn.ts),
        turn.sender.as_ref().map(|label| format!(" [{label}]")).unwrap_or_default(),
        turn.content
    );
    if turn.attachments.is_empty() {
        return json!({"role": turn.role, "content": content});
    }

    let mut blocks = vec![json!({"type": "text", "text": content})];
    let mut notes = Vec::new();
    for rel_path in &turn.attachments {
        let mime = mime_for_path(rel_path);
        let wanted = embed && ((support.images && mime.starts_with("image/")) || (support.pdf && mime == "application/pdf"));
        let block = wanted
            .then(|| std::fs::read(agent_home.join(rel_path)).ok())
            .flatten()
            .filter(|bytes| bytes.len() <= MAX_EMBED_BYTES)
            .map(|bytes| {
                use base64::Engine;
                let data_url = format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
                if mime == "application/pdf" {
                    let filename = Path::new(rel_path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    json!({"type": "file", "file": {"filename": filename, "file_data": data_url}})
                } else {
                    json!({"type": "image_url", "image_url": {"url": data_url}})
                }
            });
        let embedded = block.map(|b| blocks.push(b)).is_some();
        if !embedded {
            notes.push(format!("[附件: {rel_path}]"));
        }
    }
    if !notes.is_empty() {
        if let Some(Value::String(t)) = blocks[0].get_mut("text") {
            if !t.is_empty() {
                t.push_str("\n\n");
            }
            t.push_str(&notes.join("\n"));
        }
    }
    json!({"role": turn.role, "content": blocks})
}

/// Extension-based, not content-sniffed — good enough to gate "is this
/// worth trying to embed as a vision block", not a security boundary.
fn mime_for_path(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn session_dir(agent_home: &Path, key: &str) -> PathBuf {
    agent_home.join("logs/chat_sessions").join(key)
}

fn session_path(agent_home: &Path, key: &str) -> PathBuf {
    session_dir(agent_home, key).join("session.json")
}

fn load_session(agent_home: &Path, key: &str) -> Vec<SessionTurn> {
    std::fs::read_to_string(session_path(agent_home, key))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}



fn save_session(agent_home: &Path, key: &str, turns: &[SessionTurn]) -> anyhow::Result<()> {
    let path = session_path(agent_home, key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(turns)?)?;
    Ok(())
}

/// Moves the session to `<session_dir>/archive/<ts>.json` — "留一個 session
/// 紀錄": reset/compact never just discard history, they archive it first.
fn archive_session(agent_home: &Path, key: &str) -> anyhow::Result<Option<PathBuf>> {
    let turns = load_session(agent_home, key);
    if turns.is_empty() {
        return Ok(None);
    }
    let dir = session_dir(agent_home, key).join("archive");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", crate::logs::now_unix_secs()));
    std::fs::write(&path, serde_json::to_vec_pretty(&turns)?)?;
    Ok(Some(path))
}

async fn get_session(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "turns": load_session(&state.agent_home, DEFAULT_SESSION_KEY),
        "context_tokens": last_chat_context_tokens(&state.agent_home, DEFAULT_SESSION_KEY),
    }))
}

fn chat_context_tokens_path(agent_home: &Path, key: &str) -> PathBuf {
    session_dir(agent_home, key).join("context_tokens.json")
}

/// The exact size of the context window on the most recent *chat* turn's
/// last `llm_call` (system prompt + full history + that turn's running
/// action-loop messages so far) — written by `agent_loop.rs` itself only
/// when `trigger.type == "message"`, specifically so a `daily_maintenance`/
/// scheduled-task run elsewhere (or a session compact's own
/// summarization call) never clobbers what the *chat* panel shows; those
/// aren't the chat session's context at all.
fn last_chat_context_tokens(agent_home: &Path, key: &str) -> Option<u64> {
    let text = std::fs::read_to_string(chat_context_tokens_path(agent_home, key)).ok()?;
    serde_json::from_str::<Value>(&text).ok()?.get("tokens")?.as_u64()
}

/// Reset/compact invalidate whatever number was showing — the old figure
/// described a session that no longer exists, and the true post-reset/
/// compact size isn't known until the next real chat message actually
/// measures it (so display "—" in the meantime rather than a stale number).
fn clear_chat_context_tokens(agent_home: &Path, key: &str) {
    let _ = std::fs::remove_file(chat_context_tokens_path(agent_home, key));
}

async fn post_session_reset(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(reset_session_key(&state, DEFAULT_SESSION_KEY).await)
}

/// Archives, then clears, the given session — a fresh conversation. Shared
/// by the webui `/api/session/reset` endpoint and Discord's `!reset` DM/
/// mention command (discord.rs) — same mechanism, keyed by whichever
/// session it's asked for. Takes the same per-session lock `run_trigger`
/// does for `message`/`compact_session` — this doesn't go through
/// `run_trigger` at all (pure filesystem, no agent run needed), but
/// without the lock a Reset click landing while that session's own
/// in-flight `message` run is mid-turn could race: the run's own
/// end-of-turn `session.json` write could land *after* this clears it,
/// resurrecting the conversation the human just explicitly reset.
pub(crate) async fn reset_session_key(state: &Arc<AppState>, key: &str) -> Value {
    let _guard = state.session_lock(key).await.lock_owned().await;
    let agent_home = &state.agent_home;
    match archive_session(agent_home, key) {
        Ok(archived) => {
            let _ = save_session(agent_home, key, &[]);
            clear_chat_context_tokens(agent_home, key);
            json!({"ok": true, "archived": archived.map(|p| p.display().to_string())})
        }
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

async fn post_session_compact(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(compact_session_key(state.clone(), DEFAULT_SESSION_KEY).await)
}

/// Archives the full session, then asks the agent to collapse it into one
/// short summary turn — same idea as Claude Code's `/compact`: keep context
/// going without the message list growing forever. Shared by the webui
/// `/api/session/compact` endpoint, Discord's `!compact` command, and the
/// auto-compact check in `handle_chat_message` once a session crosses
/// `config.chat.auto_compact_tokens`.
pub(crate) async fn compact_session_key(state: Arc<AppState>, key: &str) -> Value {
    let session = load_session(&state.agent_home, key);
    if session.is_empty() {
        return json!({"ok": true, "message": "nothing to compact"});
    }
    let _ = archive_session(&state.agent_home, key);

    // compaction only needs the text to summarize, not attachment images —
    // no `turn_to_message`/vision-embedding here, plain content is enough
    let history: Vec<Value> = session.iter().map(|t| json!({"role": t.role, "content": t.content})).collect();
    // `session_key` here isn't for the guest (compact_session's own prompt
    // doesn't use it) — it's so `run_trigger`'s locking treats this the same
    // as a `message` run on the same session: without it, a `message` turn
    // and this session's own auto-compact could race on the *same*
    // session.json concurrently, exactly what per-session locking exists to
    // prevent (see `AppState::session_locks`).
    let outcome = run_trigger(state.clone(), json!({"type": "compact_session", "history": history, "session_key": key})).await;
    let summary = outcome.get("result").and_then(|r| r.get("summary")).and_then(|s| s.as_str()).unwrap_or("").to_string();

    let compacted = vec![SessionTurn {
        role: "system".to_string(),
        content: format!("(earlier conversation, compacted) {summary}"),
        attachments: Vec::new(),
        ts: crate::logs::now_unix_secs(),
        sender: None,
    }];
    let _ = save_session(&state.agent_home, key, &compacted);
    clear_chat_context_tokens(&state.agent_home, key);
    json!({"ok": true, "summary": summary})
}

// ---- grants (tofu new-domain queued for approval — writes used to queue
// here too until http_get's write path was removed, see grants.rs docs) ----

async fn get_grants(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"grants": crate::grants::load_grants(&state.agent_home)}))
}

async fn post_grant_approve(State(state): State<Arc<AppState>>, AxumPath(id): AxumPath<String>) -> Json<Value> {
    match crate::grants::approve(&state.agent_home, &id) {
        Ok(Some(g)) => Json(json!({"ok": true, "grant": g})),
        Ok(None) => Json(json!({"ok": false, "error": "no such grant"})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

async fn post_grant_deny(State(state): State<Arc<AppState>>, AxumPath(id): AxumPath<String>) -> Json<Value> {
    match crate::grants::deny(&state.agent_home, &id) {
        Ok(Some(g)) => Json(json!({"ok": true, "grant": g})),
        Ok(None) => Json(json!({"ok": false, "error": "no such grant"})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

// ---- egress log viewer ----

async fn get_egress(State(state): State<Arc<AppState>>) -> Json<Value> {
    let text = std::fs::read_to_string(state.agent_home.join("logs/egress.jsonl")).unwrap_or_default();
    let entries: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    Json(json!({"entries": entries}))
}

/// Whether a Discord user has paired as "owner" yet (`chat_send`'s
/// `target: "discord"` default destination) — and if not, the pairing code
/// valid *right now* to DM the bot (rotates every 60s, see
/// `discord::current_pairing_code`).
async fn get_discord_pairing(State(state): State<Arc<AppState>>) -> Json<Value> {
    match crate::discord::load_owner(&state.agent_home) {
        Some(user_id) => Json(json!({"paired": true, "user_id": user_id})),
        None => Json(json!({"paired": false, "code": crate::discord::current_pairing_code(&state.agent_home)})),
    }
}

// ---- llm call log viewer (browses logs/transcripts/, written by
// kernel/src/syscalls/llm_call.rs's write_transcript for every single
// llm_call — one file per call, filename is the nanosecond timestamp) ----

/// Capped to the most recent 100 — transcripts can grow one file per call,
/// no pagination yet, just enough to "quickly see what's going on".
const MAX_LLM_LOGS: usize = 100;

async fn get_llm_logs(State(state): State<Arc<AppState>>) -> Json<Value> {
    let dir = state.agent_home.join("logs/transcripts");
    let mut logs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_name().and_then(|f| f.to_str()).and_then(|f| f.strip_suffix("-llm_call.json")) else {
                continue;
            };
            let Ok(ts_nanos) = stem.parse::<u128>() else { continue };
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            logs.push(json!({
                "ts": (ts_nanos / 1_000_000_000) as i64,
                "request": v["request"], "response": v["response"], "source": v["source"],
            }));
        }
    }
    logs.sort_by(|a, b| b["ts"].as_i64().cmp(&a["ts"].as_i64())); // newest first
    logs.truncate(MAX_LLM_LOGS);
    Json(json!({"logs": logs}))
}

// ---- skills browser (mirrors agent/src/skills.rs's file format exactly,
// so anything saved/edited here is exactly what `use_skill` loads) ----

const SKILLS_DIR: &str = "skills";

async fn get_skills(State(state): State<Arc<AppState>>) -> Json<Value> {
    let dir = state.agent_home.join(SKILLS_DIR);
    let mut skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Some(skill) = parse_skill(&text) {
                    skills.push(skill);
                }
            }
        }
    }
    skills.sort_by(|a: &Value, b: &Value| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(json!({"skills": skills}))
}

#[derive(Deserialize)]
struct SkillBody {
    name: String,
    description: String,
    body: String,
}

/// Locked (`<name>.md.lock`, same physical path `agent/src/skills.rs`'s
/// `SkillLock` locks from the guest side) — this load-mutate-save used to
/// have no coordination at all with the guest's own `record_use` (fires on
/// every `use_skill`, now genuinely concurrent across sessions/background
/// triggers), so a human editing a skill's description/body via webui at
/// the same moment a run was using that skill could have either side's
/// write silently discard the other's — worse when it's the human's
/// content edit that gets reverted by a concurrent usage-stat bump than
/// the reverse, but both are real data loss either way.
async fn post_skill(State(state): State<Arc<AppState>>, Json(skill): Json<SkillBody>) -> impl IntoResponse {
    if skill.name.is_empty() || skill.name.contains(['/', '\\', '.']) {
        return (StatusCode::BAD_REQUEST, "skill name must be non-empty and contain no path separators".to_string());
    }
    let dir = state.agent_home.join(SKILLS_DIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    // preserve usage stats across a webui edit — editing description/body
    // isn't "learning it" again, `created_at`/`used_count`/`last_used`
    // shouldn't reset just because a human tweaked the content (mirrors
    // agent/src/skills.rs `save`'s same reasoning exactly)
    let path = dir.join(format!("{}.md", skill.name));
    let _lock = match FileLock::acquire(path.with_extension("md.lock"), Duration::from_secs(5)) {
        Ok(lock) => lock,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let existing = std::fs::read_to_string(&path).ok().and_then(|t| parse_skill(&t));
    let created_at = existing.as_ref().and_then(|s| s["created_at"].as_u64()).unwrap_or_else(|| crate::logs::now_unix_secs() as u64);
    let used_count = existing.as_ref().and_then(|s| s["used_count"].as_u64()).unwrap_or(0);
    // same reasoning as `used_count`/`last_used` — a webui description/body
    // edit isn't "never retrieved yet" again, don't reset the quality-gate
    // signal (adr/002-skill-v2.md §3) just because a human tweaked the text
    let retrieval_hit_count = existing.as_ref().and_then(|s| s["retrieval_hit_count"].as_u64()).unwrap_or(0);
    let last_used_line = existing
        .as_ref()
        .and_then(|s| s["last_used"].as_u64())
        .map(|t| format!("last_used: {t}\n"))
        .unwrap_or_default();
    // same trim as agent/src/skills.rs `render` — otherwise saving the same
    // skill repeatedly grows a longer run of trailing blank lines each time
    let body = skill.body.trim_end_matches('\n');
    let content = format!(
        "---\nname: {}\ndescription: {}\ncreated_at: {created_at}\nused_count: {used_count}\n\
         retrieval_hit_count: {retrieval_hit_count}\n{last_used_line}---\n{body}\n",
        skill.name, skill.description
    );
    match std::fs::write(path, content) {
        Ok(()) => (StatusCode::OK, "saved".to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Cleans up `skill_chunks`/`skill_chunks_fts` for a deleted skill —
/// adr/002-skill-v2.md §2: without this, `DELETE /api/skills/{name}`
/// removes the file but leaves its index rows behind, a dangling result
/// hybrid search can still surface pointing at a `source_path` that no
/// longer exists. Runs against its own short-lived `rusqlite::Connection`
/// (WAL mode already set by `AgentState::db()` the first time any run opens
/// `index.db`, so a second concurrent connection here is safe) rather than
/// going through the guest's `db_exec` syscall — this HTTP handler has no
/// running agent turn to make that call on its behalf. Best-effort: the file
/// removal below is the primary action, a failure here just leaves an
/// orphaned index row for the next `reindex_all_skills` sweep to catch
/// instead (same reverse-orphan-diff safety net as notes).
fn remove_skill_index_rows(agent_home: &Path, source_path: &str) {
    let Ok(conn) = rusqlite::Connection::open(agent_home.join("memory/index.db")) else { return };
    let ids: Vec<i64> = conn
        .prepare("SELECT id FROM skill_chunks WHERE source_path = ?1")
        .and_then(|mut stmt| stmt.query_map([source_path], |row| row.get(0))?.collect())
        .unwrap_or_default();
    for id in ids {
        let _ = conn.execute("DELETE FROM skill_chunks_fts WHERE rowid = ?1", [id]);
    }
    let _ = conn.execute("DELETE FROM skill_chunks WHERE source_path = ?1", [source_path]);
}

async fn delete_skill(State(state): State<Arc<AppState>>, AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    let path = state.agent_home.join(SKILLS_DIR).join(format!("{name}.md"));
    match std::fs::remove_file(&path) {
        Ok(()) => {
            // `skill_chunks.source_path` was written by the *guest* side
            // (`agent/src/skills.rs::path_for` / `SKILLS_DIR`), which sees
            // its own WASI-root-relative path (`/skills/<name>.md`), not
            // this handler's host filesystem path — must match that exact
            // string, not `path` above.
            remove_skill_index_rows(&state.agent_home, &format!("/skills/{name}.md"));
            (StatusCode::OK, "deleted".to_string())
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn parse_skill(text: &str) -> Option<Value> {
    let rest = text.trim_start().strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n').to_string();

    let mut name = None;
    let mut description = String::new();
    let mut created_at = 0u64;
    let mut used_count = 0u64;
    let mut last_used: Option<u64> = None;
    let mut retrieval_hit_count = 0u64;
    for line in front.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            match key.trim() {
                "name" => name = Some(value.to_string()),
                "description" => description = value.to_string(),
                "created_at" => created_at = value.parse().unwrap_or(0),
                "used_count" => used_count = value.parse().unwrap_or(0),
                "retrieval_hit_count" => retrieval_hit_count = value.parse().unwrap_or(0),
                "last_used" => last_used = value.parse().ok(),
                _ => {}
            }
        }
    }
    Some(json!({
        "name": name?, "description": description, "body": body,
        "created_at": created_at, "used_count": used_count, "last_used": last_used,
        "retrieval_hit_count": retrieval_hit_count,
    }))
}

// ---- live "thinking" stream + abort ----

/// Sets a flag `llm_call` checks between streamed chunks — cuts a
/// runaway/unproductive generation short instead of paying for tokens the
/// agent doesn't need (kernel/src/syscalls/llm_call.rs's stream handlers).
/// Cooperative only: doesn't reach a run that's inside `http_get`/`ssh_exec`
/// or still waiting on `llm_call`'s first response byte — accepted
/// trade-off for staying in-process (see `AppState::session_locks`'s doc
/// comment for the concurrency side of the same decision); those cases just
/// run to completion instead of stopping instantly.
///
/// `?session=<key>` scopes which run gets stopped, same key
/// `llm_call::abort_flag_path`/`thinking_path` use — used to be one
/// process-global flag, which stopped being safe once runs went
/// per-session: a global flag could abort an unrelated concurrent session's
/// run instead of the intended one. Defaults to `"webui"` (what the Chat
/// panel's abort button hits); background triggers (daily_maintenance/
/// scheduled_task) have no session of their own and all share `"_system"`.
async fn post_abort(State(state): State<Arc<AppState>>, Query(q): Query<ThinkingQuery>) -> Json<Value> {
    let key = q.session.unwrap_or_else(|| DEFAULT_SESSION_KEY.to_string());
    let path = session_dir(&state.agent_home, &key).join("abort_requested");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, "") {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(Deserialize)]
struct ThinkingQuery {
    session: Option<String>,
}

/// Unlike `/api/logs` (which tails *new lines*), this re-sends the *whole*
/// current file each time it changes — `thinking-live.txt` is one growing
/// blob per call, not a line-oriented log.
///
/// `?session=<key>` picks which session's live trace to tail — same
/// `chat_sessions/<key>/thinking-live.txt` a running turn writes to
/// (`agent_loop.rs` `thinking_live_path`, `llm_call.rs` `thinking_path`);
/// defaults to `"webui"` since that's the only session the webui Chat
/// panel itself ever watches. Without this, a Discord/scheduled run in the
/// background would blast its own trace over whatever the webui viewer was
/// just watching, and vice versa.
async fn get_thinking_sse(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ThinkingQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let key = q.session.unwrap_or_else(|| DEFAULT_SESSION_KEY.to_string());
    Sse::new(thinking_stream(session_dir(&state.agent_home, &key).join("thinking-live.txt")))
        .keep_alive(axum::response::sse::KeepAlive::default())
}

fn thinking_stream(path: PathBuf) -> impl Stream<Item = Result<Event, Infallible>> {
    let last = std::fs::read_to_string(&path).unwrap_or_default();
    stream::unfold((path, last), |(path, last)| async move {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text != last {
                return Some((Ok(Event::default().data(text.clone())), (path, text)));
            }
        }
    })
}

/// One-shot, non-SSE read of the exact same file `/api/thinking` tails —
/// for grabbing the *final* trace right after a run completes. The SSE
/// stream only emits on a poll tick that *happens* to land after the file
/// changed; a fast run (a plain reply with no actions) can write its whole
/// trace and finish before the next 200ms tick ever fires, and the webui's
/// "thinking" bubble unmounts the instant `/api/message` resolves — so the
/// live view can go the entire run showing nothing at all. Calling this
/// once right after that same request resolves guarantees the full trace,
/// no race against a poll interval.
async fn get_thinking_snapshot(State(state): State<Arc<AppState>>, Query(q): Query<ThinkingQuery>) -> Json<Value> {
    let key = q.session.unwrap_or_else(|| DEFAULT_SESSION_KEY.to_string());
    let text = std::fs::read_to_string(session_dir(&state.agent_home, &key).join("thinking-live.txt")).unwrap_or_default();
    Json(json!({"text": text}))
}

// ---- live log stream ----

async fn get_logs_sse(State(state): State<Arc<AppState>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(logs_stream(state.agent_home.join("logs/notifications.jsonl")))
        .keep_alive(axum::response::sse::KeepAlive::default())
}

struct LogPoll {
    path: PathBuf,
    last_len: u64,
    pending: VecDeque<String>,
}

/// Polling rather than inotify — simplest thing that works at toy-project
/// log volume, no extra dependency.
fn logs_stream(path: PathBuf) -> impl Stream<Item = Result<Event, Infallible>> {
    let last_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    stream::unfold(LogPoll { path, last_len, pending: VecDeque::new() }, |mut st| async move {
        loop {
            if let Some(line) = st.pending.pop_front() {
                return Some((Ok(Event::default().data(line)), st));
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let Ok(text) = std::fs::read_to_string(&st.path) else { continue };
            let len = text.len() as u64;
            if len > st.last_len {
                let new_part = text[st.last_len as usize..].to_string();
                st.last_len = len;
                st.pending.extend(new_part.lines().filter(|l| !l.trim().is_empty()).map(str::to_string));
            }
        }
    })
}

#[cfg(test)]
mod outcome_aborted_tests {
    use super::*;

    #[test]
    fn outcome_aborted_detects_both_failure_shapes() {
        assert!(outcome_aborted(&serde_json::json!({"ok": false, "error": "spawn failed"})));
        assert!(outcome_aborted(&serde_json::json!({"ok": true, "result": {"summary": "run aborted: llm_call failed 3x in a row"}})));
        assert!(!outcome_aborted(&serde_json::json!({"ok": true, "result": {"summary": "did the thing successfully"}})));
    }
}

#[cfg(test)]
mod has_new_log_entries_tests {
    use super::*;
    use std::fs;

    fn write_log_day(agent_home: &Path, day: &str, contents: &str) {
        let dir = agent_home.join("logs/run_log").join(day);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("log.md"), contents).unwrap();
    }

    /// Reproduces the self-feeding loop this test guards against: a
    /// `daily_maintenance` run's own completion is logged via
    /// `write_memory_note` (unconditional for every trigger type) with a ts
    /// after the checkpoint that gated it — before the fix, this alone made
    /// `has_new_log_entries` return true on the very next check forever,
    /// independent of whether anything external ever happened.
    #[test]
    fn ignores_a_tiers_own_bookkeeping_entry() {
        let dir = tempfile_dir();
        let today = crate::logs::civil_from_days(crate::logs::now_unix_secs() / 86_400);
        write_log_day(
            &dir,
            &today,
            "\n## run at 2026-07-21 19:13:00 UTC (ts=2000)\ntrigger: daily_maintenance — nothing new\nsummary: nothing new\n",
        );
        assert!(!has_new_log_entries(&dir, 1000), "a tier's own daily_maintenance/core_distillation entry must not count as new activity");
    }

    #[test]
    fn still_detects_a_real_message_event() {
        let dir = tempfile_dir();
        let today = crate::logs::civil_from_days(crate::logs::now_unix_secs() / 86_400);
        write_log_day(
            &dir,
            &today,
            "\n## run at 2026-07-21 19:13:00 UTC (ts=2000)\ntrigger: message [webui] — hi\nsummary: replied\n",
        );
        assert!(has_new_log_entries(&dir, 1000), "a real chat message must still count as new activity");
    }

    #[test]
    fn mixed_log_finds_the_real_event_past_the_bookkeeping_noise() {
        let dir = tempfile_dir();
        let today = crate::logs::civil_from_days(crate::logs::now_unix_secs() / 86_400);
        write_log_day(
            &dir,
            &today,
            "\n## run at 2026-07-21 19:13:00 UTC (ts=2000)\ntrigger: daily_maintenance — nothing new\nsummary: nothing new\n\
             \n## run at 2026-07-21 20:00:00 UTC (ts=2500)\ntrigger: cron — scheduled task fired\nsummary: sent report\n",
        );
        assert!(has_new_log_entries(&dir, 1000));
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebina-gateway-test-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rand_suffix() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
    }
}

#[cfg(test)]
mod missed_in_gap_tests {
    use super::*;

    // 2026-07-06 09:00:00 UTC — same reference instant `cron.rs`'s own
    // tests use, minute 0 so it never accidentally matches "17 9 * * *"
    // itself
    const NINE_AM: i64 = 1_783_328_400;
    const SPEC: &str = "17 9 * * *"; // rss_tech_report's real cron

    #[test]
    fn catches_an_exact_minute_that_fell_inside_the_gap() {
        let gap_start = NINE_AM; // 09:00:00
        let now = NINE_AM + 20 * 60; // 09:20:00 — 09:17 is strictly inside
        assert!(missed_in_gap(SPEC, gap_start, now).is_some());
    }

    #[test]
    fn no_match_found_when_the_gap_never_touches_the_matching_minute() {
        let gap_start = NINE_AM; // 09:00:00
        let now = NINE_AM + 5 * 60; // 09:05:00 — well before 09:17
        assert!(missed_in_gap(SPEC, gap_start, now).is_none());
    }

    #[test]
    fn ordinary_30s_tick_gap_never_false_positives() {
        let gap_start = NINE_AM + 17 * 60; // last tick was exactly at 09:17
        let now = gap_start + 30; // this tick, 30s later
        assert!(missed_in_gap(SPEC, gap_start, now).is_none());
    }

    #[test]
    fn does_not_count_the_current_tick_s_own_minute() {
        // 09:17 falls in [now-60, now) — that's due_now's job, not this
        // function's; missed_in_gap must not double-report it.
        let gap_start = NINE_AM; // 09:00:00
        let now = NINE_AM + 17 * 60 + 30; // 09:17:30, mid-way through minute 17
        assert!(missed_in_gap(SPEC, gap_start, now).is_none());
    }
}
