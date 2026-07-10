pub mod abi;
pub mod autocommit;
pub mod budget;
pub mod config;
pub mod cron;
pub mod discord;
pub mod filelock;
pub mod gateway;
pub mod grants;
pub mod logs;
pub mod ratelimit;
pub mod scheduler_tasks;
pub mod secrets;
pub mod state;
pub mod syscalls;

use anyhow::Result;
use config::Config;
use secrets::Secrets;
use state::AgentState;
use std::path::{Path, PathBuf};
use std::time::Duration;
use wasmtime::{Config as EngineConfig, Engine, Linker, Module, Store, StoreLimitsBuilder};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

/// PROJECT.md 4.1: epoch interruption timeout (fresh instantiate every wake,
/// so a stuck guest just gets trapped rather than needing a kernel restart).
/// Fallback only — [`run_agent`] prefers `config.toml`'s `[runtime]
/// epoch_timeout_secs` (see `config.rs` `RuntimeConfig`) and only falls
/// back to this if that config can't be loaded at all.
pub const DEFAULT_EPOCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// PROJECT.md 4.1: linear memory cap per instance
pub const MEMORY_CAP_BYTES: usize = 512 * 1024 * 1024;

pub struct RunOutcome {
    pub stdout: String,
    pub sleep_until: Option<i64>,
    /// `true` when the guest never produced a `RESULT:` line — see the
    /// `!stdout.contains("RESULT:")` check below. `gateway.rs`'s
    /// `run_trigger` uses this to hand back an actual "run trapped" summary
    /// instead of a bare `null` result, which otherwise renders as an
    /// unhelpful generic "(no summary)" in the webui's Schedule history panel.
    pub trapped: bool,
}

/// Instantiate `wasm_path` fresh, preopen `agent_home` as guest `/`, run
/// `_start` with `args`, and return whatever it wrote to stdout plus the
/// timestamp it asked to be woken at (if it called `sleep_until`).
///
/// env stays empty (no `inherit_env`) so host secrets never reach the guest;
/// the only way secrets reach the network is through host-side syscalls
/// (`llm_call`/`embed`) that read them straight from the host environment.
pub fn run_agent(agent_home: &str, wasm_path: &str, args: &[&str]) -> Result<RunOutcome> {
    let epoch_timeout = Config::load(&PathBuf::from(agent_home))
        .map(|c| Duration::from_secs(c.runtime.epoch_timeout_secs))
        .unwrap_or(DEFAULT_EPOCH_TIMEOUT);
    run_agent_with_epoch_timeout(agent_home, wasm_path, args, epoch_timeout)
}

/// Same as [`run_agent`] but with a configurable epoch-interruption timeout —
/// exists so tests can exercise the timeout trap without waiting 5 minutes.
/// Production callers should use [`run_agent`].
pub fn run_agent_with_epoch_timeout(
    agent_home: &str,
    wasm_path: &str,
    args: &[&str],
    epoch_timeout: Duration,
) -> Result<RunOutcome> {
    let agent_home_path = PathBuf::from(agent_home);
    let config = Config::load(&agent_home_path)?;
    let secrets = Secrets::load(&secrets_path(&agent_home_path));

    let mut engine_config = EngineConfig::new();
    engine_config.epoch_interruption(true);
    let engine = Engine::new(&engine_config)?;
    let module = Module::from_file(&engine, wasm_path)?;

    let mut linker: Linker<AgentState> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |state| &mut state.wasi)?;
    abi::register(&mut linker)?;

    let stdout = MemoryOutputPipe::new(64 * 1024);

    let mut builder = WasiCtxBuilder::new();
    builder
        .preopened_dir(agent_home, "/", DirPerms::all(), FilePerms::all())?
        .stdout(stdout.clone())
        .inherit_stderr();
    builder.arg("agent"); // argv[0]; guest reads real args from index 1
    for a in args {
        builder.arg(a);
    }
    let wasi_ctx: WasiP1Ctx = builder.build_p1();

    let limits = StoreLimitsBuilder::new().memory_size(MEMORY_CAP_BYTES).build();
    let state = AgentState::new(agent_home_path.clone(), config, secrets, wasi_ctx, limits);
    let mut store = Store::new(&engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(1);
    store.epoch_deadline_trap();

    // ticks the deadline exactly once after `epoch_timeout` — a stuck guest
    // traps instead of hanging the kernel forever (PROJECT.md 4.1)
    let epoch_engine = engine.clone();
    std::thread::spawn(move || {
        std::thread::sleep(epoch_timeout);
        epoch_engine.increment_epoch();
    });

    let instance = linker.instantiate(&mut store, &module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    // guest failures (traps, wasi proc_exit) are expected for escape-attempt
    // tests, so swallow them here — the caller checks captured stdout instead.
    let _ = start.call(&mut store, ());

    let sleep_until = store.data().sleep_until;
    drop(store);
    let stdout = String::from_utf8_lossy(&stdout.contents()).into_owned();

    // A real `run` invocation (`agent_loop.rs`'s `run()`) always ends with a
    // `RESULT:` line, success or not — `run()`'s own error paths still print
    // one. Its absence means the guest got trapped (most likely this epoch
    // deadline, mid-turn — e.g. right after deciding on a `chat_send` but
    // before executing it) partway through and never got back to finish,
    // which otherwise fails *completely* silently: no `RESULT:`, no
    // `write_memory_note` entry, nothing `notify`'d from inside the guest
    // (it never got the chance). Surfacing it here is the only place left
    // that still can.
    let trapped = !stdout.contains("RESULT:");
    if trapped {
        let _ = logs::notify(
            &agent_home_path,
            &format!("run produced no result (likely trapped at the {epoch_timeout:?} epoch deadline mid-turn) — args: {args:?}"),
        );
    }

    // PROJECT.md 4.1: guest stdio goes to logs, not the kernel's own terminal
    let _ = logs::append_jsonl(
        &agent_home_path.join("logs/stdout.jsonl"),
        &serde_json::json!({"ts": logs::now_unix_secs(), "stdout": stdout}),
    );

    // Phase 2: "brain time machine" — commit whatever this run changed so a
    // corrupted memory/notes state can be rolled back with `git checkout`.
    if let Err(e) = autocommit::commit_run(&agent_home_path, &format!("run at {}", logs::now_unix_secs())) {
        let _ = logs::notify(&agent_home_path, &format!("auto-commit failed: {e}"));
    }

    Ok(RunOutcome { stdout, sleep_until, trapped })
}

/// `EBINA_SECRETS` env var if set, else `<parent of agent_home>/secrets.toml`
/// — a sibling of agent_home, never inside it (PROJECT.md 4.8: the guest's
/// WASI preopen root is exactly agent_home, so a sibling path is
/// structurally unreachable from the sandbox regardless of naming).
pub fn secrets_path(agent_home: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("EBINA_SECRETS") {
        return PathBuf::from(p);
    }
    agent_home
        .parent()
        .map(|p| p.join("secrets.toml"))
        .unwrap_or_else(|| PathBuf::from("secrets.toml"))
}
