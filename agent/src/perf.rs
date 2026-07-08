use crate::time::now_unix;
use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::time::Instant;

thread_local! {
    static CURRENT_TURN: Cell<u32> = Cell::new(0);
}

/// Called once per turn from `agent_loop.rs`'s main loop — every `record`
/// after this point gets tagged with this turn number, so
/// `/logs/performance.jsonl` can answer "which turn, which node, how long"
/// without threading a turn number through every individual call site.
pub fn set_turn(turn: u32) {
    CURRENT_TURN.with(|t| t.set(turn));
}

/// Appends one `{ts, turn, node, duration_ms}` line to
/// `/logs/performance.jsonl` — same guest-side append-only pattern as the
/// per-day memory log (`agent_loop.rs::write_memory_note`); not a syscall,
/// since the guest already has direct write access to its own preopened
/// root and this is purely informational, nothing a human needs to approve
/// or a host-side budget needs to gate.
pub fn record(node: &str, started: Instant) {
    let duration_ms = started.elapsed().as_millis() as u64;
    let turn = CURRENT_TURN.with(|t| t.get());
    let entry = serde_json::json!({"ts": now_unix(), "turn": turn, "node": node, "duration_ms": duration_ms});
    let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open("/logs/performance.jsonl") else {
        return;
    };
    let _ = writeln!(f, "{entry}");
}
