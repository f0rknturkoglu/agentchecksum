# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet. The next entry lands when work starts on the version after `0.1.0`.

## [0.1.0] — not yet released

Version `0.1.0` is the version in `Cargo.toml`, but it has **not been released**: the repository is
not tagged, no GitHub Release exists, and the crate is not published on crates.io. There is no date
here on purpose — a date would claim a release that has not happened. Until the first tag, the
supported ways to install it are a build from a clone or `cargo install --git`; see
[getting-started.md](docs/getting-started.md#install).

This is the first version of the design described in
[the design specification](docs/specs/2026-09-15-agentchecksum-design.md), implemented as one Rust
crate and one binary: no service, no database, no dashboard, no telemetry.

### Added

**Deterministic dependency fingerprinting** — `init` scaffolds `agentchecksum.toml` and an example
probe; `snapshot` discovers the agent's dependencies and writes a byte-deterministic
`agentchecksum.lock`. Dependencies are identified as `(kind, id)` with individually digested facets
(`model`, `prompt`, `mcp`, `tool`), aggregated into one `ac1:` agent checksum. Every JSON digest is
SHA-256 over RFC 8785 canonical JSON, with a narrow, individually tested normalization layer above it
(CRLF/LF, `required`/`enum`/parameter-set order, missing-versus-empty), so key order, whitespace and
number formatting can never move a checksum. Behavior-relevant content is never treated as
insignificant; timestamps, absolute paths, machine identifiers, discovery order and vendor metadata
are excluded by an explicit, tested list.

**Semantic diff with per-facet risk** — `diff` compares the committed lockfile against current
dependency state and reports what moved, added or was removed, one block per dependency and one line
per facet compared — including the facets that did not move. Text facets carry a whitespace-collapsed
`shape` digest so a reflow (LOW) is distinguished from a rewrite (MEDIUM) deterministically, with no
model in the loop. Risk is a pure function over the diff, reported as an overall level labelled a
heuristic, and `diff` exits `0` whenever the comparison ran: it reports, the policy decides.

**MCP discovery and tool contracts** — both transports. A `stdio` server's configured command is
executed directly, never through a shell; a `streamable-http` server must be `http`/`https` with no
credentials, query string or fragment, and redirects are not followed. Discovery reads the server
identity (protocol era, negotiated version, supported versions, server info, declared capabilities),
the server instructions, and the tool list — names, descriptions, input and output schemas, and the
annotation capabilities a server declares, folded into effective tokens. Session establishment asks
for the stateless protocol first and falls back only when the server answers that it does not
implement discovery; a slow server is a failure, never evidence of an older era. Discovery never calls
a tool, a partial catalog is refused rather than fingerprinted, and one server that cannot be fully
discovered fails the command with nothing written.

**Behavioral probes with six metrics** — probes are committed TOML files declaring a prompt and at
least one of five expectation keys: `expect_tool`, `expect_args` (RFC 6901 JSON Pointers to matchers,
with `equals`, `contains` and `one_of` as the whole matcher vocabulary), `forbid_tools`,
`expect_no_tool` and `output_schema`. Six metrics are scored from them — `tool_selection`,
`argument_validity`, `argument_expectation`, `forbidden_tool_usage`, `tool_restraint`,
`structured_output_validity` — and every one of them points the same way, `1.0 is good`, so a single
policy vocabulary describes all six. Metrics are exact counts rather than percentages, and a metric
nothing measured is absent rather than perfect.

**The Behavior Gate: baseline, policy and verdict** — `check` samples the agent through `[model]`,
scores the samples deterministically, compares them against the committed behavioral baseline in
`.agentchecksum/baseline.json`, applies `[policy]`, and exits with a code CI can act on. Policy
supports an absolute floor (`min`), an absolute ceiling (`max`) and a relative `max_drop` measured
only against a *comparable* baseline (same probe suite digest and same runner contract), plus
`fail_on_risk` / `--fail-on-risk` for the dependency half. The verdict keeps drift apart from
regression: drift (no baseline, a changed suite, a changed runner contract, a changed dependency
checksum) exits `0` unless `--fail-on-drift` asked for a gate, and only a policy that actually failed
is a `FAIL`. A row whose policy could not be applied reads `WARN`, not `PASS`; a run that could not
finish is exit `3`, never `PASS`. `check --accept` is the only command that writes the baseline, and
it refuses to run while the dependency state itself has moved.

**Replay-integrity binding** — `check --trace <path>` scores a recorded trace or a whole run artifact
instead of calling a model: no request, no endpoint needed. The evidence is bound to the context it
describes — agent checksum, tool catalog digest, runner and version, probe name/digest/sample count,
and for a run artifact the probe suite digest — and anything that disagrees is **unusable evidence,
not a verdict**: exit `3`, with the fact that disagrees named. Run artifacts are named after the
SHA-256 of their own canonical form, so a hand-edited artifact is refused before anything evaluates
it, and the sample cache verifies the inputs each entry was keyed by rather than trusting the file
name.

**Two model providers** — `ollama` (content digest, quantization, chat template, capabilities and
reported parameters, read from the native API) and `openai-compatible` (any OpenAI-compatible
`/v1/chat/completions` endpoint). The second exposes no content digest, so the endpoint becomes part
of the model identity and the CLI warns about it rather than implying a guarantee it cannot make.

**Diagnosability** — `inspect probes` prints the parsed probe suite: what is configured, which tools
each probe references, and the metrics each probe feeds. `--format json` writes one JSON document to
stdout and nothing else for every command, and diagnostics go to stderr through `tracing`.

### Security

- Configured `[[mcp.servers]].env` values are passed to the child process, never fingerprinted, and
  redacted out of stdout, stderr, warnings, errors, tracing and the lockfile — including text a server
  echoes back. A server that reflects a configured value into anything AgentChecksum would fingerprint
  fails discovery rather than being described.
- Output schemas are compiled with JSON Schema HTTP and file resolution disabled, so a schema that
  needs them is refused instead of fetched. Probes never reach the network.
- No tool requested by the model is ever executed: the behavioral runner observes decisions and
  records them as evidence.
- Evidence that cannot be trusted is refused with exit `3` rather than scored.

See [docs/security.md](docs/security.md) for the full trust model.

### Not in this version

Stated rather than discovered: no LLM judge, no regex matchers, no tool-result or multi-turn
evaluation, no authentication for remote endpoints, trace *capture* is not bit-reproducible (trace
*evaluation* is), and `inspect` is limited to `probes` — the dependency and MCP views the subcommand
describes belong to a later phase. Prebuilt release binaries, a published crate and the composite
action's download path all begin at the first tagged release.

[Unreleased]: https://github.com/f0rknturkoglu/agentchecksum/commits/main

There is no `v0.1.0` tag to link to: that is the point of "not yet released" above.
