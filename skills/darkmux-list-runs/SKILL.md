---
name: darkmux-list-runs
description: List recent darkmux lab runs in most-recent-first order. Use this to discover run IDs for 'darkmux-analyze-run' or 'darkmux-compare-runs'. Default shows the last 5 — pass --limit N for more, --all for everything.
user_invocable: true
allowed-tools: "Bash(darkmux:*)"
---

# List recent runs

ARGUMENTS expected (all optional):
- `--limit N`   show at most N runs (default 5)
- `--all`       show every run (overrides --limit)

## Step 1 — List

```bash
darkmux lab runs $ARGUMENTS
```

`$ARGUMENTS` passes through directly to the CLI, so any of these work:

- `darkmux-list-runs` (default — last 5)
- `darkmux-list-runs --limit 10`
- `darkmux-list-runs --all`
- `darkmux-list-runs -l 20`

## Step 2 — Output shape

```
RUN ID                              WORKLOAD       PROFILE       WALL    OK
quick-q-deep-1730000123-1           quick-q        deep             12s    ✓
long-task-deep-1729998888-1         long-agentic   deep            198s    ✓
long-task-balanced-1729990000-1     long-agentic   balanced        291s    ✓
...
```

## Step 3 — Suggest follow-ups

After listing, suggest the natural next steps to the user:

- "Pass any RUN ID to `darkmux-analyze-run` for a detailed inspection"
- "Pass two RUN IDs to `darkmux-compare-runs` to diff them"

## Notes

- Reads `.darkmux/runs/` (project-local) or `~/.darkmux/runs/` (user-global), depending on which is present in the current directory tree.
- "(no runs found under .darkmux/runs/)" means no dispatches have been recorded via `darkmux lab run` yet. Suggest `darkmux-lab-run <workload>` to create one.
- Run dirs without a `manifest.json` are silently skipped (they typically come from interrupted dispatches).
