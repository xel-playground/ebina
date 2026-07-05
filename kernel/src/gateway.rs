use crate::config::Config;
use crate::secrets::Secrets;
use axum::extract::{Path as AxumPath, Request, State};
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

struct AppState {
    agent_home: PathBuf,
    wasm_path: PathBuf,
    token: String,
}

/// Kernel space: this is the only piece of the system that knows agent-home
/// exists as a directory on disk and that a wasm binary needs running — the
/// agent itself never sees the gateway (PROJECT.md 4.4).
pub async fn serve(cfg: GatewayConfig) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        agent_home: cfg.agent_home,
        wasm_path: cfg.wasm_path,
        token: cfg.token,
    });

    let api = Router::new()
        .route("/message", post(post_message))
        .route("/wake", post(post_wake))
        .route("/status", get(get_status))
        .route("/memory/notes", get(get_notes))
        .route("/memory/reports", get(get_reports))
        .route("/config", get(get_config).post(post_config))
        .route("/secrets", post(post_secret))
        .route("/logs", get(get_logs_sse))
        .route("/thinking", get(get_thinking_sse))
        .route("/abort", post(post_abort))
        .route("/session", get(get_session))
        .route("/session/reset", post(post_session_reset))
        .route("/session/compact", post(post_session_compact))
        .route("/grants", get(get_grants))
        .route("/grants/{id}/approve", post(post_grant_approve))
        .route("/grants/{id}/deny", post(post_grant_deny))
        .route("/egress", get(get_egress))
        .route("/skills", get(get_skills).post(post_skill))
        .route("/skills/{name}", axum::routing::delete(delete_skill))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone());

    let app = Router::new().nest("/api", api);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    println!("[gateway] listening on http://0.0.0.0:{}", cfg.port);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn auth(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let header_ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {}", state.token));
    // browsers' native EventSource can't set headers, so /api/logs also
    // accepts the token as a query param — the only endpoint that does
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
}

/// Chat turns are host-tracked (not just an in-browser illusion of memory):
/// each `/api/message` loads the running session, appends the user's turn,
/// hands the *whole* history to the guest as `trigger.history` so it's real
/// conversational context (not just RAG over `memory/notes/`), then appends
/// the agent's reply and saves.
async fn post_message(State(state): State<Arc<AppState>>, Json(body): Json<MessageBody>) -> Json<Value> {
    let mut session = load_session(&state.agent_home);
    session.push(SessionTurn { role: "user".to_string(), content: body.text.clone(), ts: crate::logs::now_unix_secs() });
    let history: Vec<Value> = session.iter().map(SessionTurn::as_message).collect();

    let outcome = run_trigger(state.clone(), json!({"type": "message", "text": body.text, "history": history})).await;

    let reply = outcome.get("result").and_then(|r| r.get("summary")).and_then(|s| s.as_str()).unwrap_or("").to_string();
    session.push(SessionTurn { role: "assistant".to_string(), content: reply, ts: crate::logs::now_unix_secs() });
    let _ = save_session(&state.agent_home, &session);

    Json(outcome)
}

/// dev/debug: fire an arbitrary trigger JSON immediately (PROJECT.md 4.4
/// `POST /api/wake` — "手動立即喚醒,開發調試常用")
async fn post_wake(State(state): State<Arc<AppState>>, Json(trigger): Json<Value>) -> Json<Value> {
    Json(run_trigger(state, trigger).await)
}

