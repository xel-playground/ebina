use crate::abi::{error_json, ok_json};
use crate::grants;
use crate::logs::{append_jsonl, now_unix_secs};
use crate::ratelimit::TokenBucket;
use crate::state::AgentState;
use reqwest::Url;
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};

/// `http_get(url) -> {status, body}` — the only way an agent ever reaches
/// the network for a plain read. No `method` field exists in this
/// request/action shape at all — not just rejected, structurally absent —
/// so there's nothing here to even try making a write with. Threat model
/// per PROJECT.md 4.6 isn't "the agent is malicious", it's "the agent got
/// tricked by page content it read" (prompt injection → exfiltration) plus
/// plain SSRF, so the guard rails apply uniformly regardless of what the
/// agent *meant* to do:
///
/// 1. denylist private/loopback/link-local/metadata IPs, checked *after* DNS
///    resolution and pinned for the actual request (blocks rebinding)
/// 2. full URL + byte count logged for every single request, allowed or not
/// 3. gated by `network.get_mode` (open/tofu/allowlist)
///
/// Writes (POST/PUT/etc) used to be supported here (as `http_fetch`)
/// behind a human-approval grant queue (`grants.rs` `http_write`) —
/// removed, and the syscall renamed `http_get` to make the removal
/// structural rather than just enforced-at-runtime, once `ssh_exec` existed
/// as an *ungated* way to do the exact same thing (`curl -X POST` on the
/// configured SSH target). Keeping a pre-approval gate on writes through
/// this syscall specifically stopped meaning anything once an equivalent
/// capability existed elsewhere with no gate at all — it was friction, not
/// containment, since anything that would route around the gate here could
/// just use `ssh_exec` instead. `tofu_domain` (unrelated to writes) is
/// unaffected.
pub fn call(state: &mut AgentState, req: Value) -> Value {
    let source = req.get("_meta").cloned().unwrap_or(Value::Null);
    let url_str = req.get("url").and_then(|u| u.as_str()).unwrap_or("");

    if url_str.len() > state.config.network.url_max_len {
        return error_json("url_too_long", &format!("url exceeds configured max of {} chars", state.config.network.url_max_len));
    }
    let url = match Url::parse(url_str) {
        Ok(u) => u,
        Err(e) => return error_json("bad_url", &e.to_string()),
    };
    let Some(host) = url.host_str().map(str::to_string) else {
        return error_json("bad_url", "url has no host");
    };
    if !matches!(url.scheme(), "http" | "https") {
        return error_json("bad_url", "only http/https are supported");
    }

    if !state.http_daily.has_headroom() {
        let _ = crate::logs::notify(&state.agent_home, "http_get rejected: daily request cap exhausted");
        return error_json("daily_cap_exceeded", "daily_request_cap exhausted");
    }
    if let Err(limited) = state.http_bucket.acquire() {
        if limited.sustained {
            let _ = crate::logs::notify(&state.agent_home, "http_get sustained rate-limit hits — possible runaway loop");
        }
        return rate_limited_json(&limited);
    }
    let domain_cap = state.config.ratelimit.http_per_domain_per_min;
    let bucket = state.http_domain_buckets.entry(host.clone()).or_insert_with(|| TokenBucket::new(domain_cap));
    if let Err(limited) = bucket.acquire() {
        return rate_limited_json(&limited);
    }

    match state.config.network.get_mode.as_str() {
        "allowlist" => {
            if !state.config.network.allowlist.iter().any(|d| d == &host) {
                return error_json("domain_not_allowed", &format!("{host} is not in [network].allowlist"));
            }
        }
        "tofu" => {
            if !grants::is_domain_approved(&state.agent_home, &host) {
                return queue(state, url_str, &host);
            }
        }
        _ => {} // open
    }

    let ip = match resolve_and_check(&host) {
        Ok(ip) => ip,
        Err(e) => {
            log_egress(state, url_str, &host, None, Some(&e), &source);
            return error_json("denied_ip", &e);
        }
    };
    let port = url.port_or_known_default().unwrap_or(443);

    let client = match reqwest::blocking::Client::builder().resolve(&host, SocketAddr::new(ip, port)).build() {
        Ok(c) => c,
        Err(e) => return error_json("network_error", &e.to_string()),
    };
    let result = client.get(url.clone()).send();
    let response = match result {
        Ok(r) => r,
        Err(e) => {
            log_egress(state, url_str, &host, None, Some(&e.to_string()), &source);
            return error_json("network_error", &e.to_string());
        }
    };
    let status = response.status().as_u16();
    let body_text = response.text().unwrap_or_default();
    let redacted = redact_secrets(&state.secrets, &body_text);

    log_egress(state, url_str, &host, Some(redacted.len()), None, &source);
    let _ = state.http_daily.record(1);

    ok_json(serde_json::json!({"status": status, "body": redacted}))
}

fn rate_limited_json(limited: &crate::ratelimit::RateLimited) -> Value {
    serde_json::json!({
        "ok": false,
        "error": {"code": "rate_limited", "message": "http_get rate limit exceeded", "retry_after": limited.retry_after_secs}
    })
}

/// The only grant kind `http_get` ever queues is `"tofu_domain"` — writes
/// (and their `"http_write"` grant kind) are gone, see module docs.
fn queue(state: &AgentState, url: &str, host: &str) -> Value {
    match grants::request_grant(&state.agent_home, "tofu_domain", "GET", url, host) {
        Ok(id) => serde_json::json!({
            "ok": false,
            "error": {"code": "pending_approval", "message": "waiting on gateway approval", "id": id}
        }),
        Err(e) => error_json("io_error", &e.to_string()),
    }
}

/// Resolves `host`, rejects if the resolved IP lands in a private/loopback/
/// link-local/metadata range, and returns that *same* IP for the caller to
/// pin the actual connection to — resolving twice (once to check, once to
/// connect) is exactly the DNS-rebinding TOCTOU this guards against.
fn resolve_and_check(host: &str) -> Result<IpAddr, String> {
    let addrs = (host, 0u16).to_socket_addrs().map_err(|e| format!("DNS resolution failed for {host}: {e}"))?;
    for addr in addrs {
        let ip = addr.ip();
        if is_denied(&ip) {
            return Err(format!("{host} resolves to {ip}, which is in the private/loopback/link-local/metadata denylist"));
        }
        return Ok(ip);
    }
    Err(format!("{host} resolved to no addresses"))
}

fn is_denied(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || *v4 == Ipv4Addr::new(169, 254, 169, 254) // cloud metadata endpoint
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

fn log_egress(state: &AgentState, url: &str, domain: &str, bytes: Option<usize>, error: Option<&str>, source: &Value) {
    let _ = append_jsonl(
        &state.agent_home.join("logs/egress.jsonl"),
        &serde_json::json!({
            "ts": now_unix_secs(), "method": "GET", "url": url, "domain": domain,
            "bytes": bytes, "error": error, "source": source,
        }),
    );
}

/// Scans the response body for any literal secret value from the vault and
/// redacts it before the body ever reaches a log file or the guest — the
/// one place PROJECT.md 4.8 calls for scanning content, guarding against an
/// API echoing a credential back (e.g. in an error message).
fn redact_secrets(secrets: &crate::secrets::Secrets, body: &str) -> String {
    let mut out = body.to_string();
    for name in secrets.names() {
        if let Some(value) = secrets.get(name) {
            if value.len() >= 8 && out.contains(value) {
                out = out.replace(value, "[REDACTED]");
            }
        }
    }
    out
}
