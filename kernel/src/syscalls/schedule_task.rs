use crate::abi::{error_json, ok_json};
use crate::state::AgentState;
use serde_json::Value;

/// `schedule_task(cron, data_path, description?) -> task` — lets the agent
/// set up a recurring job during a normal chat, e.g. "remind me every
/// morning": write instructions to a file, then point a cron spec at it.
/// Fired later by `gateway::scheduler_loop`, same fresh-session (no chat
/// history) treatment as the built-in daily_maintenance/cron wakes.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let cron = req.get("cron").and_then(|c| c.as_str()).unwrap_or("");
    let data_path = req.get("data_path").and_then(|d| d.as_str()).unwrap_or("");
    let description = req.get("description").and_then(|d| d.as_str()).unwrap_or("");
    match crate::scheduler_tasks::add_task(&state.agent_home, cron, data_path, description) {
        Ok(task) => ok_json(serde_json::to_value(task).unwrap_or(Value::Null)),
        Err(e) => error_json("bad_task", &e),
    }
}

/// `update_task(id, cron?, data_path?, description?, enabled?) -> task` —
/// every field but `id` is optional; only given fields change.
pub fn update(state: &mut AgentState, req: Value) -> Value {
    let Some(id) = req.get("id").and_then(|i| i.as_str()) else {
        return error_json("bad_request", "update_task requires an `id`");
    };
    let cron = req.get("cron").and_then(|c| c.as_str());
    let data_path = req.get("data_path").and_then(|d| d.as_str());
    let description = req.get("description").and_then(|d| d.as_str());
    let enabled = req.get("enabled").and_then(|e| e.as_bool());
    match crate::scheduler_tasks::update_task(&state.agent_home, id, cron, data_path, description, enabled) {
        Ok(Some(task)) => ok_json(serde_json::to_value(task).unwrap_or(Value::Null)),
        Ok(None) => error_json("not_found", &format!("no such task: {id}")),
        Err(e) => error_json("bad_task", &e),
    }
}

/// `delete_task(id) -> {}`
pub fn delete(state: &mut AgentState, req: Value) -> Value {
    let Some(id) = req.get("id").and_then(|i| i.as_str()) else {
        return error_json("bad_request", "delete_task requires an `id`");
    };
    match crate::scheduler_tasks::remove_task(&state.agent_home, id) {
        Ok(true) => ok_json(Value::Null),
        Ok(false) => error_json("not_found", &format!("no such task: {id}")),
        Err(e) => error_json("io_error", &e.to_string()),
    }
}
