# Getting started

AgentChecksum is a language-agnostic dependency fingerprint and behavioral regression gate for AI
agents. It answers two questions about an agent you already have:

```text
WHAT changed?                    DID it break?
Dependency Checksum   ─────►     Behavior Gate
```

This page is the whole walkthrough: install it, carry one small project from an empty directory to a
passing gate, read what `diff` and `check` print, and get unstuck when something fails. The
[README](../README.md) is the short product page; the deep references are
[configuration](configuration.md), [probes](probes.md), [CI](ci.md) and [security](security.md).

AgentChecksum is not an agent framework, a tracing system, or an observability product. It reads,
parses, normalizes, hashes and compares, and it runs exactly two kinds of process: the MCP server you
configure in `agentchecksum.toml`, and — only when a run is *captured* — a request to the model
endpoint in `[model]`.

## Install

`0.1.0` is released: tagged [`v0.1.0`](https://github.com/f0rknturkoglu/agentchecksum/releases/tag/v0.1.0),
with prebuilt archives attached to that release and the crate published to crates.io. Every method
below works today:

| Method | Available today? |
|---|---|
| Build from a clone: `cargo build --release` | **Yes** |
| `cargo install --git https://github.com/f0rknturkoglu/agentchecksum` | **Yes** — it builds from the repository |
| The composite action in this repository (`uses: f0rknturkoglu/agentchecksum@v0.1`) | **Yes** — it uses the release asset for the runner and verifies it against `SHA256SUMS`. See [ci.md](ci.md#the-composite-action-recommended) |
| Downloading a prebuilt release binary | **Yes** — from [the `v0.1.0` release](https://github.com/f0rknturkoglu/agentchecksum/releases/tag/v0.1.0) |
| `cargo install agentchecksum` (crates.io) | **Yes** — [the crate is published](https://crates.io/crates/agentchecksum) |

The two source-based methods need a Rust toolchain: the crate declares `rust-version = "1.98"`, and
the repository pins `1.98.1` in `rust-toolchain.toml`. The build produces one binary and needs no
runtime, database, or service. The prebuilt archives need neither — they are the reason they exist.

```bash
git clone https://github.com/f0rknturkoglu/agentchecksum
cd agentchecksum
cargo build --release
./target/release/agentchecksum --help
```

Confirm the binary's surface before you trust any command in these docs — `--help` is the authority.
(The options below are trimmed to one line each; the real `--help` follows every flag with its full
description.)

```console
$ agentchecksum --help
Language-agnostic dependency fingerprint and behavioral regression gate for AI agents

Usage: agentchecksum [OPTIONS] <COMMAND>

Commands:
  init      Scaffold agentchecksum.toml and a probes directory
  snapshot  Discover dependencies and write agentchecksum.lock
  diff      Compare the committed baseline against current dependency state
  check     Run the dependency diff and the probes, apply the policy, and report a verdict
  inspect   Debug view of what is configured: the parsed probe suite
  help      Print this message or the help of the given subcommand(s)

Options:
      --format <FORMAT>   Output format. JSON is written to stdout alone [default: human]
                          [possible values: human, json]
      --config <CONFIG>   Path to the configuration file [default: agentchecksum.toml]
      --lock <LOCK>       Path to the generated lockfile [default: agentchecksum.lock]
      --from <FROM>       Compare against a different lockfile instead of the committed one
  -h, --help              Print help (see a summary with '-h')
  -V, --version           Print version
```

`--format`, `--config`, `--lock` and `--from` are global: they work on every subcommand, before or
after it. There are no other flags — no verbosity switches; diagnostics are controlled by
`RUST_LOG`, which defaults to `warn` and writes to stderr.

## A five-minute project

Everything below runs offline except the model request in step 6, and every command is copy-pasteable
in an empty directory.

### 1. Scaffold

```bash
agentchecksum init
```

```console
$ agentchecksum init
Wrote agentchecksum.toml
Wrote ./probes/no-tools.toml
Next: add your prompts, edit the example probe, then run `agentchecksum snapshot` and `agentchecksum check`.
```

Two files appear: the configuration and one commented example probe. `init` refuses to overwrite an
existing configuration unless you pass `--force`, and it never overwrites a probe — a probe is
something you have edited.

### 2. Declare the agent

Create the prompt and point the configuration at it. The scaffold already contains the `[[prompts]]`
entry, so adding the file is enough:

```bash
mkdir -p prompts
cat > prompts/system.md <<'EOF'
You are a support assistant for an internal ticket system.
Answer in one sentence.
EOF
```

Now declare a model. Both providers need an `endpoint`, and both are described in full in
[configuration](configuration.md):

```toml
[model]
provider = "openai-compatible"
id = "demo-model"
endpoint = "http://127.0.0.1:8000/v1"
params = { temperature = 0.0, seed = 42 }
```

Ollama is the other provider:

```toml
[model]
provider = "ollama"
id = "qwen3:8b"
endpoint = "http://localhost:11434"
```

`[model]` is optional as far as `snapshot` and `diff` are concerned — only `check` needs it, and only
when it actually samples.

### 3. Fingerprint

```bash
agentchecksum snapshot
```

```console
$ agentchecksum snapshot
Agent checksum generated.

Checksum:
ac1:7721f38d65c91e94f82508eeee410838cc2e52bf32cb0ad1102522e07ae0d9bf

Dependencies:
1 model
1 prompt
```

This discovered two dependencies — `model:openai-compatible/demo-model` and
`prompt:prompts/system.md` — and wrote `agentchecksum.lock`. `snapshot` is the **only** command that
writes the lockfile.

### 4. Commit the baseline

```bash
git add agentchecksum.toml agentchecksum.lock probes prompts
git commit -m "Add the agent checksum baseline"
```

The lockfile is the state every future comparison is made against. Without it in version control
there is nothing for CI to compare against, and `diff` and `check` will tell you so.

### 5. Ask what changed

Someone edits a prompt, a model, or an MCP tool description, and now:

```bash
agentchecksum diff
```

```console
$ agentchecksum diff

AgentChecksum diff

Baseline: ac1:7721f38d65c91e94f82508eeee410838cc2e52bf32cb0ad1102522e07ae0d9bf
Current:  ac1:fa1cc294b6e9b0b2efd43d4d99ae2e8306b611e22b5a9da02810cb78512cdc35

1 dependency changed.

PROMPT  prompts/system.md  MEDIUM
  content  sha256:a7a8152c… → sha256:e5868cd0…
    classification: text-changed
  shape    sha256:d559917c… → sha256:b2169a76…

Overall behavioral risk: MEDIUM (heuristic)
```

`diff` never writes anything and always exits `0` when the comparison ran, however dangerous the
change is. It reports; the policy decides.

### 6. Ask whether it broke

```bash
agentchecksum check
```

On a project that has never accepted a baseline, the first run samples your model, scores the probes
and reports drift:

```console
$ agentchecksum check

AgentChecksum check

Agent checksum: ac1:7721f38d65c91e94f82508eeee410838cc2e52bf32cb0ad1102522e07ae0d9bf

No dependency changes detected.

Behavioral probes: 1 / 1 passed
tool_restraint  n/a → 100%  PASS

Drift reasons:
  no behavioral baseline exists

Behavior Gate: DRIFT   exit 0
```

### 7. Accept the behavior you just saw

```bash
agentchecksum snapshot                 # refresh the lockfile so the dependency state is current
agentchecksum diff                     # confirm nothing changed, or commit the change first
agentchecksum check --accept
git add .agentchecksum/baseline.json
git commit -m "Record the accepted behavior"
```

`--accept` is how a verdict becomes the baseline, and it is deliberately hard to do by accident. It
writes the baseline only when both hold:

- **The dependency state has not moved.** If it has, `--accept` exits `2` with an instruction to
  `snapshot`, review `diff`, and commit the new lockfile — scores captured under a changed agent would
  be attributed to the wrong one.
- **No policy constraint failed.** A baseline records *accepted* behavior, so a run that failed its
  own gate does not become the accepted one. It says so and writes nothing:

  ```text
  WARN agentchecksum::cli::cmd::check: `--accept` did not write `./.agentchecksum/baseline.json`:
  the run did not pass (4 failed policy constraints), and a baseline records accepted behavior only
  ```

  If you genuinely want to accept behavior the current policy rejects, that is a policy decision —
  change the policy, or fix the agent — not something to force through the flag.

It writes counts, digests and yardstick digests — never prompts, model output, or tool arguments.

The next `check` compares against it:

```console
$ agentchecksum check

AgentChecksum check

Agent checksum: ac1:7721f38d65c91e94f82508eeee410838cc2e52bf32cb0ad1102522e07ae0d9bf

No dependency changes detected.

Behavioral probes: 1 / 1 passed
tool_restraint  100% → 100%  PASS

Behavior Gate: PASS    exit 0
```

That is the loop: edit → `snapshot` → review `diff` → commit → `check` gates.

## What to commit, and what never to commit

The distinction matters because two of these directories hold raw model output. Committing them by
accident puts prompts, model answers and tool arguments into your history.

| Path | Kind | Committed? | Notes |
|---|---|---|---|
| `agentchecksum.toml` | source contract | **yes** | configuration, reviewed like code |
| `probes/*.toml` | source contract | **yes** | the assertions; a probe name is its identity in the baseline |
| `agentchecksum.lock` | generated | **yes** | the dependency baseline every comparison is made against; written only by `snapshot` |
| `.agentchecksum/baseline.json` | generated | **yes** | the accepted behavior; written only by `check --accept`; counts, digests and yardstick digests only — no prompts, no model output, no tool arguments |
| `.agentchecksum/cache/` | machine-local | **no** (gitignored) | cached samples; contains raw model answers |
| `.agentchecksum/runs/` | machine-local | **no** (gitignored) | run artifacts: traces + evaluations; contains raw model output |

The repository's own `.gitignore` encodes exactly this: `.agentchecksum/runs/` and
`.agentchecksum/cache/` are ignored, and `.agentchecksum/baseline.json` is deliberately not — with a
comment saying so, because a baseline that is silently ignored is a gate that silently stops gating.

## Reading a `diff` result

```text
Baseline: ac1:…          the committed lockfile's aggregate
Current:  ac1:…          what discovery just found
1 dependency changed.    how many dependencies moved, added, or were removed
```

Then one block per changed dependency: the kind and id, the overall risk for that dependency, and a
line per facet that was compared — including the facets that did **not** move, so a reader can tell a
facet that was checked from one that was never looked at.

```text
TOOL  demo-tools.search_repositories  MEDIUM
  capabilities   unchanged
  description    sha256:5581a1b6… → sha256:9c02cd61…
    classification: text-changed
  input_schema   unchanged
  output_schema  unchanged
```

A facet whose content moved but whose whitespace-collapsed `shape` did not is labelled
`formatting-only` and classified LOW; a facet whose shape moved is `text-changed`. An added or removed
dependency says `added` / `removed` instead of listing facets.

The last line is the aggregate:

```text
Overall behavioral risk: MEDIUM (heuristic)
```

It is labelled a heuristic because it is one: risk is a deterministic function over the diff, with no
model in the loop. The levels are ordered `LOW < MEDIUM < HIGH < CRITICAL`, and the full row-by-row
table the code asserts is [spec §8.3](specs/2026-09-15-agentchecksum-design.md#83-risk-classification).
Three rows are worth knowing before you read any output:

| Change | Risk |
|---|---|
| A text facet changed formatting only (equal `shape` digest) | LOW |
| A prompt's content changed, or a tool `description` changed | MEDIUM |
| A model, tool or server dependency was **added** | HIGH |
| A newly added tool whose own declaration names it write-capable **and** destructive | CRITICAL |

The last row is the strictest thing a diff can discover on its own, and it is still only *declared*:
it comes from the MCP annotations the server publishes about itself, which the MCP specification
tells clients to treat as untrusted. Nothing in the output claims the tool will do what its
annotation says.

## Reading a `check` result

`check` prints the dependency half first — the same report `diff` produces — and then the behavioral
half:

```console
$ agentchecksum check

AgentChecksum check

Agent checksum: ac1:a352c954766a3700fc59268081a06b735508e36d54c2b7994e190f937bf483c7

No dependency changes detected.

Behavioral probes: 4 / 5 passed
tool_selection              100% → 100%  PASS
argument_validity           100% → 100%  PASS
argument_expectation        100% → 100%  PASS
forbidden_tool_usage        100% → 100%  PASS
tool_restraint              0% → 0%  FAIL
structured_output_validity  100% → 100%  PASS

Policy failures:
  tool_restraint: score 0.0000 is below the required minimum 1.0000

Failing probes:
  no-tools  0 / 1 passed
    sample 0: tool_restraint 1 tool was called

Behavior Gate: FAIL    exit 1
```

Read it left to right:

| Column | Meaning |
|---|---|
| metric | one of the six behavioral metrics; which ones appear is decided by what the probes assert |
| `baseline → current` | the accepted score and this run's score; `n/a` means no sample made the metric applicable |
| verdict | `PASS` (a policy was applied and held, or nothing gated it and the score is perfect), `FAIL` (a policy constraint failed), `WARN` (nothing gated it — no policy declared, or what was declared could not be applied — and the score is short of perfect), or `n/a` (nothing measured the metric) |

Three deliberate properties are visible in that table:

- **A metric nothing measured is absent, not zero.** `argument_validity` applies only to samples that
  called a tool, and a probe that calls nothing does not get a free `1.0`.
- **A row whose policy could not be applied reads `WARN`, not `PASS`.** "No threshold failed" and "the
  behavior was good" are different claims.
- **Scores are counts underneath.** The baseline records `passed`/`total`, so `9 / 10` survives an
  argument that `0.9` does not.

Below the table, `Policy failures` names the constraint that failed, `Failing probes` names the probe
and the sample-level reason, and `Drift reasons` appears when the comparison could not be made at all
— no baseline, a changed probe suite, a changed runner contract, a changed dependency checksum. Drift
is not regression: it exits `0` unless you asked for `--fail-on-drift`.

For the whole verdict as one document:

```bash
agentchecksum check --format json | jq '.status, .behavior.metrics[] | select(.verdict == "fail")'
```

`--format json` writes one JSON document to stdout and nothing else; diagnostics go to stderr.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Completed, no blocking verdict. `diff` uses `0` whenever the comparison ran — even at CRITICAL — and `check` uses `0` for drift unless `--fail-on-drift` asked for a gate. |
| `1` | Gate failure: a metric policy failed, or a change was explicitly gated (`--fail-on-drift`, `--fail-on-risk`, `[policy] fail_on_risk`). |
| `2` | Usage error: a rejected flag combination, or one with two readings (`--accept` with `--from`). Clap's parse errors use the same code. |
| `3` | Runtime failure: config, discovery, network, an invalid probe, an unsupported schema, or **evidence that cannot be trusted**. AgentChecksum could not honestly produce the requested verdict. It is never `PASS`, and never "the test failed". |

## Troubleshooting

### `check` says no `[model]` is configured

```text
Error: invalid configuration: `check` samples the agent through `[model]`, and this configuration
declares none; add a `[model]` section, or run `agentchecksum check --diff-only` to gate on
dependencies alone
```

Exit code `3`. `snapshot` and `diff` do not need a model, so this is the first thing a new project
hits. Either add a `[model]` section (see [configuration](configuration.md#model)) or run
`check --diff-only` / `check --no-probes` to gate on the dependency half alone.

### The model endpoint is unreachable

```text
Error: the model request to `http://127.0.0.1:8000/v1/chat/completions` failed: the endpoint could
not be reached: error sending request for url (http://127.0.0.1:8000/v1/chat/completions)

Suggested action:
  Check that the model endpoint is reachable and that the configured `[model].id` exists there.
  AgentChecksum does not retry: a sample is one request.
```

Exit code `3`, and never `PASS`: a check that could not finish is not a passing check. Things worth
knowing while you fix it:

- `endpoint` is a **base** URL. For `openai-compatible` the runner appends `/v1/chat/completions`, and
  a `…/v1` base does not become `…/v1/v1/…`. A URL that already names the completion path is refused
  rather than guessed at.
- The provider is `openai-compatible` for any endpoint that speaks OpenAI's chat-completions contract;
  Ollama's native API (used for model metadata during `snapshot`) is the `ollama` provider, and the
  two are different endpoints.
- There are no retries. One sample is one request, with a two-minute per-sample timeout, because a
  retry would change what `repeat = N` means. `--jobs <n>` changes how many probes are in flight, not
  how many times each is sampled.
- Commit no secrets. If your endpoint needs a credential, supply it in the environment of the process
  that runs your agent; an endpoint URL carrying credentials is rejected outright.

### A probe reference does not resolve

```text
Error: probe `search` references the tool `search_repositories`, which no discovered tool provides

Suggested action:
  Check the name against the tool dependencies in `agentchecksum.lock`, or run `agentchecksum inspect
  probes` to see what resolved.
```

Exit code `3`. A probe may name a tool the way a human reads it (`search_repositories`) or by its
canonical dependency id (`tool:demo-tools.search_repositories`). Neither is guessed at:

- **Unknown** means no discovered tool has that remote name — usually a typo, or a `snapshot` that has
  not run since the server changed. Run `agentchecksum snapshot`, then
  `agentchecksum inspect probes` to see the resolved suite.
- **Ambiguous** means two servers expose that remote name; the error lists the candidates, and you
  resolve it by using the canonical id. Two servers' tools are different tools with different schemas,
  so picking one silently would validate arguments against the wrong contract.

### The probe directory is empty or missing

An empty directory:

```text
Error: invalid configuration: the probe directory `./probes` holds no `*.toml` probe file, so there
is nothing to measure
```

A directory that does not exist at all:

```text
Error: failed to read `./probes`
  caused by: No such file or directory (os error 2)
```

Both exit `3`. Probe files are read from one flat directory: subdirectories are not searched, and only
`*.toml` files count. The loader refuses an empty suite on purpose — "no probes ran" must never be
reported as a pass. `check --diff-only` skips the probe half entirely and is unaffected. See
[probes](probes.md) for the file format.

### Exit `3` versus exit `1`

These are the two failures a CI job most often confuses, and they call for different reactions:

| | Exit `3` | Exit `1` |
|---|---|---|
| What happened | The tool could not produce an honest verdict | The tool produced a verdict, and it is negative |
| Examples | unreachable endpoint, unreadable config, probe that does not resolve, evidence that does not describe the current agent | a metric policy failed, `--fail-on-drift`, `--fail-on-risk <level>` |
| What to do | Fix the run | Fix the agent, or change the policy deliberately |

An exit `3` is never a verdict about behavior, and it is never treated as a pass. The one case worth
naming is a replay: `check --trace <path>` refuses evidence captured under a different agent, tool
catalog, probe suite or runner, and that refusal is exit `3`. It is not drift, not a cache miss, and
not silently re-captured — scoring one agent's behavior as another's would be worse than no answer.

## Where to go next

- [configuration.md](configuration.md) — every `agentchecksum.toml` key, both model providers, both
  MCP transports, and the policy vocabulary.
- [probes.md](probes.md) — the probe file format, the five expectation keys, the three matchers, and
  the six metrics.
- [ci.md](ci.md) — a GitHub Actions job that builds the CLI, gates a pull request, and interprets
  every exit code.
- [security.md](security.md) — what is executed, what is never executed, what is stored, and what
  leaves the machine.
- [specs/2026-09-15-agentchecksum-design.md](specs/2026-09-15-agentchecksum-design.md) — the design
  specification, including the risk table and the JSON shapes.
