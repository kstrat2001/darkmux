---
name: darkmux-swap-stack
description: Swap to a darkmux profile (fast, balanced, deep, or any user-defined). Use this before running a task that benefits from a different stack — e.g., switch to "deep" before a long agentic run, or "fast" before a single-turn review. Confirms the swap with status check.
user_invocable: true
allowed-tools: "Bash(darkmux:*)"
---

# Swap stack

ARGUMENTS expected: `<profile-name>` (one of the registered profiles, e.g. `fast`, `balanced`, `deep`)

## Step 1 — Show what's loaded now

```bash
darkmux status
```

Note the current profile (the line "matches profile(s): X").

## Step 2 — If already on the requested profile, stop early

If the requested profile matches the current loaded state, just report "already on $ARGUMENTS — no swap needed" and stop.

## Step 3 — Swap

```bash
darkmux swap "$ARGUMENTS"
```

Wait for it to complete. The command prints lines like `unload <model>`, `load <model> @ ctx=N`, then `runtime config patched`, then any post-swap hooks (e.g. gateway restart). Total wall ~10-30s depending on model load times.

## Step 4 — Verify

```bash
darkmux status
```

Confirm the output reads `matches profile(s): $ARGUMENTS`. If not, surface the discrepancy to the user.

## Notes

- `--dry-run` is available if you want to preview the swap without executing.
- If the user hasn't created a profile registry yet (`darkmux: no profile registry found`), tell them to drop a `profiles.json` at `~/.darkmux/profiles.json` (see `profiles.example.json` in the darkmux repo).
- Swapping is idempotent — if the requested ctx values already match what's loaded, darkmux skips the lms calls and just patches runtime config + runs hooks.
