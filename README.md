# AgentChecksum

> **Know what changed in your agent — and whether it broke.**

AgentChecksum is a language-agnostic dependency fingerprint and behavioral regression gate for AI
agents.

An agent's behavior does not depend only on its source code. It also depends on the model, its
quantization, the provider, inference parameters, system prompts and prompt files, tool lists, tool
schemas, tool *descriptions*, tool permissions, MCP servers and their protocol era, retrieval
configuration, and guardrails. Change one of those and the agent still compiles, still runs, and
quietly misbehaves: it picks the wrong tool, sends the right tool the wrong arguments, breaks its
output format, or skips a call it should have made.

AgentChecksum answers two questions, in the order that matters:

```text
WHAT changed?                    DID it break?
Dependency Checksum   ─────►     Behavior Gate
```

It is not an observability platform, a tracing dashboard, an agent framework, or an eval SaaS.
It is a lockfile, a semantic diff, and a CI gate — for agents.

## Status

This is a young project; the table says exactly what runs today.

| Capability | State |
|---|---|
| `init` — config + probe scaffolding | **works** |
| `snapshot` — Model, Prompt, MCP server and MCP tool discovery; byte-deterministic `agentchecksum.lock` | **works** |
| `diff` — semantic dependency diff, per-facet risk, human and JSON output | **works** |
| MCP discovery — server era, tool contracts, declared annotation capabilities | **works** |
| `check` — behavioral probes, policy, the regression gate, `--accept`, `--trace`, `--jobs` | **works** |
| `inspect probes` — what is configured, what each probe asserts, what it feeds | **works** |
| Demo project, GitHub Action, prebuilt releases | planned |

Design decisions live in [`docs/specs`](docs/specs); the implementation plan for the current phase
lives in [`docs/plans`](docs/plans).

## Build

```bash
cargo build --release      # target/release/agentchecksum
```

Rust 1.98.1, edition 2024, single crate, single binary. No runtime, no database, no service.

## Quickstart

```bash
agentchecksum init        # writes agentchecksum.toml and an example probe
agentchecksum snapshot    # fingerprints the agent, writes agentchecksum.lock
# ... someone edits a prompt, a model, or an MCP tool ...
agentchecksum diff        # what changed, and how risky it is
agentchecksum check       # did it break? sample the agent, compare, gate
```

```console
$ agentchecksum snapshot
Agent checksum generated.

Checksum:
ac1:53bc19ec0e230dd63e7f31fe1f1847fde4f257e43764a9fdfb9c535bf19a19e1

Dependencies:
1 prompt
```

`agentchecksum.lock` is committed. It is the baseline every future comparison is made against.

### A real diff

Same prompt, different layout — the words are identical, so this is a formatting change and the risk
reflects that:

```console
$ agentchecksum diff

AgentChecksum diff

Baseline: ac1:53bc19ec0e230dd63e7f31fe1f1847fde4f257e43764a9fdfb9c535bf19a19e1
Current:  ac1:7650a9fd47a3d737cff272e794934fb3ea1191c75f19ee68ad7015f57d972109

1 dependency changed.

PROMPT  prompts/system.md  LOW
  content  sha256:bfad161d… → sha256:71fc3c90…
    classification: formatting-only
  shape    unchanged

Overall behavioral risk: LOW (heuristic)
```

Two things are deliberate in that output. Every facet that was compared appears, including the one
that did **not** move — so a reader can tell a facet that was checked from one that was never looked
at. And the risk is labelled a heuristic, because it is one.

### The diff this project exists for

The same mechanism runs over MCP tool definitions. This is the shape from the design spec's primary
demo — a "harmless documentation edit" that passes code review and passes API-compatibility checks:

```text
TOOL  demo-tools.search_repos                MEDIUM
  description    sha256:11aa88ff… → sha256:99bbccdd…
  input_schema   unchanged
  output_schema  unchanged

Overall behavioral risk: MEDIUM (heuristic)
```

The API did not break. The agent did.

## The Behavior Gate

`diff` answers *what changed*. `check` answers the second question: **did it break?** It samples the
agent through `[model]`, scores each sample against the expectations the probes declare, compares the
result against a committed baseline, applies policy, and exits with a code CI can act on.

