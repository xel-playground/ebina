use crate::abi::{error_json, ok_json};
use crate::filelock::FileLock;
use crate::logs::{append_jsonl, now_unix_nanos, now_unix_secs};
use crate::state::AgentState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_SEND_ATTEMPTS: u32 = 5;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

/// After a request comes back with a status code at all (as opposed to
/// failing to connect), the underlying TCP/TLS work is done — parsing a
/// bad-status body costs nothing extra, so it's read here regardless of
/// whether this turns out to be the final attempt.
enum SendResult {
    /// any successful status, any provider — every provider streams now,
    /// so this is handed back unread for the caller to dispatch to the
    /// right per-provider stream parser (`handle_ollama_stream`/
    /// `handle_openai_stream`/`handle_anthropic_stream`).
    Stream(reqwest::blocking::Response),
    /// the last attempt's failure (a non-success status) — retried
    /// attempts don't reach here, only the one that ran out of attempts.
    Body { status: reqwest::StatusCode, json: Value },
    /// every attempt failed before a status code ever came back (DNS/
    /// connect/timeout) — nothing to parse.
    ConnectFailed(String),
}

/// Retries *any* failure now — a connect/timeout error, and also an HTTP
/// error status (4xx/5xx). Retrying a bad status risks resending a request
/// the provider may have partially processed/billed; accepted deliberately
/// (a stuck agent run is worse than an occasional duplicate generation) —
/// see PROJECT.md's `llm_call` retry note.
///
/// Exponential backoff (500ms, 1s, 2s, 4s between the 5 attempts, ~7.5s
/// worst-case added latency) — long enough to ride out more than a single
/// blip, short enough that one call doesn't hang the whole `run_lock`-
/// serialized agent for minutes over a dead API.
fn send_with_retries(request: reqwest::blocking::RequestBuilder) -> SendResult {
    let mut last_connect_err: Option<String> = None;
    for attempt in 0..MAX_SEND_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt - 1));
        }
        let cloned = request.try_clone().expect("llm_call bodies are always in-memory JSON, always clonable");
        let response = match cloned.send() {
            Ok(r) => r,
            Err(e) => {
                last_connect_err = Some(e.to_string());
                continue;
            }
        };
        let status = response.status();
        if status.is_success() {
            return SendResult::Stream(response);
        }
        let json: Value = response.json().unwrap_or(Value::Null);
        if attempt + 1 == MAX_SEND_ATTEMPTS {
            return SendResult::Body { status, json };
        }
        // bad status, attempts remain — loop again
    }
    SendResult::ConnectFailed(last_connect_err.unwrap_or_else(|| "connection failed".to_string()))
}

const CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
const CIRCUIT_COOLDOWN_SECS: i64 = 60;

/// Persisted (not in `AgentState` — a fresh one of those exists per run, see
/// PROJECT.md's "fresh instantiate" design, but a dead API spans many runs)
/// count of *fully-exhausted* `llm_call`s in a row — each one already
/// retried up to `MAX_SEND_ATTEMPTS` times internally, so this is a second,
/// slower-moving tier: once several whole calls in a row have burned all
/// their retries, the API is very likely actually down, not just glitchy,
/// so stop spending attempts/latency on it for a cooldown window instead of
/// retrying full-strength on every single subsequent call too.
#[derive(Debug, Serialize, Deserialize, Default)]
struct CircuitState {
    consecutive_failures: u32,
    /// 0 means not tripped
    tripped_until: i64,
}

fn circuit_path(agent_home: &Path) -> PathBuf {
    agent_home.join("logs/llm_circuit_breaker.json")
}

