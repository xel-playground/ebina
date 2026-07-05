use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn agent_wasm_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/../target/wasm32-wasip1/debug/agent.wasm")
}

fn scratch_agent_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ebina-limits-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn memory_cap_stops_oversized_allocation() {
    let home = scratch_agent_home("memory");
    let outcome = kernel::run_agent(home.to_str().unwrap(), &agent_wasm_path(), &["memory_hog"])
        .expect("run_agent should not error even if the guest aborts");
    assert!(
        !outcome.stdout.contains("RESULT:allocated"),
        "600MB allocation should've been stopped by the 512MB memory cap, got: {}",
        outcome.stdout
    );
}

#[test]
fn epoch_timeout_traps_a_stuck_guest() {
    let home = scratch_agent_home("epoch");
    let start = Instant::now();
    let outcome = kernel::run_agent_with_epoch_timeout(
        home.to_str().unwrap(),
        &agent_wasm_path(),
        &["spin_forever"],
        Duration::from_millis(300),
    )
    .expect("run_agent should not error even when the guest is trapped");
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "busy loop should've been trapped around 300ms, took {elapsed:?}"
    );
    assert!(
        !outcome.stdout.contains("RESULT:"),
        "guest should never reach its RESULT print, got: {}",
        outcome.stdout
    );
}
