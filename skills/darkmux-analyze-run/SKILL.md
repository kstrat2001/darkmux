---
name: darkmux-analyze-run
description: Inspect a previously-recorded lab run. Reads the trajectory + manifest under .darkmux/runs/<id>/ and reports turns, compaction events, wall clock, fast/slow mode classification, and verify outcome. Use this to understand what a dispatch did, especially when investigating variance or compaction behavior. Also serves as the canonical reference for what data lives where + how to parse it (data-location map + per-event-type field reference + jq cookbook + diagnostic patterns).
user_invocable: true
allowed-tools: "Bash(darkmux:*), Bash(ls:*), Bash(cat:*), Bash(jq:*), Bash(grep:*), Bash(wc:*), Read"
---

# Analyze run

ARGUMENTS expected: `<run-id-or-path>`

## Step 1 — If no run-id given, list recent runs

```bash
darkmux lab runs --limit 5
```

This prints the 5 most-recent run IDs with workload + wall clock + ok/error status. Surface the table and ask the user which to analyze.

## Step 2 — Inspect

```bash
darkmux lab inspect "$ARGUMENTS"
```

Output shape:

```
run:         <session-id>
workload:    <workload-id>
wall:        <seconds>s
turns:       N
compactions: M
tokensBefore: <list>
mode:        fast | slow
notes:
  - turns=...
  - compactions=...
  - walltime=...s
  - mode=...
  - verify: ok | fail
```

## Step 3 — Report findings

Frame the run in human terms:

- **If single-turn, no compaction:** "the model kept the entire workflow in its working context — no gateway round-trips."
- **If multiple turns + 1+ compaction:** "the dispatch grew the context past the trigger threshold; compaction(s) fired and the run continued cleanly."
- **If mode=fast or slow:** explain in terms of the workload's expected distribution (cite the empirical_basis if the workload defines one).
- **If verify failed:** call out the specific reason (missing keywords, exit code, etc.).

---

# Data reference — where each piece of data lives

`darkmux lab inspect` gives a fixed summary. When you need finer-grained signal (per-turn token timeline, compression ratio per compaction, partial-event cadence, etc.) you query the raw files directly. **This section names the canonical locations + field shapes** so jq queries work first try, not after three wrong guesses.

## Run-artifact map

There are four distinct files with overlapping but **non-identical** schemas. Mixing them up is the canonical "I got null everywhere" failure mode.

| File | Path template | Authoritative for | NOT authoritative for |
|---|---|---|---|
| **Run manifest** | `~/.darkmux/runs/<run-id>/manifest.json` | host's view of the run: `runId`, `workload`, `profile`, `provider`, `sandbox`, `ok`, `durationMs`, `sessionId`, `schemaVersion`. **schemaVersion 3+** adds `final_hash` (post-dispatch sandbox content hash, `blake3:<hex>`); **schemaVersion 4+** adds a `fixture` block (`source_path`, `baseline_hash`) for fixture-backed coding-task runs | per-turn metrics; final assistant text; tool calls |
| **QA reply** | `~/.darkmux/runs/<run-id>/qa-reply.json` | runtime's JSON envelope: `final_assistant` (string), `metrics.{turns, prompt_tokens, completion_tokens, compactions, wall_ms, total_messages}`, `result` ("stop"/"max_turns"/"escalation_*"/"error"), `trajectory_path` | per-turn breakdown; tool calls; reasoning |
| **Runtime metrics** | `<sandbox>/.darkmux-runtime/metrics.json` (sandbox path is in manifest.json's `.sandbox`) | aggregate totals using `total_prompt_tokens` / `total_completion_tokens` naming (DIFFERENT from qa-reply's nested `metrics.prompt_tokens`) | history; stale on watchdog-killed dispatches |
| **Trajectory** | `<sandbox>/.darkmux-runtime/trajectory.jsonl` | event-by-event ground truth. JSONL — one event per line. Source for all derived analyses. | aggregates (derive them yourself) |

Quick locator: `cat ~/.darkmux/runs/<run-id>/manifest.json | jq -r '.sandbox'` returns the sandbox path the runtime wrote into. For `provider: "prompt"` workloads the sandbox is `null` (fresh tempdir, cleaned up after dispatch) — the qa-reply.json is your only data source for those.

## Trajectory event-type reference

Every line in `trajectory.jsonl` has a `type` field. Use `grep '"type":"<EVENT>"'` then `jq` for filtering. Here's the canonical schema for each event type, generated from `runtime/src/trajectory.rs`:

### `dispatch.start` (1 per dispatch, first event)

```
{ "type": "dispatch.start", "ts": <unix_ms>, "model": <str>, "system_chars": <int>, "prompt_chars": <int> }
```

