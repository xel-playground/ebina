use crate::config::Config;
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
    pub wasm_path: PathBuf,
    /// single bearer token gating every `/api/*` route (PROJECT.md 4.4)
    pub token: String,
    pub port: u16,
}

pub(crate) struct AppState {
    pub(crate) agent_home: PathBuf,
    wasm_path: PathBuf,
    token: String,
    /// Only `message`-type runs (a real conversation) get serialized, and
    /// only against *their own session* — same session's turns must land in
    /// strict order (`session.json` is a read-modify-write per turn; two
    /// concurrent turns on the same session would race that) and a human
    /// doesn't want two replies interleaving one conversation. Every other
    /// trigger type (`cron`/`daily_maintenance`/`scheduled_task`/`manual`)
    /// has no session to protect this way and runs fully concurrently with
    /// everything else — safe now that `write_file`/`append_file`'s target
    /// directories no longer overlap between trigger types (see
    /// `agent_loop.rs`'s `write_action_denial`): a `message` run can only
    /// touch `/workspace/`, curated `memory/notes/` is `daily_maintenance`-
    /// only, and only one `daily_maintenance` is ever in flight at a time
    /// (the scheduler's own 6h-cycle gating). The residual risk this
    /// doesn't close — two *background* runs (e.g. two `scheduled_task`s)
    /// racing on `memory/index.db` reindexing the same note, or both
    /// hitting `.git/index.lock` on autocommit at once — is accepted rather
    /// than solved with finer-grained locking; both fail soft (a duplicated
    /// index row, a skipped commit that a later run's autocommit picks up)
    /// rather than corrupting anything.
    session_locks: tokio::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// how many `run_trigger` calls are currently in flight, of any trigger
    /// type — `GET /api/status`'s `busy` field used to be "is the one
    /// global lock held", which stopped meaning anything once locking went
    /// per-session; this is trigger-type-agnostic so the webui still gets a
    /// meaningful "something's running right now" signal.
    active_runs: std::sync::atomic::AtomicUsize,
}

