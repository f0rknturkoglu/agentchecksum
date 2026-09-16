# Phase 4 — Behavioral probes and the Behavior Gate

**Status:** implementation brief, fixed before coding.
**Depends on:** Phase 1 (fingerprint), Phase 2 (diff + risk), Phase 3 (MCP discovery) — all frozen.

This document is the contract the code implements. Where it disagrees with older prose in
`docs/specs/2026-09-15-agentchecksum-design.md`, this brief is what shipped and the spec is reconciled to it.

---

## 0. What Phase 4 is

```text
probe TOML → typed probes → suite digest
                                 ↓
model HTTP ─────────────────→ Trace (evidence)
                                 ↓
                          PURE evaluator → per-sample results → six metrics
                                 ↓
baseline.json ───────────→ PURE policy → combined verdict → exit code
```

Phase 4 answers *did observable behavior regress?* for a narrow, declared set of expectations. It does
**not** judge output quality, execute tools, or claim causation.

---

## A. Probe file contract

- Root: `[probes].path`, default `probes/`, resolved against the config directory with the repository's
  existing project-relative rules (`normalize_rel_path`: no absolute paths, no `..`, no backslashes,
  no empty segments).
- Fixtures: **one directory**, regular `*.toml` files only, sorted by normalized relative path. No
  recursion. A subdirectory is an error rather than silently skipped; a non-`.toml` regular file
  (`README.md`, `.DS_Store`, …) is ignored by extension alone and that is the only silent skip.
- Each file holds `[[probe]]` array-of-tables. One file may hold many probes; an empty file is an error
  (it is more likely a mistake than an intention).
- Name grammar: `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`, unique across the whole suite. A duplicate is an error
  naming both files.
- `prompt`: required, non-whitespace, ≤ `MAX_PROMPT_BYTES` (64 KiB). No truncation.
- `repeat`: `1..=100`, defaulting through the precedence below.
- Expectations: at least one is required (§12). Every expectation key is validated strictly
  (`deny_unknown_fields`); an unknown key is a load error, not a warning.
- `output_schema`: project-relative path to one self-contained JSON Schema file, same path rules.
- Bounds: `MAX_PROBE_FILES` (64), `MAX_PROBES` (256), `MAX_PROMPT_BYTES` (64 KiB),
  `MAX_OUTPUT_SCHEMA_BYTES` (512 KiB), `MAX_EXPECT_ARG_ENTRIES` (64), `MAX_FORBID_TOOLS` (64).

**Effective repeat** (CLI wins, then the probe, then the config, then 1):

```text
--repeat > probe.repeat > [probes].repeat > 1
```

**Prompt assembly** (§28): configured prompt dependencies, normalized with the existing prompt
normalization, **sorted by dependency id**, joined with `"\n\n"`, sent as one system message. No prompts
configured → no system message at all. Order is therefore independent of TOML order, which Phase 1 does
not fingerprint.

---

## B. Expectation contract — exactly five

| Key | Shape | Applicable metric |
|---|---|---|
| `expect_tool` | tool reference | `tool_selection` |
| `expect_args` | map of JSON Pointer → matcher, requires `expect_tool` | `argument_expectation` |
| `forbid_tools` | non-empty list of tool references | `forbidden_tool_usage` |
| `expect_no_tool` | `true` | `tool_restraint` |
| `output_schema` | path | `structured_output_validity` |

Rejected combinations (§15, §23): `expect_args` without `expect_tool`; `expect_no_tool` together with
`expect_tool`, `expect_args`, or `forbid_tools`. A probe with none of the five is rejected.

### Tool references (§13)

Canonical form `tool:<alias>.<encoded-name>`. A bare remote tool name is accepted **only** when exactly
one discovered `Tool` dependency carries it; zero matches is `ProbeToolUnknown`, more than one is
`ProbeToolAmbiguous` (the diagnostic lists the canonical ids). No fuzzy matching.

### Matchers (§16–§20)

- A matcher object carries **exactly one** of `equals`, `contains`, `one_of`. Two operators is a load
  error.
- `equals`: structural JSON equality.
- `contains`: **string actual + string matcher → substring**; **array actual + any matcher → an element
  deep-equal to the matcher**. Objects are not supported. A type mismatch is an expectation failure.
- `one_of`: non-empty list; passes when the actual value deep-equals any element.
- Paths: **RFC 6901 JSON Pointer**. A key without a leading `/` is one top-level property
  (`query` ≡ `/query`); no deeper dotted shorthand. A malformed pointer is a probe validation error; a
  pointer that resolves to nothing is an expectation failure.

