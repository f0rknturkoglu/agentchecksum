# Probes reference

A **behavior probe** is one small, deterministic-first scenario: a prompt, and a declaration of what a
correct answer looks like. `check` samples your model with the probe's prompt, scores what came back
against the declarations, and folds the scores into metrics — which is what the [Behavior
Gate](configuration.md#policy) then judges.

Probes are the *yardstick*, not the measurement. They are committed source, reviewed like code, and
the same files produce the same assertions on every machine.

```text
probes/                      one flat directory of *.toml files
├── 01-no-tools.toml
├── 02-search.toml
└── 03-structured.toml
```

| Property | Value |
|---|---|
| Directory | `[probes].path`, default `probes`, relative to the config file |
| Layout | flat; a subdirectory is a load error rather than a nested suite |
| Files read | `*.toml` only, sorted by project-relative path |
| Minimum | one file, and every file must declare at least one `[[probe]]` |
| Maximum | 64 files, 256 probes |

## File format

A probe file is an array of `[[probe]]` tables, and nothing else. Parsing is **strict**: an unknown
key is a rejection, not a feature to ignore — a silently ignored key would mean the run measured
something other than what the file asserts.

```toml
[[probe]]
name = "no-tools"
prompt = """
Answer from what you already know, without calling any tool: what is the capital of Portugal?
"""
expect_no_tool = true
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | The probe's durable identity. `[A-Za-z0-9][A-Za-z0-9._-]{0,63}` |
| `prompt` | string | yes | Non-empty after trimming, at most 64 KiB. Longer is refused, never truncated |
| `repeat` | integer | no | Samples for this probe, `1`–`100`. Overrides `[probes].repeat` |
| `expect_tool` | string | no | A tool reference that must be called |
| `expect_args` | table | no | JSON Pointer → matcher. Requires `expect_tool` |
| `forbid_tools` | array | no | Tool references that must not be called |
| `expect_no_tool` | boolean | no | `true` asserts that no tool may be called |
| `output_schema` | string | no | Project-relative path to a self-contained JSON Schema |

A probe must declare **at least one** of the five expectation keys — `expect_tool`, `expect_args`,
`forbid_tools`, `expect_no_tool`, `output_schema` — and a probe with an empty prompt, an out-of-range
repeat, an unusable argument path, or too many entries is a load error. The name grammar is narrow on
purpose: a probe name names a probe in a baseline, in a cache key and in a report, and a name that
needed quoting or normalizing in any of those places would be a name that means two things.

## The five expectation keys

### `expect_tool`

A tool reference that must be called at least once in the sample. This is the only expectation that
feeds `tool_selection`, and it is what `expect_args` attaches to.

```toml
expect_tool = "search_repositories"
```

A tool reference is either the canonical dependency id from the lockfile
(`tool:demo-tools.search_repositories`) or the bare remote name (`search_repositories`). Neither is
guessed at:

- A reference that matches nothing is an error naming the probe and the reference.
- A bare name that **two servers** expose is an error listing the candidates. Two servers' tools are
  different tools with different schemas, so picking one silently would evaluate arguments against the
  wrong contract. Use the canonical id to disambiguate.

There is no fuzzy matching and no case folding: a reference either names a tool or fails. And the
reverse case is a runtime failure rather than a resolution problem: if two configured servers declare
the *same* remote name, the catalog cannot be sent to the model at all — a chat-completions request
carries one function per name — so `check` exits `3` before capturing anything. See
[configuration.md](configuration.md#two-servers-one-tool-name).

### `expect_args`

One or more JSON Pointers mapped to matchers, judged against the arguments of a call to the
`expect_tool` tool. It needs `expect_tool` to say which tool, and declaring it without one is a load
error.

```toml
expect_tool = "search_repositories"
expect_args = { query = { contains = "postgres" }, "/per_page" = { one_of = [10, 20] } }
```

A sample satisfies `expect_args` when **some** call to the expected tool has arguments that satisfy
**every** declared matcher. At most 64 entries are allowed. Two spellings of one path — `query` and
`/query` — are a load error rather than a silent drop of one of them.

### `forbid_tools`

A list of tool references, none of which may be called. At least one entry is required if the key is
present: `forbid_tools = []` is a probe that forbids nothing, which is a typo rather than an
assertion. At most 64 entries.

```toml
expect_tool = "search_repositories"
forbid_tools = ["delete_repository", "tool:demo-tools.purge_repository"]
```

This feeds `forbidden_tool_usage`, which measures *restraint*: `1.0` means no forbidden tool was
called. It can be combined with `expect_tool` (call this one, never that one) but not with
`expect_no_tool`.

### `expect_no_tool`

`true` asserts that the sample called no tool at all, and feeds `tool_restraint`.

```toml
expect_no_tool = true
```

It cannot be combined with `expect_tool`, `expect_args` or `forbid_tools` — "no tool may be called"
and "this tool must be called" are contradictory, and a probe that declares both has two readings.
`expect_no_tool = false` asserts nothing and is refused; remove the key and declare what should happen
instead.

### `output_schema`

The sample's final message must be JSON that validates against a self-contained JSON Schema.

```toml
output_schema = "schemas/answer.json"
```

- The path is project-relative, resolved against the config's directory, with no `..` and no
  backslash, and the file must be at most 512 KiB.
- The schema must be **self-contained**: internal `#/…` references only. `jsonschema` runs with HTTP
  and file resolution disabled, so a schema that needs to fetch something is refused rather than
  resolved. A probe must never be a way to reach the network.
- The schema is compiled when the suite is loaded, not when a sample is scored: an unsupported schema
  stops the run before it spends model calls on samples nobody can score.
- There is no fence stripping and no repair. Text that needs unwrapping to parse is text that does not
  have the structure the probe declared.

The schema's identity is its **content**, so moving or reformatting the file does not make every
recorded run stale.

## Matchers

Exactly three operators, one per path. Anything richer — a regex, a comparison — would be a second
language inside the probe file.

| Matcher | Passes when |
|---|---|
| `{ equals = <value> }` | The resolved value is exactly equal to `<value>`. |
| `{ contains = <value> }` | The resolved value is a **string** containing `<value>` as a substring, or an **array** with an element equal to `<value>`. |
| `{ one_of = [<value>, …] }` | The resolved value equals one of the listed values. At least one value is required. |

```toml
expect_args = { query = { contains = "postgres" } }
```

```toml
expect_args = { "/options/per_page" = { equals = 20 } }
```

```toml
expect_args = { "/kind" = { one_of = ["issue", "pull_request"] } }
```

Three rules worth stating:

- **Exactly one operator per matcher.** Two operators on one path is a load error, not a
  first-one-wins. Deserializing into the matcher directly would let the parser pick the first variant
  it recognized and silently ignore the second, which is precisely the mistake the rule exists to
  catch.
- **A type mismatch is a failure, never an error.** A model that answers with a string where a number
  was expected has made a behavioral choice, and that is worth measuring. The metric records a failed
  check.
- **`contains` on an object is not supported.** "Contains this key" and "contains this value" are
  different questions, and answering the wrong one silently would be worse than failing.

## JSON Pointers

Arg paths are [RFC 6901](https://www.rfc-editor.org/rfc/rfc6901) JSON Pointers, with one affordance:

| Written | Means |
|---|---|
| `/options/per_page` | the `per_page` member of the `options` member of the arguments object |
| `query` | shorthand for `/query`: a single **top-level** property |

The shorthand is borrowed from the same `/` prefix rather than invented as a second path syntax, so
there is one escaping story rather than two:

- `~1` is a literal `/` and `~0` is a literal `~`. Any other use of `~` is malformed and rejected — the
  RFC defines no other escape, and guessing would resolve a different value.
- A shorthand key containing `/` or `~` is refused: it is the start of a deeper path or an escape, and
  the shorthand does not claim to express either.
- `a.b` is the literal key `a.b`, never two levels. A dotted path would need its own escaping rules
  beside RFC 6901's.
- An array index follows RFC 6901's grammar: digits only, no leading zeros, no `+`, and no `-`.
- A pointer that resolves to nothing **fails the matcher it belongs to**. It is an expectation
  failure, not an error, and nothing is defaulted — a defaulted argument is not an argument the model
  sent.

A path that is not a usable pointer at all is a probe validation error rather than an expectation
failure, because a typo in a path would otherwise read as a model regression.

## `repeat`, and what sampling means

`repeat` is how many samples one probe gets. One sample is one request to the model endpoint — no
retries of any kind, because a retry would change what `repeat = 2` means on a flaky endpoint.

```toml
# probes/02-search.toml
repeat = 3
```

The effective count is resolved in this order, first match wins:

1. `check --repeat <n>`
2. the probe's own `repeat`
3. `[probes].repeat`
4. otherwise `1`

All four values are in `1..=100`, and an out-of-range value is an error rather than a clamp: a run
that quietly sampled once instead of ten is a run with a different meaning. `--jobs <n>` does not
change the count; it changes how many probes are in flight at once, and the report is identical.

Sampling is **statistical** — one sample is one observation, and a model is not a deterministic
function. Evaluation over recorded evidence is **deterministic**: the same recording scores the same
way every time, which is what makes a replay reproducible. `repeat` is your lever on the first half: an
expectation that held in five of five is a measurement, and one that held once is a coin flip.

Two counting conventions follow from that, and both are visible in a report:

- Metrics are **exact counts** underneath (`passed`/`total`), not percentages. `9 / 10` survives an
  argument about rounding that `0.9` does not.
- A metric nothing measured has **no score** rather than a perfect one. A probe that calls no tool
  does not get a free `1.0` for `argument_validity`.

## The six metrics

Which metrics apply is decided by what the probes declare — you do not select metrics, because a
metric with no assertion behind it would be a number pretending to be evidence. Every metric points
the same way: **1.0 is good**.

| Metric | Feeds | Passes when |
|---|---|---|
| `tool_selection` | `expect_tool` | The expected tool was called at least once. |
| `argument_validity` | **automatic** | Every tool call in the sample has arguments that parse and validate against the tool's declared **input schema**. Applies only to samples that called a tool. |
| `argument_expectation` | `expect_args` | Some call to the expected tool satisfied every declared matcher. |
| `forbidden_tool_usage` | `forbid_tools` | No forbidden tool was called (restraint: `1.0` is good). |
| `tool_restraint` | `expect_no_tool` | No tool was called at all. |
| `structured_output_validity` | `output_schema` | The final message is JSON that validates against the schema. |

`argument_validity` is the one metric no key declares. It is measured against the tool input schemas
in the lockfile, the same contracts the diff tracks, and a baseline records their digests so that a
moved yardstick is visible instead of silent.

Which metrics a probe feeds is decided per sample and per metric. A report shows every metric that was
measured — and also any metric a policy constrains, even when nothing measured it, because a `min` on
an unmeasured metric is a failure waiting to be explained. See
[configuration.md](configuration.md#constraints).

## How a probe is identified

A probe's identity is its **name plus a digest** of what it asserts:

| In the digest | Not in the digest |
|---|---|
| The prompt text | The file it lives in |
| The **declared** repeat | The CLI's `--repeat` override |
| The expected tool's canonical id | The reference spelling used in the file |
| Every matcher (pointer → operator → value) | The order the matchers were written in |
| The forbidden tools' canonical ids, sorted and de-duplicated | Repetition in the `forbid_tools` list |
| `expect_no_tool` | |
| The **content** digest of the output schema | The schema's path |

Everything downstream is a pure function of that identity, which is what makes a recorded run,
a cache entry and a baseline comparable — or refusable.

- **The suite digest** folds every probe's name and digest, in name order. It is what a behavioral
  baseline records, and a baseline whose suite digest differs describes different assertions, so its
  scores cannot be compared: the check reports **drift** with the reason, and `max_drop` is not
  evaluated against it.
- **Cached samples** are keyed by probe digest, agent checksum, tool-catalog digest, runner version,
  effective parameters, and sample index. A cached entry is only a hit when the inputs it recorded
  agree with the inputs this run has, so a file moved or edited into the wrong place is an error
  rather than a sample from a different experiment.
- **Traces and run artifacts** record the probe name and digest. `check --trace <path>` scores
  recorded evidence only when the agent checksum, tool catalog digest, runner and version, and the
  probe name/digest/sample count all match the current run (and, for a run artifact, the probe suite
  digest). Anything else is **unusable evidence, not a verdict**: exit `3`, with the fact that
  disagrees named.

The practical consequences are worth stating plainly: renaming a probe retires the score kept under
the old name; reformatting or moving an output-schema file does not; changing a probe means the old
baseline is drift rather than a regression; and evidence captured before a dependency change is
refused rather than reinterpreted, because scoring it would attribute one agent's behavior to another.

## Strict loading: what is refused

Every one of these is exit `3`, reported with the file and, where it applies, the probe name:

| Situation | Why it is refused |
|---|---|
| Unknown key in a probe file | A silently ignored typo means the run measured something else |
| Empty file, or a file with no `[[probe]]` | A probe file declares at least one probe |
| Two probes with one name, in any file | The name is the durable identity of an assertion |
| Empty or whitespace-only prompt | There is nothing to ask |
| Prompt over 64 KiB | A truncated prompt measures a prompt you did not write |
| `repeat` outside `1..=100` | A clamp would change what the run means |
| No expectation key at all | A probe that asserts nothing |
| `expect_args` without `expect_tool` | The matchers judge a tool's arguments; which tool is unstated |
| `expect_no_tool` combined with `expect_tool`, `expect_args` or `forbid_tools` | Two contradictory readings |
| `expect_no_tool = false` | Asserts nothing |
| `forbid_tools = []` | A probe that forbids nothing is a typo |
| More than 64 matchers, or more than 64 forbidden tools | Bounds, not truncation |
| Two matchers for one canonical path | Accepting both would silently drop one declaration |
| A matcher with no operator, or with two | The operator must be unambiguous |
| An unusable argument path | A path typo would read as a model regression |
| An unknown or ambiguous tool reference | Scoring against the wrong contract |
| A missing, unreadable, oversized or invalid output schema | A yardstick that cannot be compiled |
| A schema needing HTTP or file resolution | Probes never reach the network |
| A subdirectory inside the probe directory | Probe fixtures are one flat directory |
| More than 64 files, or more than 256 probes | Bounds, not truncation |

`agentchecksum inspect probes` prints the parsed suite, which is the fastest way to see what resolved.
It resolves references against the committed lockfile rather than live discovery — no MCP session and
no endpoint — so it needs a lockfile to exist (`agentchecksum snapshot` first, if you have none):

```console
$ agentchecksum inspect probes

AgentChecksum inspect probes

Suite:  4 probes
Digest: sha256:7926dfc5251e19c4560771fdc0110410d5e3660a6ef32704c6b5acd7fa1ea5b3

no-tools
  file     probes/01-no-tools.toml
  repeat   1
  digest   sha256:ebe527ba82fcafb36c6145998cfeb12aef75a56dc87403ae95f74cc1a8d1959c
  metrics  argument_validity, tool_restraint
  tools    none

search-repositories
  file     probes/02-search.toml
  repeat   2
  digest   sha256:c8539e37efff23d715fb82584146635b51bc794928f888ebb899745be1b1bcd5
  metrics  tool_selection, argument_validity, argument_expectation
  tool     tool:demo-tools.search_repositories
            metrics: tool_selection, argument_expectation

no-destructive-tools
  file     probes/03-restraint.toml
  repeat   1
  digest   sha256:529d81336acb730b43a321505499167311cd93d8a2c4431205dd49546cf33d14
  metrics  tool_selection, argument_validity, forbidden_tool_usage
  tool     tool:demo-tools.search_repositories
            metrics: tool_selection
  tool     tool:demo-tools.delete_repository
            metrics: forbidden_tool_usage

triage-json
  file     probes/04-structured.toml
  repeat   1
  digest   sha256:38694dad10d5ddf51d15d16adffe5bcd6bf47ce56e27274ac9bc8de2741f79be
  metrics  argument_validity, structured_output_validity
  tools    none
```

## A worked suite

Four probes, each covering a different part of the vocabulary. They assume one MCP server aliased
`demo-tools` exposing `search_repositories` (with `query` required and an optional `per_page`) and
`delete_repository`.

The assertions live in `agentchecksum.toml`:

```toml
[[mcp.servers]]
name = "demo-tools"
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-example"]

[probes]
path = "probes"
```

**`probes/01-no-tools.toml`** — restraint. The one expectation that needs no tool catalog to be
interesting.

```toml
[[probe]]
name = "no-tools"
prompt = """
Answer from what you already know, without calling any tool: what is the capital of Portugal?
"""
expect_no_tool = true
```

**`probes/02-search.toml`** — selection plus a positive argument assertion, sampled twice.

```toml
[[probe]]
name = "search-repositories"
prompt = "Find repositories about postgres, twenty per page."
repeat = 2
expect_tool = "search_repositories"
expect_args = { query = { contains = "postgres" }, "/per_page" = { one_of = [10, 20] } }
```

**`probes/03-restraint.toml`** — call this, never that.

```toml
[[probe]]
name = "no-destructive-tools"
prompt = "List repositories about postgres. Do not modify anything."
expect_tool = "search_repositories"
forbid_tools = ["delete_repository"]
```

**`probes/04-structured.toml`** — a structured final answer, against `schemas/answer.json`.

```toml
[[probe]]
name = "triage-json"
prompt = "Return the triage decision as JSON with a ticket and a priority."
output_schema = "schemas/answer.json"
```

```json
{
  "type": "object",
  "properties": { "ticket": { "type": "string" }, "priority": { "type": "string" } },
  "required": ["ticket", "priority"],
  "additionalProperties": false
}
```

Five samples in total (1 + 2 + 1 + 1). A run where every sample calls `search_repositories` with
`{"query": "postgres repositories", "per_page": 20}` and the last probe answers with valid JSON
produces (the report's opening lines are elided):

```console
$ agentchecksum check

Behavioral probes: 4 / 5 passed
tool_selection              100% → 100%  PASS
argument_validity           100% → 100%  PASS
argument_expectation        100% → 100%  PASS
forbidden_tool_usage        100% → 100%  PASS
tool_restraint              0% → 0%  WARN
structured_output_validity  100% → 100%  PASS

Failing probes:
  no-tools  0 / 1 passed
    sample 0: tool_restraint 1 tool was called
```

`tool_restraint` reads `WARN` rather than `FAIL` because no policy was configured for it: "no
threshold failed" and "the behavior was good" are different claims. Add the policy and the same run
becomes a gate failure, which is the point of the tool:

```toml
[policy.metrics.tool_restraint]
min = 1.0
```

```console
Policy failures:
  tool_restraint: score 0.0000 is below the required minimum 1.0000
```

Note what that probe measured: the model called a tool when it had been told it did not need one. No
tool was **executed** — nothing on this path ever calls a tool — so the failure is a fact about the
model's decision, recorded from what the endpoint returned.

## Related

- [configuration.md](configuration.md) — `[probes]` and `[policy]`, including the `max_drop`
  comparability rule.
- [getting-started.md](getting-started.md#reading-a-check-result) — reading a `check` result.
- [ci.md](ci.md) — gating a pull request on the verdict.
- [security.md](security.md) — why an output schema never reaches the network.
- [specs/2026-09-15-agentchecksum-design.md](specs/2026-09-15-agentchecksum-design.md) — the probe
  and gate sections of the design specification.
