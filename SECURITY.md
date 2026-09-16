# Security policy

## Reporting a vulnerability

**GitHub private vulnerability reporting is enabled on this repository.** Do not open a public issue
for a security problem.

1. Open <https://github.com/f0rknturkoglu/agentchecksum/security>
2. Click **Report a vulnerability**.
3. Describe the issue, and include the version, your platform, and the smallest reproduction you can.

That channel is private, visible only to the maintainers, and it keeps the report and the discussion
attached to the code. There is no security email address — please use the button above rather than
guessing at one.

Never include a real credential, token or production endpoint in a report. A placeholder that
demonstrates the shape of the problem is enough, and a reproduction that leaks a live secret is worse
than no reproduction.

## What to expect

- **Acknowledgement** that the report was received, and a first assessment of whether it is a
  vulnerability in AgentChecksum — usually within a few days. This is a small project, so the answer
  may take longer than a large one would promise.
- **A fix, then disclosure.** The aim is to ship a fix before the details are public, and to agree the
  disclosure timing with you. You will be credited in the changelog entry unless you ask not to be.
- **No bug bounty.** There is no payment for reports; there is a fix and a credit.

Please give the maintainers a reasonable chance to fix the issue before publishing it.

## Supported versions

During the `v0.x` series, **only the latest minor version is supported**. A fix lands on `main` and in
the newest release; older minors do not receive backports.

| Version | Supported |
|---|---|
| The latest `v0.x` release | yes |
| Any earlier `v0.x` release | no |
| `main` (untagged) | best effort — the code is in flux and is not a release |

`0.1.0` is released: tagged [`v0.1.0`](https://github.com/f0rknturkoglu/agentchecksum/releases/tag/v0.1.0),
published on [crates.io](https://crates.io/crates/agentchecksum), and built for four targets. That
release is the current supported version, and it is what a report should name. Reports against `main`
or a local build are still welcome — they are just not covered by a support promise.

## What counts as a vulnerability

The trust model in [docs/security.md](docs/security.md) is what to measure a report against. In
short, AgentChecksum reads, parses, normalizes, hashes and compares; it starts only the MCP server you
configured and, when a run is captured, an HTTP request to the model endpoint you configured. Nothing
a model requests is ever executed — there is no `tools/call`, no sandbox, no MCP request on the
behavioral path.

Reports that are in scope include, but are not limited to:

- a configured `env` value reaching **any** output — stdout, stderr, a warning, an error, tracing, the
  lockfile — including via text a server echoes back, or a redaction that a crafted value can bypass;
- an output schema or tool schema causing a network fetch (HTTP or file resolution) instead of a
  refusal;
- a tool being executed, or an MCP request being made, where the documented behavior is observation
  only;
- evidence being scored while it does not describe the current agent, catalog, probe suite or runner —
  that is, a replay binding being bypassed;
- a hand-edited or crafted artifact (lockfile, baseline, trace, run artifact, cache entry) being
  accepted and scored instead of refused;
- an artifact or diagnostic that leaks a secret it was handed, in a way the trust model says cannot
  happen;
- a crash, hang or memory-exhaustion path reachable from untrusted input — a configuration file, a
  probe file, a schema, a server's declaration, or a model's answer.

## What is documented behavior, not a vulnerability

Stated so nobody has to discover it the hard way:

- **Risk levels are a heuristic.** They encode "how likely is this change to alter agent behavior",
  not "is this change dangerous". A HIGH or CRITICAL classification that turns out to be a benign
  change is a judgement call, not a vulnerability.
- **MCP annotations are untrusted declarations.** A tool's `read-only` / `destructive` tokens are what
  the server says about itself, and the MCP specification instructs clients to treat annotations as
  untrusted unless the server is trusted. The output says so; a server lying about itself is the
  expected threat, not a bug.
- **The configuration file is trusted input.** It names a command to execute and an endpoint to
  contact. If an attacker can edit `agentchecksum.toml`, they can already run code through the MCP
  server you configured; review it like CI configuration.
- **A configured `stdio` MCP server runs with your privileges.** That is what it is for. The boundary
  AgentChecksum enforces is that it hands the process nothing else to do — it never calls a tool.
- **No authentication for remote endpoints, and no retries.** Sampling is statistical by design: one
  sample is one request, with a two-minute per-sample timeout. A flaky or slow endpoint is a failure,
  reported as exit `3`, not a silently retried sample.
- **Redaction covers values you configured.** It is not a content-aware secret scanner and makes no
  attempt to recognize a secret it was never given.

## Supported platforms

AgentChecksum is a Rust CLI with no runtime dependency, and the pinned compiler is `1.98.1` (see
`rust-toolchain.toml`). The test suite runs in CI on Linux x86_64. Release archives are built and
smoke-tested on native runners for these four targets, and they are published for every release:

```text
aarch64-apple-darwin        macOS, Apple Silicon
x86_64-apple-darwin         macOS, Intel
x86_64-unknown-linux-gnu    Linux, x86_64
x86_64-pc-windows-msvc      Windows, x86_64
```

Source builds work anywhere the compiler does, and a distribution channel that carries the binary —
[Homebrew](docs/distribution.md#homebrew), [cargo-binstall](docs/distribution.md#cargo-binstall) —
covers a subset of those targets rather than a different set. Reports about any platform are welcome,
whether or not it is in the list.
