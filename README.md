# ebina — Agent OS

Toy microkernel agent OS. See [PROJECT.md](PROJECT.md) for full design/TODO.

## Layout

- `kernel/` — host binary: agent process runner, syscalls, gateway. **Pure API
  server** — it has no knowledge of `webui/` at all, not even a static file
  path; `/` returns a plain 404.
- `agent/` — the agent, a native binary the kernel spawns once per run
- `webui/` — the gateway's web UI: a fully independent Vite + Vue project.
  Nothing wires it to `kernel/` except both talking to the same `/api/*` HTTP
  surface — no shared code, no build-time embedding, no runtime static-file
  serving from the kernel side.
- `cli/` (`ebinactl`) — the actual entry point for running an agent day to
  day: scaffolds a fresh agent (`agent init`), starts the gateway + webui
  together behind one port (`agent run`), and spins up local Docker deps
  for `[embed]`/`[search]`/`[ssh]` (`local-deps start`). Depends on `kernel`
  as a library; deliberately never touches `kernel`'s own CLI/`main.rs`.
  the `agent` binary and the built `webui/dist/` are embedded into the `ebinactl`
  binary itself at compile time (`cli/src/embedded.rs`) — `init` writes them
  out, no separate build/copy step for a human running it.
- an **`ebinactl` workspace** (e.g. `./ebina`, or `tmp/main` in this repo's
  own dev/test setup) — the container a running agent actually lives in:
  `<workspace>/agent-home/` (the agent's filesystem root: config, memory,
  workspace, logs), plus host-only siblings `<workspace>/secrets.toml`,
  `<workspace>/.git` (autocommit history), `<workspace>/agent`,
  `<workspace>/webui/`. One workspace = one self-contained agent; nothing
  inside it ever depends on where `ebinactl` itself was built or run from.

## Architecture

Core idea: **full autonomy inside agent-home, a minimal boundary around
it.** The agent runs as a plain native child process of the kernel. The
model never runs code of its own — it can only pick from the agent's fixed
action set, and every action that reaches outside agent-home (LLM, network,
SSH, the database, notifications) is a kernel syscall sent over a private
socket. File actions go through the agent's path layer (`agent/src/vfs.rs`),
which re-roots every path at agent-home and refuses `..`/symlink escapes.

This is *not* a hard sandbox the way the old wasmtime build was: the agent
process runs with the kernel's own uid, so the containment rests on the
agent binary being trusted code, not on the OS. What the kernel still
enforces from outside: a cleared environment (no host secrets), a 512MB
`RLIMIT_AS` cap, a whole-run wall-clock deadline (SIGKILL), and
stdout captured to logs.

```
┌──────────────────────────────────────────────────────────────────┐
│ ebinactl (CLI) — agent init / agent run / local-deps               │
│  spawns the gateway, serves webui/, reverse-proxies /api/*         │
└──────────────────────────────┬─────────────────────────────────────┘
                                │
┌───────────────────────────────▼─────────────────────────────────────┐
│ kernel (host, Rust)                                                   │
│                                                                        │
│  ┌─────────────┐   ┌───────────────┐   ┌────────────────────────┐   │
│  │ Gateway      │   │ Scheduler      │   │ Syscall dispatch        │   │
│  │ (axum)       │   │ (30s tick:     │   │ llm_call · embed        │   │
│  │ webui API,   │   │  cron ·        │   │ db_exec (RAG engine —   │   │
│  │ SSE, grants  │   │  daily_        │   │  only agent/src/        │   │
│  │ (tofu domain)│   │  maintenance · │   │  memory.rs calls this,  │   │
│  │              │   │  scheduled_    │   │  no guest action        │   │
│  │              │   │  task)         │   │  reaches it directly)   │   │
│  └──────┬───────┘   └───────┬────────┘   │ http_fetch (domain-gated)·│   │
│         │ per-session         │            │  search_web · ssh_exec  │   │
│         │ + per-task_id        │            │ notify · chat_send      │   │
│         │ (else concurrent)    │            │ sleep_until ·           │   │
│         └──────────┬──────────┘            │  schedule_task family   │   │
│                     ▼                       └────────────┬─────────────┘
│         ┌─────────────────────────────────────────────────┐          │
│         │ agent process — fresh spawn every single run,     │          │
│         │ zero state carried over in memory                  │          │
│         │ run deadline (SIGKILL) · RLIMIT_AS · empty env ·    │        │
│         │ stdout → log · syscalls over socketpair fd 3        │        │
│         │  ┌──────────────────────────────────────────────┐   │        │
│         │  │ agent (native) — the *only* code that can ever  │   │        │
│         │  │ emit a syscall                                  │   │        │
│         │  └──────────────────────────────────────────────┘   │        │
│         └────────────────────────┬────────────────────────────┘        │
│                                   │ cwd + vfs path layer                │
│         ┌─────────────────────────▼───────────────────────────┐        │
│         │ agent-home/ — the entire "/" the agent can see         │      │
│         │  config.toml · SOUL.md · memory/(notes, skills,        │      │
│         │  index.db) · workspace/ · scheduler/ · logs/            │      │
│         └───────────────────────────────────────────────────────┘      │
└────────────────────────────────┬────────────────────────────────────────┘
              ┌───────────────────┼──────────────────┬─────────────────┐
              │ HTTPS (API key)   │ HTTPS (GET-only)  │ SSH (one fixed  │
              ▼                   ▼                   │  target, no pty)│
        LLM / embed API     open internet (read-only)  ▼
                                                   docker/VM — the one
                                                   syscall here with no
                                                   pre-approval gate at all
```

