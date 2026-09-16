# Using AgentChecksum in CI

A CI job does two things with AgentChecksum, and they have very different requirements:

| Half | Command | Needs a model? | Network |
|---|---|---|---|
| Dependency gate — "did the fingerprint move?" | `check --no-probes` (or `--diff-only`) | no | only for MCP discovery and Ollama metadata |
| Behavior gate — "did it break?" | `check` | yes | the `[model]` endpoint, plus MCP discovery |

The dependency half is deterministic, offline-capable and cheap; put it on every pull request. The
behavioral half samples a model, so it belongs where a model endpoint and its credentials are
available — and a *new* baseline is something a human reviews, not something every PR writes.

Every command below is verified against the binary's `--help`; `--config`, `--lock`, `--from` and
`--format` are global flags that work on every subcommand.

## Installing the CLI in a job

**Nothing is released yet** — `0.1.0` is not tagged, no GitHub Release exists and the crate is not on
crates.io — so a job must not assume a download exists. Two options work today.

### The composite action (recommended)

The repository ships a composite action at its root, so a consumer repository can use it directly:

```yaml
- uses: f0rknturkoglu/agentchecksum@v0.1
  with:
    command: check
```

It locates a binary — a published release asset when one exists for the runner, otherwise a source
build from the checkout it ships, announced on stderr (`cargo install --locked --path <action>`), which
is why it works before the first release — then runs one AgentChecksum command in your project and
lets the CLI's exit code decide the step's outcome.

| Input | Default | Purpose |
|---|---|---|
| `command` | `check` | The subcommand to run: `snapshot`, `diff` or `check`. |
| `args` | `""` | Extra arguments, e.g. `--fail-on-drift --format json`. Do not repeat `--config`/`--lock`; the action passes those. |
| `version` | `latest` | Which release to use: `latest`, or a tag such as `v0.1.0`. Falls back to the source build when no asset exists for the platform. |
| `working-directory` | `.` | The project directory to run in. |
| `config` | `agentchecksum.toml` | Config path, relative to the working directory. |
| `lock` | `agentchecksum.lock` | Lock path, relative to the working directory. |

| Output | Meaning |
|---|---|
| `exit-code` | The CLI's exit code: `0`, `1`, `2` or `3`. The step fails for any non-zero code. |
| `status` | The JSON `status` (`pass`, `drift`, `regression`, `error`) when `args` contains `--format json`; empty otherwise. |

Three properties of the action matter when you are deciding whether to trust a step: it never
reinterprets a verdict, it never swallows an exit code, and it never invents arguments — the CLI's
exit code *is* the step's outcome, and the action adds no defaults beyond the inputs above.

A downloaded release binary is **always** checksum-verified before it runs. The release workflow
publishes a `SHA256SUMS` manifest alongside the archives, so a manifest that cannot be read, one with
no entry for the selected archive, or a digest that disagrees all mean the same thing: the archive
cannot be authenticated, nothing is executed, and the step fails with exit `3`. That is a distribution
failure, not a gate failure, and it is deliberately not a reason to build from source instead — an
asset that was selected has to be usable, and silently replacing it would hide a broken release.

The source build is the answer to one situation only: **no release asset exists** for the runner's
platform, which is what makes the action usable before the first release. It installs with
`cargo install --locked --path <action>`, and on Windows it runs the `agentchecksum.exe` Cargo
installs there.

### Building it yourself

If you would rather not use the action, install from source in the job. This needs a Rust toolchain
and a few minutes of build time:

```yaml
- uses: actions/checkout@v7
- uses: dtolnay/rust-toolchain@stable
- run: cargo install --locked --git https://github.com/f0rknturkoglu/agentchecksum
```

`cargo install agentchecksum` (crates.io) does **not** work yet, and no prebuilt binary can be
downloaded until the first tagged release. Both start working at that point; neither is a method to
plan around today.

## A dependency-only gate

