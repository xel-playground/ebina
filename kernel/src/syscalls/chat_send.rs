use crate::abi::{error_json, ok_json};
use crate::filelock::FileLock;
use crate::state::AgentState;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// `chat_send(message, target?, channel_id?)` — proactively pushes one
/// message into a real chat surface, for a background-triggered run
/// (`cron`/`daily_maintenance`/`scheduled_task`) that has something worth
/// telling the human, not a reply to a live message (that's `done`'s
/// `summary`).
///
/// `target` is `"webui"` (default) or `"discord"` — either way this just
/// appends one `assistant`-role turn to `chat_sessions/<key>/session.json`.
/// `chat_sessions/` is the single source of truth for what's been said on
/// every surface: for webui that's simply what `/api/message` reads/writes
/// (shows up as a normal chat bubble next time the Chat panel opens); for
/// Discord, `discord.rs`'s `session_watch_loop` is what notices the new
/// turn and actually sends it through the live bot connection — this
/// syscall runs synchronously inside the wasm sandbox and has no direct
/// line to that async client, hence the file handoff rather than sending
/// straight from here.
///
/// `target: "discord"` defaults to the paired owner's DM (errors
/// `not_paired` if nobody's paired yet, see `GET /api/discord/pairing`) —
/// pass `channel_id` (the same id `discord-channel-<id>` sessions are
/// already keyed by, see `discord.rs`'s incoming-message handler) to push
/// to a specific guild channel instead. No extra gating beyond that: the
/// Discord API itself already bounds this to channels the bot has actually
/// been added to, same "containment is blast radius, not capability"
/// reasoning `ssh_exec` documents.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let Some(message) = req.get("message").and_then(|m| m.as_str()) else {
        return error_json("bad_request", "chat_send requires a string `message` field");
    };
    // an empty assistant turn saved to session.json poisons the session
    // forever, not just this one call — see gateway.rs `handle_chat_message`
    // for the full reasoning (Anthropic/OpenAI-style APIs 400 on *any*
    // empty-content message in `messages`, and this session's full history
    // gets resent every future turn).
    if message.trim().is_empty() {
        return error_json("bad_request", "chat_send's `message` must not be empty");
    }
    let session_key = match req.get("target").and_then(|t| t.as_str()).unwrap_or("webui") {
        "discord" => match channel_id_str(req.get("channel_id")) {
            Some(channel_id) => format!("discord-channel-{channel_id}"),
            None => {
                let owner_path = state.agent_home.join("logs/discord_owner.json");
                let Ok(text) = std::fs::read_to_string(&owner_path) else {
                    return error_json(
                        "not_paired",
                        "no Discord owner paired yet — tell the human to DM the bot the pairing code (GET /api/discord/pairing shows it)",
                    );
                };
                let Some(user_id) =
                    serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("user_id").and_then(|u| u.as_str()).map(str::to_string))
                else {
                    return error_json("io_error", "discord_owner.json is malformed");
                };
                format!("discord-dm-{user_id}")
            }
        },
        _ => "webui".to_string(),
    };
    append_assistant_turn(&state.agent_home, &session_key, message)
}

/// Accepts `channel_id` as either a JSON string or a number — Discord ids
/// overflow `f64`'s exact-integer range, so a model emitting one as a raw
/// JSON number risks silent precision loss; as a string it round-trips
/// exactly. Support both since a model won't reliably know to quote it.
fn channel_id_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// A `chat_send`'d turn has no natural preceding "user" turn to explain
/// itself — whoever's on the other end of this session (a human, or this
/// same agent reading its own history back on the next reply in that
/// thread) would otherwise see it as a reply floating with nothing to reply
/// *to*. The system-role note carries the "why" forward into `history`
/// (unlike a plain metadata field, which `SessionTurn::as_message` would
/// drop before it ever reaches the LLM).
const PROACTIVE_NOTE: &str = "(the agent proactively sent the next message on its own initiative — not a reply to anything said in this conversation)";

/// Locked (`session.json.lock`, same path convention as `grants.rs`/
/// `scheduler_tasks.rs`) against two things: another concurrent `chat_send`
/// racing this one (two background triggers proactively messaging the same
/// target at once), and — the more likely case in practice — a live
/// `message` turn on this exact session concurrently reaching its own
/// end-of-turn save (`gateway.rs handle_chat_message`, which takes the same
/// lock and reloads fresh before writing, see its doc comment). Without
/// this, whichever side's blind `load → mutate → save` finishes last simply
/// erases the other's turn — not a rare timing coincidence, but guaranteed
/// to happen if a `daily_maintenance`/`cron`/`scheduled_task` run
/// `chat_send`s while a human is mid-conversation on the same session,
/// since `session_locks` (the per-session tokio lock) only ever covered the
/// *triggering* run's own session — `chat_send`'s target session is never
/// that.
fn append_assistant_turn(agent_home: &Path, session_key: &str, message: &str) -> Value {
    let path = agent_home.join("logs/chat_sessions").join(session_key).join("session.json");
    let _lock = FileLock::acquire(path.with_extension("json.lock"), Duration::from_secs(5));
    let mut turns: Vec<Value> =
        std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let now = crate::logs::now_unix_secs();
    turns.push(serde_json::json!({"role": "system", "content": PROACTIVE_NOTE, "ts": now}));
    turns.push(serde_json::json!({"role": "assistant", "content": message, "ts": now}));

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return error_json("io_error", &e.to_string());
        }
    }
    match serde_json::to_vec_pretty(&turns) {
        Ok(bytes) => match std::fs::write(&path, bytes) {
            Ok(()) => ok_json(Value::Null),
            Err(e) => error_json("io_error", &e.to_string()),
        },
        Err(e) => error_json("io_error", &e.to_string()),
    }
}