### `model.streaming.start` (1 per streaming turn, before partials)

```
{ "type": "model.streaming.start", "seq": <turn_number>, "ts": <unix_ms>, "system_chars": <int>, "prompt_chars": <int> }
```

`system_chars` is the system-role messages' total char count for THIS request (changes only across compactions); `prompt_chars` is the accumulated conversation context size in chars. Independent signal from `usage.prompt_tokens` (which uses LMStudio's tokenizer).

### `model.partial` (N per streaming turn, one per SSE chunk)

```
{ "type": "model.partial", "seq": <turn>, "partial_index": <int>, "delta_chars": <int>, "cumulative_chars": <int>, "tool_calls_present": <bool>, "ts": <unix_ms> }
```

**Stats only, never chunk content** — a 10K-token streamed response would blow up trajectory.jsonl size if every chunk's text were stored. Use `cumulative_chars` for "how big is this turn's response so far."

### `model.streaming.end` (1 per streaming turn, after partials)

```
{ "type": "model.streaming.end", "seq": <turn>, "partial_count": <int>, "total_content_chars": <int>, "tool_calls_count": <int>, "ts": <unix_ms> }
```

### `model.completed` (1 per turn, after streaming or non-streaming response)

```
{
  "type": "model.completed",
  "seq": <turn_number>,
  "ts": <unix_ms>,
  "finish_reason": "stop" | "tool_calls" | "length",
  "usage": { "prompt_tokens": <int>, "completion_tokens": <int>, "total_tokens": <int> } | null,
  "tool_calls": [{ "id": <str>, "name": <str>, "arguments_chars": <int> }] | null
}
```

`usage: null` is a signal: either non-streaming response with no usage in the envelope, OR an older runtime where `stream_options.include_usage` wasn't set (fixed in #360, commit 54d3512).

### `model.reasoning` (0 or 1 per turn, only when reasoning emitted)

```
{ "type": "model.reasoning", "seq": <turn>, "ts": <unix_ms>, "reasoning_text": <full text>, "reasoning_chars": <count>, "reasoning_format": "inline-think-tags" | "separate-field" }
```

Carries FULL reasoning text — can be 5-10× the response size on hard problems. Format names where the text came from: parsed from inline `<think>...</think>` blocks vs extracted from a separate `reasoning_content` field.

### `tool.completed` (1 per tool invocation)

```
{ "type": "tool.completed", "seq": <turn>, "tool_seq": <int>, "tool_name": <str>, "args_chars": <int>, "result_chars": <int>, "ts": <unix_ms> }
```

### `compaction` (1 per compaction trigger that fires)

```
{ "type": "compaction", "generation": <int>, "ts": <unix_ms>, "before_messages": <int>, "after_messages": <int>, "summary_chars": <int> }
```

### Runtime signal events (0+ per dispatch — heuristics + recovery)

These are emitted by the runtime's struggle-detectors and recovery paths (landed 2026-05; see `runtime/src/loop_runner.rs`). Most are **edge-triggered** (fire once per threshold crossing, not once per turn) and most are **observability-only** — they don't change dispatch behavior, they record that a pattern was seen. They pair with `dispatch.feedback.injected` (the same signal delivered to the model as a nudge). When investigating a stuck or pathological run, grep these first.

```
{ "type": "dispatch.cycle.suspected", "seq": <turn>, "ts": <ms>, "tool_name": <str>, "canonical_args": <str>, "count": <int>, "window_size": <int> }
  # same tool + canonicalized args seen `count` times in the last `window_size` (10) tool calls (#418)

{ "type": "dispatch.reasoning_loop.suspected", "seq": <turn>, "ts": <ms>, "count": <int>, "window_size": <int> }
  # same normalized reasoning text repeated `count` times in `window_size` (10) turns — sibling of cycle detector (#461)

{ "type": "dispatch.tool.repeated_failure", "seq": <turn>, "ts": <ms>, "tool_name": <str>, "consecutive_failures": <int> }
  # one tool failed `consecutive_failures` times in a row (resets on any success) (#419)

{ "type": "dispatch.per_turn_cap.salvaged", "seq": <turn>, "ts": <ms>, "completion_tokens": <int>, "cap": <int>, "salvaged_tool_calls": <int> }
  # turn hit MAX_TOKENS_PER_CALL (10000) on finish_reason=length but well-formed tool calls survived; truncated content discarded, calls dispatched anyway (#479)

{ "type": "dispatch.intra_turn_stall.recovered", "seq": <turn>, "ts": <ms>, "completion_tokens": <int>|null, "recoveries_used": <int>, "recoveries_budget": <int> }
  # finish_reason=length with no content AND no tool calls (runaway reasoning); useless turn dropped, nudge injected, retried. completion_tokens≈cap ⇒ per-call-cap stall; well below ⇒ context-overflow stall (#414)

{ "type": "tool_call.promoted", "seq": <turn>, "ts": <ms>, "source": "content"|"reasoning", "format": "bracket"|"harmony"|"xml", "promoted_call_count": <int> }
  # LMStudio didn't extract tool_calls; runtime recovered them from plain-text markup and rerouted to the tool path. Each one is a model wire-format failure the runtime caught (#406)

{ "type": "dispatch.feedback.injected", "seq": <turn>, "ts": <ms>, "message_count": <int>, "signal_kinds": [<str>, …] }
  # `message_count` synthetic [darkmux-runtime] system messages delivered to the next-turn prompt; `signal_kinds` names which signals (cycle_suspected, tool_failure_cascade, reasoning_loop, post_compaction, test_cadence_drift, inactivity_approach, per_turn_cap_approach). Disable globally with DARKMUX_FEEDBACK_INJECTION=0 (#454)
```

