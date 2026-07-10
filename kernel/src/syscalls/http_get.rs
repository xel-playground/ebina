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
    let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let body_text = response.text().unwrap_or_default();
    let redacted = redact_secrets(&state.secrets, &body_text);
    // logged against the actual bytes received over the wire, before any
    // stripping/truncation below touches what's handed back to the agent
    log_egress(state, url_str, &host, Some(redacted.len()), None, &source);
    let _ = state.http_daily.record(1);

    let content = if looks_like_html(content_type.as_deref(), &redacted) { strip_html_tags(&redacted) } else { redacted };
    let body = truncate_bytes(&content, state.config.network.response_max_bytes);

    ok_json(serde_json::json!({"status": status, "body": body}))
}

fn looks_like_html(content_type: Option<&str>, body: &str) -> bool {
    if let Some(ct) = content_type {
        return ct.contains("text/html");
    }
    // no/ambiguous content-type header — sniff the start of the body
    let head = ascii_lowercase_same_len(&body.chars().take(200).collect::<String>());
    head.contains("<!doctype html") || head.contains("<html")
}

/// Lowercases only ASCII letters, leaving every other char (byte-width and
/// all) untouched — unlike `str::to_lowercase`, this can never change the
/// string's byte length (some Unicode chars expand under real lowercasing),
/// so byte offsets found in the result stay valid to slice the original
/// string with. Only used to case-insensitively search for ASCII markers
/// (`<script`, `<style`, `<!doctype html`) in possibly-non-ASCII HTML.
fn ascii_lowercase_same_len(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c }).collect()
}

/// Strips `<script>`/`<style>` blocks (content included) and every
/// remaining tag, leaving just text — raw HTML is mostly markup/JS/CSS
/// noise the model never asked for; this keeps far more actual *content*
/// per byte than blind truncation alone would (`truncate_bytes` still runs
/// after this, as a backstop on genuinely huge pages). Not a real parser —
/// good enough for "readable text for an LLM", not for anything that needs
/// exact HTML semantics.
fn strip_html_tags(html: &str) -> String {
    let no_scripts = remove_tag_blocks(html, "<script", "</script>");
    let no_styles = remove_tag_blocks(&no_scripts, "<style", "</style>");

    let mut out = String::with_capacity(no_styles.len() / 2);
    let mut in_tag = false;
    for c in no_styles.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }

    decode_entities(&collapse_whitespace(&out))
}

fn remove_tag_blocks(s: &str, start: &str, end: &str) -> String {
    let lower = ascii_lowercase_same_len(s); // same byte offsets as `s`
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    loop {
        let Some(start_rel) = lower[pos..].find(start) else {
            out.push_str(&s[pos..]);
            break;
        };
        let start_idx = pos + start_rel;
        out.push_str(&s[pos..start_idx]);
        let Some(end_rel) = lower[start_idx..].find(end) else {
            break; // unterminated block — drop the rest rather than guess
        };
        pos = start_idx + end_rel + end.len();
    }
    out
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", " ")
}

/// Truncates at a UTF-8 char boundary at or before `max_bytes` — a plain
/// byte-index slice can land mid-codepoint on non-ASCII content (this
/// project's primary chat language is CJK) and panic.
fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n...[truncated: response was {} bytes, kept first {}]", &s[..end], s.len(), end)
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

#[cfg(test)]
mod strip_tests {
    use super::*;

    #[test]
    fn strips_scripts_styles_and_tags_keeping_text() {
        let html = r#"<!DOCTYPE html><html><head><style>body{color:red}</style>
            <script>fetch('/evil').then(x=>console.log(x))</script></head>
            <body><h1>Hello &amp; welcome</h1><p>Some   text   here.</p></body></html>"#;
        let out = strip_html_tags(html);
        assert!(!out.contains("color:red"));
        assert!(!out.contains("console.log"));
        assert!(!out.contains('<'));
        assert!(out.contains("Hello & welcome"));
        assert!(out.contains("Some text here."));
    }

    #[test]
    fn looks_like_html_prefers_content_type_over_sniffing() {
        assert!(looks_like_html(Some("text/html; charset=utf-8"), "not html at all"));
        assert!(!looks_like_html(Some("application/json"), "<html>tricky</html>"));
        assert!(looks_like_html(None, "<!DOCTYPE html><html>...</html>"));
        assert!(!looks_like_html(None, "{\"just\":\"json\"}"));
    }

    #[test]
    fn truncate_bytes_never_splits_a_utf8_char() {
        // 3 bytes/char — a byte cap landing mid-character (100 isn't a
        // multiple of 3) is exactly the case that panics without the
        // char-boundary walk-back in `truncate_bytes`
        let s = "早".repeat(1000);
        let out = truncate_bytes(&s, 100); // must not panic
        assert!(out.starts_with("早早早"));
    }
}
