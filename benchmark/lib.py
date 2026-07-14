"""Shared helpers for the ebina memory-waterfall benchmark.

The whole design leans on one fact about the runtime (see README.md):
`AgentState` is fresh per run (no in-memory carryover), so the *only* state
that matters is what's on disk under agent-home/. That makes this testable
as (disk state, trigger) -> (new disk state, outcome) — seed disk state
directly, fire a trigger via /api/wake, read disk state back, score it.
"""
import json
import os
import subprocess
import time
import urllib.request
import urllib.error

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TESTBED_DIR = os.path.join(REPO_ROOT, "tmp/benchmark-testbed")
AGENT_HOME = os.path.join(TESTBED_DIR, "agent-home")
SECRETS_PATH = os.path.join(TESTBED_DIR, "secrets.toml")
PORT = 8090
BASE_URL = f"http://localhost:{PORT}/api"


def _read_token():
    text = open(SECRETS_PATH).read()
    for line in text.splitlines():
        if line.strip().startswith("gateway_token"):
            return line.split("=", 1)[1].strip().strip('"')
    raise RuntimeError(f"no gateway_token in {SECRETS_PATH}")


TOKEN = None  # lazily read once the testbed's secrets.toml exists


def _headers():
    global TOKEN
    if TOKEN is None:
        TOKEN = _read_token()
    return {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"}


def api_post(path, body, timeout=300):
    req = urllib.request.Request(
        f"{BASE_URL}{path}", data=json.dumps(body).encode(), headers=_headers(), method="POST"
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        return {"ok": False, "error": f"HTTP {e.code}: {e.read().decode()}"}


def api_get(path, timeout=30):
    req = urllib.request.Request(f"{BASE_URL}{path}", headers=_headers(), method="GET")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


class Testbed:
    """Owns the background `ebinactl` process for the whole benchmark run."""

    def __init__(self):
        self.proc = None

    def start(self):
        # Defensive: a previous run's ebinactl child can outlive its parent
        # (killed by something other than this class's own `stop()` — a
        # colliding second run getting killed by hand, a background-task
        # harness reaping the wrapper process without letting Python's
        # `with` block run its `__exit__`, ...). An orphan left listening
        # on PORT means the *next* run's `/api/status` poll below happily
        # answers against that stale process instead of ever noticing a
        # fresh one never started — every seed/wake call then silently
        # targets an agent-home that reset_testbed() has already deleted
        # out from under it. That's exactly what burned real API spend on
        # nothing (2026-07-14 incident) — always clear the port first,
        # unconditionally, before trusting a status response on it.
        subprocess.run(["pkill", "-9", "-f", f"tmp/benchmark-testbed.*--port {PORT}"], stderr=subprocess.DEVNULL)
        time.sleep(1)

        binary = os.path.join(REPO_ROOT, "target/release/ebinactl")
        log_path = os.path.join(TESTBED_DIR, "ebinactl.log")
        log_file = open(log_path, "w")
        self.proc = subprocess.Popen(
            [binary, "agent", "run", TESTBED_DIR, "--port", str(PORT)],
            stdout=log_file,
            stderr=subprocess.STDOUT,
            cwd=REPO_ROOT,
        )
        # poll /api/status instead of a fixed sleep — startup time varies.
        # Checks *this* process is still alive on every iteration too — if
        # ebinactl exits immediately (bad config, port still contested by
        # something pkill didn't match, ...) this fails fast with the real
        # log instead of spinning the full 30s timeout on a dead process.
        for _ in range(60):
            if self.proc.poll() is not None:
                raise RuntimeError(f"testbed process exited immediately (code {self.proc.returncode}) — check {log_path}")
            try:
                api_get("/status", timeout=2)
                return
            except Exception:
                time.sleep(0.5)
        raise RuntimeError(f"testbed didn't come up — check {log_path}")

    def stop(self):
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def __enter__(self):
        self.start()
        return self

    def __exit__(self, *exc):
        self.stop()


def wait_idle(timeout=300):
    """Blocks until /api/status reports not busy — used after firing a
    trigger async-ish via a short-timeout wake, or just to be safe before
    the next step in a sequence."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        status = api_get("/status")
        if not status.get("busy"):
            return status
        time.sleep(2)
    raise TimeoutError("testbed still busy after timeout")


def seed_message(text):
    """Sends one real chat turn through /api/message — exercises the actual
    write_memory_note path, same as a real user message would, rather than
    hand-writing log.md entries (which would test a fake shape of the log,
    not what daily_maintenance actually sees in production)."""
    return api_post("/message", {"text": text})


def fire_trigger(trigger_type, since_ts=0, timeout=300):
    """Directly fires a checkpoint-driven trigger via /api/wake, bypassing
    scheduler_loop's wall-clock gating entirely — same technique used to
    manually test core_distillation in the live system. This is what makes
    multi-day simulation possible without waiting real days: `since_ts` is
    just a number, the agent doesn't know or care whether it's "real" or
    simulated elapsed time."""
    return api_post("/wake", {"type": trigger_type, "since_ts": since_ts}, timeout=timeout)


def run_full_cycle(since_ts=0):
    """daily_maintenance -> maintenance_summary -> core_distillation, in
    order, each waiting for the previous to finish — mirrors the real
    waterfall's dependency order (6h eats 1h's output, 24h eats 6h's), just
    compressed into one synchronous sequence instead of spread across a day."""
    results = {}
    results["daily_maintenance"] = fire_trigger("daily_maintenance", since_ts)
    results["maintenance_summary"] = fire_trigger("maintenance_summary", since_ts)
    results["core_distillation"] = fire_trigger("core_distillation", since_ts)
    return results


def _read_checkpoint(name):
    path = os.path.join(AGENT_HOME, "maintenance_reports", name)
    if not os.path.exists(path):
        return 0
    return int(open(path).read().strip() or 0)


def last_maintenance_ts():
    return _read_checkpoint(".last_run")


def last_summary_ts():
    return _read_checkpoint(".last_summary_run")


def last_core_ts():
    return _read_checkpoint(".last_core_run")


def read_core_md():
    path = os.path.join(AGENT_HOME, "core.md")
    return open(path).read() if os.path.exists(path) else ""


def read_all_notes():
    """{filename: content} for every top-level memory/notes/*.md file."""
    notes_dir = os.path.join(AGENT_HOME, "memory/notes")
    out = {}
    if not os.path.isdir(notes_dir):
        return out
    for name in os.listdir(notes_dir):
        path = os.path.join(notes_dir, name)
        if os.path.isfile(path) and name.endswith(".md"):
            out[name] = open(path).read()
    return out


def read_workspace_memory():
    """{filename: content} for /workspace/memory/*"""
    dir_path = os.path.join(AGENT_HOME, "workspace/memory")
    out = {}
    if not os.path.isdir(dir_path):
        return out
    for name in os.listdir(dir_path):
        path = os.path.join(dir_path, name)
        if os.path.isfile(path):
            out[name] = open(path).read()
    return out


def ask(question):
    """Fires a probe question the *true* recall test (retrieval +
    generation together), not just a placement check.

    Deliberately does NOT go through `/api/message` — that endpoint always
    uses the fixed "webui" session key (`gateway.rs`'s `DEFAULT_SESSION_KEY`,
    not settable via the request body) and hands the agent the *whole*
    conversation `history` for that session. `seed_message` also posts to
    `/api/message`, so a probe fired the same way would land in the exact
    same session — the answer to "what's my cat's name" would just be
    sitting three turns back in `history`, verbatim, letting the agent
    answer correctly without ever touching notes/core.md/retrieval at all.
    That's not testing the memory system, that's testing whether the model
    can read.

    `/api/wake` (`run_trigger_inner`) has none of that session-loading
    machinery — it passes the trigger JSON straight to the guest with no
    history, no session key, nothing persisted. Firing a `message`-type
    trigger through it gets a genuinely fresh, stateless probe: the only
    way to answer is real retrieval. `recent_staging_entries` (cross-session
    short-term memory, last ~5 entries) can still leak a very recent seed —
    a smaller, bounded, documented residual risk, not eliminated here since
    it's a real production feature, not an artifact of how this harness
    calls the API."""
    result = api_post("/wake", {"type": "message", "text": question})
    return result.get("result", {}).get("summary", "") or result.get("summary", "")
