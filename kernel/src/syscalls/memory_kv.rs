use crate::abi::{error_json, ok_json};
use crate::state::AgentState;
use serde_json::Value;

/// `memory_get(key) -> {value}` / `memory_set(key, value) -> {}` — a plain
/// key→value store in the same `memory/index.db` as the RAG `chunks` table,
/// for exact-lookup facts (a counter, a last-seen value, a small setting)
/// that don't need `db_query`'s raw SQL or `notes/`'s fuzzy RAG retrieval.
/// Simpler surface than expecting the model to hand-write correct SQL every
/// time for the common "just remember this one value" case.
fn ensure_table(db: &rusqlite::Connection) -> rusqlite::Result<()> {
    db.execute("CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)", [])?;
    Ok(())
}

pub fn get(state: &mut AgentState, req: Value) -> Value {
    let Some(key) = req.get("key").and_then(|k| k.as_str()) else {
        return error_json("bad_request", "memory_get requires a string `key`");
    };
    let db = match state.db() {
        Ok(d) => d,
        Err(e) => return error_json("db_error", &e.to_string()),
    };
    if let Err(e) = ensure_table(db) {
        return error_json("db_error", &e.to_string());
    }
    let result: rusqlite::Result<String> = db.query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| row.get(0));
    match result {
        Ok(value) => ok_json(serde_json::json!({"value": value})),
        Err(rusqlite::Error::QueryReturnedNoRows) => ok_json(serde_json::json!({"value": null})),
        Err(e) => error_json("db_error", &e.to_string()),
    }
}

pub fn set(state: &mut AgentState, req: Value) -> Value {
    let Some(key) = req.get("key").and_then(|k| k.as_str()) else {
        return error_json("bad_request", "memory_set requires a string `key`");
    };
    let Some(value) = req.get("value").and_then(|v| v.as_str()) else {
        return error_json("bad_request", "memory_set requires a string `value`");
    };
    let db = match state.db() {
        Ok(d) => d,
        Err(e) => return error_json("db_error", &e.to_string()),
    };
    if let Err(e) = ensure_table(db) {
        return error_json("db_error", &e.to_string());
    }
    match db.execute(
        "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    ) {
        Ok(_) => ok_json(Value::Null),
        Err(e) => error_json("db_error", &e.to_string()),
    }
}