This job needs no model, no secret, and no network beyond discovery. It fails when a dependency
moved, which is the check most teams want on every pull request.

```yaml
name: agent-checksum

on:
  pull_request:

jobs:
  dependencies:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7

      - name: Fingerprint the agent
        uses: f0rknturkoglu/agentchecksum@v0.1
        with:
          command: snapshot

      # The lockfile is generated. If snapshot produced different bytes than the
      # commit carries, the dependency state on disk is not the one that was reviewed.
      - name: The committed lockfile must match the agent
        run: git diff --exit-code -- agentchecksum.lock

      - name: Gate on drift
        uses: f0rknturkoglu/agentchecksum@v0.1
        with:
          command: check
          args: --no-probes --fail-on-drift
```

Two deliberate details:

- `snapshot` before the gate, then `git diff --exit-code` on the lockfile, is how a job detects "the
  MCP server changed, or a prompt was edited, and nobody re-snapshotted". It is a `git` check rather
  than an AgentChecksum feature because AgentChecksum has no git integration by design: the caller
  supplies version control's state, and the tool compares fingerprints.
- `--no-probes --fail-on-drift` turns the dependency comparison into a gate without contacting a
  model. Without `--fail-on-drift`, drift is reported and exits `0`.

## A pull-request gate against the base revision

The interesting question on a pull request is not "does this match the committed lockfile" — it is
"what changed relative to the base branch this PR will be merged into". The binary has no git
integration, so the caller supplies the old lockfile with the global `--from <path>` flag:

```bash
git show HEAD:agentchecksum.lock > /tmp/old.lock   # the shape the CLI documents
```

On a pull request, `HEAD` is the merge commit or the PR head, so the base revision has to be fetched
explicitly. The base SHA is in the event payload:

```yaml
name: agent-checksum

on:
  pull_request:

jobs:
  gate:
    runs-on: ubuntu-latest
    steps:
      # fetch-depth: 0 so the base revision's tree is available to `git show`
      - uses: actions/checkout@v7
        with:
          fetch-depth: 0

      - name: Fetch the base revision
        id: base
        run: |
          base_sha="${{ github.event.pull_request.base.sha }}"
          git fetch --no-tags origin "$base_sha"
          if git cat-file -e "$base_sha:agentchecksum.lock" 2>/dev/null; then
            git show "$base_sha:agentchecksum.lock" > "$RUNNER_TEMP/base.lock"
            echo "lock=$RUNNER_TEMP/base.lock" >> "$GITHUB_OUTPUT"
          else
            echo "::notice::the base revision has no agentchecksum.lock; nothing to compare against"
          fi

      - name: Report what changed
        if: steps.base.outputs.lock != ''
        uses: f0rknturkoglu/agentchecksum@v0.1
        with:
          command: diff
          args: --from ${{ steps.base.outputs.lock }}

      - name: Gate on it
        if: steps.base.outputs.lock != ''
        uses: f0rknturkoglu/agentchecksum@v0.1
        with:
          command: check
          args: --from ${{ steps.base.outputs.lock }} --no-probes --fail-on-drift
```

`diff --from` reports and always exits `0`, so it is the step that fills the job log with what moved;
`check --from … --fail-on-drift` is the step that can fail the build. When the base revision has no
lockfile — a first pull request into a repository that has never been snapshotted — there is nothing
to compare, and the job says so rather than treating the absence as a failure.

Add the model half by dropping `--no-probes` and providing `[model]`, if the runner can reach an
endpoint (see below). Two flags are worth knowing here:

- `--from` is accepted by every subcommand, but it only means something for `diff` and `check`: it
  replaces the lockfile the comparison starts from.
- `--accept` is refused together with `--from`. That is the CLI protecting the baseline: scores
  captured while the dependency state has moved would be attributed to the wrong revision, and a PR
  gate must never be able to write a baseline by accident. Using both is a usage error, exit `2`.

