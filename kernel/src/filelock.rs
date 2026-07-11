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
    /// `NotFound`, indistinguishable from "lock held" to the retry loop
    /// below. Reproduced live: a `chat_send` targeting a never-before-seen
    /// `discord-channel-<id>` session hung a run forever until the whole
    /// gateway process was killed.
    ///
    /// That fix covers the one root cause actually hit, but the same
    /// "create_new keeps failing for a reason that isn't `stale_after`"
    /// shape could recur from something else (disk full, a permissions
    /// problem) — so this only ever attempts the "force through a stale
    /// lock" branch *once*. If `create_new` still fails right after
    /// removing what was assumed to be an abandoned lock, that's not a
    /// contested lock anymore, it's something actually broken about the
    /// path — returns `Err` instead of spinning on it forever. Callers that
    /// surface this to the model (e.g. `chat_send`) let it decide whether
    /// to retry; the rest log and skip rather than corrupt or hang.
    pub fn acquire(path: PathBuf, stale_after: Duration) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("could not create {} for lock {path:?}: {e}", parent.display()))?;
        }
        let deadline = Instant::now() + stale_after;
        let mut forced_once = false;
        loop {
            match std::fs::OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(_) => return Ok(FileLock { path }),
                Err(_) if Instant::now() >= deadline => {
                    if forced_once {
                        return Err(format!("could not acquire lock {path:?} even after forcing through an apparently-stale holder"));
                    }
                    forced_once = true;
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
