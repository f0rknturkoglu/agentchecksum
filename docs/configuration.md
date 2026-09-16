# Configuration reference

Everything AgentChecksum knows about your agent is declared in one file, `agentchecksum.toml`. It is
a **source contract**, not a build artifact: it is committed, reviewed like code, and the lockfile is
a function of it plus the external systems it names.

- Default path: `./agentchecksum.toml`, overridable with the global `--config <path>`.
- Parsing is **strict** at every level: an unknown key is a rejection, not a warning. A silently
  ignored typo would mean AgentChecksum fingerprinted something other than what you declared.
- Project-relative paths inside the file (prompt paths, the probe directory, output-schema paths) are
  resolved against the **config file's directory**, not your shell's working directory.
- Those paths must be relative, must not contain `..`, and must not contain a backslash. An absolute
  path would make the lockfile depend on the machine that produced it.

```toml
version = 1

[agent]
name = "research-agent"

[[prompts]]
path = "prompts/system.md"

[model]
provider = "ollama"
id = "qwen3:8b"
endpoint = "http://localhost:11434"
params = { temperature = 0.0, seed = 42 }

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "placeholder-token" }

[probes]
path = "probes"
repeat = 3

[policy]
fail_on_risk = "critical"

[policy.metrics.tool_selection]
min = 0.95
```

| Top-level key | Required | Purpose |
|---|---|---|
| `version` | yes | Config format version. This build supports `1`; anything else is refused before anything is fingerprinted. |
| `agent` | yes | Names the agent. |
| `model` | no | The model the behavioral runner samples. |
| `prompts` | no | Prompt files that belong to the fingerprint. |
| `mcp` | no | MCP servers whose identity and tool contracts are fingerprinted. |
| `probes` | no | Where the probe suite lives, and the default sampling count. |
| `policy` | no | Thresholds and the risk gate. Absent means report-only. |

## `[agent]`

```toml
[agent]
name = "research-agent"
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | A human-readable name for the agent. |

The name identifies the project to a reader; it is **not** part of the checksum. Renaming the agent
does not change `agentchecksum.lock`, does not make the committed baseline non-comparable, and does
not turn a `check` into drift. Dependencies are identified by the facets of what they are, not by what
the project calls them.

## `[[prompts]]`

```toml
[[prompts]]
path = "prompts/system.md"

[[prompts]]
path = "prompts/policies/refunds.md"
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | Project-relative path to a UTF-8 prompt file. |

Each declaration becomes a dependency `prompt:<normalized path>`, where the path is normalized to
forward slashes (`./prompts/a.md` and `prompts/a.md` are one identity). Two declarations that
normalize to the same path are rejected as a collision rather than silently collapsed.

A prompt carries two facets:

| Facet | What it digests |
|---|---|
| `content` | The normalized text of the file. |
| `shape` | The same text with whitespace collapsed. |

`shape` exists so a reflow (LOW) is distinguishable from a rewrite (MEDIUM) deterministically, with no
LLM in the loop. A file that is not UTF-8, or that cannot be read, fails `snapshot` rather than being
skipped.

When the behavioral runner assembles the system message, it uses every configured prompt, **ordered
by dependency id** rather than by declaration order, separated by a blank line. Ordering by id is
deliberate: the prompt list has no meaningful order in the fingerprint, so letting TOML order decide
what the model reads would let two runs over identical dependency state send different messages. With
no prompts configured, no system message is sent at all — not an empty one.

## `[model]`

```toml
[model]
provider = "ollama"                     # ollama | openai-compatible
id = "qwen3:8b"
endpoint = "http://localhost:11434"
params = { temperature = 0.0, seed = 42 }
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `provider` | string | yes | `ollama` or `openai-compatible`. Any other value is refused. |
| `id` | string | yes | The model name the provider knows. |
| `endpoint` | string | yes for both providers | The provider's base URL. |
| `params` | table | no | Inference parameters. Free-form; hashed as declared. |

Despite being optional to the config format, `[model]` is only *needed* by `check` when it samples:
`snapshot` and `diff` fingerprint an agent with no model at all, and `check --diff-only` /
`check --no-probes` gate on the dependency half without one.

`endpoint` is required for **both** providers, and both providers reject it unless it is a usable
base URL: `http` or `https` only, with no embedded credentials, no query string, and no fragment.
Every unsupported component is rejected rather than stripped — a query string can select a different
deployment behind the same host, so removing it would fingerprint a server you did not configure. A
trailing slash is normalized away, so `http://host/v1` and `http://host/v1/` are one identity.

The `ollama` endpoint is the **native API** address (`http://localhost:11434`); it is used both for
model metadata during discovery and as the base the behavioral runner appends
`/v1/chat/completions` to. A URL that already names the completion path is refused rather than
guessed at, because accepting it would produce a path no endpoint serves.

