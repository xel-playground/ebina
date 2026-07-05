use anyhow::{Context, Result};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next();

    if first.as_deref() == Some("serve") {
        return serve().await;
    }

    // direct CLI run, mainly for dev/testing: `kernel <wasm-path> [guest-args...]`
    let agent_home = std::env::var("AGENT_HOME").unwrap_or_else(|_| "agent-home".to_string());
    let wasm_path = first.unwrap_or_else(|| "target/wasm32-wasip1/debug/agent.wasm".to_string());
    let mut guest_args: Vec<String> = args.collect();
    if guest_args.is_empty() {
        guest_args.push("hello".to_string());
    }

    // wasmtime + reqwest's blocking client both panic if driven directly on
    // a tokio runtime thread ("cannot start/drop a runtime from within a
    // runtime") — spawn_blocking moves this off the async reactor, same as
    // the gateway's run_trigger does for every request
    let outcome = tokio::task::spawn_blocking(move || {
        let guest_args: Vec<&str> = guest_args.iter().map(String::as_str).collect();
        kernel::run_agent(&agent_home, &wasm_path, &guest_args)
    })
    .await??;
    print!("{}", outcome.stdout);
    if let Some(t) = outcome.sleep_until {
        eprintln!("[kernel] agent asked to sleep until {t}");
    }

    Ok(())
}

async fn serve() -> Result<()> {
    let agent_home = PathBuf::from(std::env::var("AGENT_HOME").unwrap_or_else(|_| "agent-home".to_string()));
    let wasm_path =
        std::env::var("AGENT_WASM").unwrap_or_else(|_| "target/wasm32-wasip1/debug/agent.wasm".to_string());
    let port: u16 = std::env::var("GATEWAY_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);

    // the gateway's own login token lives in the vault like any other
    // secret — never an env var, never generated on the fly. Fail fast if
    // it's missing rather than silently starting an unauthenticated (or
    // randomly-authenticated, equally useless) gateway.
    let secrets_path = kernel::secrets_path(&agent_home);
    let secrets = kernel::secrets::Secrets::load(&secrets_path);
    let token = secrets
        .get("gateway_token")
        .map(str::to_string)
        .with_context(|| {
            format!(
                "no `gateway_token` secret in {}. Add one, e.g.:\n  gateway_token = \"pick-something-long\"",
                secrets_path.display()
            )
        })?;

    kernel::gateway::serve(kernel::gateway::GatewayConfig {
        agent_home,
        wasm_path: wasm_path.into(),
        token,
        port,
    })
    .await
}
