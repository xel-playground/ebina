use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Plain `create_new`-based advisory lock (atomic file creation, no `libc`/
/// `flock` dependency needed) for serializing a short critical section
/// across concurrent runs — used by `budget.rs` (daily cap read-modify-
/// write) and `autocommit.rs` (`git add`/`commit` racing on
/// `.git/index.lock`). Runs are per-session now, not serialized by one
/// global lock (see `gateway.rs`'s `AppState::session_locks`), so two
/// concurrent runs can genuinely touch the same shared file at once.
pub struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Spin-waits for the lock file to not exist, then atomically creates
    /// it. A lock file older than `stale_after` is treated as abandoned (a
    /// crashed holder) and force-removed rather than deadlocking every
    /// future caller forever — callers should size this to comfortably
    /// exceed how long their own critical section ever legitimately takes.
    ///
    /// Creates `path`'s parent directory up front — without this, a lock
    /// path whose directory doesn't exist yet (the very first `chat_send`
    /// to a brand-new Discord channel session, say) fails `create_new` with
    /// `NotFound` on *every* attempt, which is indistinguishable from "lock
    /// held" to the retry loop below: `Instant::now() >= deadline` goes
    /// true once and stays true forever after, so the "stale, force
    /// through" branch's own `remove_file` (on a file that was never
    /// created) just fails too and the loop never returns — an actual
    /// infinite spin, not merely a slow retry. Reproduced live: a
    /// `chat_send` targeting a never-before-seen `discord-channel-<id>`
    /// session hung a run forever until the whole gateway process was
    /// killed.
    pub fn acquire(path: PathBuf, stale_after: Duration) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let deadline = Instant::now() + stale_after;
        loop {
            match std::fs::OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(_) => return FileLock { path },
                Err(_) if Instant::now() >= deadline => {
                    let _ = std::fs::remove_file(&path); // stale lock from a crashed holder
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
