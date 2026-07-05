use crate::abi::{error_json, ok_json};
use crate::logs::{append_jsonl, now_unix_secs};
use crate::state::AgentState;
use serde_json::Value;

/// `embed(texts[]) -> {vectors[], model}` — same budget/rate-limit/logging
/// treatment as `llm_call`; counts against the same daily token cap.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let Some(texts) = req.get("texts").and_then(|t| t.as_array()) else {
        return error_json("bad_request", "embed requires a `texts` array field");
    };

    if !state.budget.has_headroom() {
        let _ = crate::logs::notify(&state.agent_home, "embed rejected: daily token budget exhausted");
        return error_json("budget_exceeded", "daily token budget exhausted");
    }

    if let Err(limited) = state.embed_bucket.acquire() {
        if limited.sustained {
            let _ = crate::logs::notify(
                &state.agent_home,
                "embed sustained rate-limit hits — possible runaway loop",
            );
        }
        return serde_json::json!({
            "ok": false,
            "error": {
                "code": "rate_limited",
                "message": "embed rate limit exceeded",
                "retry_after": limited.retry_after_secs,
            }
        });
    }

    let api_key = match crate::secrets::resolve_placeholder(&state.secrets, &state.config.embed.api_key) {
        Ok(k) => k,
        Err(e) => return error_json("no_api_key", &e),
    };

    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(&state.config.embed.model)
        .to_string();

    let client = reqwest::blocking::Client::new();
    let http_result = client
        .post(&state.config.embed.base_url)
        .bearer_auth(&api_key)
        .json(&serde_json::json!({"input": texts, "model": model}))
        .send();

    let response = match http_result {
        Ok(resp) => resp,
        Err(e) => return error_json("network_error", &e.to_string()),
    };

    let status = response.status();
    let response_json: Value = match response.json() {
        Ok(v) => v,
        Err(e) => return error_json("bad_response", &e.to_string()),
    };

    if !status.is_success() {
        return error_json("embed_error", &format!("HTTP {status}: {response_json}"));
    }

    let (vectors, total_tokens) = match state.config.embed.provider.as_str() {
        "ollama" => (
            response_json.get("embeddings").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
            response_json.get("prompt_eval_count").and_then(|v| v.as_u64()).unwrap_or(0),
        ),
        _ => (
            response_json
                .get("data")
                .and_then(|d| d.as_array())
                .map(|arr| arr.iter().filter_map(|item| item.get("embedding").cloned()).collect())
                .unwrap_or_default(),
            response_json.get("usage").and_then(|u| u.get("total_tokens")).and_then(|v| v.as_u64()).unwrap_or(0),
        ),
    };

    if let Err(e) = state.budget.record(total_tokens) {
        let _ = crate::logs::notify(&state.agent_home, &format!("failed to record budget: {e}"));
    }
    let _ = append_jsonl(
        &state.agent_home.join("logs/usage.jsonl"),
        &serde_json::json!({
            "ts": now_unix_secs(),
            "syscall": "embed",
            "total_tokens": total_tokens,
        }),
    );

    ok_json(serde_json::json!({"vectors": vectors, "model": model}))
}