impl AppState {
    /// Returns (creating if needed) the dedicated lock for one session key.
    /// Never removed once created — session keys are bounded (webui + one
    /// per Discord DM/channel), not something that grows unbounded over the
    /// life of a long-running gateway.
    async fn session_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.session_locks.lock().await;
        locks.entry(key.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
}

/// Kernel space: this is the only piece of the system that knows agent-home
/// exists as a directory on disk and that a wasm binary needs running — the
/// agent itself never sees the gateway (PROJECT.md 4.4).
pub async fn serve(cfg: GatewayConfig) -> anyhow::Result<()> {
    // once-at-startup, not lazily inside `pairing_seed()` — an
    // already-paired instance never calls that again (the pairing-code path
    // only runs while `load_owner` is still `None`), so a lazy migration
    // would silently never fire for exactly the installs that most need it
    crate::discord::migrate_legacy_seed_path(&cfg.agent_home);

    let state = Arc::new(AppState {
        agent_home: cfg.agent_home,
        wasm_path: cfg.wasm_path,
        token: cfg.token,
        session_locks: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        active_runs: std::sync::atomic::AtomicUsize::new(0),
    });

    let api = Router::new()
        .route("/message", post(post_message))
        .route("/upload", post(post_upload))
        .route("/attachment", get(get_attachment))
        .route("/wake", post(post_wake))
        .route("/status", get(get_status))
        .route("/memory/notes", get(get_notes))
        .route("/memory/reports", get(get_reports))
        .route("/config", get(get_config).post(post_config))
        .route("/soul", get(get_soul).post(post_soul))
        .route("/secrets", post(post_secret))
        .route("/logs", get(get_logs_sse))
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
        .route("/scheduler/runs", get(get_scheduled_runs))
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
/// hitting `/api/message` or `/api/wake`. Ticks every 30s and fires two
/// kinds of self-driven wake, neither carrying chat `history` (so each is
/// structurally a fresh session — `agent_loop.rs` only continues a
/// conversation when `trigger.history` is present):
///
/// - `daily_maintenance`, every `MAINTENANCE_INTERVAL_SECS` (6h) since the
///   last one, tracked by `last_maintenance_marker_path` rather than "does
///   today's report exist" — running 4x/day needs its own persisted
///   checkpoint since a bare day-existence check would only ever fire once
///   per calendar day. `since_ts` (the checkpoint being advanced) rides
///   along in the trigger so the agent only reviews what's new since then
///   (`agent_loop.rs`'s `recent_log_entries`), not the whole day.
/// - `cron`, once the agent's own last-requested `sleep_until` has passed
/// - one `scheduled_task` wake per enabled, cron-matching entry under
///   `crate::scheduler_tasks` — user/agent-defined jobs (`data_path` points
///   the woken session at its own instructions), independent of the two
///   built-in wakes above
const MAINTENANCE_INTERVAL_SECS: i64 = 6 * 3600;

fn last_maintenance_marker_path(agent_home: &Path) -> PathBuf {
    agent_home.join("memory/maintenance_reports/.last_run")
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

async fn scheduler_loop(state: Arc<AppState>) {
    let mut last_daily_maintenance_attempt: Option<i64> = None;
    let mut last_handled_sleep_until: Option<i64> = None;
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let now = crate::logs::now_unix_secs();

        let last_maintenance = read_last_maintenance(&state.agent_home);
        if now - last_maintenance >= MAINTENANCE_INTERVAL_SECS && last_daily_maintenance_attempt.is_none_or(|last| now - last >= 900) {
            last_daily_maintenance_attempt = Some(now);
            let outcome = run_scheduled(state.clone(), json!({"type": "daily_maintenance", "since_ts": last_maintenance})).await;
            // only advance the checkpoint on a real completion — `run()`'s
            // one hard-failure path (agent_loop.rs: an `llm_call` error
            // aborts the whole run) still reports a `summary`, just one
            // starting with "run aborted". Advancing on that would silently
            // skip whatever happened in `[last_maintenance, now)` forever:
            // the next run's `since_ts` moves past it with nothing ever
            // having actually reviewed that window (this is exactly what
            // happened during the 2026-07-08 Moonshot API outage — a failed
            // run's checkpoint write ate a 6h window's worth of activity).
            // run_trigger's outcome shape: {"ok":true,"result":{"summary":..},...}
            // on a completed run, {"ok":false,"error":..} only on a
            // panic/spawn failure — a plain llm_call abort still comes back
            // `ok:true` with `result.summary` starting "run aborted"
            let aborted = !outcome.get("ok").and_then(Value::as_bool).unwrap_or(false)
                || outcome.get("result").and_then(|r| r.get("summary")).and_then(Value::as_str).is_some_and(|s| s.starts_with("run aborted"));
            if !aborted {
                write_last_maintenance(&state.agent_home, now);
            }
        }

        let last_run: Option<Value> = std::fs::read_to_string(state.agent_home.join("logs/last_run.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        if let Some(sleep_until) = last_run.and_then(|v| v.get("sleep_until").and_then(Value::as_i64)) {
            if now >= sleep_until && last_handled_sleep_until != Some(sleep_until) {
                last_handled_sleep_until = Some(sleep_until);
                let trigger = json!({"type": "cron", "recent_chat": recent_chat_context(&state.agent_home, DEFAULT_SESSION_KEY)});
                run_scheduled(state.clone(), trigger).await;
            }
        }

        for task in crate::scheduler_tasks::load_tasks(&state.agent_home) {
            if !task.enabled || !crate::cron::matches(&task.cron, now) {
                continue;
            }
            // one fire per matching minute — a tick every 30s would
            // otherwise double-fire any spec that matches for a whole minute
            if task.last_run.is_some_and(|last| last / 60 == now / 60) {
                continue;
            }
            crate::scheduler_tasks::mark_run(&state.agent_home, &task.id, now);
            run_scheduled(state.clone(), json!({"type": "scheduled_task", "task_id": task.id, "data_path": task.data_path})).await;
        }
    }
}

/// Runs a self-driven trigger (as opposed to a human hitting `/api/message`/
/// `/api/wake`) and, since nothing else logs these, records `{trigger,
/// outcome}` to `logs/scheduled_runs/<ts>-<type>.json` — "要有 session 紀錄"
/// for scheduler-driven wakes, browsable via `GET /api/scheduler/runs`.
async fn run_scheduled(state: Arc<AppState>, trigger: Value) -> Value {
    let ts = crate::logs::now_unix_secs();
    let trigger_type = trigger.get("type").and_then(|t| t.as_str()).unwrap_or("cron").to_string();
    let outcome = run_trigger(state.clone(), trigger.clone()).await;
    let dir = state.agent_home.join("logs/scheduled_runs");
    if std::fs::create_dir_all(&dir).is_ok() {
        let record = json!({"ts": ts, "trigger": trigger, "outcome": outcome});
        // filename keyed by nanos, not `ts` (whole seconds) — background
        // triggers aren't serialized by any lock (only message/
        // compact_session are, see `AppState::session_locks`), so two of
        // them starting in the same scheduler_loop tick can share the same
        // *second*; `get_scheduled_runs` sorts by the `ts` field inside the
        // file content, not the filename, so nanosecond precision here only
        // has to be unique, not human-readable
        let file_name = format!("{}-{trigger_type}.json", crate::logs::now_unix_nanos());
        let _ = std::fs::write(dir.join(file_name), serde_json::to_vec_pretty(&record).unwrap_or_default());
    }
    outcome
}

async fn get_scheduled_runs(State(state): State<Arc<AppState>>) -> Json<Value> {
    let dir = state.agent_home.join("logs/scheduled_runs");
    let mut runs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    runs.push(v);
                }
            }
        }
    }
    runs.sort_by(|a, b| b["ts"].as_i64().cmp(&a["ts"].as_i64())); // newest first
    Json(json!({"runs": runs}))
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
    /// `/workspace/tasks/x.md`), same root as the wasm preopen
    path: String,
}