fn load_circuit(agent_home: &Path) -> CircuitState {
    std::fs::read_to_string(circuit_path(agent_home)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_circuit(agent_home: &Path, state: &CircuitState) {
    let path = circuit_path(agent_home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_string(state).unwrap_or_default());
}

/// Records one fully-failed call (all `MAX_SEND_ATTEMPTS` exhausted) and
/// trips the breaker once `CIRCUIT_FAILURE_THRESHOLD` such calls happen in a
/// row.
///
/// Locked (`llm_circuit_breaker.json.lock`) — this is one global file (no
/// session key, unlike `budget`/`chat_sessions`), and every concurrent
/// `llm_call` across every session/trigger hits it. Without a lock, two
/// calls finishing at nearly the same moment (routine now that background
/// triggers run fully concurrently) do a lost-update load-mutate-save race:
/// one's increment silently vanishes, undercounting consecutive failures
/// and delaying the trip.
fn record_circuit_failure(agent_home: &Path) {
    let _lock = match FileLock::acquire(circuit_path(agent_home).with_extension("json.lock"), Duration::from_secs(5)) {
        Ok(lock) => lock,
        // low-stakes bookkeeping, nobody's blocked on this — skip this
        // update rather than hold up the llm_call that's already failed
        Err(e) => {
            let _ = crate::logs::notify(agent_home, &format!("circuit breaker failed to lock: {e}"));
            return;
        }
    };
    let mut circuit = load_circuit(agent_home);
    circuit.consecutive_failures += 1;
    if circuit.consecutive_failures >= CIRCUIT_FAILURE_THRESHOLD {
        circuit.tripped_until = now_unix_secs() + CIRCUIT_COOLDOWN_SECS;
        let _ = crate::logs::notify(
            agent_home,
            &format!("llm_call circuit breaker tripped after {} consecutive failures — refusing new calls for {CIRCUIT_COOLDOWN_SECS}s", circuit.consecutive_failures),
        );
    }
    save_circuit(agent_home, &circuit);
}

fn record_circuit_success(agent_home: &Path) {
    let _lock = match FileLock::acquire(circuit_path(agent_home).with_extension("json.lock"), Duration::from_secs(5)) {
        Ok(lock) => lock,
        Err(e) => {
            let _ = crate::logs::notify(agent_home, &format!("circuit breaker failed to lock: {e}"));
            return;
        }
    };
    let circuit = load_circuit(agent_home);
    if circuit.consecutive_failures > 0 || circuit.tripped_until != 0 {
        save_circuit(agent_home, &CircuitState::default());
    }
}

/// `llm_call({messages: [{role, content}], ...}) -> {text, usage}` — host
/// holds the API key and normalizes provider-specific request/response
/// shapes so the guest only ever deals in a plain OpenAI/Ollama-style
/// `messages` array (with an optional leading `role: "system"` message) and
/// gets back a flat `{text, usage: {input_tokens, output_tokens}}`.
///
/// Enforces the daily token budget and per-minute rate limit before ever
/// touching the network, then logs full (raw, provider-shaped) prompt/
/// response + token usage.
///
/// Every provider streams now — so reasoning/"thinking" text (where the
/// provider sends any: ollama's `thinking`, an OpenAI-compatible
/// provider's `reasoning_content` delta, Anthropic's `thinking_delta`) can
/// be tailed live by the gateway (`GET /api/thinking`) while the guest is
/// still blocked waiting on this call, and so an operator can abort
/// mid-generation (`POST /api/abort`) instead of burning through the whole
/// response — regardless of which provider is configured.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    if !state.budget.has_headroom() {
        let _ = crate::logs::notify(
            &state.agent_home,
            "llm_call rejected: daily token budget exhausted",
        );
        return error_json("budget_exceeded", "daily token budget exhausted");
    }

    if let Err(limited) = crate::ratelimit::global(&state.config.ratelimit).acquire_llm() {
        if limited.sustained {
            let _ = crate::logs::notify(
                &state.agent_home,
                "llm_call sustained rate-limit hits — possible runaway loop",
            );
        }
        return serde_json::json!({
            "ok": false,
            "error": {
                "code": "rate_limited",
                "message": "llm_call rate limit exceeded",
                "retry_after": limited.retry_after_secs,
            }
        });
    }

    let circuit = load_circuit(&state.agent_home);
    if circuit.tripped_until > now_unix_secs() {
        return error_json(
            "circuit_open",
            &format!("llm_call circuit breaker is open (retry after unix ts {}) — the API has been failing repeatedly", circuit.tripped_until),
        );
    }

    let api_key = match crate::secrets::resolve_placeholder(&state.secrets, &state.config.llm.api_key) {
        Ok(k) => k,
        Err(e) => return error_json("no_api_key", &e),
    };

    // `_meta` (session_key/channel — set by agent_loop.rs from the trigger
    // it's currently handling) is logging-only, never sent to the provider;
    // pulled out before `body` becomes the actual outgoing request.
    let source = req.get("_meta").cloned().unwrap_or(Value::Null);

    let provider = state.config.llm.provider.clone();
    let mut body = req.clone();
    if let Value::Object(ref mut map) = body {
        map.remove("_meta");
        map.entry("model").or_insert_with(|| Value::String(state.config.llm.model.clone()));
        map.insert("stream".to_string(), Value::Bool(true));
        if provider == "openai" {
            // without this, an OpenAI-compatible stream's chunks carry no
            // usage at all — most such APIs (this one included) only add a
            // final usage-only chunk when this is explicitly requested
            map.insert("stream_options".to_string(), serde_json::json!({"include_usage": true}));
        }
        // see `RuntimeConfig::max_output_tokens` — bounds a runaway
        // generation regardless of provider; ollama's native API doesn't
        // recognize `max_tokens` at all (`options.num_predict` instead), so
        // it needs its own field
        if provider == "ollama" {
            map.entry("options").or_insert_with(|| serde_json::json!({}));
            if let Some(options) = map.get_mut("options").and_then(|o| o.as_object_mut()) {
                options.entry("num_predict").or_insert_with(|| Value::from(state.config.runtime.max_output_tokens));
            }
        } else {
            map.entry("max_tokens").or_insert_with(|| Value::from(state.config.runtime.max_output_tokens));
        }
    }
    if provider == "anthropic" {
        normalize_for_anthropic(&mut body);
    } else {
        collapse_content_blocks(&mut body);
    }

    let client = reqwest::blocking::Client::new();
    let mut request = client.post(&state.config.llm.base_url).header("content-type", "application/json");
    request = match provider.as_str() {
        "ollama" | "openai" => request.bearer_auth(&api_key),
        _ => request.header("x-api-key", &api_key).header("anthropic-version", "2023-06-01"),
    };
    match send_with_retries(request.json(&body)) {
        SendResult::Stream(response) => {
            record_circuit_success(&state.agent_home);
            match provider.as_str() {
                "ollama" => handle_ollama_stream(state, body, response, &source),
                "openai" => handle_openai_stream(state, body, response, &source),
                _ => handle_anthropic_stream(state, body, response, &source),
            }
        }
        SendResult::Body { status, json } => {
            write_transcript(state, &body, &json, &source);
            record_circuit_failure(&state.agent_home);
            error_json("llm_error", &format!("HTTP {status}: {json}"))
        }
        SendResult::ConnectFailed(e) => {
            record_circuit_failure(&state.agent_home);
            error_json("network_error", &e)
        }
    }
}

