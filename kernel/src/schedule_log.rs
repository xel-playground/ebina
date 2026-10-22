//! Event-sourced state for every scheduler-driven run (the three maintenance
//! tiers, the `cron` self-wake, and each `scheduled_task`).
//!
//! The unit of work is an *occurrence*: one specific due instance of a job,
//! identified by `<job>@<slot>` (e.g. `task:abc@1783328400`,
//! `daily_maintenance@1783329180`, `cron@1792616613`). That id is the
//! idempotency key — `schedule` with an id that already exists is a no-op,
//! so however many ticks (or restarts) notice the same due slot, it's
//! scheduled exactly once.
//!
//! Every state change is an append-only line in
//! `logs/scheduler_events.jsonl`; the current state of each occurrence is
//! just the fold of its events, rebuilt from that file on startup. Nothing
//! else about scheduling lives in memory, so a kernel restart loses nothing:
//!
//!   scheduled ─▶ pending ──claim──▶ running ──▶ succeeded
//!                  ▲                   │
//!                  └── retry_at ◀── failed (attempt ≤ MAX_RETRIES)
//!                                      └──▶ failed, terminal (retries spent)
//!   scheduled ─▶ skipped   (due, but nothing to do — maintenance short-circuit)
//!
//! `claim_ready` writes `started` under the same lock it checks state with,
//! so one occurrence can never be started twice. An occurrence still
//! `running` when the kernel boots was cut off mid-run (crash/restart) —
//! `recover_interrupted` turns it into an ordinary failed attempt so the
//! normal retry policy picks it up.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A failed attempt is retried after this long — same 15-minute backoff
/// the scheduler has always used.
pub const RETRY_BACKOFF_SECS: i64 = 900;
/// Retries after the first attempt, so at most `1 + MAX_RETRIES` attempts
/// per occurrence. A persistently failing job (e.g. a provider-side content
/// filter) would otherwise burn budget forever; its next natural slot is a
/// fresh occurrence with a fresh retry budget.
pub const MAX_RETRIES: u32 = 3;
/// How many occurrences `list` returns at most.
const LIST_LIMIT: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Scheduled { occurrence: String, job: String, slot: i64 },
    Skipped { occurrence: String, reason: String },
    Started { occurrence: String, attempt: u32 },
    Succeeded { occurrence: String, attempt: u32, run_file: Option<String> },
    Failed { occurrence: String, attempt: u32, error: String, retry_at: Option<i64>, run_file: Option<String> },
    /// a human asked for a terminally failed occurrence to run again
    RetryRequested { occurrence: String },
}

