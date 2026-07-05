use std::fs;
use std::path::PathBuf;

fn agent_wasm_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/../target/wasm32-wasip1/debug/agent.wasm")
}

/// Fresh scratch agent-home per test so tests can't interfere with each other.
fn scratch_agent_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ebina-isolation-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn traversal_escape_is_blocked() {
    let home = scratch_agent_home("traversal");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["traversal"])
        .expect("run_agent should not error even on guest-side failure")
        .stdout;
    assert!(
        out.starts_with("RESULT:blocked:"),
        "expected traversal escape to be blocked, got: {out}"
    );
}

#[test]
fn absolute_path_stays_inside_agent_home() {
    let home = scratch_agent_home("abs");
    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["abs"])
        .expect("run_agent should not error even on guest-side failure")
        .stdout;
    assert!(
        out.starts_with("RESULT:blocked:"),
        "expected /etc/passwd to not exist inside agent-home, got: {out}"
    );
}

#[test]
fn symlink_escape_is_blocked() {
    let home = scratch_agent_home("symlink");
    // symlink inside agent-home pointing at a real host directory outside it
    std::os::unix::fs::symlink("/etc", home.join("escape_link")).unwrap();

    let out = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["symlink"])
        .expect("run_agent should not error even on guest-side failure")
        .stdout;
    assert!(
        out.starts_with("RESULT:blocked:"),
        "expected symlink escape to be blocked, got: {out}"
    );
}
