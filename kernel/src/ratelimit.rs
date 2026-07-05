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

    /// Blocks up to 3s waiting for a token. On success, resets the
    /// sustained-limit counter. On failure, returns retry_after and whether
    /// this is the Nth consecutive failure (sustained runaway signal).
    pub fn acquire(&mut self) -> Result<(), RateLimited> {
        let deadline = Instant::now() + BLOCK_BUDGET;
        loop {
            self.refill();
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                self.consecutive_limited = 0;
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                self.consecutive_limited += 1;
                let need = 1.0 - self.tokens;
                let retry_after_secs = need / self.refill_per_sec;
                return Err(RateLimited {
                    retry_after_secs,
                    sustained: self.consecutive_limited >= SUSTAINED_THRESHOLD,
                });
            }
            thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }
}