/// Keyed by session, same as `chat_sessions/` itself — otherwise a
/// Discord/cron/daily_maintenance run in the background would overwrite
/// whatever a human is live-watching in the webui Chat panel (and vice
/// versa), since `/api/thinking`'s SSE just tails whatever's at this path.
/// `source.session_key` is only present for a `message` trigger
/// (`agent_loop.rs` `source_meta`); anything else (cron/daily_maintenance/
/// scheduled_task/compact_session) has no live audience anyway, so those
/// all share one `_system` bucket rather than each needing their own.
fn thinking_path(agent_home: &Path, source: &Value) -> PathBuf {
    let key = source.get("session_key").and_then(|s| s.as_str()).unwrap_or("_system");
    agent_home.join("logs/chat_sessions").join(key).join("thinking-live.txt")
}

/// Keyed the same way as `thinking_path` — used to be one process-global
/// `logs/abort_requested` file, which was correct back when only one run
/// could ever be in flight at a time. Now that runs are only serialized
/// per-session (`AppState::session_locks`), a global flag meant `/api/abort`
/// could stop an unrelated concurrent run instead of the intended one, and
/// worse: each call here clears the flag at the *start* of its own stream
/// (to drop a stale flag from a previous, already-finished call on this
/// same path), so a concurrent unrelated run starting its own `llm_call`
/// could silently eat another session's still-pending abort request before
/// that session's stream ever got to check it.
fn abort_flag_path(agent_home: &Path, source: &Value) -> PathBuf {
    let key = source.get("session_key").and_then(|s| s.as_str()).unwrap_or("_system");
    agent_home.join("logs/chat_sessions").join(key).join("abort_requested")
}

