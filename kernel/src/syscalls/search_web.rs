use crate::abi::{error_json, ok_json};
use crate::logs::append_jsonl;
use crate::state::AgentState;
use serde_json::Value;

/// `search_web(query) -> {results: [{title, url, snippet}]}` — same shape
/// as `llm_call`/`embed`: config-driven endpoint, key (if any) resolved
/// host-side from the vault and never handed to the guest, daily request
/// cap. `provider = "searxng"` (self-hosted, no key, no cost) or "tavily"
/// (hosted free tier, needs a key).
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let Some(query) = req.get("query").and_then(|q| q.as_str()) else {
        return error_json("bad_request", "search_web requires a string `query` field");
    };

    if !state.search_daily.has_headroom() {
        let _ = crate::logs::notify(&state.agent_home, "search_web rejected: daily request cap exhausted");
        return error_json("daily_cap_exceeded", "search.daily_request_cap exhausted");
    }

    let provider = state.config.search.provider.clone();
    let client = reqwest::blocking::Client::new();
    let http_result = if provider == "searxng" {
        let url = format!("{}?q={}&format=json", state.config.search.base_url, percent_encode(query));
        client.get(&url).send()
    } else {
        let api_key = match crate::secrets::resolve_placeholder(&state.secrets, &state.config.search.api_key) {
            Ok(k) => k,
            Err(e) => return error_json("no_api_key", &e),
        };
        let body = serde_json::json!({
            "api_key": api_key,
            "query": query,
            "max_results": state.config.search.max_results,
        });
        client.post(&state.config.search.base_url).json(&body).send()
    };

    let response = match http_result {
        Ok(r) => r,
        Err(e) => return error_json("network_error", &e.to_string()),
    };
    let status = response.status();
    let response_json: Value = match response.json() {
        Ok(v) => v,
        Err(e) => return error_json("bad_response", &e.to_string()),
    };

    let _ = append_jsonl(
        &state.agent_home.join("logs/egress.jsonl"),
        &serde_json::json!({
            "ts": crate::logs::now_unix_secs(), "method": "SEARCH", "url": state.config.search.base_url,
            "domain": "search", "bytes": response_json.to_string().len(), "error": Value::Null,
        }),
    );

    if !status.is_success() {
        return error_json("search_error", &format!("HTTP {status}: {response_json}"));
    }

    let max_results = state.config.search.max_results as usize;
    let results: Vec<Value> = response_json
        .get("results")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .take(max_results)
                .map(|r| {
                    serde_json::json!({
                        "title": r.get("title").and_then(|v| v.as_str()).unwrap_or_default(),
                        "url": r.get("url").and_then(|v| v.as_str()).unwrap_or_default(),
                        "snippet": r.get("content").and_then(|v| v.as_str()).unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let _ = state.search_daily.record(1);
    ok_json(serde_json::json!({"results": results}))
}

/// minimal percent-encoding for a query param value — no new dependency for
/// what's otherwise a one-field GET query string
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