## Capturing the behavioral baseline

A behavioral baseline is a **reviewed contract** — "this is the behavior we accepted" — not a cache.
Record it in a controlled job, on a known revision, with a model endpoint you trust, and commit the
result through a pull request:

```yaml
name: record-baseline

on:
  workflow_dispatch:

jobs:
  accept:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
        with:
          fetch-depth: 0

      # The lockfile in the tree has to describe the agent being sampled: `--accept`
      # refuses to record anything while the dependency state has moved, and a dirty
      # tree would attribute the numbers to a revision nobody can check out.
      - name: Refresh the lockfile
        uses: f0rknturkoglu/agentchecksum@v0.1
        with:
          command: snapshot

      - name: Acceptance runs on a committed dependency state
        run: git diff --exit-code -- agentchecksum.lock

      - name: Sample the agent and record the baseline
        env:
          # Only matters if your endpoint reads a credential from the environment;
          # AgentChecksum itself sends none (see docs/security.md).
          MODEL_ENDPOINT_TOKEN: ${{ secrets.MODEL_ENDPOINT_TOKEN }}
        uses: f0rknturkoglu/agentchecksum@v0.1
        with:
          command: check
          args: --accept --repeat 3

      - name: Publish the baseline for review
        uses: actions/upload-artifact@v4
        with:
          name: behavioral-baseline
          path: .agentchecksum/baseline.json
          # `--accept` writes nothing when the run failed its own policy, so a missing
          # file is information: the job should fail rather than quietly publish nothing.
          if-no-files-found: error
```

Why a separate job rather than a step on every PR:

- **`--accept` writes the baseline only when the run is acceptable.** It exits `2` and writes nothing
  while the dependency state has moved, and it warns and writes nothing when a policy constraint
  failed. Both refusals are the feature: a baseline records accepted behavior, not observed behavior.
- **The baseline is the thing that makes `max_drop` meaningful.** Accepting a baseline on every run
  would redefine "the behavior we accepted" to "the behavior we just saw", and a regression would be
  absorbed into the baseline the moment it appeared.
- **`--repeat 3` is a deliberate choice, not a tuning knob.** An expectation that held once is a coin
  flip; a baseline that records one sample per probe is a baseline that records coin flips.

Review the diff before merging: a baseline records counts, digests and yardstick digests, so it never
contains prompts, model output or tool arguments — but a change in its numbers is still a change in
what the gate accepts. Nothing is written unless you commit it.

## Exit codes in a job

| Code | Meaning for the job | What to do |
|---|---|---|
| `0` | Completed, no blocking verdict. `diff` uses it whenever the comparison ran, even at CRITICAL risk; `check` uses it for drift unless `--fail-on-drift` asked for a gate. | Nothing. |
| `1` | Gate failure: a metric policy failed, or a change was explicitly gated (`--fail-on-drift`, `--fail-on-risk`, `[policy] fail_on_risk`). | Read the report: `Policy failures` and `Failing probes` name the reason. |
| `2` | Usage error: a rejected flag combination, or one with two readings (`--accept` with `--from`, `--accept` with `--trace`). Clap's parse errors use the same code. | Fix the workflow, not the agent. |
| `3` | Runtime failure: config, discovery, network, an invalid probe, an unsupported schema, or **evidence that cannot be trusted**. AgentChecksum could not honestly produce the requested verdict. | Fix the run: an unreachable endpoint, a server that failed discovery, a probe that does not resolve. It is never `PASS`, and never "the test failed". |

The action fails the step for any non-zero code, so a job does not need to branch on these — but the
distinction matters when you read a failure. Exit `3` is "I could not answer", exit `1` is "the
answer is no". Treating them alike is how a gate starts being ignored: a flaky endpoint would look
exactly like a regression.

## Machine-readable output for downstream tooling

