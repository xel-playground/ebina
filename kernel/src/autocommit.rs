use std::path::Path;
use std::process::Command;

/// PROJECT.md Phase 2: every run ends with a commit of the whole agent-home
/// tree — the "brain time machine". A fresh instantiate each wake plus this
/// gives crash-safety and `git checkout` rollback if the agent corrupts its
/// own memory.
pub fn commit_run(agent_home: &Path, message: &str) -> anyhow::Result<()> {
    if !agent_home.join(".git").exists() {
        run_git(agent_home, &["init"])?;
        run_git(agent_home, &["config", "user.email", "agent@localhost"])?;
        run_git(agent_home, &["config", "user.name", "ebina-agent"])?;
    }

    run_git(agent_home, &["add", "-A"])?;

    // nothing staged (agent made no changes this run) — skip, don't pollute
    // history with empty commits
    let status = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(agent_home)
        .status()?;
    if status.success() {
        return Ok(());
    }

    run_git(agent_home, &["commit", "-q", "-m", message])?;
    Ok(())
}

fn run_git(agent_home: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("git").args(args).current_dir(agent_home).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