impl Event {
    fn occurrence(&self) -> &str {
        match self {
            Event::Scheduled { occurrence, .. }
            | Event::Skipped { occurrence, .. }
            | Event::Started { occurrence, .. }
            | Event::Succeeded { occurrence, .. }
            | Event::Failed { occurrence, .. }
            | Event::RetryRequested { occurrence } => occurrence,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    ts: i64,
    #[serde(flatten)]
    event: Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

impl State {
    /// still has work ahead of it — blocks scheduling a newer occurrence of
    /// the same job (see `schedule`)
    pub fn is_open(self) -> bool {
        matches!(self, State::Pending | State::Running)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Occurrence {
    pub id: String,
    pub job: String,
    pub slot: i64,
    pub state: State,
    /// attempts started so far (1-based once running)
    pub attempts: u32,
    /// earliest time a `pending` occurrence may be claimed
    pub not_before: i64,
    pub scheduled_at: i64,
    pub updated_at: i64,
    pub last_error: Option<String>,
    /// `logs/scheduled_runs/<file>` of the latest finished attempt
    pub run_file: Option<String>,
}

pub fn occurrence_id(job: &str, slot: i64) -> String {
    format!("{job}@{slot}")
}

/// Retry policy, applied when an attempt fails: `Some(retry_at)` while
/// retries remain, `None` once they're spent (terminal failure).
pub fn retry_at_after_failure(attempt: u32, now: i64, backoff: i64) -> Option<i64> {
    (attempt <= MAX_RETRIES).then_some(now + backoff)
}

#[derive(Default)]
struct Projection {
    occurrences: HashMap<String, Occurrence>,
}

impl Projection {
    fn apply(&mut self, rec: &Record) {
        let ts = rec.ts;
        if let Event::Scheduled { occurrence, job, slot } = &rec.event {
            self.occurrences.entry(occurrence.clone()).or_insert_with(|| Occurrence {
                id: occurrence.clone(),
                job: job.clone(),
                slot: *slot,
                state: State::Pending,
                attempts: 0,
                not_before: ts,
                scheduled_at: ts,
                updated_at: ts,
                last_error: None,
                run_file: None,
            });
            return;
        }
        // events for an occurrence whose `scheduled` line was lost (torn
        // write, hand-edited file) have nothing to apply to — ignored
        let Some(occ) = self.occurrences.get_mut(rec.event.occurrence()) else {
            return;
        };
        occ.updated_at = ts;
        match &rec.event {
            Event::Scheduled { .. } => {}
            Event::Skipped { reason, .. } => {
                occ.state = State::Skipped;
                occ.last_error = Some(reason.clone());
            }
            Event::Started { attempt, .. } => {
                occ.state = State::Running;
                occ.attempts = *attempt;
            }
            Event::Succeeded { run_file, .. } => {
                occ.state = State::Succeeded;
                occ.last_error = None;
                occ.run_file = run_file.clone();
            }
            Event::Failed { error, retry_at, run_file, .. } => {
                occ.last_error = Some(error.clone());
                occ.run_file = run_file.clone().or(occ.run_file.take());
                match retry_at {
                    Some(at) => {
                        occ.state = State::Pending;
                        occ.not_before = *at;
                    }
                    None => occ.state = State::Failed,
                }
            }
            Event::RetryRequested { .. } => {
                occ.state = State::Pending;
                occ.not_before = ts;
            }
        }
    }

    fn has_open(&self, job: &str) -> bool {
        self.occurrences.values().any(|o| o.job == job && o.state.is_open())
    }

    fn is_running(&self, job: &str) -> bool {
        self.occurrences.values().any(|o| o.job == job && o.state == State::Running)
    }
}

pub struct ScheduleLog {
    path: PathBuf,
    inner: Mutex<Projection>,
}

impl ScheduleLog {
    pub fn path_for(agent_home: &Path) -> PathBuf {
        agent_home.join("logs/scheduler_events.jsonl")
    }

    /// Replays the whole event file. Unparseable lines (a torn final write
    /// from a crash) are skipped, not fatal.
    pub fn load(path: PathBuf) -> Self {
        let mut projection = Projection::default();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Ok(rec) = serde_json::from_str::<Record>(line) {
                    projection.apply(&rec);
                }
            }
        }
        ScheduleLog { path, inner: Mutex::new(projection) }
    }

    fn append(&self, projection: &mut Projection, ts: i64, event: Event) {
        let rec = Record { ts, event };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // one `write_all` of one whole line on an O_APPEND file — a crash
        // can at worst leave a torn final line, which `load` skips
        if let Ok(mut line) = serde_json::to_vec(&rec) {
            line.push(b'\n');
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
                let _ = f.write_all(&line);
            }
        }
        projection.apply(&rec);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Projection> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Idempotently schedules `<job>@<slot>`. Returns `false` (and records
    /// nothing) if that occurrence already exists, or if the job still has
    /// an open (pending/running) occurrence — a slow or retrying run isn't
    /// stacked on top of with a newer one.
    pub fn schedule(&self, job: &str, slot: i64, now: i64) -> bool {
        let id = occurrence_id(job, slot);
        let mut p = self.lock();
        if p.occurrences.contains_key(&id) || p.has_open(job) {
            return false;
        }
        self.append(&mut p, now, Event::Scheduled { occurrence: id, job: job.to_string(), slot });
        true
    }

    /// Same idempotency as `schedule`, but records the occurrence as
    /// already resolved — due, checked, nothing to do.
    pub fn skip(&self, job: &str, slot: i64, now: i64, reason: &str) -> bool {
        let id = occurrence_id(job, slot);
        let mut p = self.lock();
        if p.occurrences.contains_key(&id) || p.has_open(job) {
            return false;
        }
        self.append(&mut p, now, Event::Scheduled { occurrence: id.clone(), job: job.to_string(), slot });
        self.append(&mut p, now, Event::Skipped { occurrence: id, reason: reason.to_string() });
        true
    }

    /// Atomically moves every claimable `pending` occurrence to `running`
    /// (writing its `started` event) and returns them, oldest slot first.
    /// At most one running occurrence per job.
    pub fn claim_ready(&self, now: i64) -> Vec<Occurrence> {
        let mut p = self.lock();
        let mut ready: Vec<Occurrence> =
            p.occurrences.values().filter(|o| o.state == State::Pending && o.not_before <= now).cloned().collect();
        ready.sort_by_key(|o| (o.slot, o.scheduled_at));
        let mut claimed = Vec::new();
        for occ in ready {
            if p.is_running(&occ.job) {
                continue;
            }
            let attempt = occ.attempts + 1;
            self.append(&mut p, now, Event::Started { occurrence: occ.id.clone(), attempt });
            claimed.push(p.occurrences[&occ.id].clone());
        }
        claimed
    }

    pub fn succeed(&self, id: &str, attempt: u32, now: i64, run_file: Option<String>) {
        let mut p = self.lock();
        self.append(&mut p, now, Event::Succeeded { occurrence: id.to_string(), attempt, run_file });
    }

    /// Records a failed attempt; returns the retry time if one was
    /// scheduled.
    pub fn fail(&self, id: &str, attempt: u32, now: i64, error: &str, run_file: Option<String>) -> Option<i64> {
        let retry_at = retry_at_after_failure(attempt, now, RETRY_BACKOFF_SECS);
        let mut p = self.lock();
        self.append(&mut p, now, Event::Failed { occurrence: id.to_string(), attempt, error: error.to_string(), retry_at, run_file });
        retry_at
    }

    /// Startup recovery: anything still `running` was cut off by a kernel
    /// crash/restart — whether it already had side effects is unknowable,
    /// and the chosen policy is to treat it as a failed attempt and let the
    /// normal retry policy take it from there (retrying right away, no
    /// backoff — nothing is running it now).
    pub fn recover_interrupted(&self, now: i64) -> Vec<String> {
        let mut p = self.lock();
        let running: Vec<(String, u32)> =
            p.occurrences.values().filter(|o| o.state == State::Running).map(|o| (o.id.clone(), o.attempts)).collect();
        for (id, attempt) in &running {
            let retry_at = retry_at_after_failure(*attempt, now, 0);
            self.append(
                &mut p,
                now,
                Event::Failed {
                    occurrence: id.clone(),
                    attempt: *attempt,
                    error: "interrupted: kernel restarted mid-run".to_string(),
                    retry_at,
                    run_file: None,
                },
            );
        }
        running.into_iter().map(|(id, _)| id).collect()
    }

    /// Re-queues a terminally failed occurrence for one more attempt.
    pub fn request_retry(&self, id: &str, now: i64) -> Result<(), String> {
        let mut p = self.lock();
        match p.occurrences.get(id).map(|o| o.state) {
            None => Err("no such occurrence".to_string()),
            Some(State::Failed) => {
                self.append(&mut p, now, Event::RetryRequested { occurrence: id.to_string() });
                Ok(())
            }
            Some(other) => Err(format!("occurrence is {other:?}, only failed ones can be retried")),
        }
    }

    /// Most recent occurrences first (by slot).
    pub fn list(&self) -> Vec<Occurrence> {
        let p = self.lock();
        let mut all: Vec<Occurrence> = p.occurrences.values().cloned().collect();
        all.sort_by(|a, b| b.slot.cmp(&a.slot).then(b.scheduled_at.cmp(&a.scheduled_at)));
        all.truncate(LIST_LIMIT);
        all
    }

    pub fn get(&self, id: &str) -> Option<Occurrence> {
        self.lock().occurrences.get(id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(name: &str) -> (PathBuf, ScheduleLog) {
        let dir = std::env::temp_dir().join(format!("ebina-schedlog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        (path.clone(), ScheduleLog::load(path))
    }

    #[test]
    fn scheduling_the_same_slot_twice_is_a_no_op() {
        let (_, log) = temp_log("idem");
        assert!(log.schedule("task:a", 60, 100));
        assert!(!log.schedule("task:a", 60, 130));
        assert_eq!(log.list().len(), 1);
    }

    #[test]
    fn open_occurrence_blocks_a_newer_slot_of_the_same_job() {
        let (_, log) = temp_log("open");
        assert!(log.schedule("task:a", 60, 100));
        assert!(!log.schedule("task:a", 120, 120));
        assert!(log.schedule("task:b", 120, 120));
    }

    #[test]
    fn claim_is_exclusive() {
        let (_, log) = temp_log("claim");
        log.schedule("task:a", 60, 100);
        let first = log.claim_ready(100);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].state, State::Running);
        assert_eq!(first[0].attempts, 1);
        assert!(log.claim_ready(101).is_empty());
    }

    #[test]
    fn failure_retries_after_backoff_then_goes_terminal() {
        let (_, log) = temp_log("retry");
        log.schedule("task:a", 60, 0);
        let id = occurrence_id("task:a", 60);
        let mut now = 0;
        for attempt in 1..=MAX_RETRIES + 1 {
            let claimed = log.claim_ready(now);
            assert_eq!(claimed.len(), 1, "attempt {attempt} should be claimable");
            assert_eq!(claimed[0].attempts, attempt);
            let retry = log.fail(&id, attempt, now, "boom", None);
            if attempt <= MAX_RETRIES {
                assert_eq!(retry, Some(now + RETRY_BACKOFF_SECS));
                assert!(log.claim_ready(now + RETRY_BACKOFF_SECS - 1).is_empty(), "not before backoff");
                now += RETRY_BACKOFF_SECS;
            } else {
                assert_eq!(retry, None);
            }
        }
        assert_eq!(log.get(&id).unwrap().state, State::Failed);
        assert!(log.claim_ready(now + 10 * RETRY_BACKOFF_SECS).is_empty());
        // terminal failure frees the job for its next slot
        assert!(log.schedule("task:a", 7200, now));
    }

    #[test]
    fn state_survives_reload_and_interrupted_runs_become_retries() {
        let (path, log) = temp_log("reload");
        log.schedule("task:a", 60, 0);
        log.schedule("task:b", 60, 0);
        let claimed = log.claim_ready(0);
        assert_eq!(claimed.len(), 2);
        log.succeed(&occurrence_id("task:a", 60), 1, 5, Some("x.json".into()));
        drop(log);

        let reloaded = ScheduleLog::load(path);
        assert_eq!(reloaded.get(&occurrence_id("task:a", 60)).unwrap().state, State::Succeeded);
        assert_eq!(reloaded.get(&occurrence_id("task:b", 60)).unwrap().state, State::Running);
        assert!(!reloaded.schedule("task:a", 60, 10), "idempotency key survives restart");

        let recovered = reloaded.recover_interrupted(100);
        assert_eq!(recovered, vec![occurrence_id("task:b", 60)]);
        let retried = reloaded.claim_ready(100);
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].attempts, 2);
    }

    #[test]
    fn skipped_is_resolved_and_idempotent() {
        let (_, log) = temp_log("skip");
        assert!(log.skip("daily_maintenance", 780, 790, "nothing new"));
        assert!(!log.skip("daily_maintenance", 780, 800, "nothing new"));
        assert_eq!(log.get(&occurrence_id("daily_maintenance", 780)).unwrap().state, State::Skipped);
        assert!(log.claim_ready(800).is_empty());
    }

    #[test]
    fn manual_retry_only_from_terminal_failure() {
        let (_, log) = temp_log("manual");
        log.schedule("cron", 500, 0);
        let id = occurrence_id("cron", 500);
        assert!(log.request_retry(&id, 1).is_err());
        let mut now = 0;
        for attempt in 1..=MAX_RETRIES + 1 {
            log.claim_ready(now);
            log.fail(&id, attempt, now, "boom", None);
            now += RETRY_BACKOFF_SECS;
        }
        assert!(log.request_retry(&id, now).is_ok());
        assert_eq!(log.claim_ready(now)[0].attempts, MAX_RETRIES + 2);
    }

    #[test]
    fn torn_last_line_is_ignored() {
        let (path, log) = temp_log("torn");
        log.schedule("task:a", 60, 0);
        drop(log);
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"ts\":1,\"event\":\"star").unwrap();
        let reloaded = ScheduleLog::load(path);
        assert_eq!(reloaded.list().len(), 1);
    }
}
