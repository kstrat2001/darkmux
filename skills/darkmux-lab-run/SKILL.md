---
name: darkmux-lab-run
description: Execute a darkmux lab workload (one or more dispatches) to characterize how the current stack performs on a defined task. Use this when you want empirical data about a config — wall clock, turns, compaction events, fast/slow mode classification — rather than guessing. Captures full run artifacts to .darkmux/runs/<id>/.
user_invocable: true
allowed-tools: "Bash(darkmux:*)"
---

# Lab run

ARGUMENTS expected: `<workload-id> [--profile <p>] [--runs N]`

## Step 1 — List available workloads (if user didn't specify)

```bash
darkmux lab workloads
```

If the user's prompt didn't name a workload, surface the list and ask which to run.

## Step 2 — Optional pre-run state

If the user wants to know what stack will be exercised, run `darkmux status` first.

## Step 3 — Dispatch

```bash
darkmux lab run "$ARGUMENTS"
```

`$ARGUMENTS` is the literal command tail — `<workload-id>`, optionally followed by `--profile <p>` and/or `--runs N`. Example: `quick-q --profile deep --runs 3`.

The dispatch runs synchronously and prints per-run lines like:

```
[lab] run 1/3 — workload=quick-q profile=deep → quick-q-deep-1730000000-1
  provider=prompt | wall=12s | ok | verify=pass (all keyword checks passed)
```

Total wall depends on the workload — single-turn `prompt` workloads land in seconds; `coding-task` workloads in minutes.

## Step 4 — Report

For each run, report:
- Run ID (use this for follow-up `darkmux lab inspect` calls)
- Wall clock
- ok / error
- Verify outcome (pass/fail + details)

If `--runs N` was used (N > 1), produce a quick aggregate at the end: min / max / mean wall, and any clusters observed.

## Notes

- Run artifacts live under `.darkmux/runs/<run-id>/` (project-local) or `~/.darkmux/runs/<run-id>/` (user-global), depending on whether the cwd has a `.darkmux/` dir.
- A failing verify or non-zero exit code from the runtime is reported as an error — surface it instead of silently passing through.
- Don't kick off many runs without confirming with the user — long-task workloads can saturate the machine for 30+ minutes.
