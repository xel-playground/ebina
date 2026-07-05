use crate::abi::{error_json, ok_json};
use crate::state::AgentState;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, ErrorCode};
use serde_json::Value;
use std::time::{Duration, Instant};

/// PRAGMAs are allowlisted rather than blocklisted: anything not explicitly
/// known to be read-only/informational is denied. Keeps `writable_schema`,
/// `journal_mode` flips, etc. out of the guest's reach entirely.
const PRAGMA_ALLOWLIST: &[&str] = &[
    "table_info",
    "table_list",
    "index_list",
    "index_info",
    "foreign_key_list",
    "database_list",
];

/// Applied once per connection at open time. ATTACH/DETACH always denied so
/// `db_exec` can only ever touch the one preopened `index.db` file; PRAGMA
/// restricted to the allowlist; `load_extension()` denied as defense in
/// depth even though the `load_extension` cargo feature is never enabled
/// (so the underlying C API path is compiled out regardless).
pub fn harden(conn: &Connection, _timeout_secs: u64) -> rusqlite::Result<()> {
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
        AuthAction::Pragma { pragma_name, .. } => {
            let lower = pragma_name.to_ascii_lowercase();
            if PRAGMA_ALLOWLIST.contains(&lower.as_str()) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } if function_name.eq_ignore_ascii_case("load_extension") => {
            Authorization::Deny
        }
        _ => Authorization::Allow,
    }))
}

/// `db_exec(sql, params)` — runs one statement against `memory/index.db` and
/// returns matched rows as JSON. Query wall-clock timeout enforced via
/// `progress_handler` since SQLite has no native per-call timeout for
/// CPU-bound (non-lock-contention) queries.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let Some(sql) = req.get("sql").and_then(|s| s.as_str()) else {
        return error_json("bad_request", "db_exec requires a string `sql` field");
    };
    let params = match json_to_params(req.get("params").unwrap_or(&Value::Array(vec![]))) {
        Ok(p) => p,
        Err(e) => return error_json("bad_request", &e),
    };
    let timeout_secs = state.config.db.query_timeout_secs;

    let db = match state.db() {
        Ok(d) => d,
        Err(e) => return error_json("db_error", &e.to_string()),
    };

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    // num_ops=1000: checked every ~1000 VM instructions, frequent enough that
    // the actual overshoot past `deadline` is negligible.
    let _ = db.progress_handler(1000, Some(move || Instant::now() > deadline));
    let result = run_sql(db, sql, &params);
    let _ = db.progress_handler(1000, None::<fn() -> bool>);

    match result {
        Ok(rows) => ok_json(serde_json::json!({ "rows": rows })),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::OperationInterrupted => {
            error_json("timeout", &format!("query exceeded {timeout_secs}s timeout"))
        }
        Err(rusqlite::Error::SqliteFailure(e, msg)) if e.code == ErrorCode::AuthorizationForStatementDenied => {
            error_json("denied", &msg.unwrap_or_else(|| "not authorized".to_string()))
        }
        Err(e) => error_json("sql_error", &e.to_string()),
    }
}

fn run_sql(db: &Connection, sql: &str, params: &[SqlValue]) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = db.prepare(sql)?;
    let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();

    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        let mut obj = serde_json::Map::new();
        for (i, name) in col_names.iter().enumerate() {
            obj.insert(name.clone(), value_ref_to_json(row.get_ref(i)?));
        }
        Ok(Value::Object(obj))
    })?;

    rows.collect()
}

fn value_ref_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(format!("base64:{}", base64_encode(b))),
    }
}

fn json_to_params(v: &Value) -> Result<Vec<SqlValue>, String> {
    let arr = v.as_array().ok_or("`params` must be a JSON array")?;
    arr.iter()
        .map(|p| match p {
            Value::Null => Ok(SqlValue::Null),
            Value::Bool(b) => Ok(SqlValue::Integer(*b as i64)),
            Value::Number(n) if n.is_i64() => Ok(SqlValue::Integer(n.as_i64().unwrap())),
            Value::Number(n) => Ok(SqlValue::Real(n.as_f64().ok_or("invalid number param")?)),
            Value::String(s) => Ok(SqlValue::Text(s.clone())),
            other => Err(format!("unsupported param type: {other}")),
        })
        .collect()
}

/// tiny base64 encoder so blob columns round-trip through JSON without an
/// extra dependency — Phase 1 has no blob-heavy workload yet
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}
