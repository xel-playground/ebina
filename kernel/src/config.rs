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
    /// request/response shape) or "ollama" (Bearer auth, /api/chat shape)
    pub provider: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            base_url: "https://api.anthropic.com/v1/messages".to_string(),
            api_key: "{secrets.anthropic}".to_string(),
            model: "claude-sonnet-5".to_string(),
            provider: "anthropic".to_string(),
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
    /// hard daily cap on total tokens across llm_call + embed
    pub daily_token_cap: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig {
            daily_token_cap: 1_000_000,
        }
    }
}

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
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            get_mode: "open".to_string(),
            url_max_len: 2048,
            daily_request_cap: 500,
            allowlist: Vec::new(),
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
