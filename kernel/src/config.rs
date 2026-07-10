use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    pub llm: LlmConfig,
    pub embed: EmbedConfig,
    pub search: SearchConfig,
    pub budget: BudgetConfig,
    pub ratelimit: RateLimitConfig,
    pub db: DbConfig,
    pub network: NetworkConfig,
    pub chat: ChatConfig,
    pub disk: DiskConfig,
    pub ssh: SshConfig,
    pub runtime: RuntimeConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            llm: LlmConfig::default(),
            embed: EmbedConfig::default(),
            search: SearchConfig::default(),
            budget: BudgetConfig::default(),
            ratelimit: RateLimitConfig::default(),
            db: DbConfig::default(),
            network: NetworkConfig::default(),
            chat: ChatConfig::default(),
            disk: DiskConfig::default(),
            ssh: SshConfig::default(),
            runtime: RuntimeConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct LlmConfig {
    pub base_url: String,
    /// must be a vault placeholder like `{secrets.ollama}` — never a literal
    /// key. config.toml lives *inside* agent_home (the guest reads it to
    /// build its own system prompt), so a literal value here would leak
    /// straight into the sandbox. See `secrets::resolve_placeholder`.
    pub api_key: String,
    pub model: String,
    /// "anthropic" (x-api-key + anthropic-version headers, Messages API
    /// request/response shape), "ollama" (Bearer auth, /api/chat NDJSON
    /// streaming shape), or "openai" (Bearer auth, standard OpenAI chat
    /// completions shape — `choices[].message.content` +
    /// `usage.prompt_tokens/completion_tokens`, non-streaming; covers any
    /// OpenAI-compatible API such as Kimi/Moonshot, DeepSeek, etc.)
    pub provider: String,
    /// no reliable way to auto-detect vision support on an arbitrary
    /// OpenAI-compatible endpoint, so this is a manual toggle — when true,
    /// an image attachment on a chat turn gets embedded as an `image_url`
    /// content block (gateway.rs `SessionTurn::as_message`); when false,
    /// attachments are always left as a plain text reference the agent can
    /// `read_file` itself instead, so a non-vision model never receives a
    /// content shape it can't handle.
    pub supports_vision: bool,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            base_url: "https://api.anthropic.com/v1/messages".to_string(),
            api_key: "{secrets.anthropic}".to_string(),
            model: "claude-sonnet-5".to_string(),
            provider: "anthropic".to_string(),
            supports_vision: false,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct EmbedConfig {
    pub base_url: String,
    /// see `LlmConfig::api_key`
    pub api_key: String,
    pub model: String,
    /// "voyage" (OpenAI-style `data[].embedding` + `usage.total_tokens`) or
    /// "ollama" (`embeddings` array + `prompt_eval_count`, no auth needed
    /// for a local server but a placeholder is still required by schema)
    pub provider: String,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        EmbedConfig {
            base_url: "https://api.voyageai.com/v1/embeddings".to_string(),
            api_key: "{secrets.voyage}".to_string(),
            model: "voyage-3".to_string(),
            provider: "voyage".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SearchConfig {
    pub base_url: String,
    /// see `LlmConfig::api_key` — vault placeholder, never a literal key.
    /// Ignored entirely for `provider = "searxng"` (a local instance needs
    /// no auth), but the field/schema stays the same either way.
    pub api_key: String,
    pub max_results: u32,
    /// hard daily cap on search requests, same rollover semantics as
    /// `BudgetConfig` — most free-tier search APIs meter by request count
    pub daily_request_cap: u64,
    /// "tavily" (POST, JSON body incl. `api_key`) or "searxng" (GET query
    /// params, no auth — self-hosted, see README.md)
    pub provider: String,
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig {
            base_url: "https://api.tavily.com/search".to_string(),
            api_key: "{secrets.tavily}".to_string(),
            max_results: 5,
            daily_request_cap: 100,
            provider: "tavily".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct BudgetConfig {
    /// hard daily cap on `llm_call` tokens — separate counter and cap from
    /// `embed_daily_token_cap` below (originally one shared cap across both;
    /// split so a RAG reindex burst can't starve the actual chat budget, or
    /// vice versa)
    pub daily_token_cap: u64,
    /// hard daily cap on `embed` tokens (RAG indexing/search) — its own
    /// counter, `logs/embed-budget-state.json` (see `state.rs`
    /// `AgentState::new`), same daily-rollover semantics as `daily_token_cap`
    pub embed_daily_token_cap: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig {
            daily_token_cap: 1_000_000,
            embed_daily_token_cap: 1_000_000,
        }
    }
}

/// Enforced by a process-wide singleton now (`ratelimit::global`), shared
/// by every concurrent run in this `kernel`/`ebinactl` process — genuinely
/// global, not per-run. (Briefly wasn't: each `TokenBucket` used to be
/// rebuilt fresh per run in `AgentState::new`, which was indistinguishable
/// from global back when one shared `run_lock` guaranteed only one run
/// existed at a time, but stopped being once runs went per-session —
/// `gateway.rs`'s `AppState::session_locks` — since N concurrent runs would
/// each get their own full-capacity copy.)
#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct RateLimitConfig {
    pub llm_per_min: u32,
    pub http_per_min: u32,
    /// per-domain on top of the global bucket — polite to third parties,
    /// avoids a chatty agent getting its IP banned by hammering one site
    pub http_per_domain_per_min: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        RateLimitConfig { llm_per_min: 10, http_per_min: 30, http_per_domain_per_min: 10 }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct NetworkConfig {
    /// "open" (default, GET free) | "tofu" (new domain needs one-time
    /// approval, then permanent) | "allowlist" (only `allowlist` domains)
    pub get_mode: String,
    /// caps query-string-as-exfiltration-channel size in `open` mode
    pub url_max_len: usize,
    pub daily_request_cap: u64,
    pub allowlist: Vec<String>,
    /// `http_get`'s response body had no cap at all until a plain blog page
    /// (raw HTML — scripts, styles, all of it) came back at 400KB+ and blew
    /// a single `llm_call` past its 262144-token model limit two such pages
    /// in one run was enough. This truncates the body before it ever
    /// becomes a tool-result message.
    pub response_max_bytes: usize,
    /// how long the *full* stripped page (not just the `response_max_bytes`-
    /// truncated part handed back directly) stays cached under
    /// `workspace/.http_cache/` before a lazy sweep (run at the start of
    /// every `http_get` call) deletes it. The cache is what lets the model
    /// `read_file` past the truncation point via `read_file`'s own
    /// `start_line`/`byte_offset` paging instead of losing the rest of a
    /// long page outright — this just bounds how long that costs disk space
    /// for a page nobody asked for a second look at.
    pub http_cache_ttl_secs: u64,
    /// caps total bytes on disk under `workspace/.http_cache/` — TTL alone
    /// only bounds growth *over time*, not a burst of many unique pages
    /// fetched inside one TTL window. Checked after every write; oldest
    /// entries by mtime (the closest thing to "least recently fetched" this
    /// tracks — files here are write-once-per-URL, never touched again
    /// except by a re-fetch, so mtime is as good a recency signal as an
    /// explicit LRU list would be without the bookkeeping) get evicted
    /// first until back under budget.
    pub http_cache_max_bytes: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            get_mode: "open".to_string(),
            url_max_len: 2048,
            daily_request_cap: 500,
            allowlist: Vec::new(),
            response_max_bytes: 100_000,
            http_cache_ttl_secs: 24 * 3600,
            http_cache_max_bytes: 20 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct DbConfig {
    pub query_timeout_secs: u64,
}

impl Default for DbConfig {
    fn default() -> Self {
        DbConfig {
            query_timeout_secs: 10,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct RuntimeConfig {
    /// wasmtime epoch-interruption deadline for one *whole run* (every turn
    /// combined, not a per-step limit) — a genuinely stuck guest (infinite
    /// loop, no host calls at all) traps at this deadline instead of
    /// hanging the gateway forever. A well-behaved multi-turn task (several
    /// `http_get`/`llm_call` round-trips — e.g. a report pulling from a few
    /// RSS sources) can legitimately run long, so this needs real headroom:
    /// a run that hits this trap mid-turn is silently discarded — whatever
    /// the guest was about to do (even something already decided, like a
    /// `chat_send` the model had already committed to) never happens, with
    /// nothing written to disk from that point on.
    pub epoch_timeout_secs: u64,
    /// mid-*run* context cap — distinct from `chat.auto_compact_tokens`,
    /// which only compacts a chat *session* (`session.json`) between runs.
    /// This one watches a single run's own `messages` array as it grows
    /// turn over turn (tool results piling up — a long `ssh_exec`
    /// exploration or many `http_get`s in one run) and triggers an in-run
    /// compaction once the last `llm_call`'s `input_tokens` crosses this,
    /// so a single long-running turn loop can't blow its own context on its
    /// own accumulated history before ever finishing. 150,000 leaves
    /// headroom under the 262144-token model limit for the compaction call
    /// itself plus whatever growth happens before the next check.
    pub in_run_compact_tokens: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig { epoch_timeout_secs: 30 * 60, in_run_compact_tokens: 150_000 }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct ChatConfig {
    /// once a chat session's last-measured context window (`context_tokens`,
    /// see gateway.rs `last_chat_context_tokens`) exceeds this, the *next*
    /// chat turn on that session auto-compacts it in the background (same
    /// summarize-then-replace mechanism as `/api/session/compact`). Mainly
    /// for Discord threads, which have no manual reset button — left alone
    /// they'd grow the context window forever.
    pub auto_compact_tokens: u64,
}

impl Default for ChatConfig {
    fn default() -> Self {
        ChatConfig { auto_compact_tokens: 50_000 }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SshConfig {
    /// empty disables `ssh_exec` entirely (same "optional, no config, no
    /// connection" pattern as `discord_bot_token`) — the agent can never
    /// pick its own target, only the human editing this file can, so a
    /// prompt-injected command has nowhere else to reach
    pub host: String,
    pub port: u16,
    pub user: String,
    /// hard wall-clock cap on one `ssh_exec` call, independent of how much
    /// output the remote command produces — a `docker logs -f`-style
    /// command that never exits gets killed at this deadline rather than
    /// hanging the syscall (and, transitively, `run_lock` — see
    /// `ssh_exec.rs` module docs) forever
    pub timeout_secs: u64,
    /// caps combined stdout+stderr kept from one call — same reasoning as
    /// every other syscall's output cap, so a chatty/looping remote command
    /// can't blow up the LLM context or the log file
    pub max_output_bytes: usize,
}

impl Default for SshConfig {
    fn default() -> Self {
        SshConfig { host: String::new(), port: 22, user: "root".to_string(), timeout_secs: 30, max_output_bytes: 3 * 1024 * 1024 }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(default)]
pub struct DiskConfig {
    /// hard cap on agent-home's total on-disk size. Guest writes are
    /// checked in `agent_loop.rs`'s `write_file` action (sums the whole
    /// tree, refuses + `notify`s rather than growing past this — the kernel
    /// never intercepts individual guest writes, the preopened WASI dir has
    /// no hook for that short of a much deeper wasmtime-wasi change). Host
    /// writes (`gateway.rs`'s `POST /api/upload`, for chat attachments)
    /// check the same cap on the kernel side instead, since that path never
    /// goes through the guest at all. This struct exists so the value
    /// round-trips through the same `config.toml` the human already edits
    /// via `GET/POST /api/config`, same as every other cap in this file.
    pub quota_bytes: u64,
}

impl Default for DiskConfig {
    fn default() -> Self {
        DiskConfig { quota_bytes: 2 * 1024 * 1024 * 1024 }
    }
}

impl Config {
    /// Loads `agent_home/config.toml`, falling back to defaults for any
    /// field (or the whole file) that's missing.
    pub fn load(agent_home: &Path) -> anyhow::Result<Config> {
        let path = agent_home.join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }
}