### `params`

`params` is a free-form table of inference parameters, written exactly as the provider expects to
receive them:

```toml
params = { temperature = 0.0, seed = 42, top_p = 0.9 }
```

Two *sources* of parameters are fingerprinted, and they are deliberately not merged:

- **`configured`** — the values in this file. These are the values the runner sends.
- **`reported`** — the model's own defaults, as the provider reports them (`ollama` only).

Merging them would require modelling each provider's override semantics, and a wrong merge is worse
than a conservative one: it would hide a change you need to see. The price of not merging is the
opposite error — a `reported` change that a configured value overrides still reads as a dependency
change, at MEDIUM risk. A model with no parameters at all carries no `params` facet, and an endpoint
that reports an empty `parameters` block describes the same state as one that omits it.

### What is fingerprinted

The model dependency is `model:<provider>/<id>`, with these facets:

| Facet | `ollama` | `openai-compatible` |
|---|---|---|
| `identity` | provider, id, `digest` (`sha256:…`), family, parameter size, quantization level | provider, id, canonicalized `endpoint` |
| `params` | `configured` and `reported` | `configured` |
| `template` | the model's chat template, normalized | — |
| `capabilities` | the capability list the server reports | — |

Digests exclude what is not behavior: timestamps, vendor metadata (`modified_at`, `size`, `license`),
and discovery order.

### The `openai-compatible` caveat

The `openai-compatible` provider exposes **no content digest**. Two hosts serving a model with the
same name can be entirely different backends or weights, so a model swapped behind the same endpoint
cannot be detected by digest. AgentChecksum does the only honest thing available: the endpoint becomes
part of the model's identity, and `snapshot` warns about it.

```text
warning: model `demo-model`: the `openai-compatible` provider exposes no content digest, so a model
swapped behind the same endpoint cannot be detected; the endpoint is part of the identity instead
```

The warning is a statement about what the fingerprint can promise, not a misconfiguration to silence.
The practical consequence: moving to another host is a dependency change (HIGH — endpoint identity
changed), and swapping the weights behind one endpoint is invisible. If you need the swap to be
visible, use `ollama`, which reports a content digest.

## `[[mcp.servers]]`

An MCP server is fingerprinted where it is declared. Two transports are supported, and neither
requires the server to be installed for `check --diff-only` to work.

### `stdio`

```toml
[[mcp.servers]]
name = "github"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "placeholder-token" }
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | The alias. See [alias rules](#the-alias). |
| `transport` | string | yes | `stdio`. |
| `command` | string | yes | Executed **directly**, never through a shell, so it must be an executable and every argument its own array entry. |
| `args` | array of strings | no | The argument vector, passed verbatim. |
| `env` | table | no | Environment values given to the child process. |

### `streamable-http`

```toml
[[mcp.servers]]
name = "remote"
transport = "streamable-http"
url = "https://example.com/mcp"
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | The alias. |
| `transport` | string | yes | `streamable-http`. |
| `url` | string | yes | `http` or `https`, no credentials, no query string, no fragment. Redirects are not followed. |

The transport is an enum: a value outside `stdio` and `streamable-http` fails to parse. The two
shapes are strictly separated, and a mixture is rejected rather than ignored — `stdio` with a `url`,
`stdio` without a `command`, `streamable-http` with a `command`, with `args`, or with `env` are each a
configuration error. The complaint names the field and never repeats the URL, because an endpoint is
the one configuration value that can carry a credential.

### The alias

`name` is a **namespace, not a display name**. It prefixes every dependency id the server produces:

```text
mcp:github
tool:github.search_repositories
tool:github.create_issue
```

The grammar is `[A-Za-z0-9_-]+`, at most 64 characters, and unique across servers. A dot is
deliberately excluded: `tool:<alias>.<tool>` splits on the first dot, so an alias containing one could
let two different servers claim one tool identity. The rules are checked in that order — grammar
first, then uniqueness — so a name that could never be an identity is reported as such rather than as
a duplicate.

Renaming an alias is an identity change: the old ids disappear from the lockfile and the new ones
appear, which is exactly what it is. The remote `serverInfo.name` is *not* the identity, because the
MCP specification does not guarantee it is unique across servers; it is a facet of the server's
identity, not the id.

### `env`: passed, never fingerprinted

`env` values are handed to the child process and appear nowhere else. They are excluded from the
fingerprint, excluded from the lockfile, and redacted out of everything you can see — stdout, stderr,
warnings, errors and tracing — including text a server echoes back. There is no length threshold: a
three-character token is treated like any other, and longer values are replaced first so that a
prefix cannot survive beside its longer sibling.

Two consequences worth stating before you rely on them:

- Rotating a token that does not change what the server declares produces the **same** checksum.
- If credentials *do* change the declared contract — a narrower set of authorized tools, for example
  — that is a real dependency change and is reported as one.

