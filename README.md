# AgentChecksum

**Know what changed in your agent — and whether it broke.**

[![CI](https://github.com/f0rknturkoglu/agentchecksum/actions/workflows/ci.yml/badge.svg)](https://github.com/f0rknturkoglu/agentchecksum/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

AgentChecksum is a language-agnostic dependency fingerprint and behavioral regression gate for AI
agents. It is one Rust binary, with no service, no database and no telemetry.

An agent's behavior does not live in its source code alone. It also lives in the model behind the
endpoint, the inference parameters, the prompts, the skills, the MCP servers, and the tool contracts
those servers declare. Any of those can change without breaking a build: a quantized model, a
rewritten system prompt, a tool description somebody improved on a Friday. The tests still pass, the
code still compiles, and the agent quietly starts choosing the wrong tool.

AgentChecksum answers two questions about that state:

```text
WHAT changed?   →  dependency fingerprint, then a semantic diff with behavioral risk
DID it break?   →  behavior probes, scored against a baseline you accepted, behind a gate
```

It reads, parses, normalizes, hashes and compares. It never imports your agent, and it never executes
a tool a model asked for.

```console
$ agentchecksum diff          # a tool description changed; the schema did not
TOOL  github.search_repositories  MEDIUM
  capabilities  unchanged
  description   sha256:9244b04a… → sha256:585836c7…
    classification: text-changed
  input_schema  unchanged

Overall behavioral risk: MEDIUM (heuristic)

$ agentchecksum check         # and the behavior moved with it
Behavioral probes: 1 / 2 passed
tool_restraint        100% → 0%  FAIL

Policy failures:
  tool_restraint: score 0.0000 is below the required minimum 1.0000

Behavior Gate: FAIL    exit 1
```

## What it fingerprints

| Kind | Facets |
|---|---|
| `model` | identity (provider, id, content digest, quantization, family, size), inference parameters, chat template, capabilities |
| `prompt` | content, whitespace-collapsed shape |
| `mcp` | server identity — era, negotiated protocol version, supported versions, server info, declared capabilities |
| `tool` | input schema, output schema, description (plus its shape), annotation capabilities |

Every dependency is normalized before it is hashed, so a reordered key or a trailing newline does not
produce a false change, and a moved `required` field always does. Digests are SHA-256 over RFC 8785
canonical JSON, which is a specification rather than a convention.

## What it is not

Not an agent framework, not a tracing system, not an observability platform, not an eval SaaS. It has
no dashboard, no hosted service, no LLM judge, and it sends nothing anywhere except the model endpoint
you configured, and only while capturing behavior. If you want to watch an agent run, this is the
wrong tool; if you want to know whether a change to its dependencies changed what it does, it is the
right one.

## Install

### From source (works today)

```bash
git clone https://github.com/f0rknturkoglu/agentchecksum
cd agentchecksum
cargo build --release
./target/release/agentchecksum --version     # agentchecksum 0.1.0
```

Rust 1.98 or newer. The repository pins its compiler in `rust-toolchain.toml`, so a source build uses
exactly the toolchain the test suite was verified against; CI installs that pin rather than choosing
its own.

### Prebuilt binaries (after the first tagged release)

`v0.1.0` has not been tagged yet, so no release assets exist. Once it is, the release workflow builds
these archives on native runners and attaches them to the GitHub Release, with a `SHA256SUMS` manifest
beside them:

```text
agentchecksum-v0.1.0-aarch64-apple-darwin.tar.gz      macOS, Apple Silicon
agentchecksum-v0.1.0-x86_64-apple-darwin.tar.gz       macOS, Intel
agentchecksum-v0.1.0-x86_64-unknown-linux-gnu.tar.gz  Linux, x86_64
agentchecksum-v0.1.0-x86_64-pc-windows-msvc.zip       Windows, x86_64
```

Each archive holds the binary, this README and both license files. Verify a download before running
it:

```bash
shasum -a 256 -c SHA256SUMS --ignore-missing
tar -xzf agentchecksum-v0.1.0-aarch64-apple-darwin.tar.gz
./agentchecksum-v0.1.0-aarch64-apple-darwin/agentchecksum --version
```

### Cargo (after publication)

Once the crate is published to crates.io:

```bash
cargo install agentchecksum --locked
```

That path does not work yet — the crate has not been published. It is listed here so the plan is not a
surprise, not to suggest it is available.

## Five-minute quickstart

```bash
agentchecksum init            # agentchecksum.toml, probes/, and an example probe
$EDITOR agentchecksum.toml    # name your agent, point [model] at a provider, declare prompts
agentchecksum snapshot        # discover dependencies, write agentchecksum.lock

git add agentchecksum.toml agentchecksum.lock probes/
git commit -m "Fingerprint the agent"
```

`agentchecksum.lock` is the dependency baseline: every later comparison is made against it, so it is
reviewed and committed like source.

Then, when something changes:

```bash
agentchecksum diff            # what changed, and how risky it is (never fails a build)
agentchecksum check           # did it break? sample the probes, compare, gate
agentchecksum check --accept  # accept the measured behavior as the new baseline
```

`check` needs a model to sample: either a local Ollama server, or any OpenAI-compatible endpoint. If
nothing is configured for it, it says so and exits 3 rather than pretending it measured something.
`agentchecksum check --diff-only` gates on the dependency half alone when no model is available.

### What to commit

| Path | Kind | Committed? | What it holds |
|---|---|---|---|
| `agentchecksum.toml` | source contract | **yes** | the configuration, reviewed like code |
| `probes/*.toml` | source contract | **yes** | the assertions; a probe's name is its identity in the baseline |
| `agentchecksum.lock` | generated | **yes** | the dependency baseline; written only by `snapshot` |
| `.agentchecksum/baseline.json` | generated | **yes** | the accepted behavior; written only by `check --accept`. Counts, digests and tool-schema digests — never prompts, model output or tool arguments |
| `.agentchecksum/cache/` | machine-local | **no** | cached samples. **Contains raw model output** |
| `.agentchecksum/runs/` | machine-local | **no** | run artifacts: recorded evidence and its evaluation. **Contains raw model output** |

The two machine-local directories are in the repository's `.gitignore` for exactly this reason. A
project that has never run `check --accept` has no `baseline.json` at all; committing one is how a
team agrees on what "correct behavior" means.

## Configuration

```toml
version = 1

[agent]
name = "research-agent"

[model]
provider = "openai-compatible"        # or "ollama"
id = "qwen3:8b-q4"
endpoint = "http://localhost:11434"
params = { temperature = 0.0, seed = 42 }

[[prompts]]
path = "prompts/system.md"

[[mcp.servers]]
name = "github"                       # the alias that prefixes every dependency id
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
# Passed to the server as written: no shell or `${VAR}` expansion, and never
# fingerprinted. Keep it out of the file by generating the config in CI.
env = { GITHUB_TOKEN = "ghp_example_token" }

[probes]
path = "probes"
repeat = 3

[policy]
fail_on_risk = "critical"

[policy.metrics.tool_selection]
min = 0.95

[policy.metrics.argument_validity]
max_drop = 0.05
```

Every key, every provider and every policy constraint is documented in
[docs/configuration.md](docs/configuration.md). `agentchecksum init` writes a commented version of
this file, so the first thing you read is the configuration you are about to edit.

## Behavior probes

A probe asks one question and declares what a correct answer looks like. `check` samples the model
through `[model]` and scores what came back.

```toml
# A tool must be called, with arguments that satisfy matchers.
[[probe]]
name = "repository-search"
prompt = "Find repositories about PostgreSQL vector search."
expect_tool = "search_repositories"
expect_args = { query = { contains = "postgres" }, per_page = { equals = 20 } }
forbid_tools = ["delete_file"]

# No tool may be called at all.
[[probe]]
name = "no-tools-when-not-asked"
prompt = "Answer from what you already know: what is the capital of Portugal?"
expect_no_tool = true

# The final message must be JSON that validates against a schema.
[[probe]]
name = "structured-answer"
prompt = "Return the result in the required schema."
output_schema = "schemas/answer.json"
```

Exactly five expectation keys exist — `expect_tool`, `expect_args`, `forbid_tools`, `expect_no_tool`,
`output_schema` — and a probe must declare at least one. `expect_args` maps an RFC 6901 JSON Pointer
to one of three matchers: `equals`, `contains`, `one_of`.

`repeat` is the number of samples per probe. An expectation that held once is an observation; one that
held in twenty of twenty is a measurement, and the six metrics are exact counts for that reason —
`9 / 10`, never a rounded `0.9`:

`tool_selection`, `argument_validity`, `argument_expectation`, `forbidden_tool_usage` (restraint),
`tool_restraint`, `structured_output_validity`. Every one of them is **1.0 is good**, which is why one
small policy vocabulary (`min`, `max`, `max_drop`) describes all of them.

Full reference: [docs/probes.md](docs/probes.md).

## In CI

`check` is the command that fails a build. `diff` never does: it reports what changed and lets your
policy decide, so the exit code stays useful for telling "found danger" from "could not compare".

```yaml
name: AgentChecksum
on: pull_request
jobs:
  gate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          fetch-depth: 0

      # `check` samples the model in [model], so the job needs an endpoint it can
      # reach. AgentChecksum sends no credentials, so that endpoint must not require
      # any: a local Ollama server, or a self-hosted OpenAI-compatible one.
      - run: |
          curl -fsSL https://ollama.com/install.sh | sh
          ollama serve &
          ollama pull qwen3:8b-q4

      # The composite action runs one AgentChecksum command and lets its exit code
      # decide the step. `@main` works today; pin a tag once v0.1.0 is released.
      - uses: f0rknturkoglu/agentchecksum@main
        with:
          command: check
```

If the model cannot be reached from the job, `--diff-only` still gates on the dependency half and
needs no endpoint at all.

The action is a thin wrapper: it finds a binary (a published release for the platform, or a source
build when there is no release for it yet), runs your command in your project, prints stdout and
stderr unchanged, and exits with the CLI's own code. It never reinterprets a verdict.

Exit codes, the `--from` PR-gate variant, and capturing a baseline in a controlled job:
[docs/ci.md](docs/ci.md).

| Code | Meaning |
|---|---|
| `0` | Completed. `diff` uses this even at CRITICAL risk; `check` uses it for drift unless `--fail-on-drift` asked for a gate |
| `1` | Gate failure — a metric policy failed, or a change was explicitly gated |
| `2` | Usage error — a flag combination with two readings |
| `3` | The check could not be evaluated: config, discovery, network, invalid probe, unusable evidence. **Not a behavioral verdict, and never PASS** |

## MCP servers and tool contracts

MCP is a first-class dependency source: `snapshot` reads the server identity, its tool list, each
tool's input and output schema, and its description. Both `stdio` and `streamable-http` transports are
supported.

Discovery never calls a tool, and it never fingerprints a partial catalog — a server that fails
mid-enumeration is an error, not a smaller dependency set. Configured `env` values are passed to the
server, are **never** fingerprinted, and never appear in diagnostics; if a credential changes the
declared contract, that change is reported as the dependency change it is. The configuration file is
the trust boundary, which is why it is committed and reviewed like any other input.

## Trust model

- **What is executed:** the MCP servers you configured (to ask what they declare), and nothing else.
  Capturing behavior makes exactly one outbound connection per sample, to the model endpoint in
  `[model]`.
- **What is never executed:** anything a model asks for. A probe records the tool calls a model emits
  and scores them; there is no `tools/call`, no sandbox, no plugin loading.
- **What is stored:** the lockfile and the baseline in your repository (digests, counts and scores
  only), and raw model answers under `.agentchecksum/cache` and `.agentchecksum/runs`, which are
  machine-local and gitignored. Treat those as you would treat model output anywhere else.
- **What is validated:** probe output schemas and tool input schemas run through `jsonschema` with
  HTTP and file resolution disabled, so a schema that needs a remote reference is refused rather than
  fetched.

Details: [docs/security.md](docs/security.md).

## Commands

| Command | What it does |
|---|---|
| `agentchecksum init` | Scaffold `agentchecksum.toml`, `probes/`, and an example probe |
| `agentchecksum snapshot` | Discover dependencies and write `agentchecksum.lock` |
| `agentchecksum diff` | Semantic dependency diff with per-facet behavioral risk. Reports; never gates |
| `agentchecksum check` | The gate: dependency diff plus behavior probes, policy, one verdict |
| `agentchecksum inspect probes` | Debug view of the parsed probe suite: what each probe asserts, which tools it references, which metrics it feeds |

Global flags: `--format human\|json`, `--config <path>`, `--lock <path>`, `--from <path>` (compare
against another lockfile — how CI compares a pull request with its base revision).

`check` adds `--accept`, `--diff-only`, `--probes-only`, `--no-probes`, `--trace <path>`, `--refresh`,
`--repeat <n>`, `--jobs <n>`, `--fail-on-drift` and `--fail-on-risk <level>`. `agentchecksum check
--help` is the authority.

### Replaying recorded evidence

`check --trace <trace-or-run-artifact>` scores a recorded run instead of calling a model: no request,
no endpoint needed. It is bound to the context it describes — agent checksum, tool catalog digest,
runner and its version, probe identity, and the probe suite — so evidence from a different agent,
catalog or suite is refused with exit 3 rather than scored as yours. A replay also never writes a
baseline: `--accept` accepts only from a live run.

## The honesty of a verdict

These are the distinctions the tool is built around, and they are visible in its output rather than
buried in documentation:

- Static risk classification is a **heuristic**. The report says so.
- Model sampling is **statistical**; evaluation over recorded evidence is **deterministic**. A probe
  result is a pass rate, not a proof.
- **Drift is not regression.** A missing baseline, a changed probe suite, or a baseline recorded under
  a different runner contract exits `0` unless you asked otherwise, and says why: the scores on either
  side answer different questions.
- **A metric nothing measured is absent**, not perfect. `argument_validity` applies only to samples
  that called a tool.
- **A row nothing judged reads `WARN`**, not `PASS`. "No threshold failed" and "the behavior was good"
  are different claims.
- **A check that could not finish is never `PASS`.** It exits 3 with the diagnostic.

## Demo

A deterministic, offline walkthrough of the whole story — fingerprint, a tool description change, a
behavioral baseline, a regression, and an offline replay — using the real CLI and two local fixture
servers:

```bash
./demo/run.sh
```

See [demo/README.md](demo/README.md).

## Limitations

Deliberate, at this version: no LLM judge, no regex matchers, no multi-turn or tool-result evaluation,
and **no credentials of any kind** — AgentChecksum sends no authentication header, so the model
endpoint must be reachable without one (local Ollama, or a self-hosted OpenAI-compatible server). Trace
*capture* is not bit-reproducible; trace *evaluation* is. `openai-compatible` exposes no content digest, so the endpoint is part of the model's
identity and a model swapped behind the same URL cannot be detected; the CLI warns about that when it
fingerprints one.

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Rust 1.98.1, edition 2024, a single crate and a single binary. Design decisions live in
[docs/specs](docs/specs); the phase plans under [docs/plans](docs/plans) are engineering history.
Getting started, configuration, probes, CI and the release process are documented under
[docs/](docs) and in [CONTRIBUTING.md](CONTRIBUTING.md).

The checksum format, the lockfile and baseline schemas, the report schema and the exit codes are
**contracts**. Changing one is a compatibility decision, not a refactor.

## License

MIT OR Apache-2.0, at your option.
