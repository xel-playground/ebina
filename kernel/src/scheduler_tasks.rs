use crate::filelock::FileLock;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A user- or agent-defined recurring job: "at this cron time, wake the
/// agent with instructions read from this file" — the general-purpose
/// sibling of the built-in `daily_maintenance`/`cron` wakes in
/// `gateway::scheduler_loop`. One file per task under `scheduler/` (agent-home
/// root, alongside `memory/`/`workspace/`/`logs/`) rather than one shared
/// JSON blob — same "one file per item" shape as `memory/skills/<name>.md`,
/// so a task is directly read_file/write_file-able by the agent itself, not
/// just through the dedicated actions/API below.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScheduledTask {
    pub id: String,
    pub cron: String,
    pub data_path: String,
    pub description: String,
    pub enabled: bool,
    pub created_at: i64,
    pub last_run: Option<i64>,
}

pub const TASKS_DIR: &str = "scheduler";

fn dir(agent_home: &Path) -> PathBuf {
    agent_home.join(TASKS_DIR)
}

fn task_path(agent_home: &Path, id: &str) -> PathBuf {
    dir(agent_home).join(format!("{id}.json"))
}

fn save_task(agent_home: &Path, task: &ScheduledTask) -> std::io::Result<()> {
    std::fs::create_dir_all(dir(agent_home))?;
    std::fs::write(task_path(agent_home, &task.id), serde_json::to_vec_pretty(task).unwrap_or_default())
}

/// Per-task lock for the load-mutate-save sequences below (`update_task`,
/// `mark_run`) — two different writers touching the *same* task id at once
/// (a human's webui edit landing right as the scheduler marks that task's
/// own `last_run`, say) used to race with no locking at all, one save
/// silently overwriting the other's change. Keyed per task file rather than
/// one lock for the whole directory, so editing two different tasks at once
/// still doesn't contend with each other.
fn task_lock(agent_home: &Path, id: &str) -> FileLock {
    FileLock::acquire(task_path(agent_home, id).with_extension("json.lock"), Duration::from_secs(5))
}

pub fn load_tasks(agent_home: &Path) -> Vec<ScheduledTask> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir(agent_home)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(task) = serde_json::from_str(&text) {
                out.push(task);
            }
        }
    }
    out.sort_by(|a: &ScheduledTask, b: &ScheduledTask| a.created_at.cmp(&b.created_at));
    out
}

pub fn load_task(agent_home: &Path, id: &str) -> Option<ScheduledTask> {
    let text = std::fs::read_to_string(task_path(agent_home, id)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn add_task(agent_home: &Path, cron: &str, data_path: &str, description: &str) -> Result<ScheduledTask, String> {
    crate::cron::validate(cron)?;
    if data_path.is_empty() {
        return Err("data_path must not be empty".to_string());
    }
    // whole-directory lock, not a per-task one — there's no task id to key
    // on yet, and the id itself is derived from the *current task count*
    // (`load_tasks(...).len()`), so two concurrent add_task calls without
    // this could compute the same id and the second save would silently
    // clobber the first brand-new task outright, not just collide
    let _lock = FileLock::acquire(dir(agent_home).join(".add_task.lock"), Duration::from_secs(5));
    let now = crate::logs::now_unix_secs();
    let task = ScheduledTask {
        id: format!("{now}-{}", load_tasks(agent_home).len()),
        cron: cron.to_string(),
        data_path: data_path.to_string(),
        description: description.to_string(),
        enabled: true,
        created_at: now,
        last_run: None,
    };
    save_task(agent_home, &task).map_err(|e| e.to_string())?;
    Ok(task)
}

/// Partial update — every field is optional, `None` keeps the existing
/// value. Re-validates `cron` if it's being changed.
pub fn update_task(
    agent_home: &Path,
    id: &str,
    cron: Option<&str>,
    data_path: Option<&str>,
    description: Option<&str>,
    enabled: Option<bool>,
) -> Result<Option<ScheduledTask>, String> {
    let _lock = task_lock(agent_home, id);
    let Some(mut task) = load_task(agent_home, id) else {
        return Ok(None);
    };
    if let Some(c) = cron {
        crate::cron::validate(c)?;
        task.cron = c.to_string();
    }
    if let Some(p) = data_path {
        task.data_path = p.to_string();
    }
    if let Some(d) = description {
        task.description = d.to_string();
    }
    if let Some(e) = enabled {
        task.enabled = e;
    }
    save_task(agent_home, &task).map_err(|e| e.to_string())?;
    Ok(Some(task))
}

pub fn remove_task(agent_home: &Path, id: &str) -> std::io::Result<bool> {
    match std::fs::remove_file(task_path(agent_home, id)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn mark_run(agent_home: &Path, id: &str, ts: i64) {
    let _lock = task_lock(agent_home, id);
    if let Some(mut task) = load_task(agent_home, id) {
        task.last_run = Some(ts);
        let _ = save_task(agent_home, &task);
    }
}