The guarantee is enforced in the other direction too. A server is handed its environment and can echo
it back, so if a server reflects a configured value into anything AgentChecksum would fingerprint —
its own name or version, its instructions, a tool name, a tool description, the protocol revisions it
reports, or any key or string inside a schema — discovery **fails** rather than describing it. The
declaration is not rewritten and the value is not blanked out inside the contract, because a
fingerprint taken over an edited declaration would describe a contract the server never declared.
This is a guarantee about the values you configured, not a claim to recognize secrets AgentChecksum
was never given. See [security.md](security.md) for the full trust model.

### What discovery reads, and what it refuses

Discovery connects, asks the server what it declares, and closes.

- It reads the server identity (protocol era, negotiated version, supported versions, server info,
  declared capabilities) and the tool list (names, descriptions, input schemas, output schemas,
  annotation capabilities).
- **It never calls a tool.** There is no `tools/call` anywhere on this path, so a snapshot cannot
  exercise the side effects a tool call would have.
- A **partial** tool catalog is refused rather than fingerprinted: a lockfile that describes a partial
  server is worse than no lockfile. The same applies to the bounds on discovery — timeouts, page
  count, tool count, schema size and nesting depth — exceeding one fails the run rather than
  truncating the inventory.
- One server that cannot be fully discovered fails the command, nothing is written, and a duplicate
  dependency id stops the run before a lockfile exists.

Session establishment is a two-step policy, and only the server can trigger the second step:
AgentChecksum asks for the stateless protocol first, and falls back to the session handshake only when
the server answers that it does not implement discovery. A slow server is a **failure**, never an old
one: timing is not evidence about a protocol era, and letting it decide one would mean identical
declarations fingerprinted differently from one run to the next.

Tool annotations are recorded as declared, folded into the effective tokens
(`read-only`/`write`, `destructive`/`non-destructive`, `idempotent`/`non-idempotent`,
`open-world`/`closed-world`). They are hints a server publishes about itself, not guarantees. What is
deliberately left *out* of the fingerprint is as settled: transport and session plumbing (PIDs, ports,
session ids, cache hints, timings), configured commands and environment variables, server stderr,
cosmetic metadata (`title`, `icons`), and opaque `_meta` and extension settings — their presence and
identifiers are reported, one aggregated warning per server, but never their values.

### Two servers, one tool name

Fingerprinting is fine with two servers exposing a tool with the same remote name: they are two
dependencies with two ids and two facet sets. The **behavioral runner** is not, and says so:

```text
Error: the behavioral runner cannot send the tool catalog to the model:
`tool:remote-docs.delete_repository` and `tool:tickets.delete_repository` both declare the tool name
`delete_repository`, and the chat-completions wire format carries one function per name; rename one of
them at its server
```

