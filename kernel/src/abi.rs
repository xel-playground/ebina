use crate::state::AgentState;
use wasmtime::{Caller, Linker, Memory};

/// Sentinel for a fatal ABI-level failure (bad `out_ptr`/`out_cap` — can't
/// even write an error envelope back). Guest must abort on seeing this;
/// everything else (bad name/req, oversized response) is a normal JSON
/// error envelope written through the out-buffer protocol below.
pub const FATAL: i32 = i32::MIN;

/// Guest calling convention (see agent/src/syscall.rs for the mirror side):
///
///   env.syscall(name_ptr, name_len, req_ptr, req_len, out_ptr, out_cap) -> i32
///
/// `name`/`req` are read from guest memory; `req` is a JSON object. The
/// response (always a JSON `{"ok":true,...}` or `{"ok":false,"error":{...}}`
/// envelope) is written into `out_ptr..out_ptr+out_cap`.
///
///   - returns `len >= 0`: `len` bytes of JSON were written to `out_ptr`
///   - returns `-needed`: `out_cap` was too small; guest should regrow its
///     buffer to at least `needed` bytes and retry the same call
///   - returns `FATAL`: unrecoverable ABI error, guest should abort
///
/// No `Result`/trap on the host closure itself — every recoverable failure
/// (bad utf8, bad json, missing memory) becomes a JSON error envelope
/// through the same buffer protocol as a normal syscall error, so the guest
/// has exactly one failure path to handle instead of two.
///
/// One generic import (rather than one import per syscall) keeps adding new
/// syscalls a host+guest-only change with no new wasm import wiring.
pub fn register(linker: &mut Linker<AgentState>) -> anyhow::Result<()> {
    linker.func_wrap(
        "env",
        "syscall",
        |mut caller: Caller<'_, AgentState>,
         name_ptr: i32,
         name_len: i32,
         req_ptr: i32,
         req_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                return FATAL;
            };

            let response = match (
                read_bytes(&caller, &memory, name_ptr, name_len),
                read_bytes(&caller, &memory, req_ptr, req_len),
            ) {
                (Some(name_bytes), Some(req_bytes)) => match String::from_utf8(name_bytes) {
                    Ok(name) => {
                        let req: serde_json::Value = serde_json::from_slice(&req_bytes)
                            .unwrap_or_else(|e| error_json("bad_json", &e.to_string()));
                        crate::syscalls::dispatch(caller.data_mut(), &name, req)
                    }
                    Err(e) => error_json("bad_utf8", &e.to_string()),
                },
                _ => error_json("out_of_bounds", "name/req pointer or length out of bounds"),
            };

            write_response(&mut caller, &memory, out_ptr, out_cap, &response)
        },
    )?;
    Ok(())
}

fn read_bytes(caller: &Caller<'_, AgentState>, memory: &Memory, ptr: i32, len: i32) -> Option<Vec<u8>> {
    if ptr < 0 || len < 0 {
        return None;
    }
    let data = memory.data(caller);
    let start = ptr as usize;
    let end = start.checked_add(len as usize)?;
    data.get(start..end).map(|s| s.to_vec())
}

fn write_response(
    caller: &mut Caller<'_, AgentState>,
    memory: &Memory,
    out_ptr: i32,
    out_cap: i32,
    value: &serde_json::Value,
) -> i32 {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"ok\":false,\"error\":{\"code\":\"encode_error\"}}".to_vec());
    if out_ptr < 0 || out_cap < 0 {
        return FATAL;
    }
    if bytes.len() > out_cap as usize {
        return -(bytes.len() as i32);
    }
    let start = out_ptr as usize;
    let Some(end) = start.checked_add(bytes.len()) else {
        return FATAL;
    };
    let data = memory.data_mut(caller);
    if end > data.len() {
        return FATAL;
    }
    data[start..end].copy_from_slice(&bytes);
    bytes.len() as i32
}

pub fn error_json(code: &str, message: &str) -> serde_json::Value {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}})
}

pub fn ok_json(result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"ok": true, "result": result})
}
