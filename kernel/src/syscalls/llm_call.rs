use crate::abi::{error_json, ok_json};
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
    /// `provider = "ollama"` *and* a successful status — handed back
    /// unread so the caller can stream it (`handle_ollama_stream`); this is
    /// the only outcome that isn't already read to a `Value`.
    Stream(reqwest::blocking::Response),
    /// every other success, or the last attempt's failure — retried
    /// attempts don't reach here, only the one that finally succeeded or
    /// the one that ran out of attempts.
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
fn send_with_retries(request: reqwest::blocking::RequestBuilder, is_ollama: bool) -> SendResult {
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
        if is_ollama && status.is_success() {
            return SendResult::Stream(response);
        }
        let json: Value = response.json().unwrap_or(Value::Null);
        if status.is_success() || attempt + 1 == MAX_SEND_ATTEMPTS {
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
fn record_circuit_failure(agent_home: &Path) {
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
/// For `provider = "ollama"` this streams the response so the reasoning
/// ("thinking") text can be tailed live by the gateway (`GET /api/thinking`)
/// while the guest is still blocked waiting on this call, and so an
/// operator can abort mid-generation (`POST /api/abort`) instead of burning
/// through the whole response.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    if !state.budget.has_headroom() {
        let _ = crate::logs::notify(
            &state.agent_home,
            "llm_call rejected: daily token budget exhausted",
        );
        return error_json("budget_exceeded", "daily token budget exhausted");
    }

    if let Err(limited) = state.llm_bucket.acquire() {
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
        map.insert("stream".to_string(), Value::Bool(provider == "ollama"));
    }
    if provider == "anthropic" {
        normalize_for_anthropic(&mut body);
    }

    let client = reqwest::blocking::Client::new();
    let mut request = client.post(&state.config.llm.base_url).header("content-type", "application/json");
    request = match provider.as_str() {
        "ollama" | "openai" => request.bearer_auth(&api_key),
        _ => request.header("x-api-key", &api_key).header("anthropic-version", "2023-06-01"),
    };
    let (status, response_json) = match send_with_retries(request.json(&body), provider == "ollama") {
        SendResult::Stream(response) => {
            record_circuit_success(&state.agent_home);
            return handle_ollama_stream(state, body, response, &source);
        }
        SendResult::Body { status, json } => (status, json),
        SendResult::ConnectFailed(e) => {
            record_circuit_failure(&state.agent_home);
            return error_json("network_error", &e);
        }
    };

    write_transcript(state, &body, &response_json, &source);
    if !status.is_success() {
        record_circuit_failure(&state.agent_home);
        return error_json("llm_error", &format!("HTTP {status}: {response_json}"));
    }
    record_circuit_success(&state.agent_home);

    // only Anthropic/OpenAI-shaped (or an ollama error body) reaches here — ollama success is streamed above
    let (text, input_tokens, output_tokens) = if provider == "openai" {
        let text = response_json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let input = response_json.get("usage").and_then(|u| u.get("prompt_tokens")).and_then(|v| v.as_u64()).unwrap_or(0);
        let output = response_json.get("usage").and_then(|u| u.get("completion_tokens")).and_then(|v| v.as_u64()).unwrap_or(0);
        (text, input, output)
    } else {
        let input = response_json.get("usage").and_then(|u| u.get("input_tokens")).and_then(|v| v.as_u64()).unwrap_or(0);
        let output = response_json.get("usage").and_then(|u| u.get("output_tokens")).and_then(|v| v.as_u64()).unwrap_or(0);
        let text = response_json
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|block| block.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        (text, input, output)
    };

    record_usage(state, input_tokens, output_tokens, &source);
    ok_json(serde_json::json!({
        "text": text,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    }))
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

fn abort_flag_path(agent_home: &Path) -> PathBuf {
    agent_home.join("logs/abort_requested")
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

    let abort_path = abort_flag_path(&state.agent_home);
    // clear out anything left over from a previous, already-finished call
    // before it can be mistaken for a request to abort *this* one
    let _ = std::fs::remove_file(&abort_path);

    let mut full_content = String::new();
    let mut full_thinking = String::new();
    let mut last_message = Value::Null;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut aborted = false;

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
            }
            if let Some(t) = msg.get("thinking").and_then(|v| v.as_str()) {
                full_thinking.push_str(t);
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
            "prompt_eval_count": input_tokens, "eval_count": output_tokens, "aborted": aborted,
        }),
        source,
    );

    if aborted {
        return error_json("aborted", "generation cancelled by operator");
    }

    record_usage(state, input_tokens, output_tokens, source);
    ok_json(serde_json::json!({
        "text": text,
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
/// `role: "system"` entry in `messages`, and requires `max_tokens`. This
/// keeps the guest's request shape identical across providers.
fn normalize_for_anthropic(body: &mut Value) {
    let Value::Object(map) = body else { return };
    map.entry("max_tokens").or_insert(Value::from(1024));
    map.remove("stream");

    let Some(Value::Array(messages)) = map.get_mut("messages") else {
        return;
    };
    if messages.first().and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("system") {
        let system_msg = messages.remove(0);
        if let Some(content) = system_msg.get("content").and_then(|c| c.as_str()) {
            map.insert("system".to_string(), Value::String(content.to_string()));
        }
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
