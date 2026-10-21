use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};

/// fd the kernel hands us the syscall channel on — mirrors
/// kernel/src/lib.rs::SYSCALL_FD
const SYSCALL_FD: i32 = 3;

struct Channel {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

fn channel() -> &'static Mutex<Channel> {
    static CHANNEL: OnceLock<Mutex<Channel>> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        // SAFETY: the kernel dup2()s one end of a socketpair onto this fd
        // before exec and nothing else in this process opens or closes it
        let stream = unsafe { UnixStream::from_raw_fd(SYSCALL_FD) };
        let writer = stream.try_clone().expect("syscall channel on fd 3 — is this running under the kernel?");
        Mutex::new(Channel { reader: BufReader::new(stream), writer })
    })
}

/// Calls host syscall `name` with JSON request `req` over the kernel's
/// line-delimited JSON channel (see kernel/src/abi.rs). Always returns a
/// JSON value: on transport failure, a synthesized `{"ok":false,...}`
/// envelope in the same shape the host would produce.
///
/// Every host call funnels through this one function regardless of which
/// action triggered it, so it's the single choke point to time every node
/// (`llm_call`, `http_get`, `ssh_exec`, `embed`, ...) without instrumenting
/// each of `agent_loop.rs`'s ~15 action arms individually — see `perf::record`.
pub fn call(name: &str, req: &Value) -> Value {
    let started = std::time::Instant::now();
    let result = call_inner(name, req);
    crate::perf::record(&format!("syscall:{name}"), started);
    result
}

fn call_inner(name: &str, req: &Value) -> Value {
    let mut line = serde_json::to_vec(&serde_json::json!({"name": name, "req": req})).expect("request must serialize");
    line.push(b'\n');

    let mut ch = channel().lock().unwrap_or_else(|e| e.into_inner());
    let mut response = String::new();
    let io_result = ch.writer.write_all(&line).and_then(|_| ch.reader.read_line(&mut response));
    match io_result {
        Ok(0) => transport_error("channel_closed", "kernel closed the syscall channel"),
        Ok(_) => serde_json::from_str(&response).unwrap_or_else(|e| transport_error("bad_response_json", &e.to_string())),
        Err(e) => transport_error("channel_error", &e.to_string()),
    }
}

fn transport_error(code: &str, message: &str) -> Value {
    serde_json::json!({"ok": false, "error": {"code": code, "message": message}})
}