The reason is the wire format, not a policy choice: a chat-completions request carries one function per
name, so a catalog with a duplicate name cannot be sent to a model without silently dropping one of
them. That failure is exit `3` and happens before any sample is captured. The same duplication makes a
bare tool reference in a probe ambiguous — see [probes.md](probes.md#expect_tool).

## `[probes]`

```toml
[probes]
path = "probes"
repeat = 3
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `path` | string | `"probes"` | Project-relative directory of flat `*.toml` probe files. |
| `repeat` | integer | absent (effective `1`) | Samples per probe, `1`–`100`, unless the probe or `--repeat` overrides it. |

The probe directory must hold at least one `*.toml` file; subdirectories are not searched. `repeat`
here is the project default: a probe's own `repeat` overrides it, and `check --repeat <n>` overrides
both. A sample count is never clamped — a run that quietly sampled once instead of never is a run with
a different meaning — so an out-of-range value is a configuration error. See [probes.md](probes.md)
for the file format and for what sampling means.

## `[policy]`

```toml
[policy]
fail_on_risk = "critical"

[policy.metrics.tool_selection]
min = 0.95

[policy.metrics.argument_validity]
max_drop = 0.05

[policy.metrics.forbidden_tool_usage]
min = 1.0
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `fail_on_risk` | `none` \| `low` \| `medium` \| `high` \| `critical` | absent = report only | Fail when the dependency diff's overall risk reaches this level. |
| `[policy.metrics.<metric>]` | table | absent = no constraint | Thresholds for one behavioral metric. |

With no `[policy]` section at all, AgentChecksum reports and never fails on a change: a changed
dependency is a fact, and whether it should block a merge is your decision. `fail_on_risk = "none"`
is a level rather than the absence of one — it means *fail on any change at all*.

`check --fail-on-risk <level>` overrides the configured level for one run without rewriting the file.

### The metric vocabulary

Every metric points the same way: **1.0 is good**. That is what lets one small vocabulary —
`min`, `max`, `max_drop` — describe all six, and it is why the report never shows a percentage with
no denominator behind it. The full set, and which expectation feeds each, is in
[probes.md](probes.md#the-six-metrics); the names are:

`tool_selection`, `argument_validity`, `argument_expectation`, `forbidden_tool_usage`,
`tool_restraint`, `structured_output_validity`.

A metric name that does not exist in that list is a configuration error, not an unused policy. A typo
that silently gated nothing would be the worst outcome available.

### Constraints

| Key | Meaning |
|---|---|
| `min` | Absolute floor on this run's score. A floor is a demand for evidence: if no sample made the metric applicable, the floor cannot be met, and that is a failure — passing it because nothing was measured would be the most flattering possible reading of an absent result. |
| `max` | Absolute ceiling on this run's score. A ceiling needs no evidence to hold: nothing measured cannot exceed it. For a metric you want to keep low, this is the key — the metric itself is still scored so that 1.0 is good. |
| `max_drop` | Allowed fall **relative to the committed behavioral baseline**. Measured as `baseline − current` and evaluated only against a *comparable* baseline. |

All values must be finite and within `0.0..=1.0`. `min > max` is refused, because no score could
satisfy both. Every declared constraint must pass; none excuses another — a score can clear `min` and
still fail `max_drop`, and that is a failure.

**A constraint on a metric nothing measures fails.** A floor is a demand for evidence, so
`min = 0.95` on `tool_selection` fails when no probe declares `expect_tool`:

```text
Policy failures:
  tool_selection: no sample made this metric applicable, so its minimum cannot be met
```

That is deliberate — "we never asked" is not "it passed" — and it is the most common reason a policy
looks wrong on a first run. Declare constraints only for the metrics your probes feed, and use
[probes.md](probes.md#the-six-metrics) to see which expectation feeds which.

### When `max_drop` is not evaluated

`max_drop` needs a baseline that describes the same experiment. It is evaluated only when the
committed baseline was recorded under:

- the same **probe suite digest** (the same assertions), and
- the same **runner contract** (the same capture rules).

Otherwise the relative comparison cannot honestly be made. It is **not** a regression and not a
silent pass: `max_drop` is left undecided, and the check reports **drift** with the reason — "no
behavioral baseline exists", "behavior probe suite changed", or "the behavioral baseline was recorded
by a different runner contract". Absolute `min`/`max` thresholds are applied regardless, because a
floor this run misses is a fact about this run. That is also why drift exits `0` unless you pass
`--fail-on-drift`.

## A complete example

This is one valid configuration using every section. It is deliberately a project with a local stdio
MCP server, an Ollama model, two prompts, three probes and a full policy:

```toml
version = 1

[agent]
name = "triage-agent"

[[prompts]]
path = "prompts/system.md"

[[prompts]]
path = "prompts/policies/refunds.md"

[model]
provider = "ollama"
id = "qwen3:8b"
endpoint = "http://localhost:11434"
params = { temperature = 0.0, seed = 42 }

[[mcp.servers]]
name = "tickets"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-example"]
env = { EXAMPLE_TOKEN = "placeholder-token" }

[[mcp.servers]]
name = "remote-docs"
transport = "streamable-http"
url = "https://example.com/mcp"

[probes]
path = "probes"
repeat = 3

[policy]
fail_on_risk = "high"

[policy.metrics.tool_selection]
min = 0.95

[policy.metrics.argument_validity]
min = 0.9
max_drop = 0.05

[policy.metrics.argument_expectation]
max_drop = 0.1

[policy.metrics.forbidden_tool_usage]
min = 1.0

[policy.metrics.tool_restraint]
min = 1.0

[policy.metrics.structured_output_validity]
min = 0.9
```

One note on the third metric, because it is the mistake this project sees most often: there is no
"forbidden tool usage rate" to minimize. `forbidden_tool_usage` measures *restraint*, so `1.0` means
no forbidden tool was called, and the constraint is `min = 1.0` — not `max = 0.0`.

## Where the state lives

| Path | Written by | Committed? |
|---|---|---|
| `agentchecksum.lock` | `snapshot` | yes |
| `.agentchecksum/baseline.json` | `check --accept` | yes |
| `.agentchecksum/cache/` | `check` (capture) | no — machine-local |
| `.agentchecksum/runs/` | `check` (capture) | no — machine-local |

`check` never writes the lockfile, which is what makes it safe as a read-only CI step, and `snapshot`
is the only command that writes the dependency baseline. `check --accept` writes the behavioral
baseline only when the dependency state has not moved **and** no policy constraint failed — a baseline
records accepted behavior, not observed behavior. The machine-local directories are addressed and
verified by content, so deleting them costs time and nothing else. See
[getting-started.md](getting-started.md#what-to-commit-and-what-never-to-commit) for the full table
and [security.md](security.md) for what those artifacts contain.
