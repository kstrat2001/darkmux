---
name: darkmux-list-stacks
description: List the darkmux profiles (stacks) available in the registry — names, descriptions, model counts, context lengths. Use this to discover stack names before invoking 'darkmux-swap-stack'. The default stack is marked.
user_invocable: true
allowed-tools: "Bash(darkmux:*)"
---

# List stacks

ARGUMENTS: none — this skill takes no arguments.

## Step 1 — List

```bash
darkmux profiles
```

## Step 2 — Output shape

```
registry: /Users/<you>/.darkmux/profiles.json

balanced
  Mid-range tasks. Tuned compaction with a small companion compactor — predictable behavior across mixed workloads.
  - primary    <model-id>    @ ctx 101000
  - compactor  <model-id>    @ ctx 68000

deep (default)
  Long agentic tasks. Maximum primary context for fewer compactions; companion compactor as a safety net.
  - primary    <model-id>    @ ctx 262144
  - compactor  <model-id>    @ ctx 120000

fast
  Single-turn tasks. Slim primary, no compactor — fastest dispatch, lowest RAM.
  - primary    <model-id>    @ ctx 32000
```

## Step 3 — Suggest follow-ups

Surface the list to the user, then offer:

- "Pass a stack name to `darkmux-swap-stack` to switch to it"
- "Run `darkmux-status` to see which stack is currently loaded"

## Notes

- "(default)" marks the stack `darkmux swap` selects when no name is given. Set `default_profile:` in `~/.darkmux/profiles.json` to change it.
- Custom stacks defined in `~/.darkmux/profiles.json` show alongside any reference profiles. The registry is single-file JSON — readable + editable by hand.
- "no profile registry found" means there's no `profiles.json` yet. Suggest `cp <darkmux-repo>/profiles.example.json ~/.darkmux/profiles.json` and edit it.
