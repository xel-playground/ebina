use std::path::{Path, PathBuf};
use std::process::Command;

/// PROJECT.md Phase 2: every run ends with a commit of the whole agent-home
/// tree — the "brain time machine". A fresh instantiate each wake plus this
/// gives crash-safety and `git checkout` rollback if the agent corrupts its
/// own memory.
///
/// The git-dir itself lives *outside* agent_home (a sibling, same trick as
/// `secrets_path`) — never `agent_home/.git`. The guest's WASI preopen root
/// is exactly agent_home with full read/write, so a `.git` sitting inside it
/// would be just as writable as any other file: a prompt-injected agent
/// could corrupt objects, rewrite refs, or `reset --hard` its own history,
/// destroying the very tamper-evidence trail this function exists to
/// provide. Keeping the git-dir external makes it structurally unreachable
/// from the sandbox, same guarantee secrets.toml already has.
pub fn commit_run(agent_home: &Path, message: &str) -> anyhow::Result<()> {
    let git_dir = git_dir_path(agent_home);
    migrate_legacy_git_dir(agent_home, &git_dir)?;

    if !git_dir.exists() {
        run_git(agent_home, &git_dir, &["init", "-q"])?;
        run_git(agent_home, &git_dir, &["config", "user.email", "agent@localhost"])?;
        run_git(agent_home, &git_dir, &["config", "user.name", "ebina-agent"])?;
    }

    ensure_private_dir_excluded(&git_dir)?;
    untrack_private_dir(agent_home, &git_dir)?;

    run_git(agent_home, &git_dir, &["add", "-A"])?;

    // nothing staged (agent made no changes this run) — skip, don't pollute
    // history with empty commits
    let status = Command::new("git")
        .args(["--git-dir", &git_dir.to_string_lossy(), "--work-tree", &agent_home.to_string_lossy(), "diff", "--cached", "--quiet"])
        .status()?;
    if status.success() {
        return Ok(());
    }

    run_git(agent_home, &git_dir, &["commit", "-q", "-m", message])?;
    Ok(())
}

/// Convention, not a maintained list: anything that must exist and keep
/// working inside agent_home (the guest and whatever syscall relies on it
/// still needs it on disk) but must never enter permanent git history goes
/// under this one directory. A path-list would need a code change — and
/// someone remembering to make it — every time a new feature introduces
/// another secret-equivalent file (see `discord.rs`'s pairing seed, the
/// case that prompted this); a directory means the exclusion rule never
/// needs to change again, only where new files land.
///
/// Excluded via `<git_dir>/info/exclude` rather than a `.gitignore` inside
/// agent_home, deliberately: a `.gitignore` would live in the guest's own
/// writable work-tree, letting a prompt-injected agent add its own
/// exclusions to hide tampering from the audit trail this whole module
/// exists to provide. `info/exclude` lives in the external git-dir instead
/// — as structurally unreachable from the sandbox as the git-dir itself
/// (see the doc comment above).
pub(crate) const PRIVATE_DIR: &str = "logs/private";

/// Idempotently ensures `PRIVATE_DIR` (trailing slash — matches the whole
/// directory, not a same-named file) is in `info/exclude` — runs every
/// `commit_run`, so an agent-home created before this existed still gets
/// covered on its next run, not just brand-new ones.
fn ensure_private_dir_excluded(git_dir: &Path) -> anyhow::Result<()> {
    let exclude_path = git_dir.join("info/exclude");
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    let rule = format!("{PRIVATE_DIR}/");
    if existing.lines().any(|l| l == rule) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&rule);
    updated.push('\n');
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&exclude_path, updated)?;
    Ok(())
}

/// `info/exclude` only stops *future* `add -A` calls from picking files in
/// `PRIVATE_DIR` back up — anything already committed in an earlier run
/// (e.g. before this directory convention existed) is still tracked and
/// keeps getting included every commit after that regardless of the
/// exclude rule. `git rm -r --cached` drops the whole directory from the
/// index (working files on disk untouched) so the exclude rule actually
/// takes effect going forward. `--ignore-unmatch` since most runs have
/// nothing tracked there to remove — that's not an error. Doesn't touch
/// history already written; scrubbing past commits needs a separate,
/// destructive `git filter-repo` pass this function deliberately doesn't
/// take on its own.
fn untrack_private_dir(agent_home: &Path, git_dir: &Path) -> anyhow::Result<()> {
    run_git(agent_home, git_dir, &["rm", "-r", "--cached", "-q", "--ignore-unmatch", PRIVATE_DIR])
}

/// `<parent of agent_home>/.git` — a sibling of agent_home, not a child of
/// it (same convention as `secrets_path` in lib.rs), just using the
/// conventional dotfile name instead of a derived one.
fn git_dir_path(agent_home: &Path) -> PathBuf {
    agent_home.parent().map(|p| p.join(".git")).unwrap_or_else(|| PathBuf::from(".git"))
}

/// One-time migration for agent-homes created before the git-dir moved
/// outside the sandbox: if an old `agent_home/.git` exists and the new
/// external location doesn't yet, move the whole history over rather than
/// starting fresh and losing it.
fn migrate_legacy_git_dir(agent_home: &Path, git_dir: &Path) -> anyhow::Result<()> {
    let legacy = agent_home.join(".git");
    if legacy.exists() && !git_dir.exists() {
        std::fs::rename(&legacy, git_dir)?;
    }
    Ok(())
}

fn run_git(agent_home: &Path, git_dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("git")
        .args(["--git-dir", &git_dir.to_string_lossy(), "--work-tree", &agent_home.to_string_lossy()])
        .args(args)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
