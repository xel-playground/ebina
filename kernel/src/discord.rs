use crate::gateway::AppState;
use serenity::all::{ChannelId, Context, CreateMessage, EventHandler, GatewayIntents, Message, Ready, UserId};
use serenity::async_trait;
use serenity::http::Http;
use serenity::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Discord messages over 2000 chars get rejected by the API outright — long
/// replies get split into multiple messages instead of truncated.
const MAX_DISCORD_LEN: usize = 2000;

/// Optional adapter, same idea as the (still unbuilt) Telegram one in
/// PROJECT.md's 未來糖果罐: attaches to the gateway, doesn't touch kernel
/// core. Only runs if a `discord_bot_token` secret is configured — no token,
/// no Discord connection, gateway works exactly as before.
///
/// Only replies to a DM *from the paired owner* or an @mention in a guild
/// channel (not every message in every channel the bot can see — too
/// noisy/expensive otherwise, and a DM from anyone else is silently
/// ignored — 2026-07-12, previously any stranger's DM got a full reply in
/// their own isolated session). Each DM/channel gets its own chat session
/// (`discord-dm-<user>` / `discord-channel-<channel>`,
/// `gateway::handle_chat_message`) so Discord conversations don't bleed
/// into the webui's session or each other's.
///
/// `chat_sessions/<key>/session.json` is the single source of truth for
/// what's been said — the *only* thing that ever sends an outbound Discord
/// message is `session_watch_loop` noticing a new `assistant`-role turn
/// appended to one of these files, whether that turn came from a live
/// reply (`handle_chat_message`) or a proactive `chat_send`. No separate
/// "reply directly" path, so there's exactly one way this ever happens —
/// never two mechanisms that could drift out of sync with each other.
///
/// A one-time pairing step (DM the bot a numeric code shown in the gateway
/// log / `GET /api/discord/pairing`) records one Discord user as the
/// "owner" — the default target for `chat_send`'s `target: "discord"`, so
/// the agent has somewhere to proactively push to without needing to
/// already know a channel/user id.
///
/// `!reset`/`!compact`, sent as a DM or an @mention, work per-session just
/// like the webui Chat panel's buttons — Discord threads have no such
/// button of their own and would otherwise just grow forever. A session
/// also auto-compacts on its own once it crosses `config.chat.
/// auto_compact_tokens` (`gateway::maybe_auto_compact`).
pub(crate) async fn run(state: Arc<AppState>) {
    let secrets = crate::secrets::Secrets::load(&crate::secrets_path(&state.agent_home));
    let Some(token) = secrets.get("discord_bot_token").map(str::to_string) else {
        println!("[discord] no `discord_bot_token` secret configured — Discord adapter disabled");
        return;
    };

    match load_owner(&state.agent_home) {
        Some(user_id) => println!("[discord] already paired with Discord user id {user_id}"),
        None => println!("[discord] not paired yet — check GET /api/discord/pairing or the webui's Apps tab for the current code (rotates every 60s)"),
    }

    // MESSAGE_CONTENT is a privileged intent — must be toggled on for the
    // bot application in the Discord Developer Portal, or `msg.content`
    // arrives empty for guild messages (README has the setup steps).
    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::DIRECT_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let handler = Handler { state: state.clone(), bot_id: OnceLock::new() };

    match Client::builder(token, intents).event_handler(handler).await {
        Ok(mut client) => {
            tokio::spawn(session_watch_loop(client.http.clone(), state.agent_home.clone()));
            if let Err(e) = client.start().await {
                eprintln!("[discord] client error: {e}");
            }
        }
        Err(e) => eprintln!("[discord] failed to build client: {e}"),
    }
}

