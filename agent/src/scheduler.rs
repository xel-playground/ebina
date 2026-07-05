use serde_json::Value;
use std::fs;

/// Mirrors `kernel::scheduler_tasks::ScheduledTask`'s file shape exactly —
/// one JSON file per task under `/scheduler/`, so the agent can list its own
/// scheduled jobs the same way it lists skills (`skills::list`), without any
/// dedicated "list" syscall. No serde derive in this crate (see
/// `skills::parse`'s hand-rolled frontmatter for the same reasoning) — plain
/// `Value` field lookups instead.
pub const TASKS_DIR: &str = "/scheduler";

pub struct Task {
    pub id: String,
    pub cron: String,
    pub data_path: String,
    pub description: String,
    pub enabled: bool,
}

pub fn list() -> Vec<Task> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(TASKS_DIR) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
        let (Some(id), Some(cron), Some(data_path)) =
            (v.get("id").and_then(|x| x.as_str()), v.get("cron").and_then(|x| x.as_str()), v.get("data_path").and_then(|x| x.as_str()))
        else {
            continue;
        };
        out.push(Task {
            id: id.to_string(),
            cron: cron.to_string(),
            data_path: data_path.to_string(),
            description: v.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            enabled: v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(true),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}