`--format json` writes one JSON document to stdout and nothing else; diagnostics go to stderr. A
runtime failure writes its diagnostic to stderr and leaves stdout empty, so a consumer never has to
distinguish "failed" from "printed nothing".

```bash
agentchecksum check --format json > report.json
```

| Command | Top-level keys |
|---|---|
| `snapshot` | `status`, `lock_version`, `agent_checksum`, `dependency_count`, `warnings` |
| `diff` | `status`, `changed`, `overall_risk`, `baseline_checksum`, `current_checksum`, `changes` |
| `check` | `status`, `agent_checksum`, `baseline_checksum`, `dependency`, `behavior`, `error` |
| `inspect probes` | `status`, `probe_suite_digest`, `probes` |

`check`'s fields are always present — `null` or `[]` where there is no answer — so a consumer never
has to tell a missing key from an absent result. `status` is one of `pass`, `drift`, `regression`
or `error`; `behavior.metrics` is an array of rows, each with `metric`, `baseline`, `current`,
`verdict` and the `policy` constraints that applied:

```bash
# Which metrics failed, and against what
agentchecksum check --format json | jq '.behavior.metrics[] | select(.verdict == "fail")'

# Overall risk and what moved
agentchecksum diff --format json | jq '{overall_risk, changes: [.changes[].id]}'

# Annotate instead of failing (a locally or self-installed CLI, not the action)
agentchecksum check --format json > report.json || true
jq -r '.behavior.metrics[] | select(.verdict == "fail")
       | "::error::\(.metric) failed its policy"' report.json
```

The action publishes `status` as an output when `args` contains `--format json`, which is the easiest
way to make a later step conditional on the verdict without parsing stdout yourself.

## Practical notes

- **MCP servers must be installable in the job.** A `stdio` server is executed directly with its
  configured argument vector, so `npx`, `uvx` (or whatever `command` names) has to exist on the
  runner. A server that cannot start fails the run with exit `3`; it is never silently skipped.
  Discovery never calls a tool, so running it in CI cannot trigger a tool's side effects.
- **A job with no model endpoint still has a gate.** Use `--no-probes`, or run a model in the job:
  the [README](../README.md#in-ci) shows an Ollama-in-the-job pattern, which works because
  AgentChecksum sends no credential of its own.
- **Credentials.** An endpoint URL carrying credentials is rejected, and the runner sends no
  authentication header, so configure an endpoint your job can reach without one. Values declared in
  `[[mcp.servers]].env` are passed to the child process, never fingerprinted, and redacted out of
  everything you can see — see [security.md](security.md).
- **Time and cost are predictable.** One sample is one request, with no retries and a two-minute
  per-sample timeout. Total requests are the sum of the probes' effective `repeat`; `--jobs <n>`
  bounds how many probes are in flight and never changes the report. `--repeat <n>` overrides the
  count for a single run.
- **`check` never writes the lockfile.** It writes `.agentchecksum/runs/` and `.agentchecksum/cache/`
  (machine-local, gitignored) and, only with `--accept`, `.agentchecksum/baseline.json`. That is what
  makes it safe as a read-only CI step.
- **Reuse captured evidence deliberately.** `check --trace <path>` scores a recorded trace or run
  artifact with no endpoint at all, but it is bound to the agent, tool catalog, probe suite and runner
  it describes: evidence captured before a dependency change is refused with exit `3`. Within one job,
  the cache makes a re-run cheap; `--refresh` ignores it and samples every probe again.
- **Watch your `RUST_LOG`.** Diagnostics go to stderr through `tracing` and default to `warn`. If a
  step looks silent, `RUST_LOG=debug` is the lever; with `--format json` stdout stays clean either way.

## Related

- [getting-started.md](getting-started.md) — the walkthrough, and what to commit.
- [configuration.md](configuration.md) — `[model]`, `[[mcp.servers]]` and `[policy]`.
- [probes.md](probes.md) — the probe suite the gate scores.
- [security.md](security.md) — what a CI job actually executes.
