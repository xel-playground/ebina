# ebina — WASM Agent OS

Toy microkernel agent OS. See [PROJECT.md](PROJECT.md) for full design/TODO.

## Layout

- `kernel/` — host binary: wasmtime embed, syscalls, gateway. **Pure API
  server** — it has no knowledge of `webui/` at all, not even a static file
  path; `/` returns a plain 404.
- `agent/` — guest agent, built to `wasm32-wasip1`
- `webui/` — the gateway's web UI: a fully independent Vite + Vue project.
  Nothing wires it to `kernel/` except both talking to the same `/api/*` HTTP
  surface — no shared code, no build-time embedding, no runtime static-file
  serving from the kernel side. Run them separately (see below); a wrapper
  that starts both together with one command doesn't exist yet.
- `tmp/agent-home/` — an agent's whole world (config, memory, workspace, logs); gitignored
- `tmp/secrets.toml` — API keys, lives *outside* agent-home so the sandboxed guest can never read it

## Build

```bash
cargo build -p kernel
cd agent && cargo build --target wasm32-wasip1
cd webui && npm install && npm run build   # produces webui/dist/
```

## Running

One-off CLI run (mainly for dev/testing):

```bash
AGENT_HOME=tmp/agent-home cargo run -p kernel -- target/wasm32-wasip1/debug/agent.wasm run '{"type":"manual","text":"..."}'
```

Gateway API (chat/status/notes/skills/grants/config/secrets/logs — no UI, just `/api/*`):

```bash
export AGENT_HOME=tmp/agent-home
export AGENT_WASM=target/wasm32-wasip1/debug/agent.wasm
export GATEWAY_PORT=8099
cargo run -p kernel -- serve
```

The gateway's login token is **not** an env var — it must be a `gateway_token`
entry in `tmp/secrets.toml` (kernel refuses to start without one).

**Real frontend/backend split** (PROJECT.md §4.4 note — this reverses an
earlier "single embedded HTML file, no build step" decision, and then goes
further at the user's explicit request: the kernel doesn't even serve
`webui/dist/` as static files anymore — it used to, via `tower-http`'s
`ServeDir`, and that coupling was deliberately removed too). Run both
separately — there's no single command that starts both yet:

```bash
# terminal 1 — the gateway
cargo run -p kernel -- serve

# terminal 2 — Vite dev server on :5173, proxies /api to localhost:8099
# (see webui/vite.config.js) so the browser only ever talks to one origin
cd webui && npm run dev
```

For a built, non-dev frontend (`npm run build` → `webui/dist/`), you need
your own static file server pointed at that directory (`npx serve dist`,
`python3 -m http.server`, etc.) — the kernel won't do it, and without a dev
proxy in front, cross-origin requests from that server's port to the
kernel's port need CORS or a reverse proxy in between. Neither exists yet;
a wrapper tying kernel + built webui together into one command is planned
but not built.

## Credentials

`tmp/agent-home/config.toml` never holds a literal API key — it lives inside
agent-home, which the guest reads to build its own prompt. Instead:

```toml
[llm]
api_key = "{secrets.ollama}"   # name of an entry in tmp/secrets.toml
```

```toml
# tmp/secrets.toml
ollama = "the-real-key"
```

## LLM / embed providers

`[llm]`/`[embed]` each have a `provider` field (`"anthropic"` or `"ollama"`)
that controls request/response shaping (auth header style, `messages` vs
Anthropic's top-level `system`, token-usage field names, etc.) — the guest
never needs to know which provider is behind `llm_call`/`embed`.

### No embed provider available? Run one locally

Some API keys (e.g. an Ollama Cloud key on the free tier) only cover chat
models, not embeddings. Rather than trying to run an embedding model *inside*
the wasm sandbox (see below for why not), run Ollama locally as the embed
backend:

```bash
# no sudo needed — extract anywhere, e.g. ~/.local/ollama
curl -fsSL https://github.com/ollama/ollama/releases/latest/download/ollama-linux-amd64.tar.zst -o /tmp/ollama.tar.zst
mkdir -p ~/.local/ollama && zstd -d /tmp/ollama.tar.zst -c | tar -x -C ~/.local/ollama
~/.local/ollama/bin/ollama serve &          # listens on localhost:11434
~/.local/ollama/bin/ollama pull nomic-embed-text
```

Then in `tmp/agent-home/config.toml`:

```toml
[embed]
base_url = "http://localhost:11434/api/embed"
model = "nomic-embed-text"
provider = "ollama"
api_key = "{secrets.local}"   # value ignored by an unauthenticated local server
```

### Why not run the embedding model inside the wasm sandbox instead?

Would avoid the local-install step, but not worth it:

- **Efficiency**: wasm32-wasip1 has no stable SIMD, is single-threaded, and
  is capped at 512MB memory (PROJECT.md 4.1). Transformer inference that
  takes single-digit milliseconds natively can take orders of magnitude
  longer under those constraints.
- **Doesn't fit the security model**: `embed`/`llm_call` are designed as
  syscalls precisely so the API key (or, for local inference, the model
  weights) stays on the host side. Moving inference into the guest either
  ships model weights into the sandbox for no security benefit, or still
  ends up calling back out to the host for the actual compute — at which
  point it's just a syscall with extra steps.

Host-side inference (local Ollama or a cloud API) through the existing
`embed` syscall is the intended design, not a workaround.

## Web search (`search_web` syscall)

Same pattern as `llm_call`/`embed`: config-driven endpoint, key (if any)
resolved host-side from the vault, guest never sees it. Two providers:

- **`tavily`** — hosted, free tier, needs an API key (`{secrets.tavily}`)
- **`searxng`** — self-hosted, free forever, no key, no per-query cost —
  aggregates results from other search engines rather than having its own
  index

Self-hosting SearXNG locally (same idea as the local-Ollama section above):

```bash
mkdir -p ~/.local/searxng
cat > ~/.local/searxng/settings.yml <<'EOF'
use_default_settings: true
general:
  instance_name: "ebina-local-searxng"
search:
  formats:
    - html
    - json          # disabled by default upstream — required for the API
server:
  secret_key: "change-me-if-ever-exposed-beyond-localhost"
  limiter: false
  method: "GET"
EOF

docker run -d --name ebina-searxng \
  -p 127.0.0.1:8888:8080 \
  -v ~/.local/searxng:/etc/searxng:rw \
  --restart unless-stopped \
  searxng/searxng:latest
```

Then in `tmp/agent-home/config.toml`:

```toml
[search]
base_url = "http://127.0.0.1:8888/search"
provider = "searxng"
max_results = 5
```

No `api_key` needed for `searxng` (the field is ignored for that provider,
but must still resolve if you *do* set it — omit it entirely). For `tavily`,
set `provider = "tavily"` and `api_key = "{secrets.tavily}"` with a matching
entry in `tmp/secrets.toml`.

**Why bind to `127.0.0.1` and not `0.0.0.0`**: the container has no auth of
its own — anything that can reach the port can query (and, on some
deployments, configure) it. Loopback-only keeps it reachable from this host
alone, same reasoning as the local Ollama embed server.