/// Cheap, provider-agnostic guard against a model looping on the same text
/// forever — a real autoregressive-decoding failure mode caught live: a
/// short, low-content trigger ("測試測試") sent kimi-k2.6's `reasoning_content`
/// into thousands of tokens of a templated garbage script, never converging
/// on a real action. `RuntimeConfig::max_output_tokens` bounds the worst
/// case, but that still means burning tens of seconds and a five-figure
/// token bill before the provider's own limit kicks in — this catches it
/// far earlier.
///
/// Works purely on bytes, never re-slicing into `&str` — this stream is
/// mostly Chinese, and a naive `&buf[a..b]` on a `String` panics the moment
/// a boundary lands mid-character. Every `NEEDLE_LEN` bytes of growth, takes
/// the most recent `NEEDLE_LEN` bytes and counts how many times that exact
/// run appears within the last `SEARCH_WINDOW` bytes (bounded, not the
/// whole response — a real *loop* repeats close together, not once near the
/// start and once now); `THRESHOLD` exact repeats of the same `NEEDLE_LEN`
/// bytes essentially never happens in legitimate prose/code, so this has no
/// realistic false-positive path. An earlier version chunked into
/// *non-overlapping* fixed windows instead of searching — cheaper, but
/// wrong: whether a repeated unit's own length happens to stay in phase
/// with the window boundary is pure luck, and a unit whose length doesn't
/// evenly divide the window size can repeat indefinitely without any two
/// windows ever landing on the same bytes (caught by this file's own
/// `ignores_varying_template`-style tests before this ever shipped).
struct RepeatGuard {
    buf: Vec<u8>,
}

impl RepeatGuard {
    // real observed incident's fixed (non-varying) template runs were only
    // ~44-45 bytes long — a needle any longer than that would straddle into
    // the part that changes each iteration and never match verbatim twice
    const NEEDLE_LEN: usize = 32;
    const SEARCH_WINDOW: usize = 4_000;
    const THRESHOLD: usize = 6;
    // candidate needle end-positions are sampled every `STRIDE` bytes
    // across whatever's newly arrived, not just at the very end of the
    // chunk — a chunk boundary lining up with the *end* of one loop
    // iteration (as real streaming deltas often do, one iteration per
    // chunk) would otherwise mean the trailing needle always straddles
    // into that iteration's varying part and never repeats verbatim, even
    // though a fixed run earlier in that same chunk plainly did
    const STRIDE: usize = 8;

    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed newly-arrived text in; returns true the moment some `NEEDLE_LEN`
    /// window ending within the newly-arrived text has recurred
    /// `THRESHOLD` times nearby — caller should stop consuming the stream
    /// immediately rather than let it keep running.
    fn push(&mut self, chunk: &str) -> bool {
        let old_len = self.buf.len();
        self.buf.extend_from_slice(chunk.as_bytes());
        let new_len = self.buf.len();
        if new_len < Self::NEEDLE_LEN * Self::THRESHOLD {
            return false;
        }
        let mut pos = old_len.max(Self::NEEDLE_LEN);
        while pos <= new_len {
            let needle = &self.buf[pos - Self::NEEDLE_LEN..pos];
            let search_from = pos.saturating_sub(Self::SEARCH_WINDOW);
            let haystack = &self.buf[search_from..new_len];
            if haystack.windows(Self::NEEDLE_LEN).filter(|w| *w == needle).count() >= Self::THRESHOLD {
                return true;
            }
            pos += Self::STRIDE;
        }
        false
    }
}

