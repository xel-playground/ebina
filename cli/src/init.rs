use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

/// `ebinactl agent init [workspace]` — scaffolds a fresh agent-home at
/// `<workspace>/agent-home` so `ebinactl agent run` works right after with
/// zero env vars/flags/manual build steps: the directory tree, a starter
/// `config.toml` (guest-visible, inside agent_home — only a `{secrets.*}`
/// placeholder ever goes here, never a real key, see
/// `kernel::secrets::resolve_placeholder`), a starter `SOUL.md`, a sibling
/// `<workspace>/secrets.toml` (host-only, outside agent_home, same
/// convention as the kernel's git-dir/model-cache assets — see
/// `kernel::secrets_path`) seeded with a freshly generated `gateway_token`
/// so `run`'s fail-fast check for one already passes, plus `agent.wasm` and
/// a `webui/` build — both baked into this very binary at compile time (see
/// `embedded.rs`), written out here rather than making the human separately
/// build and copy them into place.
///
/// `agent_home` here is always `<workspace>/agent-home` (main.rs's
/// `agent_home_path`) — never the workspace root itself, specifically so
/// the sibling `secrets.toml`/git-dir (computed as `agent_home.parent()`)
/// land inside a container the human chose, not scattered into whatever
/// directory `ebinactl` happened to be invoked from. Each workspace is its
/// own vault; run separate agents from separate workspaces.
///
/// Refuses to touch anything that already looks initialized — meant for a
/// brand new agent-home, not to silently overwrite one that already has
/// real config/memory in it. Re-running against an existing agent-home is
/// an error, not a reset.
pub fn init_agent_home(agent_home: &Path) -> Result<()> {
    let config_path = agent_home.join("config.toml");
    if config_path.exists() {
        anyhow::bail!("{} already exists — this agent-home looks already initialized, refusing to overwrite it", config_path.display());
    }

    for dir in ["memory/notes", "memory/skills", "workspace", "logs", "scheduler"] {
        std::fs::create_dir_all(agent_home.join(dir)).with_context(|| format!("creating {dir}"))?;
    }

    std::fs::write(&config_path, STARTER_CONFIG_TOML).context("writing config.toml")?;
    std::fs::write(agent_home.join("SOUL.md"), STARTER_SOUL_MD).context("writing SOUL.md")?;

    let workspace = agent_home.parent().unwrap_or(agent_home);

    let secrets_path = kernel::secrets_path(agent_home);
    let mut secrets = kernel::secrets::Secrets::load(&secrets_path);
    // don't clobber a real vault that already exists (e.g. re-running init
    // after manually deleting just config.toml/SOUL.md) — only fill in the
    // keys `run`/`ssh_exec` actually need, and only if missing
    if secrets.get("gateway_token").is_none() {
        secrets.set("gateway_token", &generate_token()?);
        secrets.save(&secrets_path).context("writing secrets.toml")?;
    }
    // matches the starter config.toml's `[ssh]` section (localhost:2224,
    // user ebina) and local_deps.rs's compose template, which mounts
    // `<workspace>/ssh_ebina.pub` into the ssh-target container — without a
    // keypair here, `local-deps start` would be bind-mounting a file that
    // doesn't exist, and ssh_exec has nothing to authenticate with anyway
    let ssh_key_path = workspace.join("ssh_ebina");
    if secrets.get("ssh_key_path").is_none() {
        generate_ssh_keypair(&ssh_key_path)?;
        secrets.set("ssh_key_path", &ssh_key_path.canonicalize().unwrap_or(ssh_key_path.clone()).to_string_lossy());
        secrets.save(&secrets_path).context("writing secrets.toml")?;
    }

    let wasm_path = workspace.join("agent.wasm");
    std::fs::write(&wasm_path, crate::embedded::AGENT_WASM).context("writing agent.wasm")?;
    let webui_dir = workspace.join("webui");
    crate::embedded::WEBUI_DIST.extract(&webui_dir).context("extracting embedded webui build")?;

    println!("initialized agent-home at {}", agent_home.display());
    println!("secrets vault at {} (gateway_token + ssh_key_path generated)", secrets_path.display());
    println!("ssh keypair at {} / {}.pub", ssh_key_path.display(), ssh_key_path.display());
    println!("agent.wasm written to {}", wasm_path.display());
    println!("webui written to {}", webui_dir.display());
    println!();
    println!("next steps:");
    let run_target = if workspace == Path::new("ebina") { String::new() } else { format!(" {}", workspace.display()) };
    println!("  1. ebinactl local-deps start{run_target}   (Ollama/SearXNG/an ssh_exec target — [embed]/[search]/[ssh] already point at these)");
    println!("  2. set a real [llm] provider — the one thing with no local option:");
    println!("       edit {} (or use the webui's Config panel once running)", config_path.display());
    println!("       add the matching key to {} (or the webui's Secrets panel) — e.g.:", secrets_path.display());
    println!("         llm = \"sk-...\"");
    println!("  3. ebinactl agent run{run_target}");
    Ok(())
}

