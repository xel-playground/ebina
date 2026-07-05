use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn agent_wasm_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/../target/wasm32-wasip1/debug/agent.wasm")
}

fn scratch_agent_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ebina-syscalls-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn db_exec_attach_is_denied() {
    let home = scratch_agent_home("attach");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["db_attach_escape"])
        .expect("run_agent should not error")
        .stdout;
    assert!(
        out.contains("\"code\":\"denied\""),
        "expected ATTACH to be denied by the authorizer, got: {out}"
    );
}

#[test]
fn db_exec_slow_query_is_interrupted_by_timeout() {
    let home = scratch_agent_home("timeout");
    // 1s timeout so the test doesn't have to wait long even if the
    // progress_handler wiring is broken and the query runs to completion
    fs::write(home.join("config.toml"), "[db]\nquery_timeout_secs = 1\n").unwrap();

    let start = Instant::now();
    let outcome = kernel::run_agent(
        home.to_str().unwrap(),
        &agent_wasm_path(),
        &["db_slow_query"],
    )
    .expect("run_agent should not error");
    let elapsed = start.elapsed();

    assert!(
        outcome.stdout.contains("\"code\":\"timeout\""),
        "expected slow query to hit the configured timeout, got: {}",
        outcome.stdout
    );
    assert!(
        elapsed.as_secs() < 10,
        "query should've been interrupted around 1s, took {elapsed:?}"
    );
}

#[test]
fn db_exec_create_insert_select_roundtrip() {
    let home = scratch_agent_home("roundtrip");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["db_exec"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"text\":\"hello from guest\""), "got: {out}");
}

#[test]
fn notify_writes_to_notifications_log() {
    let home = scratch_agent_home("notify");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["notify"])
        .expect("run_agent should not error")
        .stdout;
    assert!(out.contains("\"ok\":true"), "got: {out}");

    let log = fs::read_to_string(home.join("logs/notifications.jsonl")).expect("notifications.jsonl should exist");
    assert!(log.contains("hello from agent guest"));
}

#[test]
fn sleep_until_is_reported_back_to_host() {
    let home = scratch_agent_home("sleep");
    let outcome = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["sleep_until"])
        .expect("run_agent should not error");
    assert_eq!(outcome.sleep_until, Some(1_900_000_000));
}
