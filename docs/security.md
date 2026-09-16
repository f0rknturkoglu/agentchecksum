# Security and the trust model

This page states, as plainly as it can, what AgentChecksum runs, what it reads, what it writes, what
leaves the machine, and what it refuses to claim. It is written for someone deciding whether to run
it in CI against an untrusted pull request, or against a production agent's configuration.

The short version:

- AgentChecksum reads, parses, normalizes, hashes and compares. It does not import or execute the
  project it inspects.
- It starts exactly two kinds of process: the MCP server **you** configured, and — only when a run is
  *captured* — an HTTP request to the model endpoint **you** configured.
- **No tool the model requests is ever executed.** There is no `tools/call`, no sandbox and no MCP
  request on the behavioral path.
- The configuration file is the trust boundary: that is why it is committed and reviewed like code.

## What is executed

| Runs | When | How |
|---|---|---|
| A configured `stdio` MCP server | `snapshot`, `diff`, `check` | The configured `command` is executed **directly, never through a shell**, with the configured argument vector. `env` values you declared are given to the child. |
| A configured `streamable-http` MCP server | `snapshot`, `diff`, `check` | An `http`/`https` request to the configured URL. Credentials, query strings and fragments in the URL are rejected rather than stripped; redirects are not followed. |
| The model endpoint (`ollama` metadata) | `snapshot`, `diff`, `check` | An HTTP request to the configured endpoint's native API, to read the model digest, template, capabilities and reported parameters. |
| The model endpoint (samples) | `check`, when it captures | One OpenAI-compatible `/v1/chat/completions` request per sample. No retries, a two-minute per-sample timeout. |

`inspect probes` starts nothing at all: it reads the configuration, the probe suite and the committed
lockfile's tool catalog, with no MCP session and no endpoint. That is deliberate — it is the view you
need when a suite will not load, and a server being down must not change what it says the probes are.

There is nothing else. No plugin system, no hooks, no build scripts, no shell, no sandbox, no
sidecar, and no code from your repository is executed — prompts, probe files, schemas and the
lockfile are read as data.

A `stdio` server is a program you asked for, running with your user's privileges and whatever
environment you gave it, so **the server you configure is part of your trust boundary**. That is true
of any MCP client; what AgentChecksum adds is that it never hands that process anything else to do.

## What is never executed

**No tool the model requests is ever executed.** The behavioral runner asks the endpoint what the
model would like to call, records the answer as evidence, and stops. There is no `tools/call`, no MCP
request, and no sandbox anywhere on that path: the recorded tool decisions *are* the measurement.

Two consequences worth stating, because they are the reason the design is shaped this way:

- A `check` run cannot exercise the side effects a tool call would have. A probe that asserts
  `expect_tool = "delete_repository"` measures whether the model *chose* to delete a repository, and
  nothing is deleted.
- Discovery never calls a tool either. It connects, asks the server what it declares, and closes.

The flip side is a limitation, not a guarantee: AgentChecksum cannot tell you what a tool would have
done. It measures decisions, and only decisions.

## What is read

| Input | Contents |
|---|---|
| `agentchecksum.toml` | The declaration: model, prompts, MCP servers, probes, policy |
| Prompt files | Text, as UTF-8 |
| Probe files | TOML, strictly parsed |
| Output schemas | Self-contained JSON Schema, compiled locally |
| `agentchecksum.lock` | The committed fingerprint |
| `.agentchecksum/baseline.json` | The accepted behavior |
| A `--trace` path | A recorded trace or run artifact, validated against the current run before it is scored |
| From an MCP server | Server identity (protocol era, negotiated version, supported versions, server info, declared capabilities), instructions, and the tool list: names, descriptions, input/output schemas, annotation capabilities |
| From the model endpoint | The chat completion it returned for a sample (Ollama metadata during discovery) |

Two boundaries are enforced on what is read:

- **A schema is never resolved over the network.** `jsonschema` is built with HTTP and file
  resolution disabled, so an output schema (or a tool input schema) that references an external
  resource is **refused**, not fetched. Internal `#/…` references work normally. A probe must never
  become a way to reach the network.
- **A partial tool catalog is refused, never truncated.** The bounds on discovery — timeouts, page
  count, tool count, schema size and nesting depth — fail the run rather than return an inventory that
  understates what a server declares. Discovery is fail-closed: one server that cannot be fully
  discovered fails the command and nothing is written.

## What is stored, and where

| Path | Committed? | What it holds |
|---|---|---|
| `agentchecksum.lock` | **yes** | For each dependency, per-facet digests: prompt content/shape digests, tool description digests, server instructions digests, and the normalized payloads for model identity and parameters, MCP server identity, and tool input/output schemas and capability tokens. |
| `.agentchecksum/baseline.json` | **yes** | Counts, scores, and the digests needed to tell whether the baseline still describes the same test: agent checksum, probe-suite digest, runner contract, per-metric and per-probe `passed`/`total`, and the tool input-schema yardstick digests. |
| `.agentchecksum/cache/<key>.json` | **no** (gitignored) | One cached sample per file: **raw model output** — tool calls with their arguments, and the final message — plus the inputs it was keyed by. |
| `.agentchecksum/runs/<digest>.json` | **no** (gitignored) | A run artifact: the traces (raw model output), the evaluation of those traces, and the aggregate counts. Named after the SHA-256 of its own canonical form. |

The distinction is deliberate and is the reason the two machine-local directories are gitignored in
this repository's `.gitignore` with a comment saying so:

- The **committed** artifacts are contracts. A baseline holds no prompts, no model output and no tool
  arguments — only counts and digests — so it can be reviewed in a pull request without leaking
  anything the run saw.
