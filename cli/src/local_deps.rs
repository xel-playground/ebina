use anyhow::{Context, Result};
use std::path::Path;

/// `ebinactl local-deps start/stop [workspace]` — a docker-compose stack
/// for the three external services `config.toml` can point at:
/// `[embed]`/Ollama, `[search]`/SearXNG, and `[ssh]`'s target for
/// `ssh_exec`. Writes `<workspace>/docker-compose.yml` if missing, then
/// shells out to `docker compose` — same "shell out to a well-known system
/// tool rather than add a library dependency" call `autocommit.rs` already
/// makes for `git`, and there's no meaningful Rust API for "drive
/// docker-compose" worth depending on for this.
///
/// Never invoked automatically by anything else in this CLI — starting
/// containers is a real, visible action on the host (open ports, running
/// processes, disk used for images/volumes), so it only happens when a
/// human explicitly types this subcommand.
const COMPOSE_FILENAME: &str = "docker-compose.yml";

pub fn start(workspace: &Path) -> Result<()> {
    let compose_path = ensure_compose_file(workspace)?;
    run_compose(&compose_path, &["up", "-d"])?;

    println!();
    pull_and_warm_up_embed_model(&compose_path);

    println!();
    println!("started. matching config.toml section for each:");
    println!("  [embed] base_url = \"http://localhost:11434/api/embed\", provider = \"ollama\"");
    println!("  [search] base_url = \"http://localhost:8888/search\", provider = \"searxng\"");
    println!("  [ssh] host = \"localhost\", port = 2224, user = \"ebina\"");
    println!("    (authenticates with {}'s sibling ssh_ebina keypair — generate one with `ssh-keygen` if this workspace doesn't have one yet)", workspace.display());
    Ok(())
}

pub fn stop(workspace: &Path) -> Result<()> {
    let compose_path = workspace.join(COMPOSE_FILENAME);
    if !compose_path.exists() {
        anyhow::bail!("{} doesn't exist — nothing to stop (never ran `local-deps start` here?)", compose_path.display());
    }
    run_compose(&compose_path, &["down"])
}

/// Pulls `nomic-embed-text` into the fresh ollama container and forces one
/// throwaway embed call so the model's already loaded in memory before the
/// agent's first real one — the model itself isn't part of the `ollama`
/// image, and Ollama loads a model into memory lazily on first inference,
/// which took ~20s in testing on a freshly-started container. Skipping
/// straight to a real `embed` call would make the agent's first-ever memory
/// recall eat that cold-start cost instead. Best-effort: `ollama serve`
/// needs a moment after `up -d` returns before it accepts requests, so both
/// steps retry briefly rather than failing outright on the first attempt.
fn pull_and_warm_up_embed_model(compose_path: &Path) {
    println!("pulling nomic-embed-text into the ollama container (first run only, ~270MB)...");
    let pulled = retry(10, || {
        std::process::Command::new("docker")
            .args(["compose", "-f", &compose_path.to_string_lossy(), "exec", "-T", "ollama", "ollama", "pull", "nomic-embed-text"])
            .status()
            .is_ok_and(|s| s.success())
    });
    if !pulled {
        println!("(pull failed — not fatal, but the model isn't ready; pull manually with:");
        println!("  docker compose -f {} exec ollama ollama pull nomic-embed-text)", compose_path.display());
        return;
    }

    println!("warming up the model (loads it into memory so the first real request isn't ~20s slow)...");
    let warmed = retry(5, || {
        std::process::Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-X",
                "POST",
                "http://localhost:11434/api/embed",
                "-H",
                "Content-Type: application/json",
                "-d",
                r#"{"model":"nomic-embed-text","input":["warmup"]}"#,
            ])
            .status()
            .is_ok_and(|s| s.success())
    });
    if !warmed {
        println!("(warm-up ping failed — not fatal, the model just loads lazily on first real use instead)");
    }
}

/// Up to `attempts` tries, 1s apart, stopping as soon as `f` succeeds —
/// containers reporting "up" doesn't mean the process inside is accepting
/// requests yet.
fn retry(attempts: u32, mut f: impl FnMut() -> bool) -> bool {
    for attempt in 0..attempts {
        if f() {
            return true;
        }
        if attempt + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    false
}

fn ensure_compose_file(workspace: &Path) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(workspace).context("creating workspace directory")?;
    let path = workspace.join(COMPOSE_FILENAME);
    if !path.exists() {
        std::fs::write(&path, COMPOSE_TEMPLATE).context("writing docker-compose.yml")?;
        println!("wrote {}", path.display());
    }
    Ok(path)
}

fn run_compose(compose_path: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("docker")
        .arg("compose")
        .args(["-f", &compose_path.to_string_lossy()])
        .args(args)
        .status()
        .context("running `docker compose` — is Docker installed and on PATH?")?;
    if !status.success() {
        anyhow::bail!("docker compose {args:?} exited with {status}");
    }
    Ok(())
}

const COMPOSE_TEMPLATE: &str = r#"# ebina local deps — Ollama (embed), SearXNG (search_web), and an SSH
# target (ssh_exec) that authenticates with this workspace's sibling
# ssh_ebina/ssh_ebina.pub keypair. Written by `ebinactl local-deps start`;
# edit freely, it won't be overwritten once it exists.

services:
  ollama:
    image: ollama/ollama:latest
    ports:
      - "11434:11434"
    volumes:
      - ollama-data:/root/.ollama
    restart: unless-stopped

  searxng:
    image: searxng/searxng:latest
    ports:
      - "8888:8080"
    environment:
      - SEARXNG_BASE_URL=http://localhost:8888/
    restart: unless-stopped

  ssh-target:
    image: linuxserver/openssh-server:latest
    ports:
      - "2224:2222"
    environment:
      - PUBLIC_KEY_FILE=/keys/ssh_ebina.pub
      - USER_NAME=ebina
      - PASSWORD_ACCESS=false
      - SUDO_ACCESS=true
    volumes:
      - ./ssh_ebina.pub:/keys/ssh_ebina.pub:ro
    restart: unless-stopped

volumes:
  ollama-data:
"#;
