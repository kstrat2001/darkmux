---
name: darkmux-status
description: Quick check of what local LLM configuration is currently loaded — which models, which context lengths, and which darkmux profile (if any) it matches. Run this when you need to decide whether to swap stacks before a task, or to confirm a swap took effect.
user_invocable: true
allowed-tools: "Bash(darkmux:*)"
---

# Status

## Step 1 — Run

```bash
darkmux status
```

## Step 2 — Interpret

Output shape:

```
registry: /Users/<you>/.darkmux/profiles.json
loaded models (N):
  <identifier>     ctx=<num>     <status>
  ...
matches profile(s): <name1>, <name2>
```

Or `matches no registered profile` if the loaded state doesn't match any defined profile.

## Step 3 — Report

Tell the user:
- Which models are loaded and at what context length
- Which profile is currently active (if any)
- If no profile matches, suggest the closest one or invite them to define a custom profile

## Notes

- Status is read-only and never modifies state.
- If `darkmux` is not installed, the user can `cargo install --path .` from the darkmux repo.