---

## C. Runner contract

- One runner: OpenAI-compatible `POST {base}/v1/chat/completions` with function `tools`.
- Endpoint derivation from the configured base, never producing `/v1/v1/…`:
  `…:11434` → `…:11434/v1/chat/completions`; `https://h` → `https://h/v1/chat/completions`;
  `https://h/v1` → `https://h/v1/chat/completions`; `https://h/api/openai/v1` →
  `https://h/api/openai/v1/chat/completions`. An unrecognised shape is a `RunnerUnsupported` error.
- Request: `{ model, messages, tools, stream: false, n: 1, …effective_params }`.
  `model`, `messages`, `tools`, `stream`, `n` are AgentChecksum-owned: configuring one of them in
  `[model].params` is a validation error (§39).
- Defaults, always recorded in the trace: `temperature = 0.0`, `seed = 42`, applied only when the user
  did not configure them. A backend that rejects `seed` fails with a diagnostic; the runner never
  silently retries without it (§40).
- Messages: optional system message (assembled above), then one user message = the probe prompt.
- Tools: current live `Tool` dependencies as OpenAI functions — `name`, `description`, `input_schema`
  only (§30). The remote tool name is the function name, unrenamed; two tools sharing a name is a
  `RunnerUnsupported` error for the *runner* only (static snapshot/diff keeps working), and a name the
  wire format cannot carry is a diagnostic, never a silent sanitisation (§31).
- Timeout: `MODEL_SAMPLE_TIMEOUT = 120s`, one request per sample, **no retries** of any kind (§36, §37).
- Bounds: `MAX_TOOL_CALLS_PER_SAMPLE` (128), `MAX_RESPONSE_BYTES` (16 MiB), `MAX_FINAL_TEXT_BYTES`
  (1 MiB), `MAX_TOOL_ARGUMENTS_BYTES` (1 MiB). Exceeding one is a runtime error, never truncation.

---

## D. Trace contract

```json
{
  "trace_version": 1,
  "probe": "repository-search",
  "probe_digest": "sha256:…",
  "agent_checksum": "ac1:…",
  "captured_with": {
    "runner": "openai-chat-completions",
    "runner_version": 1,
    "model_id": "qwen3:8b",
    "effective_params": { "temperature": 0.0, "seed": 42 },
    "tool_catalog_digest": "sha256:…"
  },
  "samples": [
    {
      "index": 0,
      "tool_calls": [
        {
          "name": "search_repositories",
          "tool_id": "tool:github.search_repositories",
          "arguments": { "query": "postgres" },
          "arguments_parse_error": null
        }
      ],
      "final_text": null
    }
  ]
}
```

- `tool_id` is `null` for a name the model invented — recorded as observed behavior, not an error.
- Arguments arrive as a JSON string or an already-parsed value; both normalise to `Value`. A parse
  failure records `arguments_parse_error` and fails the argument metrics for that call.
- Call order is preserved for debugging; metrics never depend on map order.

---

## E. Evaluation semantics (pure)

Per sample, per metric — every metric is a **quality score: 1.0 good, 0.0 bad**:

| Metric | Applicable when | Numerator |
|---|---|---|
| `tool_selection` | `expect_tool` present | expected tool appears ≥ 1 time |
| `argument_expectation` | `expect_args` present | ≥ 1 expected-tool call satisfies **every** matcher |
| `forbidden_tool_usage` | `forbid_tools` non-empty | no forbidden tool appears |
| `tool_restraint` | `expect_no_tool = true` | `tool_calls` is empty |
| `structured_output_validity` | `output_schema` present | `final_text` parses as JSON **and** validates (no fence stripping) |
| `argument_validity` | sample has ≥ 1 tool call | every call resolves to a declared tool, its arguments parse, and they validate against that tool's current `input_schema` |

Aggregation stores `passed`, `total`, and the derived `score = passed / total`. `total = 0` means the
metric is absent (N/A) and is never reported as `0%`. Zero tool calls leaves `argument_validity` N/A —
it is never a free 1.0.

A sample passes its probe iff **every applicable check passes** (declared expectations + automatic
`argument_validity`). Probe score = passing samples / effective repeat.

---

## F. Baseline

Path: `.agentchecksum/baseline.json`, committed, `2`-space JSON with a trailing newline, no timestamps,
no raw prompt/output/arguments.

```json
{
  "baseline_version": 1,
  "agent_checksum": "ac1:…",
  "probe_suite_digest": "sha256:…",
  "runner_contract": "openai-chat-completions-v1",
  "metrics": { "tool_selection": { "passed": 9, "total": 10 } },
  "probes": { "repository-search": { "passed": 3, "total": 3 } },
  "yardsticks": { "tool_input_schema": { "tool:github.search_repositories": "sha256:…" } }
}
```