```console
$ agentchecksum check

AgentChecksum check

Agent checksum: ac1:ca92e73c857f9c1e8289dd3da4e497412308a21d887f1c31cbb8c0bfd306a25d

No dependency changes detected.

Behavioral probes: 0 / 1 passed
argument_validity  n/a → 0%  WARN
tool_restraint     100% → 0%  FAIL

Policy failures:
  tool_restraint: score 0.0000 is below the required minimum 1.0000

Failing probes:
  no-tools  0 / 1 passed
    sample 0: argument_validity `web_search` is not a declared tool, so its arguments cannot be checked
    sample 0: tool_restraint 1 tool was called

Behavior Gate: FAIL    exit 1
```

The probe that produced that verdict is a file:

```toml
[[probe]]
name = "no-tools"
prompt = """
Answer from what you already know, without calling any tool: what is the capital of
Portugal?
"""
expect_no_tool = true
```

Five expectations exist, and a probe must declare at least one: `expect_tool`, `expect_args`
(JSON Pointer → matcher, requires `expect_tool`), `forbid_tools`, `expect_no_tool`, and
`output_schema`. The six resulting metrics are all scored the same way — **1.0 is good** — which is
why one policy vocabulary (`min` for a floor, `max` for a ceiling, `max_drop` against the baseline)
describes all of them:

```toml
[policy]
fail_on_risk = "critical"

[policy.metrics.tool_restraint]
min = 1.0

[policy.metrics.argument_validity]
max_drop = 0.05
```

### What the verdict refuses to claim

The gate is built so that the less it knows, the less it says:

- **A count, not a percentage.** `passed`/`total` is what a baseline records, because `0.9` invites an
  argument that `9 / 10` does not.
- **A metric nothing measured is absent, not zero.** `argument_validity` applies only to samples that
  called a tool; a probe that calls nothing does not get a free 1.0.
- **A row with no policy says `WARN`, not `PASS`.** "No threshold failed" and "the behavior was good"
  are different claims, and a table should not make the second one by accident.
- **Drift is not regression.** No baseline, or a changed probe suite, exits `0` (unless
  `--fail-on-drift`) and says why: the scores on either side answer different questions. Only a policy
  that actually failed is a `FAIL`.
- **A check that could not finish is never a `PASS`.** An unreachable model, an invalid probe, a
  malformed response: exit `3`, with the diagnostic. `--no-probes` is the explicit way to skip.
- **The runner observes; it never executes.** No `tools/call`, no MCP request, no sandbox, nothing the
  model asked for is ever run. The recorded tool decisions *are* the evidence.

### Replaying recorded evidence

`check --trace <path>` scores a recorded run instead of sampling a model: no request, no endpoint, no
`[model]` connection — the same evidence, the same deterministic evaluation, on a machine that cannot
reach the model at all. The path may name a single recorded trace or a whole run artifact.

Evidence is only scored when it demonstrably describes the agent being measured *now*. The recorded
run carries the context it was captured with, and every part of it has to agree with the current one:

| The evidence says | It must equal |
|---|---|
| `agent_checksum` | the agent this project fingerprints now |
| `captured_with.tool_catalog_digest` | the catalog the model would be shown now |
| `probe_suite_digest` (run artifacts) | the probe suite being scored now |
| `captured_with.runner` / `runner_version` | the runner this build implements |
| `probe` / `probe_digest` / sample count | the probe being asserted now |

Anything else is **unusable evidence, not a verdict**: exit `3`, with the fact that disagrees named.
It is never silently treated as drift, as a regression, or as a cache miss and re-captured. The
practical consequence is worth stating plainly — a replay is a reproduction of a measurement, not a
way to score an old agent against a new one. Evidence captured before a dependency change is refused
rather than reinterpreted, because scoring it would attribute one agent's behavior to another.

The same rule applies one level down, to what a call claims about itself. A recorded call that carries
a canonical `tool_id` is asserting an identity, and both halves of that assertion are checked: the
tool must exist in the catalog, and its declared name must be the name the call reported. A model that
invents a name carries no `tool_id` and is *measured* — a hallucinated tool is behavior worth scoring,
not a corrupt record — while a call whose `tool_id` and `name` contradict each other is refused, since
believing either half would award credit for a tool the model never called.

Baselines are compared under the same discipline. A committed baseline records the runner contract it
was produced under, and a baseline from another contract is non-comparable: its scores came from
different capture rules, so `max_drop` is not evaluated against it and the check reports **drift** with
the reason. Absolute thresholds are applied regardless — a floor this run misses is a fact about this
run.

`check --accept` is how a verdict becomes the baseline. It refuses to run while the dependency state
has moved — scores captured against a dependency set nobody committed would be attributed to the
wrong revision — and it writes counts, digests and yardstick digests, never prompts, model output, or
tool arguments.

## What it fingerprints

