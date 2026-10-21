use crate::state::AgentState;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

/// Agent calling convention (see agent/src/syscall.rs for the mirror side):
///
/// The agent process inherits one end of a `socketpair` as fd
/// [`crate::SYSCALL_FD`]. Each syscall is one line of JSON
///
///   {"name": "<syscall>", "req": {...}}\n
///
/// answered by exactly one line of JSON — always a `{"ok":true,...}` or
/// `{"ok":false,"error":{...}}` envelope. `serde_json` never emits a raw
/// newline in compact output, so line framing is unambiguous.
///
/// No transport-level error path — every recoverable failure (bad utf8,
/// bad json, missing name) becomes a JSON error envelope on the same
/// channel as a normal syscall error, so the agent has exactly one failure
/// path to handle instead of two.
///
/// One generic channel (rather than one entrypoint per syscall) keeps
/// adding new syscalls a host+agent-only change with no new wiring.
///
/// Runs until the agent closes its end (i.e. exits or is killed).
pub fn serve(state: &mut AgentState, stream: UnixStream) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let response = match serde_json::from_slice::<serde_json::Value>(&line) {
            Ok(call) => match call.get("name").and_then(|n| n.as_str()) {
                Some(name) => {
                    let req = call.get("req").cloned().unwrap_or(serde_json::Value::Null);
                    crate::syscalls::dispatch(state, name, req)
                }
                None => error_json("bad_request", "missing `name`"),
            },
            Err(e) => error_json("bad_json", &e.to_string()),
        };
        let mut bytes = serde_json::to_vec(&response)
            .unwrap_or_else(|_| b"{\"ok\":false,\"error\":{\"code\":\"encode_error\"}}".to_vec());
        bytes.push(b'\n');
        if writer.write_all(&bytes).is_err() {
            return;
        }
    }
}

pub fn error_json(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}})
}

pub fn ok_json(result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"ok": true, "result": result})
}