/// Shells out to `ssh-keygen` rather than a Rust SSH key-generation crate —
/// same "well-known system tool over a new dependency" call as
/// `autocommit.rs` shelling out to `git`. ed25519: smaller, faster to
/// generate, and openssh-server (local_deps.rs's ssh-target image) accepts
/// it fine as an authorized key.
fn generate_ssh_keypair(path: &Path) -> Result<()> {
    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f", &path.to_string_lossy(), "-N", "", "-q"])
        .status()
        .context("running ssh-keygen — is it installed?")?;
    if !status.success() {
        anyhow::bail!("ssh-keygen exited with {status}");
    }
    Ok(())
}

/// 24 bytes of `/dev/urandom`, hex-encoded — no `rand` crate dependency for
/// something this small and Linux-only-already (this whole project assumes
/// a Linux host: `ssh2`, `rusqlite` bundled sqlite, wasmtime's Linux epoch
/// timer). 48 hex chars, 192 bits, plenty for a bearer token nothing else
/// derives from.
fn generate_token() -> Result<String> {
    let mut buf = [0u8; 24];
    std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?.read_exact(&mut buf).context("reading /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

const STARTER_CONFIG_TOML: &str = r#"# ebina agent config — guest-readable (the agent itself reads this to build
# its own system prompt), so only ever put a `{secrets.NAME}` placeholder in
# an api_key field here, never a literal key. Add the matching real value to
# the sibling secrets.toml instead (see the `init` output above for its path)
# — or skip editing files by hand entirely and use the webui's Config/Secrets
# panels once `ebinactl agent run` is up.
#
# [embed]/[search]/[ssh] below are pre-wired to `ebinactl local-deps start`'s
# docker-compose stack (Ollama/SearXNG/an ssh_exec target) — run that and
# these just work with no edits. [llm] is the one section with no local
# option: every provider needs a real hosted API key, there's nothing to
# spin up locally for it — see kernel/src/config.rs for supported providers.
#
# `disk`/`budget`/`ratelimit`/`chat`/`network`/`db` are left out entirely —
# they use kernel/src/config.rs's built-in defaults, fine for a first run.

[llm]
base_url = "https://api.anthropic.com/v1/messages"
model = "claude-sonnet-5"
# "anthropic" | "openai" (covers any OpenAI-compatible chat completions API,
# e.g. Moonshot/Kimi, DeepSeek) | "ollama"
provider = "anthropic"
api_key = "{secrets.llm}"

[embed]
# RAG memory recall (memory/notes/ search) — matches `local-deps start`'s
# ollama container + its auto-pulled nomic-embed-text; `embed` calls just
# fail with a clear error until something real is listening on this URL,
# so this is safe to leave as-is until you actually run local-deps
base_url = "http://localhost:11434/api/embed"
model = "nomic-embed-text"
provider = "ollama"
api_key = "{secrets.embed}"

[search]
# search_web — matches local-deps' searxng container
base_url = "http://localhost:8888/search"
provider = "searxng"
max_results = 5

[ssh]
# ssh_exec's one fixed target — matches local-deps' ssh-target container.
# Empty host disables ssh_exec entirely; this is deliberately NOT empty by
# default once local-deps is in the picture, since that container exists
# specifically to give the agent something to practice against
host = "localhost"
port = 2224
user = "ebina"
timeout_secs = 15
max_output_bytes = 65536
"#;

const STARTER_SOUL_MD: &str = r#"# SOUL.md

Who you are, how you talk, what you won't do — written and editable by you
or a human. Shown in full to you above every turn (see the system prompt).

This is a starting point, not a fixed identity — refine it as you actually
figure out who you are.

## Who I am

(a short persona — name, role, tone)

## Boundaries

- external actions I'm unsure about: ask before acting
- prefer reversible operations over irreversible ones
"#;
