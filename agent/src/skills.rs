use crate::time::now_unix;
use std::fs;

pub const SKILLS_DIR: &str = "/memory/skills";

pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub created_at: u64,
    pub used_count: u64,
    /// `None` until `record_use` fires at least once
    pub last_used: Option<u64>,
}

/// Lists every skill's `name`/`description`/usage stats (never the body —
/// that's the point: keep every prompt cheap, load the full procedure only
/// once the agent actually decides to use one via `use_skill`). Same
/// progressive-disclosure shape as Claude Code's own Skill system.
pub fn list() -> Vec<Skill> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(SKILLS_DIR) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&path) {
            if let Some(skill) = parse(&text) {
                out.push(skill);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn load_body(name: &str) -> Option<String> {
    list().into_iter().find(|s| s.name == name).map(|s| s.body)
}

pub fn path_for(name: &str) -> String {
    format!("{SKILLS_DIR}/{name}.md")
}

/// Per-skill advisory lock — same `create_new`-busy-retry technique as
/// `agent_loop.rs`'s `DiskQuotaLock`, keyed by skill name rather than one
/// fixed path since unrelated skills shouldn't contend with each other.
/// Needed because this file gets written from two independent, uncoordinated
/// places: `record_use` below (guest-side, fires on every `use_skill`, and
/// now genuinely concurrent across sessions/background triggers since
/// per-session locking replaced the old single global run lock) and
/// `kernel/src/gateway.rs`'s `post_skill` (host-side, a webui edit) — both
/// load-mutate-save the same file with no shared state otherwise. A lock
/// file created here with `create_new` is a real file under this skill's
/// WASI-preopened directory, the same physical path the host's `FileLock`
/// can also open — that's already proven interoperable by `DiskQuotaLock`
/// coexisting with host-side quota accounting, just applied here to a
/// guest/host pair racing the exact same file instead.
struct SkillLock {
    path: String,
}

impl SkillLock {
    fn acquire(name: &str) -> Self {
        let path = format!("{SKILLS_DIR}/{name}.md.lock");
        for _ in 0..2000 {
            if fs::OpenOptions::new().create_new(true).write(true).open(&path).is_ok() {
                return SkillLock { path };
            }
        }
        // stale lock from a run that crashed/got killed mid-write — force
        // through rather than deadlocking every future save/use forever
        let _ = fs::remove_file(&path);
        SkillLock { path }
    }
}

impl Drop for SkillLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Writes `---\nname: ..\ndescription: ..\ncreated_at: ..\nused_count: ..\n
/// [last_used: ..]\n---\n<body>` — a dedicated action instead of leaving
/// frontmatter formatting to `write_file` freehand, so a self-authored skill
/// reliably round-trips through `parse` below. Preserves the existing
/// `created_at`/`used_count`/`last_used` when overwriting an already-saved
/// skill (editing a skill's description/body isn't "learning it" again —
/// its usage history shouldn't reset just because the content changed);
/// `created_at` is only ever set once, the first time a given name is saved.
pub fn save(name: &str, description: &str, body: &str) -> std::io::Result<()> {
    fs::create_dir_all(SKILLS_DIR)?;
    let _lock = SkillLock::acquire(name);
    let existing = list().into_iter().find(|s| s.name == name);
    let created_at = existing.as_ref().map(|s| s.created_at).unwrap_or_else(now_unix);
    let used_count = existing.as_ref().map(|s| s.used_count).unwrap_or(0);
    let last_used = existing.and_then(|s| s.last_used);
    fs::write(path_for(name), render(name, description, created_at, used_count, last_used, body))
}

/// Increments `used_count` and stamps `last_used` — called by
/// `agent_loop.rs`'s `use_skill` action handler each time a skill's body is
/// actually loaded into context, not just listed.
pub fn record_use(name: &str) {
    let _lock = SkillLock::acquire(name);
    let Some(skill) = list().into_iter().find(|s| s.name == name) else { return };
    let _ = fs::write(
        path_for(name),
        render(name, &skill.description, skill.created_at, skill.used_count + 1, Some(now_unix()), &skill.body),
    );
}

fn render(name: &str, description: &str, created_at: u64, used_count: u64, last_used: Option<u64>, body: &str) -> String {
    let last_used_line = last_used.map(|t| format!("last_used: {t}\n")).unwrap_or_default();
    // `body` already ends in the trailing `\n` this function itself always
    // adds — trim it back off first, or re-saving the same skill
    // repeatedly (every `record_use`) grows a longer run of blank lines at
    // the end each time.
    let body = body.trim_end_matches('\n');
    format!("---\nname: {name}\ndescription: {description}\ncreated_at: {created_at}\nused_count: {used_count}\n{last_used_line}---\n{body}\n")
}

/// Minimal frontmatter parser (no yaml crate) — same "flat known-shape text
/// is simpler than a real parser" call as `memory::current_embed_model`.
/// `created_at`/`used_count`/`last_used` default to `0`/`0`/`None` when
/// absent so skill files saved before these fields existed still parse.
fn parse(text: &str) -> Option<Skill> {
    let rest = text.trim_start().strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n').to_string();

    let mut name = None;
    let mut description = None;
    let mut created_at = 0u64;
    let mut used_count = 0u64;
    let mut last_used = None;
    for line in front.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            match key.trim() {
                "name" => name = Some(value.to_string()),
                "description" => description = Some(value.to_string()),
                "created_at" => created_at = value.parse().unwrap_or(0),
                "used_count" => used_count = value.parse().unwrap_or(0),
                "last_used" => last_used = value.parse().ok(),
                _ => {}
            }
        }
    }
    Some(Skill { name: name?, description: description.unwrap_or_default(), body, created_at, used_count, last_used })
}
