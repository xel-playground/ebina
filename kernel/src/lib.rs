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
pub mod schedule_log;
pub mod scheduler_tasks;
pub mod secrets;
pub mod state;
pub mod syscalls;
pub mod workspace_memory;

use anyhow::{Context, Result};
use config::Config;
use secrets::Secrets;
use state::AgentState;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// PROJECT.md 4.1: whole-run wall-clock timeout (fresh process every wake,
/// so a stuck agent just gets killed rather than needing a kernel restart).
/// Fallback only — [`run_agent`] prefers `config.toml`'s `[runtime]
/// epoch_timeout_secs` (see `config.rs` `RuntimeConfig`) and only falls
/// back to this if that config can't be loaded at all.
pub const DEFAULT_EPOCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// PROJECT.md 4.1: address-space cap per agent process (`RLIMIT_AS`)
pub const MEMORY_CAP_BYTES: usize = 512 * 1024 * 1024;
/// How much of the agent's stdout is kept — anything past this is drained
/// and dropped so the child never blocks on a full pipe.
const STDOUT_CAP_BYTES: usize = 64 * 1024;
/// The fd number the agent finds its syscall channel on (see `abi.rs`).
pub const SYSCALL_FD: i32 = 3;

pub struct RunOutcome {
    pub stdout: String,
    pub sleep_until: Option<i64>,
    /// `true` when the agent never produced a `RESULT:` line — see the
    /// `!stdout.contains("RESULT:")` check below. `gateway.rs`'s
    /// `run_trigger` uses this to hand back an actual "run trapped" summary
    /// instead of a bare `null` result, which otherwise renders as an
    /// unhelpful generic "(no summary)" in the webui's Schedule history panel.
    pub trapped: bool,
}

/// Spawn the native agent binary at `agent_path` fresh with `agent_home`
/// as its cwd, serve its syscalls until it exits, and return whatever it
/// wrote to stdout plus the timestamp it asked to be woken at (if it called
/// `sleep_until`).
///
/// env is cleared (no inherited env) so host secrets in the kernel's own
/// environment never reach the agent; the only way secrets reach the
/// network is through host-side syscalls (`llm_call`/`embed`/...).
pub fn run_agent(agent_home: &str, agent_path: &str, args: &[&str]) -> Result<RunOutcome> {
    run_agent_with_epoch_timeout(agent_home, agent_path, args, epoch_timeout_for(agent_home))
}

/// `config.toml`'s `[runtime] epoch_timeout_secs`, falling back to
/// [`DEFAULT_EPOCH_TIMEOUT`] if that config can't be loaded at all.
pub fn epoch_timeout_for(agent_home: &str) -> Duration {
    Config::load(&PathBuf::from(agent_home)).map(|c| Duration::from_secs(c.runtime.epoch_timeout_secs)).unwrap_or(DEFAULT_EPOCH_TIMEOUT)
}

/// Same as [`run_agent`] but with a configurable whole-run timeout —
/// exists so tests can exercise the kill path without waiting 30 minutes.
/// Production callers should use [`run_agent`].
pub fn run_agent_with_epoch_timeout(agent_home: &str, agent_path: &str, args: &[&str], epoch_timeout: Duration) -> Result<RunOutcome> {
    let agent_home_path = PathBuf::from(agent_home);
    let config = Config::load(&agent_home_path)?;
    let secrets = Secrets::load(&secrets_path(&agent_home_path));
    let canonical_home = agent_home_path.canonicalize().with_context(|| format!("agent_home {agent_home}"))?;

    // absolute before `current_dir` — a relative program path would
    // otherwise be resolved against agent_home after the chdir
    let agent_bin = Path::new(agent_path).canonicalize().with_context(|| format!("agent binary {agent_path}"))?;

    let (host_end, guest_end) = UnixStream::pair()?;
    let guest_fd = guest_end.as_raw_fd();

    let mut cmd = Command::new(&agent_bin);
    cmd.args(args)
        .current_dir(&canonical_home)
        .env_clear()
        .env("EBINA_AGENT_HOME", &canonical_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // SAFETY: only async-signal-safe libc calls between fork and exec
    unsafe {
        cmd.pre_exec(move || {
            // dup2 clears FD_CLOEXEC on the target, so the channel survives exec
            if libc::dup2(guest_fd, SYSCALL_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let cap = MEMORY_CAP_BYTES as libc::rlim_t;
            let limit = libc::rlimit { rlim_cur: cap, rlim_max: cap };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().with_context(|| format!("spawning agent {agent_path}"))?;
    // the child has its own copy now — dropping ours means EOF on
    // `host_end` once the child exits, which ends the syscall loop below
    drop(guest_end);

    let mut child_stdout = child.stdout.take().expect("stdout is piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match child_stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = STDOUT_CAP_BYTES.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        kept
    });

    // kills the child once `epoch_timeout` passes — a stuck agent dies
    // instead of hanging the kernel forever (PROJECT.md 4.1). Cancelled by
    // dropping `cancel_tx` once the syscall loop sees EOF; the child isn't
    // reaped until after this thread is joined, so its pid can't have been
    // recycled by the time `kill` runs.
    let pid = child.id() as libc::pid_t;
    let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = cancel_rx.recv_timeout(epoch_timeout) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    });

    let mut state = AgentState::new(agent_home_path.clone(), config, secrets);
    abi::serve(&mut state, host_end);

    drop(cancel_tx);
    let _ = watchdog.join();
    let _ = child.wait();
    let sleep_until = state.sleep_until;
    drop(state);
    let stdout = String::from_utf8_lossy(&stdout_reader.join().unwrap_or_default()).into_owned();

    // A real `run` invocation (`agent_loop.rs`'s `run()`) always ends with a
    // `RESULT:` line, success or not — `run()`'s own error paths still print
    // one. Its absence means the agent got killed (most likely this
    // timeout, mid-turn — e.g. right after deciding on a `chat_send` but
    // before executing it) or crashed partway through and never got back to
    // finish, which otherwise fails *completely* silently: no `RESULT:`, no
    // `write_memory_note` entry, nothing `notify`'d from inside the agent
    // (it never got the chance). Surfacing it here is the only place left
    // that still can.
    let trapped = !stdout.contains("RESULT:");
    if trapped {
        let _ = logs::notify(
            &agent_home_path,
            &format!("run produced no result (likely killed at the {epoch_timeout:?} run deadline mid-turn) — args: {args:?}"),
        );
    }

    // PROJECT.md 4.1: agent stdio goes to logs, not the kernel's own terminal
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
/// — a sibling of agent_home, never inside it (PROJECT.md 4.8: the agent's
/// filesystem root is exactly agent_home and its path layer refuses
/// anything resolving outside it, so a sibling path is never reachable
/// through the agent's own file actions regardless of naming).
pub fn secrets_path(agent_home: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("EBINA_SECRETS") {
        return PathBuf::from(p);
    }
    agent_home
        .parent()
        .map(|p| p.join("secrets.toml"))
        .unwrap_or_else(|| PathBuf::from("secrets.toml"))
}