Counts, not percentages: `passed`/`total` is what a reader can audit, and the score is derived from
them wherever one is needed (`MetricScore::score`). A serialized float beside the counts it came from
is a second source of truth, and it would be the one nobody re-derives.

Only `check --accept` writes it, atomically (temp + rename), and only after the whole run succeeded and
every absolute policy passed. A newer `baseline_version` fails closed.

---

## G. Cache

Key: `sha256(JCS({ runner_version, agent_checksum, probe_digest, tool_catalog_digest, effective_params,
sample_index }))` — no timestamps, no paths, no repeat override.

One file per sample at `.agentchecksum/cache/<key>.json`, holding the trace fragment **and** the key
inputs, so the file can be re-verified rather than trusted by name. A malformed or contradictory entry is
a runtime error, never a PASS. `--refresh` rewrites the affected entries only. Cache is machine-local,
gitignored, and never a baseline.

---

## H. Gate semantics

| Status | Meaning | Exit |
|---|---|---|
| **Pass** | every requested check ran and every applicable policy passed | 0 |
| **Regression** | behavior comparison or a metric policy failed | 1 |
| **Drift** | baseline conditions are not comparable: no behavioral baseline, probe-suite digest changed, or dependency drift that policy does not fail | 0 unless policy says otherwise |
| **Error** | the check could not be evaluated (model unreachable, probe invalid, trace invalid, unsupported schema, discovery failure, malformed provider response) | 3 |

Policies: `min` (current `< min` fails), `max` (current `> max` fails), `max_drop` (`baseline - current >
max_drop` fails, improvements pass), `fail_on_risk` (CLI > config > off; unchanged state never fails even
at threshold `none`), `--fail-on-drift` (any dependency drift fails). All constraints must pass; none
excuses another. Scores compare as derived `f64` from exact counts.

---

## I. CLI and flag compatibility

New: `check` (`--accept`, `--diff-only`, `--probes-only`, `--no-probes`, `--trace <path>`, `--refresh`,
`--repeat <n>`, `--jobs <n>`, `--fail-on-drift`, `--fail-on-risk <level>`, `--from <path>`, plus the
existing global `--format`/`--config`/`--lock`) and `inspect probes`.

Rejected combinations: `--diff-only`+`--probes-only`, `--probes-only`+`--no-probes`,
`--accept`+`--no-probes`, `--accept`+`--diff-only`, `--accept`+`--from`, `--trace`+`--refresh`,
`--trace`+`--jobs`, `--trace`+`--repeat`.

`--jobs` defaults to `1`, maximum `32`; concurrency must not affect sample index, ordering, cache
identity, or the rendered result. `--diff-only` never contacts the model and needs no behavioral
baseline. A missing baseline with probes requested is `Drift`, not a silent pass. `check` never writes
`agentchecksum.lock`; `--accept` refuses to run while dependency drift exists (§78).

---

## J. Security, privacy and known limitations

- **Probes never execute tools.** No `tools/call`, no MCP request, no sandbox. The runner stops a sample
  after recording the emitted calls.
- **No external schema resolution.** `jsonschema` is added with `default-features = false`, which
  disables `resolve-http` and `resolve-file`: internal `#/…` references only. A schema needing anything
  else is `SchemaUnsupported`, not a fetch.
- **Committed state carries no raw evidence.** Runs and cache are gitignored; `baseline.json` holds
  counts, scores, digests and yardstick digests only.
- **Known limitations:** no LLM judge, no regex, no tool-result or multi-turn evaluation, no
  authentication for remote OpenAI-compatible endpoints, no causal claims, and trace *capture* is not
  bit-reproducible (trace *evaluation* is).

---

## Module layout

As built (the plan below it; `artifact.rs`, `runner/mod.rs` and `gate/result.rs` carry what the slices
turned out to need):

```text
src/probes/   mod.rs  load.rs  model.rs  matchers.rs  eval.rs  metrics.rs  digest.rs  resolve.rs
src/runner/   mod.rs  openai.rs  trace.rs  cache.rs  catalog.rs  artifact.rs
src/gate/     mod.rs  baseline.rs  policy.rs  result.rs
src/report/   human.rs, json.rs (the check report), mod.rs  (the report type lives in gate/result.rs)
src/cli/cmd/  init.rs  snapshot.rs  diff.rs  check.rs  inspect.rs
```
