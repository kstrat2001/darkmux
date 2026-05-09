---
name: darkmux-analyze-run
description: Inspect a previously-recorded lab run. Reads the trajectory + manifest under .darkmux/runs/<id>/ and reports turns, compaction events, wall clock, fast/slow mode classification, and verify outcome. Use this to understand what a dispatch did, especially when investigating variance or compaction behavior.
user_invocable: true
allowed-tools: "Bash(darkmux:*), Bash(ls:*), Bash(cat:*)"
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

## Notes

- A run dir without `manifest.json` will error with "no run manifest" — that means the dispatch wasn't done via `darkmux lab run` (or was interrupted before writing).
- For a side-by-side diff of two runs, use `darkmux-compare-runs` instead.
