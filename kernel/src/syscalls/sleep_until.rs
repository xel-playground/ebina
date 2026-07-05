use crate::abi::{error_json, ok_json};
use crate::state::AgentState;
use serde_json::Value;

/// `sleep_until(timestamp)` — agent declares its next wake time and ends
/// this run. The scheduler (Phase 2+) reads `state.sleep_until` after
/// `_start` returns; there's no scheduler yet, so this just records it.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let Some(timestamp) = req.get("timestamp").and_then(|t| t.as_i64()) else {
        return error_json(
            "bad_request",
            "sleep_until requires an integer `timestamp` field (unix seconds)",
        );
    };
    state.sleep_until = Some(timestamp);
    ok_json(Value::Null)
}
