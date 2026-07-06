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

#[test]
fn http_fetch_post_queues_for_gateway_approval() {
    let home = scratch_agent_home("post");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_fetch_post"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"pending_approval\""), "expected POST to queue for approval, got: {out}");

    let grants = kernel::grants::load_grants(&home);
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].kind, "http_write");
    assert_eq!(grants[0].status, "pending");
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
