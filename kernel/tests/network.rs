use std::fs;
use std::path::PathBuf;

fn agent_wasm_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/../target/wasm32-wasip1/debug/agent.wasm")
}

fn scratch_agent_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ebina-network-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn http_fetch_denies_localhost() {
    let home = scratch_agent_home("localhost");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_ssrf_localhost"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"denied_ip\""), "expected localhost to be denied, got: {out}");
}

#[test]
fn http_fetch_denies_rfc1918_private_range() {
    let home = scratch_agent_home("private");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_ssrf_private"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"denied_ip\""), "expected 192.168.x.x to be denied, got: {out}");
}

#[test]
fn http_fetch_denies_cloud_metadata_endpoint() {
    let home = scratch_agent_home("metadata");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_ssrf_metadata"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"denied_ip\""), "expected 169.254.169.254 to be denied, got: {out}");
}

#[test]
fn http_fetch_rejects_oversized_url() {
    let home = scratch_agent_home("longurl");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_long_url"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"url_too_long\""), "expected oversized url to be rejected, got: {out}");
}

/// Unlike the old GET-only `http_get`, `method` is real now — a `POST`
/// actually posts (this is the whole point of reintroducing write support,
/// domain-gated instead of ungated-via-ssh_exec-only). No grant gets
/// created either way (writes don't queue separately from `tofu_domain`
/// reads, see module docs).
#[test]
fn http_fetch_post_actually_posts() {
    let home = scratch_agent_home("post");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_post"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"ok\":true"), "expected the POST to complete, got: {out}");
    assert!(kernel::grants::load_grants(&home).is_empty(), "no grant should ever be created for a write — only tofu_domain gates the domain itself");
}

/// A `{secrets.NAME}` placeholder in a header must fail closed when `NAME`
/// isn't bound to the request's host in `[network].credentials` — the
/// request must never go out with the literal placeholder text, and must
/// never resolve a secret to a domain it wasn't explicitly bound to.
#[test]
fn http_fetch_rejects_unbound_secret_placeholder() {
    let home = scratch_agent_home("unboundsecret");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_secret_header"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"bad_secret_placeholder\""), "expected an unbound secret placeholder to be rejected, got: {out}");
}

/// real network calls to example.com — this environment has outbound
/// internet access; cap=1 means the second call must be rejected before
/// ever reaching the network
#[test]
fn http_fetch_daily_request_cap_is_enforced() {
    let home = scratch_agent_home("dailycap");
    fs::write(home.join("config.toml"), "[network]\ndaily_request_cap = 1\n").unwrap();

    let first = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_demo"])
        .expect("run_agent should not error")
        .stdout;
    assert!(first.contains("\"ok\":true"), "first request should succeed within cap, got: {first}");

    let second = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_demo"])
        .expect("run_agent should not error")
        .stdout;
    assert!(
        second.contains("\"code\":\"daily_cap_exceeded\""),
        "second request should be rejected once daily_request_cap=1 is exhausted, got: {second}"
    );
}
