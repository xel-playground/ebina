mod embedded;
mod init;
mod local_deps;
mod webui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

const DEFAULT_WORKSPACE: &str = "ebina";
const AGENT_HOME_DIRNAME: &str = "agent-home";
const AGENT_WASM_FILENAME: &str = "agent.wasm";
const WEBUI_DIRNAME: &str = "webui";
const DEFAULT_PORT: u16 = 8080;

/// CLI wrapping the `kernel` library — subcommand groups are per resource
/// (just `agent` for now, single-agent scope) so a later addition (e.g.
/// managing memory/skills from the CLI instead of only the webui) has
/// somewhere to live without flattening everything under the top level.
#[derive(Parser)]
#[command(name = "ebinactl", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the (single, for now) agent
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Manage local docker-compose deps (Ollama/SearXNG/an ssh_exec target)
    #[command(subcommand)]
    LocalDeps(LocalDepsCommand),
}

#[derive(Subcommand)]
enum LocalDepsCommand {
    /// Write docker-compose.yml if missing, then `docker compose up -d`
    Start {
        /// workspace root (default: ./ebina) — compose file + ssh_ebina.pub
        /// (mounted into the ssh target) both live here
        workspace: Option<PathBuf>,
    },
    /// `docker compose down`
    Stop {
        workspace: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Scaffold a fresh agent-home
    Init {
        /// workspace root (default: ./ebina) — creates <root>/agent-home/
        /// (guest-visible) plus <root>/secrets.toml and <root>/.git
        /// (host-only siblings, see `kernel::secrets_path`/`autocommit.rs`)
        workspace: Option<PathBuf>,
    },
    /// Start the gateway serving an agent-home
    Run {
        /// workspace root (default: ./ebina) — same one passed to `init`
        workspace: Option<PathBuf>,
        /// gateway HTTP port
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// path to the compiled agent.wasm (default: <workspace>/agent.wasm
        /// — a sibling of agent-home, same tier as secrets.toml/.git, so a
        /// workspace is one self-contained, portable unit: drop a built
        /// agent.wasm in there and the whole thing runs from anywhere)
        #[arg(long)]
        wasm: Option<PathBuf>,
        /// path to a built webui (default: <workspace>/webui — the output
        /// of `npm run build` in webui/, i.e. its `dist/` renamed/copied
        /// here). Optional: if this doesn't exist and wasn't explicitly
        /// passed, `run` just serves the bare API on --port same as
        /// always — the webui is an opt-in extra, not a hard requirement
        #[arg(long)]
        webui: Option<PathBuf>,
    },
}

/// `<workspace>/agent-home` — never `<workspace>` itself. The kernel's
/// sibling-of-agent_home convention (secrets.toml, the autocommit git-dir)
/// computes siblings as `agent_home.parent()`; if `agent_home` were a bare
/// single-segment path like the workspace root itself, `.parent()` is the
/// empty path, meaning secrets.toml/.git would land directly in whatever
/// directory `ebinactl` happened to be invoked from — silently scattering
/// host-only files into an unrelated cwd (and risking colliding with a real
/// `.git` if run inside one). One extra path segment fixes that
/// structurally: `workspace` is the container, `agent-home` is the guest's
/// root, siblings (secrets.toml, .git, and — see `Command::Run`'s default
/// `--wasm` — agent.wasm itself) land inside `workspace` alongside it,
/// nothing escapes.
fn agent_home_path(workspace: &std::path::Path) -> PathBuf {
    workspace.join(AGENT_HOME_DIRNAME)
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Agent(AgentCommand::Init { workspace }) => {
            init::init_agent_home(&agent_home_path(&workspace.unwrap_or_else(|| DEFAULT_WORKSPACE.into())))
        }
        Command::Agent(AgentCommand::Run { workspace, port, wasm, webui }) => {
            let workspace = workspace.unwrap_or_else(|| DEFAULT_WORKSPACE.into());
            let wasm_path = wasm.unwrap_or_else(|| workspace.join(AGENT_WASM_FILENAME));
            let webui_dir = webui.unwrap_or_else(|| workspace.join(WEBUI_DIRNAME));
            run(agent_home_path(&workspace), wasm_path, webui_dir, port).await
        }
        Command::LocalDeps(LocalDepsCommand::Start { workspace }) => {
            local_deps::start(&workspace.unwrap_or_else(|| DEFAULT_WORKSPACE.into()))
        }
        Command::LocalDeps(LocalDepsCommand::Stop { workspace }) => {
            local_deps::stop(&workspace.unwrap_or_else(|| DEFAULT_WORKSPACE.into()))
        }
    }
}

async fn run(agent_home: PathBuf, wasm_path: PathBuf, webui_dir: PathBuf, port: u16) -> Result<()> {
    if !agent_home.join("config.toml").exists() {
        anyhow::bail!(
            "{} has no config.toml — run `ebinactl agent init` on its workspace first",
            agent_home.display(),
        );
    }
    if !wasm_path.exists() {
        anyhow::bail!(
            "{} doesn't exist — build the agent (`cargo build -p agent --target wasm32-wasip1`) \
             and place/copy it there, or pass a different one with --wasm",
            wasm_path.display(),
        );
    }

    // the gateway's own login token lives in the vault like any other
    // secret — never an env var, never generated on the fly here (that
    // happens once, in `init`). Fail fast if it's missing rather than
    // silently starting an unauthenticated (or randomly-authenticated,
    // equally useless) gateway.
    let secrets_path = kernel::secrets_path(&agent_home);
    let secrets = kernel::secrets::Secrets::load(&secrets_path);
    let token = secrets.get("gateway_token").map(str::to_string).with_context(|| {
        format!("no `gateway_token` secret in {} — run `ebinactl agent init` on its workspace to generate one", secrets_path.display())
    })?;

    println!("[ebinactl] agent-home: {}", agent_home.display());
    println!("[ebinactl] wasm: {}", wasm_path.display());

    if !webui::looks_like_a_build(&webui_dir) {
        println!("[ebinactl] no webui build at {} — serving the bare API only", webui_dir.display());
        return kernel::gateway::serve(kernel::gateway::GatewayConfig { agent_home, wasm_path, token, port }).await;
    }

    // webui present: the gateway moves to `port + 1` and `webui::serve`
    // takes the public `port` instead, serving static files +
    // reverse-proxying `/api/*` to the gateway — see webui.rs's doc comment
    // for why (same-origin `fetch`, no CORS setup needed on either side,
    // matches the dev-time Vite proxy). Not truly internal-only: kernel's
    // own `gateway::serve` always binds `0.0.0.0` (not configurable from
    // here without changing kernel itself), so `port + 1` is still
    // network-reachable directly, just undocumented/not meant to be used
    // that way — it's still gated by the same bearer token either way, so
    // this isn't an auth gap, just one more open port than strictly ideal.
    let internal_port = port.checked_add(1).context("--port is too close to u16::MAX to also bind an internal port at port+1")?;
    let internal_addr = std::net::SocketAddr::from(([127, 0, 0, 1], internal_port));
    println!("[ebinactl] gateway (internal): http://0.0.0.0:{internal_port} (go through --port {port} instead)");

    let gateway = tokio::spawn(kernel::gateway::serve(kernel::gateway::GatewayConfig {
        agent_home,
        wasm_path,
        token,
        port: internal_port,
    }));
    let web = tokio::spawn(webui::serve(webui_dir, internal_addr, port));

    tokio::select! {
        res = gateway => res.context("gateway task panicked")?,
        res = web => res.context("webui task panicked")?,
    }
}