**Diagnostic use:** a high `tool_call.promoted` rate means the loaded model emits malformed tool-call wire format (consider a different model); repeated `dispatch.cycle.suspected` / `reasoning_loop.suspected` mean the model is stuck and burning budget; `intra_turn_stall.recovered` with `completion_tokens` ≈ 10000 confirms a per-call-cap runaway rather than genuine context overflow.

### `dispatch.complete` (1 per dispatch, last event — unless watchdog-killed)

```
{ "type": "dispatch.complete", "ts": <unix_ms>, "result": <str>, "wall_ms": <u128> }
```

`result` discriminates terminal reason: `"stop"` (clean), `"max_turns"` (hit operator `--max-turns`), `"escalation_cumulative_tokens_exceeded"` (hit `--max-tokens`), `"escalation_intra_turn_stall_exhausted"` (stall budget exhausted), `"escalation_compaction_limit_reached"` (hit `bail_after_compactions`), `"error"`.

## Naming-convention traps — gotchas worth memorizing

These are the specific names where the schema has evolved or where similar concepts use different keys depending on the file. Getting them wrong silently returns `null`, which is the same shape as "field exists but is unset" — easy to misread as a bug.

| Wrong | Right | Where the right one lives |
|---|---|---|
| `before_count` / `after_count` (in compaction events) | `before_messages` / `after_messages` | trajectory.jsonl `compaction` event |
| `total_prompt_tokens` (at qa-reply top level) | `metrics.prompt_tokens` (nested) | qa-reply.json envelope |
| `metrics.prompt_tokens` (in runtime metrics.json) | `total_prompt_tokens` (top level) | runtime's `<sandbox>/.darkmux-runtime/metrics.json` — note this is the OPPOSITE nesting from qa-reply.json |
| `tokens` (anywhere) | `prompt_tokens` + `completion_tokens` + `total_tokens` | always the three-way split; never a single "tokens" field |
| `wall_ms` (in manifest.json) | `durationMs` (in manifest.json) | manifest uses camelCase; runtime metrics + qa-reply use snake_case |
| `model.completed.usage.tokens` | `model.completed.usage.{prompt,completion,total}_tokens` | usage is always the three-field object |

If a query returns `null` for a field you expect populated:

1. **First check that the field name exists in the schema** for the event type (use the per-event-type reference above).
2. **Then check that you're querying the right file** (per-turn data only in trajectory; aggregates in metrics or qa-reply).
3. **Then check that the dispatch ran with the relevant feature on** — e.g. `usage` requires `stream_options.include_usage` which was added in #362; old runs predating that won't have usage data even though the field is present in the schema.

## Diagnostic — "is the field really null, or am I querying wrong?"

Two-line check:

```bash
# Line 1: what your query says (might be null)
grep '"type":"compaction"' <trajectory> | head -1 | jq '.before_count'

# Line 2: what fields ACTUALLY exist at the path you're querying
grep '"type":"compaction"' <trajectory> | head -1 | jq 'keys'
```

If `keys` shows the field you expected isn't there (e.g. `["after_messages", "before_messages", "generation", "summary_chars", "ts", "type"]` — no `before_count`), the field name is wrong, not the data.

Always run line 2 before filing a "field is null" bug.

## Canonical jq cookbook — copy-paste-ready

The queries below were validated against Beat 44 + Beat 41 trajectories. Field names are correct; adjust paths to point at your run.

### Per-turn token timeline