struct Handler {
    state: Arc<AppState>,
    /// set once in `ready`, read in `message` — used to detect "was this
    /// message an @mention of me" without needing serenity's `cache`
    /// feature (mentions are part of the message payload itself)
    bot_id: OnceLock<UserId>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, data_about_bot: Ready) {
        let _ = self.bot_id.set(data_about_bot.user.id);
        println!("[discord] connected as {}", data_about_bot.user.name);
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return; // ignore other bots and our own messages
        }
        let is_dm = msg.guild_id.is_none();

        // pairing: a DM whose *entire* content is the current (or previous
        // minute's, for grace) rotating code claims ownership, one-time —
        // checked before the normal reply gating below so it works even
        // before `ready` has set `bot_id`
        if is_dm && load_owner(&self.state.agent_home).is_none() && is_valid_pairing_code(&self.state.agent_home, msg.content.trim()) {
            match save_owner(&self.state.agent_home, msg.author.id) {
                Ok(()) => {
                    println!("[discord] paired with {} ({})", msg.author.name, msg.author.id);
                    let _ = msg.channel_id.say(&ctx.http, "配對成功,以後會把你當主人來推播訊息。").await;
                }
                Err(e) => eprintln!("[discord] failed to save pairing: {e}"),
            }
            return; // don't also treat the bare code as a chat message
        }

        let Some(&bot_id) = self.bot_id.get() else {
            return; // gateway not fully connected yet
        };
        let mentioned = msg.mentions.iter().any(|u| u.id == bot_id);
        // Diagnostic for the "mention in a thread doesn't seem to register"
        // report: this line fires for *every* non-bot guild message the
        // gateway actually delivers to us, before the reply-gate below, so
        // it tells apart two very different problems that look identical
        // from Discord's side — the event never reaching the bot at all
        // (nothing logged — likely a thread-membership/visibility gap on
        // Discord's end) vs. the event arriving but `mentioned` coming back
        // false (a real parsing bug on our end).
        if !is_dm {
            println!("[discord] message in channel {} (guild {:?}): mentioned={mentioned} mentions={:?}", msg.channel_id, msg.guild_id, msg.mentions.iter().map(|u| u.id).collect::<Vec<_>>());
        }
        if !is_dm && !mentioned {
            return; // only reply to a DM or an explicit @mention (avoid replying to every message in a busy channel)
        }
        // A DM from anyone but the paired owner gets silently ignored, not
        // just flagged as suspect after the fact — before this, `is_dm`
        // alone was enough to get a full conversational reply from a
        // complete stranger (their own isolated `discord-dm-<their-id>`
        // session, but a real reply all the same). Falls through here for
        // an unpaired vault too (`load_owner` returns `None`, which never
        // equals `Some(id)`) — that's correct: before pairing, the only DM
        // that should ever get a response is the exact rotating code,
        // already handled above.
        if is_dm && load_owner(&self.state.agent_home).as_deref() != Some(msg.author.id.to_string().as_str()) {
            return;
        }

        let text = strip_mention(&msg.content, bot_id);
        if text.is_empty() && msg.attachments.is_empty() {
            return;
        }

        let session_key =
            if is_dm { format!("discord-dm-{}", msg.author.id) } else { format!("discord-channel-{}", msg.channel_id) };

        // `!reset`/`!compact` — same archive-first mechanism as the webui
        // Chat panel's buttons, just addressed at this Discord session
        // specifically. Handled here (not as a normal chat turn) since
        // there's no reason to burn an `llm_call` deciding whether "!reset"
        // means what it obviously means.
        if text.eq_ignore_ascii_case("!reset") {
            let ok = crate::gateway::reset_session_key(&self.state, &session_key).await.get("ok").and_then(|v| v.as_bool()) == Some(true);
            let reply = if ok { "已重設對話,重新開始。" } else { "重設失敗。" };
            let _ = msg.channel_id.say(&ctx.http, reply).await;
            return;
        }
        if text.eq_ignore_ascii_case("!compact") {
            let typing = msg.channel_id.start_typing(&ctx.http);
            let outcome = crate::gateway::compact_session_key(self.state.clone(), &session_key).await;
            typing.stop();
            let ok = outcome.get("ok").and_then(|v| v.as_bool()) == Some(true);
            let reply = if ok { "已壓縮對話,保留重點繼續。" } else { "壓縮失敗。" };
            let _ = msg.channel_id.say(&ctx.http, reply).await;
            return;
        }

        // download each Discord attachment and stash it in agent_home the
        // same way a webui upload does (`gateway::save_attachment`) — one
        // consistent quota-counted, guest-visible storage path regardless
        // of which surface a file came in from
        let mut attachments = Vec::new();
        for att in &msg.attachments {
            match download_attachment(&att.url).await {
                Ok(bytes) => match crate::gateway::save_attachment(&self.state.agent_home, &att.filename, &bytes) {
                    Ok(path) => attachments.push(path),
                    Err(e) => eprintln!("[discord] attachment save failed ({}): {e}", att.filename),
                },
                Err(e) => eprintln!("[discord] attachment download failed ({}): {e}", att.filename),
            }
        }

        // Host-verified against the id saved at pairing time — being a DM
        // doesn't imply being the owner (anyone can DM the bot; pairing is
        // a deliberate one-time claim, see `is_valid_pairing_code`/
        // `save_owner` above), and a guild channel has no login of its own
        // either, so "whoever's typing" is never automatically the paired
        // owner in either case. Never trust conversation content for this;
        // `msg.author.id` comes from Discord's own signed gateway payload,
        // not from anything the model could be talked into believing.
        let is_owner = load_owner(&self.state.agent_home).as_deref() == Some(msg.author.id.to_string().as_str());
        // `discord-` prefixed so this can never collide with webui's
        // `webui-owner` id if the two ever end up compared/logged side by
        // side — wording/phrasing itself lives in `handle_chat_message`,
        // this is just the raw facts.
        let sender = Some(crate::gateway::MessageSender { name: msg.author.name.clone(), id: format!("discord-{}", msg.author.id), is_owner });

        // Discord's native "Bot is typing…" indicator — `Typing` re-sends
        // it in the background on its own until `.stop()`/dropped, so one
        // call covers the whole (possibly many-second, multi-turn) run
        // rather than needing a manual repeat loop
        let typing = msg.channel_id.start_typing(&ctx.http);
        crate::gateway::handle_chat_message(self.state.clone(), session_key.clone(), text.clone(), attachments, Some("discord".to_string()), sender).await;
        typing.stop();
        // reply itself: handle_chat_message already appended it to
        // chat_sessions/<session_key>/session.json — session_watch_loop
        // picks it up from there and actually sends it, same as chat_send
    }
}

