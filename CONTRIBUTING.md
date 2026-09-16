# Contributing

AgentChecksum is one Rust crate and one binary. Contributions are welcome; this page covers the parts
that are not guessable from the code: the pinned toolchain, the checks a change has to pass, how to
run one test instead of all of them, and the schemas that are contracts rather than implementation
details.

## Toolchain

`rust-toolchain.toml` at the repository root is the **single source of truth** for the compiler:

```toml
[toolchain]
channel = "1.98.1"
components = ["rustfmt", "clippy"]
profile = "minimal"
```

That means, and this is deliberate:

- Any `cargo` or `rustc` invocation through the rustup shims installs and uses `1.98.1`. You do not
  need to select it, and you should not override it locally.
- CI does **not** pass a version of its own — it asserts that the running compiler is the pin. A
  tool whose whole value proposition is reproducibility must not have CI and a local checkout drift
  apart; if your change needs a newer compiler, the pin is the thing to change, in its own commit,
  with the reason.
- The crate also declares `rust-version = "1.98"` and `edition = "2024"` in `Cargo.toml`.

## Build

```bash
cargo build              # target/debug/agentchecksum
cargo build --release    # target/release/agentchecksum
```

There is no runtime, database or service to start. The only long-running processes involved in
development are the test fixture servers under `examples/` (below).

## The checks a change has to pass

These are exactly what CI runs, in this order:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

`cargo clippy --all-targets -- -D warnings` means warnings are errors: a lint that fires is a failing
build, whatever the lint is. Run all three before opening a pull request; a change that only passes
locally under `--release`, or only in one target, is not done.

## Running one test

The unit tests live beside the code they test, so a module path is a test filter:

```bash
cargo test --lib probes::eval       # the probe evaluation rules
cargo test --lib config::           # configuration parsing and validation
cargo test --lib diff::risk         # the risk table
cargo test --lib discovery::mcp     # MCP discovery
```

`cargo test --lib probes::eval` runs 27 tests and filters out the rest, which is the difference
between a two-second loop and a two-minute one.

### Integration suites

The integration tests in `tests/` are named after the behavior they defend. Each is a target, so it
can be run on its own:

| Target | What it covers |
|---|---|
| `behavior_check` | `check` end to end: policy, baselines, drift, `--accept`, exit codes |
| `behavior_runner` | capture against a scripted endpoint: tool calls, malformed answers, timeouts, replays |
| `snapshot_determinism` | byte-determinism of `agentchecksum.lock` across runs and orderings |
| `diff_commands` | `diff` behavior and its report shapes |
| `mcp_discovery` | discovery against a real MCP server over both transports, including protocol eras |
| `lockfile_contract` | the committed lockfile schema and its version refusal paths |
| `cli_basics` | argument parsing, `--help`, `--version`, usage exits |
| `cli_commands` | command-level contracts shared across subcommands |

```bash
cargo test --test mcp_discovery
cargo test --test behavior_check -- --list    # see what a target contains
```

Some tests compare against stored snapshots (`insta`). When a snapshot assertion fails, the candidate
is written beside it as `*.snap.new` and **not** committed. Read it, and only if the new output is
what you intend:

```bash
cargo insta accept
```

## The local fixture servers

`examples/` holds two development-only servers. Nothing under `src/` refers to them, they are built
only as example targets, and they exist so that discovery and capture can be exercised against real
processes and real HTTP — with the interesting behavior stated in data rather than buried in code.

### `examples/mcp_fixture_server.rs`

A real MCP server whose declarations come from a JSON file: server info, instructions, the tool
catalog, page size, and how `server/discover` is answered (modern, refused with a chosen JSON-RPC
error code, or delayed). Both transports are served — `--stdio` (the default) over stdin/stdout, and
`--http` on `127.0.0.1` with an ephemeral port.

```bash
cargo build --example mcp_fixture_server
AC_FIXTURE_SPEC=/tmp/mcp-spec.json \
AC_FIXTURE_PORT_FILE=/tmp/mcp-port \
  target/debug/examples/mcp_fixture_server --stdio
```

