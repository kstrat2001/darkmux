---
name: darkmux-lab-notebook
description: Draft a lab notebook entry for a recorded darkmux lab run, from `darkmux lab run stats <run-id> --json` (and the run's manifest.json when needed). Observation first, methodology over polish, the verify outcome stated exactly as recorded. Writes the entry wherever the operator's own instructions say a notebook lives; asks when they say nothing. Use this after a lab run whose result is worth keeping, or when the operator says "notebook this run" / "write up run X". Replaces the retired `lab notebook` verbs and the `scribe` role (4.0, #2913).
user_invocable: true
allowed-tools: "Bash(darkmux:*), Bash(cat:*), Bash(jq:*), Bash(ls:*), Bash(date:*), Read, Write"
---

# Lab notebook entry

ARGUMENTS expected: `<run-id-or-path>` (one run), or several run ids for a set.

You are the recorder. darkmux measures; you write the entry, in the style of an electrical-engineering experiment log: date stamps, a one-line headline, concrete numbers, uncertainty marked, no editorial flourish. The reader already knows they are reading a lab notebook; skip introductions and conclusions.

## Step 1 — Find the run, if none was named

```bash
darkmux lab run list --limit 5
```

Show the table and ask which run to write up.

## Step 2 — Read the run's derived numbers

```bash
darkmux lab run stats "<run-id>" --json
```

One run prints one JSON object. The fields an entry is built from:

- **Identity:** `run`, `model`, `bounds` (the resolved caps with their provenance, read from the run rather than assumed).
- **Outcome:** `result` (the runtime's terminal reason, e.g. `"stop"`), `ok` (did the dispatch path finish), and `verify` (`"pass"`, `"fail"`, or `null` when the workload declares no verify). `verify_ungated: true` means the run predates the write-the-tests gate, so its `verify` is the older, weaker signal; say so if you quote it.
- **Time:** `wall_ms`, `active_ms` (wall minus rest), `rest_ms`, `rest_events`, `rest_reasons`, `thermal_ratchet_fired`.
- **Work:** `turns`, `compactions`, `tool_calls` (per tool), `tool_calls_total`, `tool_calls_failed`, `completion_tokens`, `reasoning_tokens`.
- **Rate:** `tok_per_s` (over billed generation only), `billed_gen_fraction`, `gen_ms_billed`.
- **Detection:** `gates.stream.{observations,aborts,degenerate_turns,min_tail_ratio}` for the streaming gate, and `gates.checkpoint.{observations,degenerate_turns,concluded_turns,min_tail_ratio,policy}` for the per-call-cap gate. `gates.checkpoint.policy` is the detection policy in force; under the `"observe"` policy a turn can appear in `gates.checkpoint.degenerate_turns` without appearing in `gates.checkpoint.concluded_turns`, which is the policy working, not a missing cut. Also `suspect_turns`.
- **Host:** `gpu_w_busy`, `pkg_w_busy`, `pkg_j_busy`, `gpu_duty_pct`, `thermal_states_busy`, `throttled_samples`, `mem_pct_busy_max`.
- **Trust:** `checks`. Every figure above still prints when a check fails; the check tells you whether it may be quoted. Read `checks` before quoting anything, and carry the caveat into the entry (for example, when `checks.tokens_reconcile` is false, say so next to any token figure).

Several run ids print a set instead: `runs` (one object each, as above), `summary` (median with min and max, never a bare mean), `errors`, `duplicates`, and with `--baseline`, a `baseline` set plus `cross_arm_overlap`. Quote the set's ranges, not one run's numbers, when the entry is about a series.

## Step 3 — Read the manifest when the entry needs what stats does not carry

`manifest.json` sits in the run directory. Runs live under `~/.darkmux/runs/<run-id>/` by default (under `$DARKMUX_HOME/runs/` when that is set, or `./.darkmux/runs/` when the run was launched from a directory with its own `.darkmux/`); `$DARKMUX_LAB_DIR` or `dirs.lab` in `config.json` moves that root. `darkmux lab run stats` also accepts the run directory's path in place of an id, and when a run id does not resolve its error names the root it searched. It carries `workload`, `provider`, `profile`, `session_id`, `duration_ms`, `ok`, the fixture that was used (`fixture`), and `verify` as `{passed, details}`. `details` is where a failed verify says why; quote it.

## Step 4 — Draft the entry

Roughly 150 to 300 words, simple markdown, in this order:

- **DATE + headline.** One line.
- **WHAT WAS RUN.** Workload, profile, model, the bounds that mattered.
- **RESULT.** Wall clock, active time, turns, compactions, tok/s, and the outcome. State `verify` exactly as recorded: `pass`, `fail` (with `details`), or "not checked" when it is `null`. `ok: true` is the dispatch path finishing; it is not evidence the work was correct. A run whose tests failed is written up as a run whose tests failed, never as "completed successfully" (#2494).
- **OBSERVATION.** One to three bullets on what is mechanically interesting: where compaction fired, a gate that cut a turn, a rest that dominated the wall clock, a single-turn windfall, a check that failed and why the number is still shown.
- **NEXT.** Optional. What would be worth running given this result.

Rules that hold throughout:

- Report what happened, not what should have happened.
- Mark speculation as `uncertain` or `assumed`. If a figure the entry needs is absent, write `missing info:` and name it rather than filling it in.
- Do not reference people, companies, or non-public projects beyond what is literally in the run data.
- Methodology over polish. Tables are fine; keep them simple.

## Step 5 — Write it where the operator keeps their notebook

The operator's own instructions (their CLAUDE.md, a project note, a memory) say where notebook entries go and in what form. Follow them. If nothing says, ask; never invent a location or write into the darkmux root. A sensible default filename, when the operator has none, is `<date>-<workload>-<run-id>.md`.

If the operator collates entries across machines, open the entry with the darkmux header comment so entries stay attributable:

```markdown
<!-- darkmux:notebook-entry: run=<run-id> machine=<machine-id> date=<YYYY-MM-DD> -->
```

`darkmux doctor --verbose` prints the resolved machine id (the `machine_id` line).

Show the operator the entry before writing it. They may want the OBSERVATION reframed; the numbers are darkmux's, the reading is theirs.
