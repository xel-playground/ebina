use crate::abi::{error_json, ok_json};
use crate::state::AgentState;
use serde_json::Value;
use wasmtime::{Config as EngineConfig, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

/// tool binaries get a smaller ceiling than the agent itself — these are
/// meant to be coreutils-style single-purpose utilities, not another agent
const TOOL_MEMORY_CAP_BYTES: usize = 128 * 1024 * 1024;
const TOOL_FUEL: u64 = 5_000_000_000;

struct ToolState {
    wasi: WasiP1Ctx,
    limits: StoreLimits,
}

/// `exec_wasm(wasm_path, args, stdin?) -> {stdout, exit_code}` — runs a tool
/// the agent installed under `workspace/bin/`. A brand new Store per call,
/// preopening *only* `workspace/` (not all of agent_home — the tool can't
/// see `memory/`, `config.toml`, or anything else), no WASI sockets (a
/// preview1 command module never gets any), fuel + memory capped. The
/// sandbox guarantee comes from the Store boundary, not from trusting the
/// binary (PROJECT.md 3/4.7) — a malicious tool can trash `workspace/` at
/// worst.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let Some(rel_path) = req.get("wasm_path").and_then(|p| p.as_str()) else {
        return error_json("bad_request", "exec_wasm requires a string `wasm_path` field");
    };

    let workspace = state.agent_home.join("workspace");
    let _ = std::fs::create_dir_all(&workspace);
    let full_path = workspace.join(rel_path);

    let (canon_workspace, canon_full) = match (workspace.canonicalize(), full_path.canonicalize()) {
        (Ok(w), Ok(f)) => (w, f),
        (_, Err(e)) => return error_json("not_found", &format!("{rel_path}: {e}")),
        (Err(e), _) => return error_json("io_error", &e.to_string()),
    };
    if !canon_full.starts_with(&canon_workspace) {
        return error_json("bad_path", "wasm_path must resolve inside workspace/");
    }

    let args: Vec<String> = req
        .get("args")
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let stdin_data = req.get("stdin").and_then(|s| s.as_str()).map(String::from).unwrap_or_default();

    let mut engine_config = EngineConfig::new();
    engine_config.consume_fuel(true);
    let engine = match Engine::new(&engine_config) {
        Ok(e) => e,
        Err(e) => return error_json("engine_error", &e.to_string()),
    };
    let module = match Module::from_file(&engine, &canon_full) {
        Ok(m) => m,
        Err(e) => return error_json("bad_wasm", &format!("not a valid wasm module: {e}")),
    };

    let mut linker: Linker<ToolState> = Linker::new(&engine);
    if let Err(e) = p1::add_to_linker_sync(&mut linker, |t| &mut t.wasi) {
        return error_json("engine_error", &e.to_string());
    }

    let stdout = MemoryOutputPipe::new(256 * 1024);
    let mut builder = WasiCtxBuilder::new();
    if let Err(e) = builder.preopened_dir(&canon_workspace, "/", DirPerms::all(), FilePerms::all()) {
        return error_json("io_error", &e.to_string());
    }
    builder.stdout(stdout.clone()).stdin(MemoryInputPipe::new(stdin_data.into_bytes()));
    builder.arg("tool");
    for a in &args {
        builder.arg(a);
    }
    let wasi = builder.build_p1();
    let limits = StoreLimitsBuilder::new().memory_size(TOOL_MEMORY_CAP_BYTES).build();

    let mut store = Store::new(&engine, ToolState { wasi, limits });
    store.limiter(|t| &mut t.limits);
    if let Err(e) = store.set_fuel(TOOL_FUEL) {
        return error_json("engine_error", &e.to_string());
    }

    let instance = match linker.instantiate(&mut store, &module) {
        Ok(i) => i,
        Err(e) => return error_json("bad_wasm", &e.to_string()),
    };
    let start = match instance.get_typed_func::<(), ()>(&mut store, "_start") {
        Ok(f) => f,
        Err(e) => return error_json("bad_wasm", &format!("no _start export: {e}")),
    };

    let exit_code = match start.call(&mut store, ()) {
        Ok(()) => 0,
        Err(e) => match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
            Some(exit) => exit.0,
            None => {
                drop(store);
                let stdout_text = String::from_utf8_lossy(&stdout.contents()).into_owned();
                return ok_json(serde_json::json!({"stdout": stdout_text, "exit_code": -1, "trapped": e.to_string()}));
            }
        },
    };

    drop(store);
    let stdout_text = String::from_utf8_lossy(&stdout.contents()).into_owned();
    ok_json(serde_json::json!({"stdout": stdout_text, "exit_code": exit_code}))
}
