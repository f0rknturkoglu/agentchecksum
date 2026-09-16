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
| Behavioral probes, `check`, the regression gate | next |
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
agentchecksum init        # writes agentchecksum.toml and probes/
agentchecksum snapshot    # fingerprints the agent, writes agentchecksum.lock
# ... someone edits a prompt, a model, or an MCP tool ...
agentchecksum diff        # what changed, and how risky it is
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
own name or version, its instructions, a tool name, a tool description, or any key or string inside a
schema — discovery **fails** rather than describing it. The declaration is not rewritten and the value is
not blanked out inside the contract; a fingerprint taken over an edited declaration would describe a
contract the server never declared. What a server sends in opaque fields AgentChecksum never reads, such
as tool `_meta`, is not scanned either: unread data cannot reach a fingerprint. When credentials do change the declared contract — a
narrower set of authorized tools, for example — that is a real dependency change, and it is reported as
one.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. `diff` uses this **even when the change is CRITICAL** — it reports, the gate decides |
| `1` | Gate failure (behavioral regression or policy violation) |
| `2` | Usage error |
| `3` | Runtime error — config, discovery, network, unsupported input |

Comparing successfully and finding danger are different outcomes, and the exit codes keep them
apart so CI can tell them apart.

## Machine-readable output

`--format json` puts one JSON document on stdout and nothing else, so it is always parseable:

```bash
agentchecksum diff --format json | jq '.overall_risk, .changes[].id'
```

A runtime failure writes its diagnostic to stderr and leaves stdout empty. The documented shape is
in [spec §8.4](docs/specs).

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

## License

MIT OR Apache-2.0, at your option.
