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

## Persona (`SOUL.md`)

`tmp/agent-home/SOUL.md` — free-form markdown, no required shape (persona,
values, tone, whatever). If present, it's included **in full** in every
system prompt (unlike skills, which are progressive-disclosure: name +
description until asked for). No dedicated action for it — the agent reads
and edits it with the same `read_file`/`write_file` actions it uses for
anything else at `/SOUL.md`; a human can do the same via `GET`/`POST
/api/soul` (raw text, same shape as `/api/config`) or the webui's Soul tab.
It's fine for the file not to exist yet.

## LLM / embed providers

`[llm]`/`[embed]` each have a `provider` field (`"anthropic"`, `"ollama"`, or
`[llm]`-only `"openai"`) that controls request/response shaping (auth header
style, `messages` vs Anthropic's top-level `system`, token-usage field
names, etc.) — the guest never needs to know which provider is behind
`llm_call`/`embed`.

`provider = "openai"` is the generic OpenAI-compatible chat completions
shape (Bearer auth, `choices[].message.content`,
`usage.prompt_tokens`/`completion_tokens`, non-streaming) — covers any
OpenAI-style API, e.g. [Kimi/Moonshot](https://platform.kimi.ai):

```toml
[llm]
base_url = "https://api.moonshot.ai/v1/chat/completions"
model = "kimi-k2.6"
provider = "openai"
api_key = "{secrets.kimi}"
```

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

## Directory/file management (`list_dir`/`make_dir`/`delete_path` actions)

`read_file`/`write_file` only handle one already-known file each — these
three cover "what's actually in this directory" and basic file/directory
management. Same as `read_file`/`write_file`, these run guest-side against
the agent's own already-preopened `/` (`agent_loop.rs`, plain `std::fs`) —
no separate sandbox, no host syscall, since it's the agent's own trusted
code operating on its own already-fully-accessible agent-home, not
untrusted code that needs isolating:

- `{"action":"list_dir","path":"..."}` — lists a directory's entries
  (directories get a trailing `/`)
- `{"action":"make_dir","path":"..."}` — creates a directory, parent
  directories included
- `{"action":"delete_path","path":"...","recursive":false}` — removes a
  file; a directory needs `"recursive":true` or it's refused

## SSH (`ssh_exec` syscall)

Runs one command on a single, human-fixed SSH target — e.g. a Docker Linux
container reachable over the network — for things like driving `git` or
other dev work the sandboxed agent-home can't do on its own. Deliberately
the one syscall here that hands the agent something close to a real shell:
there's no bounded operation set like `db_exec`'s SQL authorizer, so treat
it as real remote code execution and scope the target accordingly (a
disposable dev container, not anything holding data you care about).

**Setup**:

```toml
# agent-home/config.toml
[ssh]
host = "192.168.1.50"
port = 22
user = "dev"
timeout_secs = 30        # hard wall-clock cap per command, see below
max_output_bytes = 65536 # combined stdout+stderr cap
```

```toml
# tmp/secrets.toml — private key file lives on disk outside agent-home;
# only its *path* is a secret here, never the key bytes themselves
ssh_key_path = "/home/you/.ssh/id_ed25519"
ssh_key_passphrase = "only if the key needs one"
```

No `[ssh]` host or no `ssh_key_path` secret → `ssh_exec` returns
`not_configured`, same graceful-disable pattern as the Discord adapter.

**Containment is about blast radius, not capability** — the command itself
can do anything that SSH user can do on that host. What's actually bounded:
the target is fixed by the human in `config.toml` (the agent can't be
tricked via prompt injection into connecting somewhere else), there's no
interactive shell/pty (one command in, one result out), and every call is
logged in full to `agent-home/logs/ssh.jsonl` (command, exit code, byte
counts, timed-out flag).

**The timeout is not optional**: a command that never exits on its own
(`docker logs -f`, `tail -f`, an interactive prompt waiting on input) would
otherwise hang forever — and since `run_lock` (kernel/src/gateway.rs) holds
one global mutex for the agent's entire run, one hung `ssh_exec` call
freezes *every* surface (webui, Discord, cron, everything), not just
itself. `timeout_secs` is a hard wall-clock deadline checked on every read,
independent of whether the command is still actively producing output — it
returns `{"timed_out": true}` with whatever partial output was captured
rather than hanging.

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

## Discord adapter

Optional — attaches to the gateway (`kernel/src/discord.rs`), doesn't touch
kernel core (same idea as PROJECT.md's not-yet-built Telegram adapter). No
`discord_bot_token` secret configured → Discord is simply not connected,
gateway runs exactly as without it.

**Setup**:

1. [Discord Developer Portal](https://discord.com/developers/applications) →
   New Application → Bot tab → Reset Token, copy it.
2. Same Bot tab → enable **Message Content Intent** (a privileged intent —
   without it, `message.content` arrives empty for guild channel messages;
   DMs still work either way, but @mentions in a server won't).
3. OAuth2 → URL Generator → scope `bot`, permissions at least "Send
   Messages" + "Read Message History" → open the generated URL, invite the
   bot to your server.
4. Add the token to the vault:

```toml
# tmp/secrets.toml
discord_bot_token = "the-real-bot-token"
```

5. Restart the gateway (it reads secrets once at startup) — `[discord]
   connected as <bot name>` in the log means it's live.

**Behavior**: only replies to a **DM** or an **@mention** in a server
channel — not every message in every channel it can see (too noisy/costly
otherwise). Each DM/channel gets its own chat session
(`discord-dm-<user>`/`discord-channel-<channel>`, under
`agent-home/logs/chat_sessions/<key>/`) — separate history from the webui's
`webui` session and from each other, so conversations don't bleed together.
Everything else (memory/notes/ RAG, `SOUL.md`, skills, scheduled tasks) is
shared — one agent, one long-term brain, multiple separate conversation
threads with it. Long replies get split across multiple Discord messages
(2000-char API limit per message), not truncated.

**Compact/reset**: send `!reset` (archive + start fresh) or `!compact`
(archive + collapse into one short summary turn) as a DM or @mention — same
mechanism as the two buttons on the webui Chat panel, just scoped to that
one Discord session. A session also auto-compacts on its own once its
context grows past `[chat] auto_compact_tokens` in `config.toml` (default
50000) — mainly for Discord, since unlike webui there's no one watching a
context-window indicator to know when to hit the button.