/// `data_path` is guest-absolute; the guest's preopen root is exactly
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
pub(crate) async fn handle_chat_message(state: Arc<AppState>, session_key: String, text: String, attachments: Vec<String>, channel: Option<String>) -> Value {
    let mut session = load_session(&state.agent_home, &session_key);
    // an empty text block inside a multimodal content array (attachments
    // with no caption) trips the same "must not be empty" rejection the
    // all-empty-reply guard further down exists for — give it a placeholder
    // instead of ever sending "" as a block's text
    let display_text = if text.trim().is_empty() && !attachments.is_empty() { "(附件)".to_string() } else { text.clone() };
    session.push(SessionTurn { role: "user".to_string(), content: display_text, attachments, ts: crate::logs::now_unix_secs() });
    let supports_vision = Config::load(&state.agent_home).map(|c| c.llm.supports_vision).unwrap_or(false);
    let history: Vec<Value> = session.iter().map(|t| turn_to_message(&state.agent_home, supports_vision, t)).collect();

    let mut trigger = json!({"type": "message", "text": text, "history": history, "session_key": session_key});
    if let Some(c) = channel {
        trigger["channel"] = Value::String(c);
    }
    let outcome = run_trigger(state.clone(), trigger).await;

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
    session.push(SessionTurn { role: "assistant".to_string(), content: reply, attachments: Vec::new(), ts: crate::logs::now_unix_secs() });
    let _ = save_session(&state.agent_home, &session_key, &session);

    maybe_auto_compact(&state, &session_key);

    outcome
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
    let handle = tokio::spawn(handle_chat_message(state, DEFAULT_SESSION_KEY.to_string(), body.text, body.attachments, None));
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

/// Runs one trigger in-process (`spawn_blocking`, since wasmtime + reqwest's
/// blocking client both panic if driven directly on a tokio runtime
/// thread). Locking is per-session, for any trigger that actually touches a
/// session — `message` (a live turn) and `compact_session` (reads +
/// rewrites the same `session.json`, see `compact_session_key`) — see
/// `AppState::session_locks`'s doc comment for why everything else runs
/// fully concurrently with no lock at all.
async fn run_trigger(state: Arc<AppState>, trigger: Value) -> Value {
    let trigger_type = trigger.get("type").and_then(|t| t.as_str());
    let touches_session = matches!(trigger_type, Some("message") | Some("compact_session"));
    let session_key = trigger.get("session_key").and_then(|s| s.as_str()).unwrap_or(DEFAULT_SESSION_KEY).to_string();

    let _session_guard = if touches_session { Some(state.session_lock(&session_key).await.lock_owned().await) } else { None };
    state.active_runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let outcome = run_trigger_inner(&state, trigger).await;
    state.active_runs.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    outcome
}

async fn run_trigger_inner(state: &Arc<AppState>, trigger: Value) -> Value {
    let agent_home = state.agent_home.clone();
    let wasm_path = state.wasm_path.clone();
    let trigger_str = trigger.to_string();

    let outcome = tokio::task::spawn_blocking(move || {
        let agent_home = agent_home.to_string_lossy().into_owned();
        let wasm_path = wasm_path.to_string_lossy().into_owned();
        crate::run_agent(&agent_home, &wasm_path, &["run", &trigger_str])
    })
    .await;

    match outcome {
        Ok(Ok(outcome)) => {
            // `outcome.trapped` means the guest never reached its own
            // `RESULT:` line (almost always the epoch-timeout trap mid-turn
            // — see `run_agent_with_epoch_timeout`'s doc comment) — without
            // this, `result` stays `null` and every caller (the webui's
            // Schedule history panel included) has no way to tell "it
            // failed silently" apart from "it genuinely had nothing to
            // say", both of which otherwise render as the same unhelpful
            // "(no summary)".
            let result = if outcome.trapped {
                json!({"summary": "(run timed out — trapped mid-turn with no result, see notifications log)"})
            } else {
                extract_result(&outcome.stdout)
            };
            let _ = persist_last_run(&state.agent_home, outcome.sleep_until, &result);
            json!({"ok": true, "result": result, "sleep_until": outcome.sleep_until, "stdout": outcome.stdout})
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

fn persist_last_run(agent_home: &Path, sleep_until: Option<i64>, result: &Value) -> anyhow::Result<()> {
    let path = agent_home.join("logs/last_run.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = json!({"ts": crate::logs::now_unix_secs(), "sleep_until": sleep_until, "result": result});
    std::fs::write(path, serde_json::to_vec_pretty(&payload)?)?;
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

/// one report per maintenance run (every 6h, not once/day — see
/// `scheduler_loop`) — `date` is the filename stem, e.g. `"2026-07-05_1830"`.
/// `.last_run` (the checkpoint marker, no `.md` extension) is filtered out
/// by the extension check below, same directory or not.
async fn get_reports(State(state): State<Arc<AppState>>) -> Json<Value> {
    let dir = state.agent_home.join("memory/maintenance_reports");
    let mut reports = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(date) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            if let Ok(content) = std::fs::read_to_string(&path) {
                reports.push(json!({"date": date, "content": content}));
            }
        }
    }
    reports.sort_by(|a, b| b["date"].as_str().cmp(&a["date"].as_str())); // newest first
    Json(json!({"reports": reports}))
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

// ---- secrets (write-only — no endpoint ever returns a value) ----

#[derive(Deserialize)]
struct SetSecretBody {
    name: String,
    value: String,
}

/// Sets one secret in the vault. Response echoes back only the *names*
/// present in the vault, never values, as confirmation — there is no GET
/// endpoint for secrets at all.
async fn post_secret(State(state): State<Arc<AppState>>, Json(body): Json<SetSecretBody>) -> impl IntoResponse {
    let path = crate::secrets_path(&state.agent_home);
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
}

/// Builds the OpenAI/Ollama-style `{role, content}` message `agent_loop.rs`
/// clones straight into its `messages` array (PROJECT.md: the guest treats
/// `content` as an opaque JSON value, string or block-array alike, so
/// nothing on the guest side needs to know attachments exist).
///
/// No attachments → plain string content, unchanged from before. With
/// attachments: an image gets embedded as a base64 `image_url` block only
/// if `supports_vision` is on (config.toml `[llm] supports_vision` — no
/// reliable way to detect this automatically for an arbitrary
/// OpenAI-compatible endpoint) *and* the file's extension maps to a known
/// image type; anything else (vision off, non-image file, unreadable file)
/// just gets named in the text so the agent can `read_file`/`list_dir` it
/// itself instead of the model silently never learning it exists.
fn turn_to_message(agent_home: &Path, supports_vision: bool, turn: &SessionTurn) -> Value {
    if turn.attachments.is_empty() {
        return json!({"role": turn.role, "content": turn.content});
    }

    let mut blocks = vec![json!({"type": "text", "text": turn.content})];
    let mut notes = Vec::new();
    for rel_path in &turn.attachments {
        let mime = mime_for_path(rel_path);
        let embedded = supports_vision
            && mime.starts_with("image/")
            && std::fs::read(agent_home.join(rel_path)).ok().map(|bytes| {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                blocks.push(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{b64}")}}));
            }).is_some();
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

const RECENT_CHAT_TURNS: usize = 6;
const RECENT_CHAT_MAX_CHARS: usize = 500;

/// Last `RECENT_CHAT_TURNS` turns from the given session, each capped to
/// `RECENT_CHAT_MAX_CHARS` — fixed-size regardless of how long the real
/// conversation grows, unlike full `history` (which only a `message`
/// trigger gets). Gives a `cron` wake enough "what were we just talking
/// about" for `chat_send` not to feel completely blind, without the prompt
/// growing forever as the chat session grows. `cron` is a single global
/// wake (not per-Discord-channel), so it always reads the `"webui"` one.
fn recent_chat_context(agent_home: &Path, key: &str) -> Vec<Value> {
    let turns = load_session(agent_home, key);
    let start = turns.len().saturating_sub(RECENT_CHAT_TURNS);
    turns[start..].iter().map(|t| json!({"role": t.role, "content": truncate_chars(&t.content, RECENT_CHAT_MAX_CHARS)})).collect()
}

/// Truncates by *char* count, not bytes — `String::truncate` panics/
/// corrupts on a non-char-boundary byte offset, which CJK text (this
/// project's primary chat language) hits constantly.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
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
/// `cron`/scheduled-task run elsewhere (or a session compact's own
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
/// `run_trigger` at all (pure filesystem, no wasmtime run needed), but
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

const SKILLS_DIR: &str = "memory/skills";

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
    let existing = std::fs::read_to_string(&path).ok().and_then(|t| parse_skill(&t));
    let created_at = existing.as_ref().and_then(|s| s["created_at"].as_u64()).unwrap_or_else(|| crate::logs::now_unix_secs() as u64);
    let used_count = existing.as_ref().and_then(|s| s["used_count"].as_u64()).unwrap_or(0);
    let last_used_line = existing
        .as_ref()
        .and_then(|s| s["last_used"].as_u64())
        .map(|t| format!("last_used: {t}\n"))
        .unwrap_or_default();
    // same trim as agent/src/skills.rs `render` — otherwise saving the same
    // skill repeatedly grows a longer run of trailing blank lines each time
    let body = skill.body.trim_end_matches('\n');
    let content = format!(
        "---\nname: {}\ndescription: {}\ncreated_at: {created_at}\nused_count: {used_count}\n{last_used_line}---\n{body}\n",
        skill.name, skill.description
    );
    match std::fs::write(path, content) {
        Ok(()) => (StatusCode::OK, "saved".to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn delete_skill(State(state): State<Arc<AppState>>, AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    let path = state.agent_home.join(SKILLS_DIR).join(format!("{name}.md"));
    match std::fs::remove_file(path) {
        Ok(()) => (StatusCode::OK, "deleted".to_string()),
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
    for line in front.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            match key.trim() {
                "name" => name = Some(value.to_string()),
                "description" => description = value.to_string(),
                "created_at" => created_at = value.parse().unwrap_or(0),
                "used_count" => used_count = value.parse().unwrap_or(0),
                "last_used" => last_used = value.parse().ok(),
                _ => {}
            }
        }
    }
    Some(json!({
        "name": name?, "description": description, "body": body,
        "created_at": created_at, "used_count": used_count, "last_used": last_used,
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
async fn post_abort(State(state): State<Arc<AppState>>) -> Json<Value> {
    let path = state.agent_home.join("logs/abort_requested");
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
/// panel itself ever watches. Without this, a Discord/cron run in the
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
