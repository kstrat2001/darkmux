---
name: darkmux-list-runs
description: List recent darkmux lab run records in most-recent-first order. Use this to discover run IDs for 'darkmux-analyze-run' or 'darkmux-compare-runs'. Default shows the last 5 — pass --limit N for more, --all for everything.
user_invocable: true
allowed-tools: "Bash(darkmux:*)"
---

# List recent runs

ARGUMENTS expected (all optional):
- `--limit N`   show at most N runs (default 5)
- `--all`       show every run (overrides --limit)

## Step 1 — List

```bash
darkmux run list --kind lab $ARGUMENTS
```

`$ARGUMENTS` passes through directly to the CLI, so any of these work:

- `darkmux-list-runs` (default — last 5)
- `darkmux-list-runs --limit 10`
- `darkmux-list-runs --all`
- `darkmux-list-runs -l 20`

## Step 2 — Output shape

One row per run, most recent first: `KIND`, `STATUS`, `STARTED`, `DURATION`, `TOKENS`, `ID`, then a subtitle (role, model, workload). `--kind lab` keeps the lab rows; drop it (`darkmux run list`) to see mission and dispatch runs too. `--json` prints every row.

## Step 3 — Suggest follow-ups

After listing, suggest the natural next steps to the user:

- "Pass any RUN ID to `darkmux-analyze-run` for a detailed inspection"
- "Pass two RUN IDs to `darkmux-compare-runs` to diff them"

## Notes

- Lab runs are read from the lab dir (`~/.darkmux/lab/` by default). "no recorded lab runs yet" means none have been recorded via `darkmux lab run` yet. Suggest `darkmux-lab-run <workload>` to create one.
- A 3.x install kept them in `~/.darkmux/runs/`. While that dir still holds runs, `--kind lab` refuses and prints the `mv` that moves them; run it, then list again.
- Run dirs without a `manifest.json` are silently skipped (they typically come from interrupted dispatches).