The port and pid files are written once the server is listening; an attempt file receives one line per
start, which is how a test can count how many times the client started it. It reads nothing else from
its environment: a credential the client hands it is never looked at, echoed or logged.

### `examples/openai_fixture_server.rs`

An OpenAI-compatible `/v1/chat/completions` endpoint whose answers come from a JSON file: a text turn,
a tool-call turn with arguments as a JSON string or an already-parsed value, malformed arguments, a
tool name that is not in the catalog, an HTTP error, a reply that arrives too late, and a body that is
not a chat completion at all. It also records every request it receives, which is the only way a test
can see *what the runner sent* rather than what the runner believes it sent.

```bash
cargo build --example openai_fixture_server
AC_FIXTURE_SPEC=/tmp/openai-spec.json \
AC_FIXTURE_PORT_FILE=/tmp/openai-port \
  target/debug/examples/openai_fixture_server
```

Responses are served in order, and the last one repeats — so a `repeat = N` capture against a
single-response spec is N identical samples. A missing or unreadable spec is a loud exit, never a
plausible-looking default that a test could mistake for a successful capture.

## Contracts, not refactors

Some artifacts in this project are consumed by other machines and other repositories. Changing them is
a compatibility decision, and it needs a version bump with a thought-through refusal path — not the
kind of edit that rides along with a feature:

| Contract | Where it is pinned |
|---|---|
| The checksum aggregate format (`ac1:`) | The agent checksum every comparison is keyed by |
| The lockfile schema | `lock_version` |
| The behavioral baseline schema | `baseline_version` |
| The run artifact schema | `run_version` |
| The trace and cache schemas | `trace_version`, `cache_version` |
| The human and JSON report shapes | What `--format json` promises, and what CI parses |
| The exit codes | `0` / `1` / `2` / `3`, with the semantics documented in [getting-started.md](docs/getting-started.md#exit-codes) |
| The runner contract string | `openai-chat-completions-v1`, recorded in every baseline |

The reason to be strict is that the failure mode is silent. A newer lockfile, baseline, trace or run
artifact is **refused** rather than reinterpreted, because a version that is silently coerced is a
version that scores the wrong agent. If you change any schema above, the change includes: the new
version constant, the refusal path for anything newer, and a note in [CHANGELOG.md](CHANGELOG.md).

Two related rules the codebase already follows, worth keeping:

- **Deterministic bytes.** Committed artifacts must be reviewable and diffable, and identical inputs
  must produce identical bytes. If a new field is derived from a clock, a path, a PID or a hash-map
  iteration order, it does not belong in a committed artifact.
- **Refuse rather than truncate.** Bounds on discovery, probe suites and schemas fail a run; they do
  not quietly return a partial inventory, because an artifact that understates what a server declares
  is worse than no artifact.

## Reporting a bug

Open an issue at <https://github.com/f0rknturkoglu/agentchecksum/issues>. A report that can be
reproduced is worth ten that cannot, so include:

- the AgentChecksum version (`agentchecksum --version`) and your platform;
- the exact command, and the exit code (not just "it failed" — `1` and `3` mean very different
  things);
- the full stderr, including the `Suggested action:` block if there is one;
- the smallest configuration and probe suite that reproduces it, with credentials and endpoints
  replaced by placeholders.

For a vulnerability, do **not** open a public issue — use GitHub's private vulnerability reporting as
described in [SECURITY.md](SECURITY.md).

## Where to start

- [README.md](README.md) — what the product is and what it is not.
- [docs/specs/2026-09-15-agentchecksum-design.md](docs/specs/2026-09-15-agentchecksum-design.md) —
  the design specification, including the risk table, the fingerprint definition and the invariant
  list a change has to keep true.
- [docs/configuration.md](docs/configuration.md), [docs/probes.md](docs/probes.md),
  [docs/ci.md](docs/ci.md), [docs/security.md](docs/security.md) — the user-facing contracts.
- `docs/plans/` — the phase plans, kept as engineering history rather than as current documentation.
