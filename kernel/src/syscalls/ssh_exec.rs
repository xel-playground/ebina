use crate::abi::{error_json, ok_json};
use crate::logs::{append_jsonl, now_unix_secs};
use crate::state::AgentState;
use serde_json::Value;
use ssh2::Session;
use std::io::Read;
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// `ssh_exec(command) -> {stdout, stderr, exit_code}` — runs one command on
/// a single, human-fixed SSH target (`config.toml` `[ssh]` — host/port/user,
/// never chosen by the agent) authenticating with a private key that lives
/// outside agent_home (`secrets.toml` `ssh_key_path`, resolved host-side,
/// the guest never sees the key itself or its passphrase).
///
/// This is deliberately the one syscall in this codebase that hands the
/// agent something close to a real shell — there's no bounded operation set
/// like `db_exec`'s SQL authorizer or the (now-removed) `exec_wasm`'s wasm
/// sandbox. The containment here is entirely about *blast radius*, not
/// *capability*: fixed target (can't be redirected to some other host via
/// injection), no interactive shell/pty (one command in, one result out,
/// same shape as `db_exec`/`http_get`), a hard wall-clock deadline, an
/// output size cap, and a full audit log — not a sandbox around what the
/// command itself can do once it's running on that target.
///
/// The wall-clock deadline matters more here than anywhere else in this
/// file: locking is per-session/per-scheduled-task now, not one global
/// mutex (`AppState::session_locks`/`task_run_locks`, kernel/src/gateway.rs)
/// — but a stuck `ssh_exec` still freezes whatever *did* queue behind it
/// (that session's next message, or the next tick of that same scheduled
/// task), and `/api/abort`'s cooperative flag never reaches a run blocked
/// here (only `llm_call`'s stream loop checks it). A command like
/// `docker logs -f` never exits on its own; without a deadline that's
/// independent of how much output is still trickling in, one `ssh_exec`
/// call would hang its session/task forever. `session.set_timeout` alone
/// doesn't cover this (it's an *idle* timeout — a command still actively
/// producing output keeps resetting it) so there's a separate
/// `Instant`-based deadline checked every read.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let Some(command) = req.get("command").and_then(|c| c.as_str()) else {
        return error_json("bad_request", "ssh_exec requires a string `command` field");
    };
    let source = req.get("_meta").cloned().unwrap_or(Value::Null);

    let result = if state.config.ssh.host.is_empty() {
        Err(("not_configured", "no `[ssh]` host configured in config.toml — ssh_exec is disabled".to_string()))
    } else if let Some(key_path) = state.secrets.get("ssh_key_path").map(str::to_string) {
        let passphrase = state.secrets.get("ssh_key_passphrase").map(str::to_string);
        let cfg = state.config.ssh.clone();
        let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs);
        run_command(&cfg, &key_path, passphrase.as_deref(), command, deadline).map_err(|e| ("ssh_error", e))
    } else {
        Err(("not_configured", "no `ssh_key_path` secret in the vault — ssh_exec is disabled".to_string()))
    };

    let (response, log_entry) = match result {
        Ok(outcome) => (
            ok_json(serde_json::json!({
                "stdout": outcome.stdout, "stderr": outcome.stderr,
                "exit_code": outcome.exit_code, "timed_out": outcome.timed_out,
            })),
            serde_json::json!({
                "ts": now_unix_secs(), "command": command, "exit_code": outcome.exit_code,
                "timed_out": outcome.timed_out, "stdout_bytes": outcome.stdout.len(),
                "stderr_bytes": outcome.stderr.len(), "error": Value::Null, "source": source,
            }),
        ),
        Err((code, e)) => (
            error_json(code, &e),
            serde_json::json!({
                "ts": now_unix_secs(), "command": command, "exit_code": Value::Null,
                "timed_out": false, "stdout_bytes": 0, "stderr_bytes": 0, "error": e, "source": source,
            }),
        ),
    };
    let _ = append_jsonl(&state.agent_home.join("logs/ssh.jsonl"), &log_entry);
    response
}

struct Outcome {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
}

fn run_command(
    cfg: &crate::config::SshConfig,
    key_path: &str,
    passphrase: Option<&str>,
    command: &str,
    deadline: Instant,
) -> Result<Outcome, String> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let tcp = TcpStream::connect(&addr).map_err(|e| format!("connect to {addr} failed: {e}"))?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(cfg.timeout_secs)));

    let mut sess = Session::new().map_err(|e| format!("session init failed: {e}"))?;
    // idle timeout on individual blocking calls — a genuinely dead
    // connection errors out here rather than blocking forever; a live one
    // that just keeps streaming output is caught by the `deadline` check in
    // the read loop below instead, since this alone wouldn't catch that
    sess.set_timeout((cfg.timeout_secs * 1000) as u32);
    sess.set_tcp_stream(tcp);
    sess.handshake().map_err(|e| format!("handshake failed: {e}"))?;
    sess.userauth_pubkey_file(&cfg.user, None, Path::new(key_path), passphrase)
        .map_err(|e| format!("authentication failed: {e}"))?;
    if !sess.authenticated() {
        return Err("authentication failed (unknown reason)".to_string());
    }

    let mut channel = sess.channel_session().map_err(|e| format!("channel open failed: {e}"))?;
    channel.exec(command).map_err(|e| format!("exec failed: {e}"))?;

    let (stdout, timed_out_1) = read_capped(&mut channel, cfg.max_output_bytes, deadline);
    let (stderr, timed_out_2) = if timed_out_1 {
        (Vec::new(), true)
    } else {
        let mut stderr_stream = channel.stderr();
        read_capped(&mut stderr_stream, cfg.max_output_bytes, deadline)
    };
    let timed_out = timed_out_1 || timed_out_2;

    if timed_out {
        let _ = channel.close();
    } else {
        let _ = channel.send_eof();
        let _ = channel.wait_close();
    }
    let exit_code = if timed_out { -1 } else { channel.exit_status().unwrap_or(-1) };

    Ok(Outcome {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
        timed_out,
    })
}

/// Reads until EOF, `max_bytes`, or `deadline`, whichever comes first —
/// checked every iteration regardless of whether data is still arriving, so
/// a command that never stops producing output (`docker logs -f`) still
/// gets cut off instead of running past the deadline just because it kept
/// the idle timer from firing.
fn read_capped(r: &mut impl Read, max_bytes: usize, deadline: Instant) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if Instant::now() >= deadline || buf.len() >= max_bytes {
            return (buf, true);
        }
        match r.read(&mut chunk) {
            Ok(0) => return (buf, false),
            Ok(n) => {
                let remaining = max_bytes.saturating_sub(buf.len());
                buf.extend_from_slice(&chunk[..n.min(remaining)]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => return (buf, false), // idle-timeout or connection error — treat as end of stream
        }
    }
}
