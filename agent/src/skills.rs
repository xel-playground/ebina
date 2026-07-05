use std::fs;

pub const SKILLS_DIR: &str = "/memory/skills";

pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// Lists every skill's `name`/`description` (never the body — that's the
/// point: keep every prompt cheap, load the full procedure only once the
/// agent actually decides to use one via `use_skill`). Same progressive-
/// disclosure shape as Claude Code's own Skill system.
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

/// Writes `---\nname: ..\ndescription: ..\n---\n<body>` — a dedicated action
/// instead of leaving frontmatter formatting to `write_file` freehand, so a
/// self-authored skill reliably round-trips through `parse` below.
pub fn save(name: &str, description: &str, body: &str) -> std::io::Result<()> {
    fs::create_dir_all(SKILLS_DIR)?;
    let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n");
    fs::write(path_for(name), content)
}

/// Minimal frontmatter parser (no yaml crate) — same "flat known-shape text
/// is simpler than a real parser" call as `memory::current_embed_model`.
fn parse(text: &str) -> Option<Skill> {
    let rest = text.trim_start().strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n').to_string();

    let mut name = None;
    let mut description = None;
    for line in front.lines() {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => name = Some(value.trim().to_string()),
                "description" => description = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    Some(Skill { name: name?, description: description.unwrap_or_default(), body })
}