/// Fetched straight from Discord's CDN rather than downloaded once and
/// stored as-is at attachment-URL granularity — simplest correct thing, and
/// the result gets persisted into agent_home right after via
/// `save_attachment` anyway so this URL never needs to be reachable again.
async fn download_attachment(url: &str) -> Result<Vec<u8>, reqwest::Error> {
    Ok(reqwest::get(url).await?.bytes().await?.to_vec())
}

/// Strips the bot's own `<@id>`/`<@!id>` mention token out of the message
/// text — the model shouldn't have to see its own raw mention syntax.
fn strip_mention(content: &str, bot_id: UserId) -> String {
    content.replace(&format!("<@{bot_id}>"), "").replace(&format!("<@!{bot_id}>"), "").trim().to_string()
}

/// Splits by *char* count (not bytes — CJK text breaks on raw byte slicing)
/// into Discord's 2000-char message cap.
fn split_for_discord(text: &str) -> Vec<String> {
    text.chars().collect::<Vec<_>>().chunks(MAX_DISCORD_LEN).map(|c| c.iter().collect()).collect()
}

// ---- pairing (who is "the owner" this bot pushes proactive messages to) ----

fn owner_path(agent_home: &Path) -> PathBuf {
    agent_home.join("logs/discord_owner.json")
}

/// Lives under `crate::autocommit::PRIVATE_DIR`, not plain `logs/` — this
/// value lets anyone who reads it compute every future pairing code, so
/// unlike everything else in `logs/` it must never enter git history (see
/// `autocommit.rs`). `migrate_legacy_seed_path` moves an existing
/// pre-convention file over the first time this runs on an older agent-home.
fn pairing_seed_path(agent_home: &Path) -> PathBuf {
    agent_home.join(crate::autocommit::PRIVATE_DIR).join("discord_pairing_seed.json")
}

pub(crate) fn migrate_legacy_seed_path(agent_home: &Path) {
    let legacy = agent_home.join("logs/discord_pairing_seed.json");
    let current = pairing_seed_path(agent_home);
    if legacy.exists() && !current.exists() {
        if let Some(parent) = current.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::rename(&legacy, &current);
    }
}

pub(crate) fn load_owner(agent_home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(owner_path(agent_home)).ok()?;
    serde_json::from_str::<serde_json::Value>(&text).ok()?.get("user_id")?.as_str().map(str::to_string)
}

fn save_owner(agent_home: &Path, user_id: UserId) -> std::io::Result<()> {
    let path = owner_path(agent_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::json!({"user_id": user_id.to_string()}).to_string())
}

/// One-time random seed, generated once and persisted forever — mixed with
/// the current minute (below) so the pairing code rotates every 60s without
/// needing a background timer or any per-code file write, while staying
/// unguessable to anyone who hasn't seen the gateway's own log/API (wall
/// clock alone isn't enough without this seed).
fn pairing_seed(agent_home: &Path) -> u64 {
    migrate_legacy_seed_path(agent_home);
    if let Ok(text) = std::fs::read_to_string(pairing_seed_path(agent_home)) {
        if let Some(seed) = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("seed")?.as_u64()) {
            return seed;
        }
    }
    let seed = crate::logs::now_unix_nanos() as u64;
    let path = pairing_seed_path(agent_home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, serde_json::json!({"seed": seed}).to_string());
    seed
}

