pub mod chat_send;
pub mod db_exec;
pub mod embed;
pub mod http_fetch;
pub mod llm_call;
pub mod memory_kv;
pub mod notify;
pub mod schedule_task;
pub mod search_web;
pub mod sleep_until;

use crate::abi::error_json;
use crate::state::AgentState;
use serde_json::Value;

pub fn dispatch(state: &mut AgentState, name: &str, req: Value) -> Value {
    match name {
        "notify" => notify::call(state, req),
        "chat_send" => chat_send::call(state, req),
        "sleep_until" => sleep_until::call(state, req),
        "db_exec" => db_exec::call(state, req),
        "memory_get" => memory_kv::get(state, req),
        "memory_set" => memory_kv::set(state, req),
        "llm_call" => llm_call::call(state, req),
        "embed" => embed::call(state, req),
        "http_fetch" => http_fetch::call(state, req),
        "search_web" => search_web::call(state, req),
        "schedule_task" => schedule_task::call(state, req),
        "update_task" => schedule_task::update(state, req),
        "delete_task" => schedule_task::delete(state, req),
        other => error_json("unknown_syscall", &format!("no such syscall: {other}")),
    }
}