**Every run is stateless, persistent on disk.** Each trigger
(`message`/`cron`/`daily_maintenance`/`scheduled_task`/`manual`) gets a
brand-new agent process — nothing survives in memory between runs.
What *does* persist is entirely on the filesystem, under `agent-home/`:
`memory/notes/` (RAG-searched curated facts + a raw per-run log, see
[Memory](#memory-memorynotes-rag--daily_maintenance) below),
`logs/chat_sessions/<key>/session.json` (per-surface conversation history —
webui and each Discord DM/channel get their own key), `scheduler/` (cron
jobs the agent set up for itself), and `logs/*.jsonl` (egress, SSH,
notifications, LLM transcripts — full audit trail, nothing silently
dropped).

**Locking is per-session (and per-scheduled-task), not global.** Only
`message`/`compact_session` runs get serialized against their own session —
two turns on the *same* session queue behind each other (`session.json` is
a read-modify-write per turn), but two different sessions, or a background
trigger (`cron`/`daily_maintenance`/`scheduled_task`/`manual`) running
alongside a chat reply, run fully concurrently with no lock at all. A
`scheduled_task` additionally locks per task id, so the same task can't run
twice at once if a cron tick and a manual `/api/wake` land together. Both
lock tables prune idle entries automatically so a long-running gateway
doesn't accumulate one forever per session/task it's ever seen. This model
took a real audit to get right — a background trigger's `chat_send`
(proactively messaging a session it doesn't own) used to be able to race a
live conversation's own end-of-turn save and silently erase either side;
fixed by having the conversation's own save always reload fresh immediately
before writing rather than trust a snapshot taken before the run started.

That concurrency was only real in theory until 2026-07-12, though:
`scheduler_loop`'s 30s tick used to `.await` every fired trigger inline, one
after another, in a single sequential loop body — a multi-minute
`daily_maintenance` LLM call blocked the same tick from ever reaching that
tick's `scheduled_task` due-check, so an exact-minute cron task (e.g.
`rss_tech_report`'s `17 9 * * *`) due a few minutes later could silently
miss its entire day with no error at all. Found because a user-configured
task actually missed a day in production. Fixed by having the tick loop
`tokio::spawn` each fired trigger instead of awaiting it — the scheduler's
event log (see [Scheduler](#scheduler-event-sourced-idempotent)) keeps at
most one occurrence of each maintenance tier in flight (the one invariant
that still needs enforcing; everything else was already documented as safe
to run concurrently, just never actually exploited).
`POST /api/abort?session=<key>` (the webui's Stop button, defaulting to the
webui session) is cooperative: it sets a flag `llm_call` checks between
streamed chunks, scoped per session so aborting one doesn't touch another
concurrent run — but a run blocked inside `http_fetch`/`ssh_exec`, or still
waiting on `llm_call`'s first response byte, just runs to completion
instead of stopping instantly. An earlier version of this ran every trigger
as its own killable child process for a true instant stop; reverted — the
guaranteed-stop property wasn't worth carrying a second binary that had to
ship alongside the first and could drift out of version sync with it.

**Syscalls are the entire capability surface** — 12 of them
(`llm_call`, `embed`, `db_exec`, `http_fetch`, `search_web`, `ssh_exec`,
`notify`, `chat_send`, `sleep_until`, `schedule_task`/`update_task`/
`delete_task`), dispatched from one match statement
(`kernel/src/syscalls/mod.rs`). Each caps a specific kind of harm rather
than trying to sandbox arbitrary behavior: `http_fetch`'s containment is
the domain gate (`[network] get_mode`), applied uniformly whether the
request is a plain read or a `POST`/`PUT`/`PATCH`/`DELETE` — not a
structural ban on writing, `db_exec` is
never reachable from a guest action at all (only the host-side RAG engine
calls it), `ssh_exec` is the one deliberate exception — a fixed,
human-configured target with no pre-approval gate, because an equivalent
capability (arbitrary command execution) existing ungated *somewhere* made
gating a second, weaker path to the same place pure friction rather than
real containment.

**Trust boundary, not a content filter**: nothing here scans `http_fetch`
results or tool output for prompt injection. The bet is a narrow boundary
(writes only to a fixed target — `ssh_exec` — or a domain-gated one —
`http_fetch` — never an unfiltered arbitrary destination, no filesystem
access outside agent-home) beats trying to detect malicious instructions in
arbitrary text — read [Web fetch](#web-fetch-http_fetch-syscall)'s
"untrusted content from the open internet, same as a tool's stdout" framing
for how the agent itself is told to treat it.

## Scheduler (event-sourced, idempotent)

Every self-driven run — the three maintenance tiers, the agent's own
`sleep_until` wake (`cron`), and each `scheduled_task` — is an
*occurrence* `<job>@<slot>` (e.g. `task:abc@1783328400`), and its whole
lifecycle is an append-only event log, `logs/scheduler_events.jsonl`
(`kernel/src/schedule_log.rs`):

- **Idempotent scheduling** — the occurrence id is the key; the 30s tick
  noticing the same due slot twice, or again after a restart, is a no-op.
  A job with an open (pending/running) occurrence doesn't get a newer one
  stacked on top.
- **Exclusive execution** — a pending occurrence is claimed by writing its
  `started` event under the same lock that checks its state, so it can
  never start twice; at most one running occurrence per job.
- **States** — `pending` → `running` → `succeeded` / `failed`, or
  `skipped` (due, but nothing to do). A failed attempt goes back to
  `pending` with a 15-minute backoff, up to 3 retries; then it's terminal
  until its next natural slot (or a manual Retry from the webui).
- **Restart recovery** — the state is rebuilt by replaying the log; an
  occurrence still `running` at boot was cut off mid-run and is recorded as
  a failed attempt, so the normal retry policy re-runs it (this can repeat
  side effects like `chat_send` that the cut-off attempt already made).

`GET /api/scheduler/occurrences` returns the projected state (the
Schedule history panel shows it); `POST
/api/scheduler/occurrences/{id}/retry` re-queues a terminally failed one.
Per-attempt details (trigger + outcome) still land in
`logs/scheduled_runs/`, linked from each occurrence's `run_file`.

## Quick start (`ebinactl`)

This is the normal way to run an agent — build once, then everything else
is one binary:

```bash
cargo build -p kernel
cargo build -p agent --release
cd webui && npm install && npm run build && cd ..   # produces webui/dist/
cargo build -p ebinactl   # embeds the agent binary + webui build from the two steps above
```

```bash
./target/debug/ebinactl agent init ./ebina        # scaffolds ./ebina/agent-home, generates gateway_token + an ssh keypair
./target/debug/ebinactl local-deps start ./ebina   # optional: Ollama (embed) + SearXNG (search) + an ssh_exec target, docker compose
# edit ./ebina/agent-home/config.toml's [llm] section + add the matching key
# to ./ebina/secrets.toml — the one thing local-deps can't provide for you
./target/debug/ebinactl agent run ./ebina          # gateway + webui on one port (default 8080)
```

`agent run` moves the raw gateway to `port + 1` and serves the webui (static
files + a reverse proxy to `/api/*`) on the public `--port` — one origin, no
CORS, matches how the Vite dev proxy behaves. If `<workspace>/webui/` isn't
there (or was never written), it just serves the bare `/api/*` gateway on
`--port` directly. See `ebinactl --help` / `ebinactl agent --help` for every
flag (`--agent`, `--webui`, `--port`, workspace defaults to `./ebina`).

The gateway's login token is **not** an env var — it's the `gateway_token`
entry `agent init` generates in `<workspace>/secrets.toml` (kernel refuses to
start without one).

### `local-deps` — Docker-hosted `[embed]`/`[search]`/`[ssh]`

```bash
./target/debug/ebinactl local-deps start ./ebina   # docker compose up -d + pull/warm the embed model
./target/debug/ebinactl local-deps stop ./ebina    # docker compose down
```

Writes `<workspace>/docker-compose.yml` if missing (edit it freely after —
never overwritten once it exists) and shells out to `docker compose`. Three
services (`ollama`, `searxng`, `ssh-target`), pre-wired to exactly match
`agent init`'s starter `config.toml` so there's nothing to edit after
running this — `[embed]`/`[search]`/`[ssh]` just work.

Never invoked by anything else in this CLI automatically — starting
containers is a real, visible host action (ports, processes, disk for
images/volumes), so it only happens when a human explicitly runs this
command. Requires Docker + the `compose` plugin (`docker compose version`);
not installed for you.

### Manual / dev (no `ebinactl`)

Still works, and is what `ebinactl agent run` does under the hood — useful
for iterating on `kernel`/`webui` themselves without rebuilding the CLI:

```bash
# terminal 1 — the gateway only
cargo build -p agent
AGENT_HOME=tmp/main/agent-home AGENT_BIN=target/debug/agent GATEWAY_PORT=8099 cargo run -p kernel -- serve

# terminal 2 — Vite dev server on :5173, proxies /api to localhost:8099
# (see webui/vite.config.js) so the browser only ever talks to one origin
cd webui && npm run dev
```

One-off single trigger, no gateway (mainly for dev/testing):

```bash
AGENT_HOME=tmp/main/agent-home cargo run -p kernel -- target/debug/agent run '{"type":"manual","text":"..."}'
```

## Credentials

`<workspace>/agent-home/config.toml` never holds a literal API key — it lives inside
agent-home, which the guest reads to build its own prompt. Instead:

```toml
[llm]
api_key = "{secrets.ollama}"   # name of an entry in <workspace>/secrets.toml
```

```toml
# <workspace>/secrets.toml
ollama = "the-real-key"
```

## Persona (`SOUL.md`)

`<workspace>/agent-home/SOUL.md` — free-form markdown, no required shape (persona,
values, tone, whatever). If present, it's included **in full** in every
system prompt (unlike skills, which are progressive-disclosure: name +
description until asked for). No dedicated action for it — the agent reads
and edits it with the same `read_file`/`write_file` actions it uses for
anything else at `/SOUL.md`; a human can do the same via `GET`/`POST
/api/soul` (raw text, same shape as `/api/config`) or the webui's Soul tab.
It's fine for the file not to exist yet.

## Memory

Three layers, two distillation passes:

```
logs/run_log/<date>/log.md   raw log, one entry per run (written automatically)
   │  hourly  daily_maintenance
   ▼
memory/notes/*.md            deep memory — curated facts, searched every turn
   │  daily   core_distillation
   ▼
core.md                      summary — shown in full every turn
```

- **Raw log** — `logs/run_log/<date>/log.md`, one entry per *every* run
  (any trigger type, success or abort), tagged with session and sender.
  Never embedded/searched (embedding it once meant an old, since-corrected
  fact stayed permanently retrievable). Every run's prompt does get the
  last few `message` entries from *other* sessions
  (`[chat] cross_session_staging_entries`, default 5) — the cross-session
  short-term memory, so a Discord chat knows what was just said in the
  webui without waiting for distillation.
- **Notes** — `memory/notes/*.md`, one topic per file. Embedded and
  searched (`hybrid_search` — BM25 + vectors, reciprocal rank fusion)
  every turn. A live chat turn can't write them (its `write_file`/
  `append_file` is scoped to `/workspace/`), so an ad-hoc mid-conversation
  edit can't silently re-clobber an already-corrected fact.
  Background-triggered runs (`cron`/`scheduled_task`/`manual`) keep full
  access. Note size (estimated as 1 token per non-ASCII char + 1 per 4
  ASCII chars) has a soft and a hard cap under `[memory]`: past
  `note_soft_max_tokens` (default 2000) a write still goes through with a
  warning and the note gets listed for distillation in the next
  `core_distillation`; past `note_max_tokens` (default 4000) the write is
  refused.
- **`core.md`** — the summary layer, shown in full in every run's prompt
  (2000-char cap). Only `core_distillation` can write it.

**`daily_maintenance`** (hourly, at :13) — raw log → notes. Reviews only
what's new since its own last successful run (`since_ts`, tracked in
`maintenance_reports/.last_run` — a run that fails doesn't advance it, so a
transient failure can't silently skip a window), plus
`/workspace/memory/` — the spot for short-term notes and pending
human-only action items. Durable facts and real research/analysis go into
notes; a pending human-only item stays in `/workspace/memory/` and gets
called out in the report. One report per run lands in
`maintenance_reports/hourly/<date>_<HHMM>.md`. Skipped without an LLM call
when nothing new was logged and `/workspace/memory/` is empty.

**`core_distillation`** (daily, at :23) — notes → core. Reads the hourly
reports written since its last run (bounded at ~24, never the raw log) and
from those: merges/dedupes fragmented notes and distills over-cap ones,
`chat_send`s the human about a "needs attention" item that's shown up in
3+ reports unchanged, runs the skill quality gate, and promotes/demotes
what belongs in `core.md`. Report lands in
`maintenance_reports/core/<date>_<HHMM>.md`. Skipped when there were no
hourly reports in the window.

**Housekeeping** (every 6h, no LLM) — resets chat sessions idle for over
6h, and checks that `memory/notes/` actually got git commits in the last
6h if anything was logged (`notify()` if not — catches a maintenance pass
self-reporting distillation without anything landing on disk).

### Mid-run compaction (`runtime.in_run_compact_tokens`)

Separate from `daily_maintenance` and from `[chat] auto_compact_tokens`
(which only ever compacts a saved chat *session* between runs) — this one
watches a single *run's own* `messages` array as it grows turn over turn
(tool results piling up: a long `ssh_exec` exploration, several `http_fetch`s
in one run). Once the last `llm_call`'s `input_tokens` crosses
`runtime.in_run_compact_tokens` (default 100,000 — deliberately well under
the model's real context limit, not just avoiding overflow: every internal
tool-call iteration resends the *entire* growing `messages` array, so a
high threshold lets one action-heavy turn rack up several expensive/slow
large-context calls before ever compacting), everything except the system
prompt, the very first task message (so the original ask survives —
summarizing a summary of a summary drifts, but the root intent never
should), and the 2 most recent messages gets summarized via one extra
`llm_call` and spliced back in. Without this, a single long-running turn
loop had no backstop of its own and could blow its own context before ever
reaching `done`.

## LLM / embed providers

`[llm]`/`[embed]` each have a `provider` field (`"anthropic"`, `"ollama"`, or
`[llm]`-only `"openai"`) that controls request/response shaping (auth header
style, `messages` vs Anthropic's top-level `system`, token-usage field
names, etc.) — the guest never needs to know which provider is behind
`llm_call`/`embed`.

`llm_call` always streams now, regardless of provider — each has its own
SSE/NDJSON parser (`kernel/src/syscalls/llm_call.rs`) that appends live
reasoning/thinking deltas to the session's `thinking-live.txt`
(tailed by the webui's Live Log panel via `GET /api/thinking`) and checks
the abort flag between chunks, so a Stop click can cut a response short
mid-stream rather than only ever being able to kill the whole process.

`provider = "openai"` is the generic OpenAI-compatible chat completions
shape (Bearer auth, `choices[].message.content`,
`usage.prompt_tokens`/`completion_tokens`) — covers any OpenAI-style API,
e.g. [Kimi/Moonshot](https://platform.kimi.ai):

```toml
[llm]
base_url = "https://api.moonshot.ai/v1/chat/completions"
model = "kimi-k2.6"
provider = "openai"
api_key = "{secrets.kimi}"
```

### No embed provider available? Run one locally

Some API keys (e.g. an Ollama Cloud key on the free tier) only cover chat
models, not embeddings. Run Ollama locally as the embed backend:

```bash
# no sudo needed — extract anywhere, e.g. ~/.local/ollama
curl -fsSL https://github.com/ollama/ollama/releases/latest/download/ollama-linux-amd64.tar.zst -o /tmp/ollama.tar.zst
mkdir -p ~/.local/ollama && zstd -d /tmp/ollama.tar.zst -c | tar -x -C ~/.local/ollama
~/.local/ollama/bin/ollama serve &          # listens on localhost:11434
~/.local/ollama/bin/ollama pull nomic-embed-text
```

Then in `<workspace>/agent-home/config.toml`:

```toml
[embed]
base_url = "http://localhost:11434/api/embed"
model = "nomic-embed-text"
provider = "ollama"
api_key = "{secrets.local}"   # value ignored by an unauthenticated local server
```

## Directory/file management (`list_dir`/`make_dir`/`delete_path` actions)

`read_file`/`write_file` only handle one already-known file each — these
three cover "what's actually in this directory" and basic file/directory
management. Same as `read_file`/`write_file`, these run guest-side against
the agent's own `/` (`agent_loop.rs` through `vfs.rs`'s re-rooting path
layer) — no host syscall, since it's the agent's own trusted
code operating on its own already-fully-accessible agent-home, not
untrusted code that needs isolating:

- `{"action":"list_dir","path":"..."}` — lists a directory's entries
  (directories get a trailing `/`)
- `{"action":"make_dir","path":"..."}` — creates a directory, parent
  directories included
- `{"action":"delete_path","path":"...","recursive":false}` — removes a
  file; a directory needs `"recursive":true` or it's refused
- `{"action":"append_file","path":"...","content":"..."}` — appends instead
  of overwriting (creating the file if missing); for a growing log/report
  file so the agent isn't forced to `read_file` the whole thing back just to
  add one entry. On a live chat turn, `write_file`/`append_file` are scoped
  to `/workspace/` only (see [Memory](#memory-memorynotes-rag--daily_maintenance)
  above) — background-triggered runs keep full access

### Reading large files (`read_file` paging, `grep_file`)

`read_file` had no size limit at all until `logs/transcripts/*.json` (a full
LLM request/response dump per call) was found to hit 900KB+ — reading one
whole in a single tool result is 200k+ tokens, enough to blow a whole
`llm_call` past its own context limit on its own. Every response now
includes `total_bytes`/`total_lines` regardless of how much content
actually came back, and a file over 100,000 bytes with none of the
following auto-windows to its first 200 lines instead of refusing outright:

- `{"action":"read_file","path":"...","start_line":N,"head_lines":N}` —
  page through a file in windows rather than only ever seeing its start
- `{"action":"read_file","path":"...","tail_lines":N}` — jump to the end;
  almost always what you want for `logs/*.jsonl`-style append-only files,
  where the newest entries are at the end, not the start
- `{"action":"read_file","path":"...","byte_offset":N,"byte_count":N}` —
  raw byte-position slicing, ignoring line breaks entirely. Line-based
  paging can't help when a single line is itself huge (a pretty-printed
  transcript keeps a whole escaped multi-KB string on one line) — this is
  the only way to page through that
- `{"action":"grep_file","path":"...","pattern":"...","max_matches":N}` —
  plain substring search, returns `{"line":N,"text":"..."}` per match (each
  capped at 2,000 bytes); find where something is before deciding how to
  `read_file` it, rather than guessing a `start_line`

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
# <workspace>/agent-home/config.toml
[ssh]
host = "192.168.1.50"
port = 22
user = "dev"
timeout_secs = 30          # hard wall-clock cap per command, see below
max_output_bytes = 3145728 # combined stdout+stderr cap; default shown (3MB), all fields optional/overridable
```

```toml
# <workspace>/secrets.toml — private key file lives on disk outside agent-home;
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
otherwise hang forever. Locking is per-session/per-scheduled-task now, not
one global mutex, so a hung `ssh_exec` no longer freezes *every* surface —
but it still hangs whatever *did* queue behind it (that session's next
message, or that task's next tick), and `POST /api/abort`'s cooperative
flag never reaches a run blocked here. `timeout_secs` is a hard wall-clock
deadline checked on every read, independent of whether the command is
still actively producing output — it returns `{"timed_out": true}` with
whatever partial output was captured rather than hanging.

## Web fetch (`http_fetch` syscall)

`method` defaults to `"GET"` with no `headers`/`body`, which behaves
exactly like a plain read (same HTML-stripping/truncation/caching pipeline
below). `POST`/`PUT`/`PATCH`/`DELETE` with `headers`/`body` are supported
too — writes used to route through here behind a separate approval queue,
removed (and the syscall renamed `http_get` to make that removal
structural) once `ssh_exec` existed as an *ungated* way to do the exact
same thing, which made a write-specific gate here friction rather than
containment; re-added (2026-07-12, back under the name `http_fetch`) once
domain-bound credential support gave writes here a real use (calling an
authenticated API without a remote host). Guards apply uniformly regardless
of method: denylists private/loopback/link-local/metadata IPs (checked
post-DNS-resolution, pinned against rebinding), every request logged in
full regardless of outcome, gated by `[network] get_mode` (`"open"` default
/ `"tofu"` — new domain needs one-time approval / `"allowlist"`).

A header or body value may reference `{secrets.NAME}` — resolved host-side
(`secrets::resolve_placeholders_in`, same function `ssh_exec` uses), the
guest only ever sees the placeholder text. Gated by `[network].credentials`
(`[[network.credentials]] host = "..."`, `secret = "..."`) — `NAME` must be
bound to the *exact* host the request's URL resolves to, or the whole
request fails closed with `bad_secret_placeholder` before anything is sent.
Deliberately a static, human-edited list rather than anything reachable
through `grants.rs`'s tofu queue: that queue is fine for "can this agent
read from this domain at all", a low-stakes, often-routine approval; a
domain+secret binding is a materially different, higher-stakes grant (a
live credential, not just read access), and mixing the two into one
approval flow risks a human approving a credential-carrying request while
thinking it's routine domain access.

An HTML response (by `Content-Type`, or sniffed if that header's missing)
comes back as **extracted text**, not raw markup — `<script>`/`<style>`
blocks and all tags stripped, common entities decoded. Raw HTML is mostly
markup/JS/CSS noise; a plain blog page came back at 400KB+ of it once and
alone blew a single `llm_call` past its model's token limit. The `body`
handed back directly is capped at `[network] response_max_bytes` (default
100,000 bytes ≈ 25-30k tokens) regardless of content type — a marker at the
end says so if it got cut.

The *full* stripped page still gets cached under
`workspace/.http_cache/<hash-of-url>.txt` either way — the response
includes `total_bytes` (the real, untruncated size) and `cache_path`, so
the model can `read_file` past the truncation point (via its own
`start_line`/`byte_offset`/`tail_lines` paging, see
[Reading large files](#reading-large-files-read_file-paging-grep_file)
above) instead of losing the rest of a long page outright. Cache entries
are bounded two ways: a lazy TTL sweep (`[network] http_cache_ttl_secs`,
default 24h, run at the start of every `http_fetch` call) and an LRU eviction
pass keyed on file mtime once the cache exceeds `[network]
http_cache_max_bytes` (default 20MB) — a burst of many unique pages inside
one TTL window doesn't grow the cache unbounded either.

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

Then in `<workspace>/agent-home/config.toml`:

```toml
[search]
base_url = "http://127.0.0.1:8888/search"
provider = "searxng"
max_results = 5
```

No `api_key` needed for `searxng` (the field is ignored for that provider,
but must still resolve if you *do* set it — omit it entirely). For `tavily`,
set `provider = "tavily"` and `api_key = "{secrets.tavily}"` with a matching
entry in `<workspace>/secrets.toml`.

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
# <workspace>/secrets.toml
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
120000) — mainly for Discord, since unlike webui there's no one watching a
context-window indicator to know when to hit the button.