| Kind | Facets |
|---|---|
| `model` | identity (provider, id, content digest, quantization, family, size), inference parameters, chat template, capabilities |
| `prompt` | content, whitespace-collapsed shape |
| `mcp` | server identity — era, negotiated protocol version, supported versions, server info, declared capabilities |
| `tool` | input schema, output schema, description (+ shape), annotation capabilities |

`shape` exists so that a whitespace-only change (LOW) is distinguished from a semantic change
(MEDIUM) deterministically, with no LLM in the loop.

## MCP servers and tools

An MCP server is fingerprinted where it is declared in `agentchecksum.toml`:

```toml
[[mcp.servers]]
name = "demo-tools"           # the alias: it prefixes every dependency id
transport = "stdio"
command = "uvx"               # executed directly, never through a shell
args = ["demo-tools-server"]
# env = { DEMO_TOKEN = "…" }  # passed to the child, never fingerprinted, redacted in diagnostics

# or a remote server:
# [[mcp.servers]]
# name = "remote"
# transport = "streamable-http"
# url = "https://example.com/mcp"
```

Both transports are supported: **`stdio`** (the configured command is executed directly, with the
configured argument vector — never through a shell) and **`streamable-http`** (`http`/`https` only;
credentials, query strings, and fragments in the URL are rejected rather than stripped, and redirects
are not followed).

Session establishment is a two-step policy, and only the server can trigger the second step: AgentChecksum
asks for the stateless protocol first, and falls back to the session handshake only when the server answers
that it does not implement it. A slow server is a **failure**, never a legacy one — timing is not evidence
about a protocol era, and letting it decide one would mean identical declarations fingerprinted differently
from one run to the next.

Each server becomes a dependency `mcp:<alias>` carrying its identity and, when the server declares them,
its **instructions** — the prose it gives the model about how to use it. Instructions are fingerprinted the
way a prompt or a tool description is, as a content digest plus a whitespace-collapsed shape digest, so a
reflow reads as LOW and a rewrite as MEDIUM, and the lockfile keeps hashes rather than server prose.
Changing only the instructions, with every tool untouched, is therefore still a dependency change. Each
declared tool becomes
`tool:<alias>.<name>` — the name is percent-encoded, so two distinct names can never produce one id —
carrying its description, its input schema, its output schema when it declares one, and its annotation
capabilities. The alias is a namespace, not a display name: renaming it is an identity change, and it is
deliberately restricted to `[A-Za-z0-9_-]+` because a dot would collide with the separator between alias
and tool.

**Discovery never calls a tool.** It connects, asks the server what it declares, and closes: no tool is
ever invoked on your behalf, so a snapshot cannot exercise the side effects a tool call would have.
`prompts/*`, `resources/*`, tasks, sampling, roots, elicitation, and subscriptions are out of scope.

What is deliberately left out of the fingerprint is as settled as what goes in: transport and session
plumbing (PIDs, ports, session ids, cache hints, timings), configured commands and environment variables,
server stderr, cosmetic metadata (`title`, `icons`), and opaque `_meta` and extension *settings* (their
presence and identifiers are reported — one aggregated warning per server — but never their values). The
bounds on discovery —
timeouts, page count, tool count, schema size and nesting depth — live in a single file, and exceeding one
fails the run rather than truncating the inventory: a lockfile that describes a partial server is worse
than no lockfile. Discovery is fail-closed. One server that cannot be fully discovered fails the command,
nothing is written, and a duplicate id stops the run before a lockfile exists.

Tool annotations are recorded as declared: AgentChecksum folds in the protocol defaults and reports the
effective tokens (`read-only`/`write`, `destructive`/`non-destructive`, `idempotent`/`non-idempotent`,
`open-world`/`closed-world`). They are hints a server declares about itself, not guarantees — a server
that says `read-only` may still write, and nothing in the output claims otherwise.

Those tokens are also what makes one diff case stricter than the rest. A newly added tool is HIGH because
it is new invocation surface; a newly added tool whose own declaration names it **write-capable and
destructive** is CRITICAL — the worst thing a diff can discover on its own, and still only a *declared*
one. The escalation needs both tokens, and a capability payload this build cannot decode leaves the
ordinary added-tool risk in place rather than inflating or deflating it.

Session establishment is deliberately strict about one thing: AgentChecksum falls back to the session
protocol only when the server answers, in so many words, that it does not implement `server/discover`
(`-32601`). Every other answer — a generic error, an internal failure, a timeout, a transport or
authorization problem — is a **failure**, not evidence that the server is old. Nothing about timing or
reachability is allowed to change which protocol era a dependency is fingerprinted under.

