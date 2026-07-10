use crate::config::RateLimitConfig;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const BLOCK_BUDGET: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// consecutive limited acquisitions before we treat it as a runaway-loop signal
const SUSTAINED_THRESHOLD: u32 = 3;

pub struct RateLimited {
    pub retry_after_secs: f64,
    /// true once this bucket has been limited `SUSTAINED_THRESHOLD` times in a
    /// row — an early warning for a runaway loop, per PROJECT.md 4.6
    pub sustained: bool,
}

/// simple token bucket, refilled continuously at `per_minute / 60` tokens/sec
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
    consecutive_limited: u32,
}

impl TokenBucket {
    pub fn new(per_minute: u32) -> Self {
        let capacity = per_minute.max(1) as f64;
        TokenBucket {
            capacity,
            refill_per_sec: capacity / 60.0,
            tokens: capacity,
            last_refill: Instant::now(),
            consecutive_limited: 0,
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last_refill = now;
    }

    /// Non-blocking: refills, then takes one token if available right now.
    /// No sleep/retry here — callers sharing this bucket across concurrent
    /// runs (see `acquire_shared`/`acquire_domain` below) need to release
    /// the mutex between attempts, not hold it through a wait.
    fn try_take(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            self.consecutive_limited = 0;
            true
        } else {
            false
        }
    }

    fn record_limited(&mut self) -> RateLimited {
        self.consecutive_limited += 1;
        let need = 1.0 - self.tokens;
        RateLimited {
            retry_after_secs: need / self.refill_per_sec,
            sustained: self.consecutive_limited >= SUSTAINED_THRESHOLD,
        }
    }
}

/// Blocks up to 3s waiting for a token from a *shared* (mutex-guarded)
/// bucket — re-locks each attempt rather than holding the mutex for the
/// whole wait. Holding it throughout would mean one caller waiting on
/// refill freezes out every other concurrent run sharing this same global
/// bucket, recreating exactly the kind of global bottleneck per-session
/// locking (`gateway.rs`'s `AppState::session_locks`) was built to remove.
fn acquire_shared(bucket: &Mutex<TokenBucket>) -> Result<(), RateLimited> {
    let deadline = Instant::now() + BLOCK_BUDGET;
    loop {
        {
            let mut b = bucket.lock().unwrap();
            if b.try_take() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(b.record_limited());
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Same as `acquire_shared`, but for a per-domain bucket in a shared map —
/// creates the domain's bucket on first use.
fn acquire_domain(domains: &Mutex<HashMap<String, TokenBucket>>, domain: &str, cap: u32) -> Result<(), RateLimited> {
    let deadline = Instant::now() + BLOCK_BUDGET;
    loop {
        {
            let mut map = domains.lock().unwrap();
            let bucket = map.entry(domain.to_string()).or_insert_with(|| TokenBucket::new(cap));
            if bucket.try_take() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(bucket.record_limited());
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Process-wide rate limiters — `llm_per_min`/`http_per_min`/
/// `http_per_domain_per_min` are documented ("Every field here is a
/// per-run budget, not a true global one" — `config.rs`'s
/// `RateLimitConfig`) as per-run precisely because each `AgentState` used
/// to build its own fresh `TokenBucket`. Runs are per-session now, not
/// serialized by one global lock, so N concurrent runs each getting their
/// own full-capacity bucket meant the *real* aggregate rate was N times the
/// configured cap. This makes it a real, shared-across-the-process limit
/// instead — every `AgentState` in this same `kernel`/`ebinactl` process
/// acquires from the same buckets. Capacities are fixed at first use (a
/// `config.toml` edit changing these needs a restart to take effect,
/// same as it always effectively did — a per-run bucket never rebuilt from
/// a *changed* config mid-run either).
pub struct GlobalRateLimiters {
    llm: Mutex<TokenBucket>,
    embed: Mutex<TokenBucket>,
    http: Mutex<TokenBucket>,
    http_domains: Mutex<HashMap<String, TokenBucket>>,
    domain_cap: u32,
}

static GLOBAL: OnceLock<GlobalRateLimiters> = OnceLock::new();

pub fn global(cfg: &RateLimitConfig) -> &'static GlobalRateLimiters {
    GLOBAL.get_or_init(|| GlobalRateLimiters {
        llm: Mutex::new(TokenBucket::new(cfg.llm_per_min)),
        // matches the pre-existing (not this fix's) choice of reusing
        // llm_per_min's value for embed's own, separate bucket — no
        // dedicated `embed_per_min` field exists in `RateLimitConfig`
        embed: Mutex::new(TokenBucket::new(cfg.llm_per_min)),
        http: Mutex::new(TokenBucket::new(cfg.http_per_min)),
        http_domains: Mutex::new(HashMap::new()),
        domain_cap: cfg.http_per_domain_per_min,
    })
}

impl GlobalRateLimiters {
    pub fn acquire_llm(&self) -> Result<(), RateLimited> {
        acquire_shared(&self.llm)
    }

    pub fn acquire_embed(&self) -> Result<(), RateLimited> {
        acquire_shared(&self.embed)
    }

    pub fn acquire_http(&self) -> Result<(), RateLimited> {
        acquire_shared(&self.http)
    }

    pub fn acquire_http_domain(&self, domain: &str) -> Result<(), RateLimited> {
        acquire_domain(&self.http_domains, domain, self.domain_cap)
    }
}
