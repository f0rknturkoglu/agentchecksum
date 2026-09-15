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
| `snapshot` — Model + Prompt discovery, byte-deterministic `agentchecksum.lock` | **works** |
| `diff` — semantic dependency diff, per-facet risk, human and JSON output | **works** |
| MCP discovery — server identity, tool inventory, schemas, descriptions | next |
| Behavioral probes, `check`, the regression gate | planned |
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

Once MCP discovery lands (next phase), the same mechanism runs over tool definitions. This is the
shape from the design spec's primary demo — a "harmless documentation edit" that passes code review
and passes API-compatibility checks:

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
| `tool` | input schema, output schema, description (+ shape), capabilities |
| `mcp_server` | identity (protocol era, protocol version, supported versions, server info) |

`shape` exists so that a whitespace-only change (LOW) is distinguished from a semantic change
(MEDIUM) deterministically, with no LLM in the loop.

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

- Every digest is **SHA-256 over RFC 8785 canonical JSON**, so key order, whitespace, and number
  formatting can never influence a checksum.
- A narrow, individually tested normalization layer above JCS: `required` order, `enum` order,
  parameter-set order, CRLF/LF, missing-versus-empty.
- Behavior-relevant content is **never** treated as insignificant: `description`, `title`,
  `examples`, `default`, chat templates, and capabilities all move the fingerprint.
- Timestamps, absolute paths, machine identifiers, discovery order, and vendor metadata
  (`modified_at`, `size`, `license`) are excluded by an explicit, tested list.
- The agent checksum is a function of dependency inputs only — never of lockfile serialization.

## How it treats your project

AgentChecksum reads, parses, normalizes, hashes, and compares. It does **not** import or execute the
project it inspects. That matters when CI is examining an untrusted pull request.

## License

MIT OR Apache-2.0, at your option.