```bash
TRAJ=<sandbox>/.darkmux-runtime/trajectory.jsonl
grep '"type":"model.completed"' "$TRAJ" \
  | jq -c '{turn: .seq, prompt: .usage.prompt_tokens, completion: .usage.completion_tokens}'
```

Output is one line per turn. Watch the `prompt` value climb until it crosses the trigger threshold; the next turn after a compaction will show a notable drop.

### Compression ratio per compaction

```bash
grep '"type":"compaction"' "$TRAJ" \
  | jq -c '{gen: .generation, msgs_before: .before_messages, msgs_after: .after_messages, summary_chars}'
```

Also useful: pair compaction events with the model.completed turn immediately before + after to see the prompt-token reduction. Beat 44 turn 3 → 4 reduction: 39265 → 14714 = 63% reduction.

### Aggregate analysis (total tokens, max-turn, threshold crossings)

```bash
THRESHOLD=35350  # adjust to your formula trigger (context_window × threshold_ratio)
grep '"type":"model.completed"' "$TRAJ" | jq -s '
  {
    turns: length,
    total_prompt_tokens: map(.usage.prompt_tokens // 0) | add,
    total_completion_tokens: map(.usage.completion_tokens // 0) | add,
    max_prompt_in_turn: map(.usage.prompt_tokens // 0) | max,
    turns_above_threshold: (map(.usage.prompt_tokens // 0) | map(select(. >= '"$THRESHOLD"')) | length)
  }'
```

### Tool-call breakdown by name

```bash
grep '"type":"tool.completed"' "$TRAJ" \
  | jq -s 'group_by(.tool_name) | map({tool: .[0].tool_name, count: length, total_args: map(.args_chars) | add, total_result: map(.result_chars) | add})'
```

### Reasoning text by turn (for thinking-mode model analysis)

```bash
grep '"type":"model.reasoning"' "$TRAJ" \
  | jq -c '{seq, format: .reasoning_format, chars: .reasoning_chars, preview: (.reasoning_text | .[0:200])}'
```

### Wall-clock per turn (turn-to-turn `ts` deltas from model.streaming.start)

```bash
grep '"type":"model.streaming.start"' "$TRAJ" \
  | jq -s 'sort_by(.ts) | [.[0]] + (.[1:] | to_entries | map({seq: .value.seq, dt_ms: (.value.ts - (input | .[.key].ts // .value.ts))}))'
```

Useful for spotting variance — a turn that took 60s in wall-clock indicates either a long primary generation or a stuck compactor call.

### Per-compaction "what did the agent know" (tier-2 only)

When the dispatch ran with `compact-strategy: structured-slot`, persisted compaction artifacts live at `<sandbox>/.darkmux-runtime/compaction-<gen>.json`:

```bash
for f in <sandbox>/.darkmux-runtime/compaction-*.json; do
  echo "=== $f ==="
  cat "$f" | jq '{
    gen: .compaction_metadata.generation,
    objective_chars: (.objective | length),
    files_chars: (.current_truth.active_files // "" | length),
    test_chars: (.current_truth.test_outcomes // "" | length),
    decisions_chars: (.completed_decisions // "" | length),
    errors_chars: (.errors_to_preserve // "" | length),
    next_chars: (.next_concrete_actions // "" | length)
  }'
done
```

Beat 41/42 forensic methodology: track which slots stay populated vs empty across compactions to grade compactor capability.

### Reproducibility hashes (fixture-backed coding-task runs)

For runs whose workload declared `requires_fixture`, the manifest carries content hashes. Compare them across two runs of the same fixture to check reproducibility:

```bash
for r in <run-a> <run-b>; do
  jq -c '{run: .runId, baseline: .fixture.baseline_hash, final: .final_hash}' \
    ~/.darkmux/runs/$r/manifest.json
done
```

Equal `baseline_hash` ⇒ both runs started from the same source state. Equal `final_hash` ⇒ both runs left the sandbox bitwise-identical (the strongest reproducibility signal). A `final_hash` of `null` means hashing was skipped (non-coding-task provider, or a best-effort failure logged at run time).

## Notes

- A run dir without `manifest.json` will error with "no run manifest" — that means the dispatch wasn't done via `darkmux lab run` (or was interrupted before writing).
- A `<sandbox>/.darkmux-runtime/metrics.json` showing `turns: 0, total_prompt_tokens: 0` is almost always stale from a prior dispatch — the runtime overwrites it on clean exit only, so watchdog-killed dispatches leave the previous run's data in place. Trust the trajectory.jsonl for ground truth.
- For a side-by-side diff of two runs, use `darkmux-compare-runs` instead.
- When in doubt, run the diagnostic two-line check (`.field` then `keys`) before assuming a feature is broken.