- The **machine-local** artifacts are evidence, and evidence contains raw model output. Treat
  `.agentchecksum/cache/` and `.agentchecksum/runs/` as you would treat any log of model traffic:
  keep them out of version control and out of anything they were not intended for. They are addressed
  and verified by content, so deleting them costs time and nothing else.

One thing not to put in the configuration: `[model].params` is recorded **as configured** in the
lockfile and participates in the checksum. It is for inference parameters, not credentials.

## What leaves the machine

Exactly the requests listed under [what is executed](#what-is-executed), and nothing else:

- to the MCP servers you configured (stdio: a child process; streamable-http: your endpoint);
- to the model endpoint's metadata API during discovery (the `ollama` provider only);
- to the model endpoint during capture.

No telemetry, no analytics, no crash reporting, no update check, no remote configuration, and no
network access for schema resolution or for reading a recorded trace. `check --trace` evaluates
recorded evidence with no request to the model at all, and `check --no-probes`/`--diff-only` gates on
the dependency half without contacting a model endpoint.

A replay still needs the current agent to be *describable*: `check` fingerprints the dependency state
it compares against, and for a provider whose identity requires a live server (Ollama's model digest,
for example) that fingerprinting is the one thing a replay cannot do offline. That is discovery, not
replay, and it is the same requirement `snapshot` has.

## `env` values: passed, never fingerprinted, never leaked

Values declared in `[[mcp.servers]].env` are connection material. They are:

- passed to the child process;
- **never fingerprinted**, so they never influence a checksum and never appear in the lockfile;
- **redacted out of everything you can see** — stdout, stderr, warnings, errors and tracing —
  including text a server echoes back. There is no length threshold: a three-character token is
  treated like any other, and longer values are replaced before shorter ones so a prefix cannot
  survive beside its longer sibling.

The boundary is enforced in the other direction too, because a server is handed its environment and
can echo it back. If a server reflects a configured value into anything AgentChecksum would
fingerprint — its own name or version, its instructions, a tool name, a tool description, the protocol
revisions it reports, or any key or string inside a schema — then discovery **fails** rather than
describing it. The declaration is not rewritten and the value is not blanked out inside the contract:
a fingerprint taken over an edited declaration would describe a contract the server never declared.
Opaque fields AgentChecksum never reads, such as tool `_meta`, are not scanned either — unread data
cannot reach a fingerprint.

What this is, and is not:

- It **is** a guarantee about the values you configured. Rotating a token that does not change what
  the server declares produces the same checksum.
- It is **not** a content-aware secret scanner. It cannot recognize a secret it was never given, and
  it makes no attempt to guess.
- When credentials **do** change the declared contract — a narrower set of authorized tools, for
  example — that is a real dependency change, and it is reported as one.

Endpoints get the same treatment in the opposite direction: a URL containing credentials is
**rejected**, never stripped, because the lockfile is committed and a sanitizer that parses a secret
before discarding it is a habit worth not having. Diagnostics name the component that is wrong, never
the URL itself.

## Evidence that cannot be trusted is refused

A verdict is only as good as the evidence behind it, so AgentChecksum refuses evidence it cannot
place:

- `check --trace <path>` scores a recording only when its agent checksum, tool catalog digest, runner
  and version, and probe name/digest/sample count all match the current run — and, for a run artifact,
  its probe-suite digest. Anything else exits `3` with the fact that disagrees named. It is never
  silently treated as drift, as a regression, or as a cache miss and re-captured, because scoring one
  agent's behavior as another's would be worse than no answer.
- A cached sample is only a hit when the inputs it recorded agree with the inputs this run has; a file
  moved, copied or hand-edited into the wrong place is an error rather than a sample from a different
  experiment. A malformed entry is an error, never a miss and never a `PASS`.
- A run artifact is named after its own content, so a hand-edited artifact is refused before anything
  evaluates it.
- A baseline from a different probe suite or runner contract is **non-comparable**: it is reported as
  drift with the reason, not compared. See
  [configuration.md](configuration.md#when-max_drop-is-not-evaluated).

Running AgentChecksum against an untrusted pull request is a supported use: nothing in the repository
under inspection is executed, `check` never writes the lockfile, and `--accept` — the one flag that
writes a committed artifact — writes nothing unless the dependency state is unchanged and no policy
constraint failed.

## What is not a security boundary

Stated so nobody has to discover it:

- **Risk levels are a heuristic.** They encode "how likely is this change to alter agent behavior",
  not "is this change dangerous". The tool hashes and compares; it does not interpret intent.
- **MCP annotations are untrusted declarations.** A tool's `read-only`/`destructive` tokens are what
  *the server says about itself*, and the MCP specification instructs clients to treat annotations as
  untrusted unless the server is trusted. A CRITICAL finding on a newly added destructive tool is a
  statement about a declaration, not proof of behavior.
- **The configuration file is trusted input.** It names a command to execute and an endpoint to
  contact. Review it the way you review CI configuration.
- **No authentication for remote endpoints.** AgentChecksum sends no credential of its own; a
  credential belongs in the environment of the process that runs your agent.
- **Capture is not deterministic.** Sampling is statistical — one sample is one request — while
  evaluation over a recording is deterministic. Trace *capture* is not bit-reproducible; trace
  *evaluation* is.
- **No LLM judge, no regex matchers, no tool-result or multi-turn evaluation.** These are limitations
  rather than hidden features, and the probes you write are the yardstick.

## Reporting a vulnerability

See [SECURITY.md](../SECURITY.md) for how to report one. It is the right place to look, and the trust
model above is what to measure a report against.