/// FNV-1a-style mix of `seed` and `minute` into a 6-digit code — same
/// hand-rolled-hash approach as `memory::content_hash` on the agent side,
/// no extra dependency for something this low-stakes (a short-lived local
/// setup step, not a long-term security boundary).
fn code_for_minute(seed: u64, minute: i64) -> String {
    let mut hash: u64 = seed ^ 0xcbf29ce484222325;
    for b in minute.to_le_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:06}", hash % 1_000_000)
}

/// The pairing code valid *right now* — rotates every 60s.
pub(crate) fn current_pairing_code(agent_home: &Path) -> String {
    let seed = pairing_seed(agent_home);
    code_for_minute(seed, crate::logs::now_unix_secs() / 60)
}

/// Accepts the current minute's code or the previous minute's — a 1-minute
/// grace window so a code that just rotated right as someone reads it and
/// sends it isn't rejected as stale.
fn is_valid_pairing_code(agent_home: &Path, candidate: &str) -> bool {
    let seed = pairing_seed(agent_home);
    let minute = crate::logs::now_unix_secs() / 60;
    candidate == code_for_minute(seed, minute) || candidate == code_for_minute(seed, minute - 1)
}

// ---- session watcher: chat_sessions/ is the only source of truth ----

/// Watches every `chat_sessions/discord-*/session.json` for newly appended
/// `assistant`-role turns and actually sends them to the right Discord
/// destination (parsed straight out of the session key). This is the
/// *only* place an outbound Discord message ever gets sent — a live reply
/// (`handle_chat_message`, called from `message` above) and a proactive
/// push (`chat_send`'s `target: "discord"`, kernel/src/syscalls/
/// chat_send.rs) both just append to the session file and let this loop
/// do the actual sending, so there's one mechanism instead of two that
/// could drift out of sync.
///
/// A session key is baselined (not replayed) the first time this loop
/// sees it — on gateway restart, existing history doesn't get re-sent,
/// only turns appended *after* that point.
async fn session_watch_loop(http: Arc<Http>, agent_home: PathBuf) {
    let mut last_counts: HashMap<String, usize> = HashMap::new();
    let sessions_dir = agent_home.join("logs/chat_sessions");
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(entries) = std::fs::read_dir(&sessions_dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(key) = path.file_name().and_then(|f| f.to_str()).map(str::to_string) else { continue };
            if !key.starts_with("discord-") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path.join("session.json")) else { continue };
            let Ok(turns) = serde_json::from_str::<Vec<serde_json::Value>>(&text) else { continue };

            let Some(&last) = last_counts.get(&key) else {
                last_counts.insert(key, turns.len()); // first sight — baseline, don't replay history
                continue;
            };
            if turns.len() < last {
                // shrank — a `!reset`/`!compact`/auto-compact replaced the
                // file out from under us. Resync to the new length instead
                // of leaving `last` stale: without this, growth past the
                // *old* `last` on the next tick would slice `turns[last..]`
                // past the end of the now-shorter vec and panic.
                last_counts.insert(key, turns.len());
                continue;
            }
            if turns.len() == last {
                continue;
            }
            for turn in &turns[last..] {
                if turn.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                    continue;
                }
                if let Some(content) = turn.get("content").and_then(|c| c.as_str()) {
                    send_to_session_key(&http, &key, content).await;
                }
            }
            last_counts.insert(key, turns.len());
        }
    }
}

async fn send_to_session_key(http: &Http, session_key: &str, message: &str) {
    if let Some(id) = session_key.strip_prefix("discord-dm-").and_then(|s| s.parse::<u64>().ok()) {
        let user_id = UserId::new(id);
        for chunk in split_for_discord(message) {
            if let Err(e) = user_id.direct_message(http, CreateMessage::new().content(chunk)).await {
                eprintln!("[discord] failed to send to {session_key}: {e}");
                break;
            }
        }
    } else if let Some(id) = session_key.strip_prefix("discord-channel-").and_then(|s| s.parse::<u64>().ok()) {
        let channel_id = ChannelId::new(id);
        for chunk in split_for_discord(message) {
            if let Err(e) = channel_id.say(http, chunk).await {
                eprintln!("[discord] failed to send to {session_key}: {e}");
                break;
            }
        }
    }
}