/// Reads Ollama's NDJSON stream (one `{"message":{"content","thinking"},...}`
/// object per line, deltas rather than cumulative text) line by line —
/// `reqwest::blocking::Response` implements `Read`, so this is plain
/// synchronous I/O, no async runtime needed. Appends `thinking` deltas to
/// the same live-progress file `agent_loop.rs` writes its per-turn action
/// trace to (cleared once per *run*, not per call — this is one `llm_call`
/// among possibly several turns in that run, so clearing here would erase
/// earlier turns' trace lines), and checks the abort flag between lines so
/// an operator can cut a runaway generation short.
fn handle_ollama_stream(state: &mut AgentState, body: Value, response: reqwest::blocking::Response, source: &Value) -> Value {
    let think_path = thinking_path(&state.agent_home, source);
    let _ = std::fs::create_dir_all(think_path.parent().unwrap());

    let abort_path = abort_flag_path(&state.agent_home, source);
    // clear out anything left over from a previous, already-finished call
    // before it can be mistaken for a request to abort *this* one
    let _ = std::fs::remove_file(&abort_path);

    let mut full_content = String::new();
    let mut full_thinking = String::new();
    let mut last_message = Value::Null;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut aborted = false;
    let mut content_guard = RepeatGuard::new();
    let mut thinking_guard = RepeatGuard::new();
    let mut stuck = false;

    for line in BufReader::new(response).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if let Some(msg) = chunk.get("message") {
            last_message = msg.clone();
            if let Some(c) = msg.get("content").and_then(|v| v.as_str()) {
                full_content.push_str(c);
                stuck |= content_guard.push(c);
            }
            if let Some(t) = msg.get("thinking").and_then(|v| v.as_str()) {
                full_thinking.push_str(t);
                stuck |= thinking_guard.push(t);
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&think_path) {
                    use std::io::Write as _;
                    let _ = f.write_all(t.as_bytes());
                }
            }
        }
        if chunk.get("done").and_then(|v| v.as_bool()) == Some(true) {
            input_tokens = chunk.get("prompt_eval_count").and_then(|v| v.as_u64()).unwrap_or(0);
            output_tokens = chunk.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0);
        }

        if abort_path.exists() {
            let _ = std::fs::remove_file(&abort_path);
            aborted = true;
            break;
        }
        if stuck {
            let _ = crate::logs::notify(&state.agent_home, "llm_call cut short: model appears stuck repeating itself");
            break;
        }
    }

    let text = if !full_content.is_empty() {
        full_content
    } else {
        // same harmony-template tool_calls fallback as the non-streaming path
        last_message
            .get("tool_calls")
            .and_then(|tc| tc.as_array())
            .and_then(|arr| arr.first())
            .and_then(|call| call.get("function"))
            .and_then(|f| f.get("arguments"))
            .map(|args| args.to_string())
            .unwrap_or_default()
    };

    write_transcript(
        state,
        &body,
        &serde_json::json!({
            "message": {"content": text, "thinking": full_thinking},
            "prompt_eval_count": input_tokens, "eval_count": output_tokens, "aborted": aborted || stuck,
        }),
        source,
    );

    if aborted {
        return error_json("aborted", "generation cancelled by operator");
    }
    if stuck {
        return error_json("repetition_loop", "model appears stuck repeating itself — generation was cut short automatically");
    }

    record_usage(state, input_tokens, output_tokens, source);
    ok_json(serde_json::json!({
        "text": text,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    }))
}