Because no credential-derived value is fingerprinted, rotating a token that does not change what the
server declares produces the same checksum. And because configured environment values are connection
material, every non-empty one is redacted out of everything you can see — stdout, stderr, warnings,
errors, tracing, the lockfile — including text a server echoes back. There is no length threshold: a
three-character token is treated like any other. This is a guarantee about values you configured, not a
claim to recognize secrets AgentChecksum was never given.

The boundary is enforced in the other direction too, because a server is handed its environment and can
echo it back: if a server reflects a configured value into anything AgentChecksum would fingerprint — its
own name or version, its instructions, a tool name, a tool description, the protocol revisions it reports,
or any key or string inside a schema — discovery **fails** rather than describing it. The declaration is not rewritten and the value is
not blanked out inside the contract; a fingerprint taken over an edited declaration would describe a
contract the server never declared. What a server sends in opaque fields AgentChecksum never reads, such
as tool `_meta`, is not scanned either: unread data cannot reach a fingerprint. When credentials do change the declared contract — a
narrower set of authorized tools, for example — that is a real dependency change, and it is reported as
one.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. `diff` uses this **even when the change is CRITICAL**, and `check` uses it for drift — they report, the policy decides |
| `1` | Gate failure — a metric policy failed, or `--fail-on-drift` / `--fail-on-risk` turned a change into one |
| `2` | Usage error, including a flag combination that cannot mean anything |
| `3` | Runtime error — config, discovery, network, unsupported input, an interrupted check |

Comparing successfully and finding danger are different outcomes, and the exit codes keep them
apart so CI can tell them apart.

## Machine-readable output

`--format json` puts one JSON document on stdout and nothing else, so it is always parseable:

```bash
agentchecksum diff --format json | jq '.overall_risk, .changes[].id'
```

A runtime failure writes its diagnostic to stderr and leaves stdout empty. The documented shape is
in [spec §8.4](docs/specs).

`check --format json` emits the whole verdict as one document — `status`, `agent_checksum`,
`baseline_checksum`, `dependency`, `behavior`, `error` — and every field is always present, `null` or
`[]` where there is no answer, so a consumer never has to tell a missing key from an absent result:

```bash
agentchecksum check --format json | jq '.status, .behavior.metrics[] | select(.verdict == "fail")'
```

## How it stays deterministic

- Every JSON digest is **SHA-256 over RFC 8785 canonical JSON**, so key order, whitespace, and number
  formatting can never influence a checksum. Text facets — prompt content, prompt shape, tool
  descriptions — digest normalized text instead, because that is what a model reads.
- A narrow, individually tested normalization layer above JCS: `required` order, `enum` order,
  parameter-set order, CRLF/LF, missing-versus-empty.
- Behavior-relevant content is **never** treated as insignificant: schema `description`, `title`,
  `examples` and `default`, prompts, tool descriptions, chat templates, and capabilities all move the
  fingerprint.
- Timestamps, absolute paths, machine identifiers, discovery order, and vendor metadata
  (`modified_at`, `size`, `license`) are excluded by an explicit, tested list.
- The agent checksum is a function of dependency inputs only — never of lockfile serialization.

## How it treats your project

AgentChecksum reads, parses, normalizes, hashes, and compares. It does **not** import or execute the
project it inspects: the only process it starts is the MCP server you configured in
`agentchecksum.toml`, and only to ask what that server declares. That matters when CI is examining an
untrusted pull request — the configuration file is the trust boundary, which is why it is committed and
reviewed like any other input.

`check` adds exactly one outbound connection — the model endpoint in `[model]` — and no capability
beyond it. That connection is made only when a run is *captured*: probes never execute tools, schemas
are never fetched (`jsonschema` runs with HTTP and file resolution disabled, so a schema that needs
them is refused), and `--trace` evaluates recorded evidence without contacting anything. Capture is
statistical — one sample is one request, and no retry pretends otherwise — while evaluation is a pure
function of the recording, which is what makes a replay reproducible.

A replay still needs the current agent to be *describable*: `check` fingerprints the dependency state
it compares against, and for a provider whose identity requires a live server (Ollama's model digest,
for example) that fingerprinting is the one thing replay cannot do offline. That is discovery, not
replay, and it is the same requirement `snapshot` has.

Known limitations, stated rather than discovered: no LLM judge, no regex matchers, no tool-result or
multi-turn evaluation, no authentication for remote endpoints, and trace *capture* is not
bit-reproducible (trace *evaluation* is).

## License

MIT OR Apache-2.0, at your option.
