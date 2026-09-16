#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# The AgentChecksum walkthrough, end to end, on this machine.
#
# It tells the product's whole story with the real CLI and no cloud account:
#
#   1. fingerprint the agent                          (`snapshot`)
#   2. change one dependency and read the diff        (`diff`)
#   3. measure the behavior a probe asserts           (`check`)
#   4. accept that behavior as the baseline           (`check --accept`)
#   5. change what the model does and watch it fail   (`check`)
#   6. reproduce the verdict offline                  (`check --trace`)
#
# Both servers are local fixtures from `examples/`: the MCP one declares the tool
# catalog the probes reference, and the OpenAI-compatible one stands in for a model
# provider. Nothing leaves 127.0.0.1 and nothing here needs a network.
#
# Usage:  ./demo/run.sh          (from the repository root, or from anywhere)

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/agentchecksum-demo.XXXXXX")"
model_pid=""
holder_pid=""

cleanup() {
    for pid in "$model_pid" "$holder_pid"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

step() { printf '\n== %s\n\n' "$1"; }
note() { printf '   %s\n' "$1"; }

# ---------------------------------------------------------------------------
# Build the CLI and the two fixtures
# ---------------------------------------------------------------------------

step "Building agentchecksum and the local fixtures"

cargo build --quiet --release --manifest-path "$root/Cargo.toml"
cargo build --quiet --manifest-path "$root/Cargo.toml" --example mcp_fixture_server
cargo build --quiet --manifest-path "$root/Cargo.toml" --example openai_fixture_server

agentchecksum="$root/target/release/agentchecksum"
mcp_fixture="$root/target/debug/examples/mcp_fixture_server"
model_fixture="$root/target/debug/examples/openai_fixture_server"

# ---------------------------------------------------------------------------
# The model the agent talks to: a local fixture that answers the same sentence to
# every request. A fixture is how this demo stays deterministic and offline.
# ---------------------------------------------------------------------------

start_model_fixture() {
    # The fixture exits on stdin EOF, which is how a caller stops it. It is started
    # with a writer that stays open for its lifetime, so nothing closes it early.
    rm -f "$work/fixture-stdin"
    mkfifo "$work/fixture-stdin"
    sleep 86400 > "$work/fixture-stdin" &
    holder_pid=$!
    rm -f "$work/port"
    AC_FIXTURE_SPEC="$work/model.json" AC_FIXTURE_PORT_FILE="$work/port" \
        "$model_fixture" < "$work/fixture-stdin" > "$work/model.log" 2>&1 &
    model_pid=$!

    for _ in $(seq 1 200); do
        if [ -s "$work/port" ]; then
            return 0
        fi
        sleep 0.05
    done
    echo "the model fixture never reported a listening port" >&2
    cat "$work/model.log" >&2
    exit 1
}

stop_model_fixture() {
    [ -n "$model_pid" ] && kill "$model_pid" 2>/dev/null || true
    [ -n "$holder_pid" ] && kill "$holder_pid" 2>/dev/null || true
    wait "$model_pid" 2>/dev/null || true
    wait "$holder_pid" 2>/dev/null || true
    model_pid=""
    holder_pid=""
}

# ---------------------------------------------------------------------------
# The project under test, written the way a user would write it
# ---------------------------------------------------------------------------

mkdir -p "$work/project/prompts" "$work/project/probes"

cat > "$work/project/prompts/system.md" <<'PROMPT'
You are a careful research assistant.

Prefer the search tool when the user asks about code.
PROMPT

cat > "$work/project/mcp.json" <<'JSON'
{
  "tools": [
    {
      "name": "search_repositories",
      "description": "Search public repositories by keyword.",
      "input_schema": {
        "type": "object",
        "properties": { "query": { "type": "string" } },
        "required": ["query"]
      }
    }
  ]
}
JSON

cat > "$work/project/probes/no-tools.toml" <<'PROBE'
[[probe]]
name = "no-tools-when-not-asked"
prompt = """
Answer from what you already know, without calling any tool: what is the capital of
Portugal?
"""
expect_no_tool = true
PROBE

cat > "$work/project/probes/search.toml" <<'PROBE'
[[probe]]
name = "repository-search"
prompt = "Find repositories about PostgreSQL vector search."
expect_tool = "search_repositories"
expect_args = { query = { contains = "postgres" } }
PROBE

# The fixture answers one response per request, in order, and repeats the last one.
# The walkthrough makes exactly two requests per sampled run, in probe order
# (`no-tools.toml` before `search.toml`), so the list below is the script's own
# timeline: two green runs, then a run where the model reaches for a tool it was
# told not to call.
cat > "$work/model.json" <<'JSON'
{
  "responses": [
    { "type": "text", "content": "The capital of Portugal is Lisbon." },
    { "type": "tool_calls",
      "calls": [ { "name": "search_repositories",
                   "arguments": "{\"query\":\"postgres vector search\"}" } ] },
    { "type": "text", "content": "The capital of Portugal is Lisbon." },
    { "type": "tool_calls",
      "calls": [ { "name": "search_repositories",
                   "arguments": "{\"query\":\"postgres vector search\"}" } ] },
    { "type": "tool_calls",
      "calls": [ { "name": "search_repositories", "arguments": "{\"query\":\"portugal\"}" } ] },
    { "type": "tool_calls",
      "calls": [ { "name": "search_repositories",
                   "arguments": "{\"query\":\"postgres vector search\"}" } ] }
  ]
}
JSON
start_model_fixture
port="$(cat "$work/port")"
note "model fixture listening on 127.0.0.1:$port"

cat > "$work/project/agentchecksum.toml" <<CONFIG
version = 1

[agent]
name = "research-agent"

[model]
provider = "openai-compatible"
id = "demo-model"
endpoint = "http://127.0.0.1:$port"

[[prompts]]
path = "prompts/system.md"

# The tool catalog comes from a real MCP server. Discovery reads what it declares and
# never calls a tool.
[[mcp.servers]]
name = "github"
transport = "stdio"
command = "$mcp_fixture"
args = ["--stdio"]
env = { AC_FIXTURE_SPEC = "$work/project/mcp.json" }

[probes]
path = "probes"
repeat = 1

[policy.metrics.tool_restraint]
min = 1.0

[policy.metrics.tool_selection]
min = 1.0
CONFIG

cd "$work/project"
git init --quiet .
git config user.email "demo@example.com"
git config user.name "AgentChecksum demo"

# ---------------------------------------------------------------------------
# 1. Fingerprint the agent
# ---------------------------------------------------------------------------

step "1. agentchecksum snapshot — what this agent is made of"

"$agentchecksum" snapshot 2>/dev/null

note "agentchecksum.lock is committed: it is the dependency baseline every later"
note "comparison is made against. This demo's project is written for you; your own"
note "starts with \`agentchecksum init\`."

git add -A
git commit --quiet -m "The agent as it stands"

# ---------------------------------------------------------------------------
# 2. One dependency changes: the tool description, not its schema
# ---------------------------------------------------------------------------

step "2. Someone improves a tool description — a documentation change, not an API change"

python3 - "$work/project/mcp.json" <<'PY'
import json, sys
path = sys.argv[1]
spec = json.load(open(path))
spec["tools"][0]["description"] = (
    "Search public repositories by keyword. "
    "Use this tool whenever the user mentions a repository, an owner, or code search, "
    "and pass the most specific query you can construct."
)
json.dump(spec, open(path, "w"), indent=2)
PY

"$agentchecksum" diff 2>/dev/null

note "The schema did not move; the description did. That is not a breaking API change"
note "and it is a behavior-relevant dependency change: the model reads that text when"
note "it decides which tool to call."

"$agentchecksum" snapshot > /dev/null 2>&1
git commit --quiet -am "Accept the new tool description"

# ---------------------------------------------------------------------------
# 3. Did it break? Measure the behavior the probes assert
# ---------------------------------------------------------------------------

step "3. agentchecksum check — nothing has been accepted yet, so this is drift"

set +e
"$agentchecksum" check 2>/dev/null
code=$?
set -e
echo "exit $code"

# ---------------------------------------------------------------------------
# 4. Accept that behavior as the contract
# ---------------------------------------------------------------------------

step "4. agentchecksum check --accept — record what was measured as the baseline"

set +e
"$agentchecksum" check --accept 2>/dev/null
code=$?
set -e
echo "exit $code"

git add -A
git commit --quiet -m "Accept the behavior"

step "4b. agentchecksum check — the same run, now compared against the baseline"

# Sampled again rather than read from the cache, because a repeat is what makes a
# pass rate a measurement instead of one observation.
set +e
"$agentchecksum" check --refresh 2>/dev/null
code=$?
set -e
echo "exit $code"

# ---------------------------------------------------------------------------
# 5. The model starts doing something it was asked not to do
# ---------------------------------------------------------------------------

step "5. The same endpoint starts answering differently: the model calls a tool the probe told it not to call"

set +e
"$agentchecksum" check --refresh 2>/dev/null
code=$?
set -e
echo "exit $code"

# ---------------------------------------------------------------------------
# 6. Reproduce the verdict with no model at all
# ---------------------------------------------------------------------------

step "6. The model goes away, and the recorded evidence still gives the same verdict"

stop_model_fixture

# Run artifacts are addressed by their content, so "the newest" is a timestamp and not
# a name: the replay has to be the run that just failed.
artifact="$(ls -t .agentchecksum/runs/*.json | head -1)"
note "replaying $(basename "$artifact")"

set +e
"$agentchecksum" check --trace "$artifact" 2>/dev/null
code=$?
set -e
echo "exit $code"

step "What the six steps showed"

cat <<'SUMMARY'
  snapshot   fingerprinted the agent: a prompt, a model, an MCP server, and the tool
             contract that server declares
  diff       the tool description moved and the input schema did not — MEDIUM risk,
             because the API did not break and the agent might
  check      drift, because no behavior had been accepted yet
  --accept   recorded the measured behavior as the committed baseline
  check      PASS: the same run, compared against what was accepted
  check      FAIL: the model reached for a tool the probe forbade
  --trace    the same FAIL, replayed from recorded evidence with no model running

Your own project commits: agentchecksum.toml, probes/, agentchecksum.lock, and
.agentchecksum/baseline.json once you accept behavior. It never commits
.agentchecksum/cache or .agentchecksum/runs — they hold raw model output.

Everything this demo created lived in a temporary directory and has been removed.
SUMMARY