/// Reads an OpenAI-compatible SSE stream (`data: {...}` lines, `[DONE]`
/// sentinel) — same live-thinking-trace + abort-check pattern as
/// `handle_ollama_stream`. `delta.reasoning_content` (the Kimi/DeepSeek-
/// style extended-thinking extension several OpenAI-compatible providers
/// support) goes to the same live trace file ollama's `thinking` deltas
/// do; plain `delta.content` accumulates into the final answer. Usage only
/// arrives because `call()` set `stream_options.include_usage` on the
/// request — without it most providers never send one in stream mode.
fn handle_openai_stream(state: &mut AgentState, body: Value, response: reqwest::blocking::Response, source: &Value) -> Value {
    let think_path = thinking_path(&state.agent_home, source);
    let _ = std::fs::create_dir_all(think_path.parent().unwrap());

    let abort_path = abort_flag_path(&state.agent_home, source);
    let _ = std::fs::remove_file(&abort_path);

    let mut full_content = String::new();
    let mut full_thinking = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut aborted = false;
    let mut content_guard = RepeatGuard::new();
    let mut thinking_guard = RepeatGuard::new();
    let mut stuck = false;

    for line in BufReader::new(response).lines() {
        let Ok(line) = line else { break };
        let Some(data) = line.strip_prefix("data: ") else { continue };
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue };

        if let Some(delta) = chunk.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()).and_then(|c| c.get("delta")) {
            if let Some(c) = delta.get("content").and_then(|v| v.as_str()) {
                full_content.push_str(c);
                stuck |= content_guard.push(c);
            }
            if let Some(t) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                full_thinking.push_str(t);
                stuck |= thinking_guard.push(t);
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&think_path) {
                    use std::io::Write as _;
                    let _ = f.write_all(t.as_bytes());
                }
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            input_tokens = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(input_tokens);
            output_tokens = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(output_tokens);
        }

        if abort_path.exists() {
            let _ = std::fs::remove_file(&abort_path);
            aborted = true;
            break;
        }
        if stuck {
            let _ = crate::logs::notify(&state.agent_home, "llm_call cut short: model appears stuck repeating itself");
            break;
        }
    }

    write_transcript(
        state,
        &body,
        &serde_json::json!({
            "choices": [{"message": {"content": full_content, "reasoning_content": full_thinking}}],
            "usage": {"prompt_tokens": input_tokens, "completion_tokens": output_tokens},
            "aborted": aborted || stuck,
        }),
        source,
    );

    if aborted {
        return error_json("aborted", "generation cancelled by operator");
    }
    if stuck {
        return error_json("repetition_loop", "model appears stuck repeating itself — generation was cut short automatically");
    }

    record_usage(state, input_tokens, output_tokens, source);
    ok_json(serde_json::json!({
        "text": full_content,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    }))
}

/// Reads Anthropic's SSE stream — event type lives in each `data: {...}`
/// line's own `"type"` field (the redundant top-level `event: <type>` line
/// is ignored, standard practice for SSE clients). `content_block_delta`
/// carries either a `text_delta` (final answer) or a `thinking_delta`
/// (extended thinking, when enabled) — the latter goes to the same live
/// trace file as the other two providers'. `message_start`/`message_delta`
/// carry input/output token counts respectively (Anthropic reports them in
/// two separate events, not one final usage object like the other two).
fn handle_anthropic_stream(state: &mut AgentState, body: Value, response: reqwest::blocking::Response, source: &Value) -> Value {
    let think_path = thinking_path(&state.agent_home, source);
    let _ = std::fs::create_dir_all(think_path.parent().unwrap());

    let abort_path = abort_flag_path(&state.agent_home, source);
    let _ = std::fs::remove_file(&abort_path);

    let mut full_content = String::new();
    let mut full_thinking = String::new();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut aborted = false;
    let mut content_guard = RepeatGuard::new();
    let mut thinking_guard = RepeatGuard::new();
    let mut stuck = false;

    for line in BufReader::new(response).lines() {
        let Ok(line) = line else { break };
        let Some(data) = line.strip_prefix("data: ") else { continue };
        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue };

        match chunk.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message_start" => {
                input_tokens = chunk
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
            }
            "content_block_delta" => {
                if let Some(delta) = chunk.get("delta") {
                    if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                        full_content.push_str(t);
                        stuck |= content_guard.push(t);
                    }
                    if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                        full_thinking.push_str(t);
                        stuck |= thinking_guard.push(t);
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&think_path) {
                            use std::io::Write as _;
                            let _ = f.write_all(t.as_bytes());
                        }
                    }
                }
            }
            "message_delta" => {
                output_tokens = chunk.get("usage").and_then(|u| u.get("output_tokens")).and_then(|v| v.as_u64()).unwrap_or(output_tokens);
            }
            _ => {}
        }

        if abort_path.exists() {
            let _ = std::fs::remove_file(&abort_path);
            aborted = true;
            break;
        }
        if stuck {
            let _ = crate::logs::notify(&state.agent_home, "llm_call cut short: model appears stuck repeating itself");
            break;
        }
    }

    write_transcript(
        state,
        &body,
        &serde_json::json!({
            "content": [{"type": "text", "text": full_content}],
            "thinking": full_thinking,
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
            "aborted": aborted || stuck,
        }),
        source,
    );

    if aborted {
        return error_json("aborted", "generation cancelled by operator");
    }
    if stuck {
        return error_json("repetition_loop", "model appears stuck repeating itself — generation was cut short automatically");
    }

    record_usage(state, input_tokens, output_tokens, source);
    ok_json(serde_json::json!({
        "text": full_content,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    }))
}

