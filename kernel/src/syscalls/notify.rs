use crate::abi::{error_json, ok_json};
use crate::state::AgentState;
use serde_json::Value;

/// `notify(message)` — one-way notification to the human (gateway displays
/// it; Phase 1 has no gateway yet, so it's a println + logs/notifications.jsonl)
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let Some(message) = req.get("message").and_then(|m| m.as_str()) else {
        return error_json("bad_request", "notify requires a string `message` field");
    };
    let result = match req.get("_meta") {
        Some(source) => crate::logs::notify_with_source(&state.agent_home, message, source),
        None => crate::logs::notify(&state.agent_home, message),
    };
    match result {
        Ok(()) => ok_json(Value::Null),
        Err(e) => error_json("io_error", &e.to_string()),
    }
}
