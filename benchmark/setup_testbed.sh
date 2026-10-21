#!/usr/bin/env bash
# Builds an isolated agent-home for benchmark runs — never touches tmp/main
# (the real, in-use deployment). Lives under tmp/ so it's gitignored
# automatically (secrets.toml gets copied in here, must never be committed).
set -euo pipefail
cd "$(dirname "$0")/.."

TESTBED="tmp/benchmark-testbed"
SOURCE="tmp/main"

if [ ! -f "$SOURCE/secrets.toml" ]; then
  echo "error: $SOURCE/secrets.toml not found — benchmark needs a real LLM/embed" >&2
  echo "config to actually exercise the distillation pipeline, not a mock." >&2
  exit 1
fi

echo "Resetting $TESTBED..."
rm -rf "$TESTBED"
mkdir -p "$TESTBED/agent-home"

# Deliberately NOT the production persona (蝦奈) — this benchmark tests the
# *memory architecture*, not that specific character's behavior, and
# reusing a real deployment's identity in a synthetic test-data context
# isn't appropriate. A minimal, neutral persona keeps the variable under
# test to "does the waterfall distill/promote/retrieve correctly", not
# "how does this particular character respond".
cat > "$TESTBED/agent-home/SOUL.md" <<'EOF'
# SOUL.md — benchmark test agent

A plain, neutral assistant persona used only for testing the memory
distillation pipeline. No specific identity, tone, or relationship framing
— answer directly and factually.
EOF

# Same LLM/embed provider + credentials as production — a benchmark against
# a different/weaker model wouldn't tell you anything about whether *this*
# distillation design works with the model you actually run.
cp "$SOURCE/agent-home/config.toml" "$TESTBED/agent-home/config.toml"
cp "$SOURCE/secrets.toml" "$TESTBED/secrets.toml"

# Build the agent fresh so the testbed always reflects current agent/ code —
# a stale binary here would silently test old distillation logic.
echo "Building agent..."
cargo build -p agent --release >/dev/null
cp target/release/agent "$TESTBED/agent"

echo "Building ebinactl release binary..."
cargo build --workspace --release >/dev/null

echo "Testbed ready at $TESTBED (empty notes/core.md/skills — clean slate)."
