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
fn http_get_denies_localhost() {
    let home = scratch_agent_home("localhost");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_get_ssrf_localhost"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"denied_ip\""), "expected localhost to be denied, got: {out}");
}

#[test]
fn http_get_denies_rfc1918_private_range() {
    let home = scratch_agent_home("private");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_get_ssrf_private"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"denied_ip\""), "expected 192.168.x.x to be denied, got: {out}");
}

#[test]
fn http_get_denies_cloud_metadata_endpoint() {
    let home = scratch_agent_home("metadata");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_get_ssrf_metadata"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"denied_ip\""), "expected 169.254.169.254 to be denied, got: {out}");
}

#[test]
fn http_get_rejects_oversized_url() {
    let home = scratch_agent_home("longurl");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_get_long_url"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"code\":\"url_too_long\""), "expected oversized url to be rejected, got: {out}");
}

/// `http_get`'s request shape has no `method` field at all — writes used to
/// queue for gateway approval (`http_write` grant) but that gate got
/// removed once `ssh_exec` existed as an ungated way to do the same thing
/// (see `http_get.rs` module docs). This isn't a runtime rejection of a
/// `method: "POST"` field — that field is just never read, so passing one
/// (as this fixture does, imitating an old caller that hasn't updated) is
/// silently ignored and a plain GET happens anyway. No grant gets created
/// either way, since there's no write path left to queue.
#[test]
fn http_get_ignores_a_method_field_and_always_gets() {
    let home = scratch_agent_home("post");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_get_post"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"ok\":true"), "expected a plain GET regardless of the `method` field, got: {out}");
    assert!(kernel::grants::load_grants(&home).is_empty(), "no grant should ever be created — there's no write path left");
}

/// real network calls to example.com — this environment has outbound
/// internet access; cap=1 means the second call must be rejected before
/// ever reaching the network
#[test]
fn http_get_daily_request_cap_is_enforced() {
    let home = scratch_agent_home("dailycap");
    fs::write(home.join("config.toml"), "[network]\ndaily_request_cap = 1\n").unwrap();

    let first = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_get_demo"])
        .expect("run_agent should not error")
        .stdout;
    assert!(first.contains("\"ok\":true"), "first request should succeed within cap, got: {first}");

    let second = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["http_get_demo"])
        .expect("run_agent should not error")
        .stdout;
    assert!(
        second.contains("\"code\":\"daily_cap_exceeded\""),
        "second request should be rejected once daily_request_cap=1 is exhausted, got: {second}"
    );
}