fn record_usage(state: &mut AgentState, input_tokens: u64, output_tokens: u64, source: &Value) {
    let total_tokens = input_tokens + output_tokens;
    if let Err(e) = state.budget.record(total_tokens) {
        let _ = crate::logs::notify(&state.agent_home, &format!("failed to record budget: {e}"));
    }
    let _ = append_jsonl(
        &state.agent_home.join("logs/usage.jsonl"),
        &serde_json::json!({
            "ts": now_unix_secs(),
            "syscall": "llm_call",
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": total_tokens,
            "source": source,
        }),
    );
}

/// Anthropic's Messages API wants `system` as a top-level string, not a
/// `role: "system"` entry in `messages`. `max_tokens` is already set by
/// `call()` (`RuntimeConfig::max_output_tokens`) before this runs — the
/// `or_insert` below is just a defensive fallback, not the real default.
fn normalize_for_anthropic(body: &mut Value) {
    let Value::Object(map) = body else { return };
    map.entry("max_tokens").or_insert(Value::from(1024));
    // `stream` stays as `call()` set it (true) — Anthropic streams now too,
    // see `handle_anthropic_stream`

    let Some(Value::Array(messages)) = map.get_mut("messages") else {
        return;
    };
    if messages.first().and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("system") {
        let system_msg = messages.remove(0);
        let system_value = match system_msg.get("content") {
            // legacy/other-provider shape: one plain string, no cache split
            Some(Value::String(s)) => Value::String(s.clone()),
            // `agent_loop.rs` build_system_prompt's stable/volatile split —
            // each block optionally carries `cache: true`, translated here
            // into Anthropic's own `cache_control: {type: "ephemeral"}` so
            // the stable half (soul/config/actions doc/skills/tasks) gets
            // reused server-side across runs instead of being recomputed
            // (and re-billed at full price) on every single call just
            // because the volatile half — recent chat / retrieved memory /
            // the trigger — changed underneath it.
            Some(Value::Array(blocks)) => Value::Array(
                blocks
                    .iter()
                    .filter_map(|b| {
                        let text = b.get("text").and_then(|t| t.as_str())?;
                        let mut block = serde_json::json!({"type": "text", "text": text});
                        if b.get("cache").and_then(|c| c.as_bool()) == Some(true) {
                            block["cache_control"] = serde_json::json!({"type": "ephemeral"});
                        }
                        Some(block)
                    })
                    .collect(),
            ),
            _ => return,
        };
        map.insert("system".to_string(), system_value);
    }
}

/// Non-Anthropic providers (openai/ollama) don't understand the `cache`
/// hint on a system content block — collapses `agent_loop.rs`'s stable/
/// volatile split back into the one plain string those APIs expect, same
/// as before per-provider prompt caching existed.
fn collapse_content_blocks(body: &mut Value) {
    let Value::Object(map) = body else { return };
    let Some(Value::Array(messages)) = map.get_mut("messages") else { return };
    for msg in messages.iter_mut() {
        let Some(Value::Array(blocks)) = msg.get("content").cloned() else { continue };
        let joined: String = blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect();
        msg["content"] = Value::String(joined);
    }
}