async fn run_trigger(state: Arc<AppState>, trigger: Value) -> Value {
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
            let result = extract_result(&outcome.stdout);
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
    Json(json!({
        "budget": read_json(state.agent_home.join("logs/budget-state.json")),
        "last_run": read_json(state.agent_home.join("logs/last_run.json")),
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

/// one report per day, like `memory/notes/<date>/log.md` — `date` is the
/// filename stem, e.g. `"2026-07-05"`
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

#[derive(Debug, Serialize, Deserialize, Clone)]
struct SessionTurn {
    role: String,
    content: String,
    ts: i64,
}

impl SessionTurn {
    fn as_message(&self) -> Value {
        json!({"role": self.role, "content": self.content})
    }
}

fn session_path(agent_home: &Path) -> PathBuf {
    agent_home.join("logs/session.json")
}

fn load_session(agent_home: &Path) -> Vec<SessionTurn> {
    std::fs::read_to_string(session_path(agent_home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_session(agent_home: &Path, turns: &[SessionTurn]) -> anyhow::Result<()> {
    let path = session_path(agent_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(turns)?)?;
    Ok(())
}

/// Moves the current session to `logs/sessions/<ts>.json` — "留一個 session
/// 紀錄": reset/compact never just discard history, they archive it first.
fn archive_session(agent_home: &Path) -> anyhow::Result<Option<PathBuf>> {
    let turns = load_session(agent_home);
    if turns.is_empty() {
        return Ok(None);
    }
    let dir = agent_home.join("logs/sessions");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", crate::logs::now_unix_secs()));
    std::fs::write(&path, serde_json::to_vec_pretty(&turns)?)?;
    Ok(Some(path))
}

async fn get_session(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({"turns": load_session(&state.agent_home)}))
}

/// Archives, then clears, the current session — a fresh conversation.
async fn post_session_reset(State(state): State<Arc<AppState>>) -> Json<Value> {
    match archive_session(&state.agent_home) {
        Ok(archived) => {
            let _ = save_session(&state.agent_home, &[]);
            Json(json!({"ok": true, "archived": archived.map(|p| p.display().to_string())}))
        }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

/// Archives the full session, then asks the agent to collapse it into one
/// short summary turn — same idea as Claude Code's `/compact`: keep context
/// going without the message list growing forever.
async fn post_session_compact(State(state): State<Arc<AppState>>) -> Json<Value> {
    let session = load_session(&state.agent_home);
    if session.is_empty() {
        return Json(json!({"ok": true, "message": "nothing to compact"}));
    }
    let _ = archive_session(&state.agent_home);

    let history: Vec<Value> = session.iter().map(SessionTurn::as_message).collect();
    let outcome = run_trigger(state.clone(), json!({"type": "compact_session", "history": history})).await;
    let summary = outcome.get("result").and_then(|r| r.get("summary")).and_then(|s| s.as_str()).unwrap_or("").to_string();

    let compacted = vec![SessionTurn {
        role: "system".to_string(),
        content: format!("(earlier conversation, compacted) {summary}"),
        ts: crate::logs::now_unix_secs(),
    }];
    let _ = save_session(&state.agent_home, &compacted);
    Json(json!({"ok": true, "summary": summary}))
}

// ---- grants (tofu new-domain / http_fetch writes queued for approval) ----

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
    let content = format!("---\nname: {}\ndescription: {}\n---\n{}\n", skill.name, skill.description, skill.body);
    match std::fs::write(dir.join(format!("{}.md", skill.name)), content) {
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
    for line in front.lines() {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => name = Some(value.trim().to_string()),
                "description" => description = value.trim().to_string(),
                _ => {}
            }
        }
    }
    Some(json!({"name": name?, "description": description, "body": body}))
}

// ---- live "thinking" stream + abort ----

/// Sets a flag `llm_call` checks between streamed chunks — cuts a
/// runaway/unproductive generation short instead of paying for tokens the
/// agent doesn't need (kernel/src/syscalls/llm_call.rs `handle_ollama_stream`).
async fn post_abort(State(state): State<Arc<AppState>>) -> Json<Value> {
    let path = state.agent_home.join("logs/abort_requested");
    match std::fs::write(&path, "") {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

/// Unlike `/api/logs` (which tails *new lines*), this re-sends the *whole*
/// current file each time it changes — `thinking-live.txt` is one growing
/// blob per call, not a line-oriented log.
async fn get_thinking_sse(State(state): State<Arc<AppState>>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(thinking_stream(state.agent_home.join("logs/thinking-live.txt")))
        .keep_alive(axum::response::sse::KeepAlive::default())
}

fn thinking_stream(path: PathBuf) -> impl Stream<Item = Result<Event, Infallible>> {
    let last = std::fs::read_to_string(&path).unwrap_or_default();
    stream::unfold((path, last), |(path, last)| async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text != last {
                return Some((Ok(Event::default().data(text.clone())), (path, text)));
            }
        }
    })
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