#[cfg(test)]
mod repeat_guard_tests {
    use super::*;

    #[test]
    fn detects_exact_repetition() {
        let mut guard = RepeatGuard::new();
        let chunk = "chmod +x /tmp/.build-thing-update.sh\n"; // 38 bytes, close to WINDOW
        let mut stuck = false;
        for _ in 0..20 {
            stuck |= guard.push(chunk);
        }
        assert!(stuck, "20 exact repeats of the same chunk should trip the guard");
    }

    #[test]
    fn ignores_varying_template() {
        // mirrors the real incident: fixed text either side of a varying
        // package name — some 40-byte window should still land entirely
        // inside one of the long fixed runs and repeat identically
        let mut guard = RepeatGuard::new();
        let names = ["coreutils", "util-linux", "systemd", "kmod", "e2fsprogs", "xfsprogs", "btrfs-progs", "dosfstools", "exfatprogs", "f2fs-tools"];
        let mut stuck = false;
        for name in names {
            let chunk = format!("chmod +x /tmp/.build-{name}-update.sh\n    # execute it\n    /tmp/.build-{name}-update.sh &\nfi\nEOF\n");
            stuck |= guard.push(&chunk);
        }
        assert!(stuck, "the fixed portions of a templated loop should still trip the guard even with varying infill");
    }

    #[test]
    fn does_not_trip_on_diverse_text() {
        // deliberately no shared fixed template this time (a fixed prefix/
        // suffix around a varying number, like real legitimate output
        // rarely produces but the earlier test's loop pathology exactly
        // does, would defeat the point of this test)
        let sentences = [
            "The quarterly report shows revenue up twelve percent year over year.",
            "Meeting notes: discussed the new deployment pipeline and rollback plan.",
            "I checked the logs and found nothing unusual in the last hour.",
            "Let's schedule the retro for Thursday afternoon if that works.",
            "The database migration completed without any errors this time.",
            "Customer feedback mentioned slow load times on the settings page.",
            "We should probably refactor that module before adding more features.",
            "Weather forecast says rain tomorrow, might affect the outdoor event.",
            "The API rate limit was hit twice during the load test yesterday.",
            "Someone left a comment on the PR asking about the edge case handling.",
        ];
        let mut guard = RepeatGuard::new();
        let mut stuck = false;
        for s in sentences {
            stuck |= guard.push(s);
        }
        assert!(!stuck, "diverse, non-repeating text should never trip the guard");
    }
}

#[cfg(test)]
mod cache_block_tests {
    use super::*;

    #[test]
    fn normalize_for_anthropic_splits_cache_block() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "stable", "cache": true},
                    {"type": "text", "text": "volatile"}
                ]},
                {"role": "user", "content": "hi"}
            ]
        });
        normalize_for_anthropic(&mut body);
        let system = &body["system"];
        assert_eq!(system[0]["text"], "stable");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(system[1]["text"], "volatile");
        assert!(system[1].get("cache_control").is_none());
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn normalize_for_anthropic_keeps_plain_string_system() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "plain"},
                {"role": "user", "content": "hi"}
            ]
        });
        normalize_for_anthropic(&mut body);
        assert_eq!(body["system"], "plain");
    }

    #[test]
    fn collapse_content_blocks_joins_text_for_non_anthropic() {
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "stable", "cache": true},
                    {"type": "text", "text": "volatile"}
                ]}
            ]
        });
        collapse_content_blocks(&mut body);
        assert_eq!(body["messages"][0]["content"], "stablevolatile");
    }
}

fn write_transcript(state: &AgentState, request: &Value, response: &Value, source: &Value) {
    let path = state
        .agent_home
        .join(format!("logs/transcripts/{}-llm_call.json", now_unix_nanos()));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({"request": request, "response": response, "source": source}))
            .unwrap_or_default(),
    );
}
