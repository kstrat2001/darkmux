---
name: darkmux-compare-runs
description: Diff two darkmux lab runs — wall clock delta, turn delta, compaction delta, mode change. Use this when isolating the effect of a single config change (the "scientific A/B" pattern) or investigating variance between repeated runs of the same config.
user_invocable: true
allowed-tools: "Bash(darkmux:*)"
---

# Compare runs

ARGUMENTS expected: `<run-A> <run-B>`

`run-A` and `run-B` are run-ids or full paths to run directories.

## Step 0 — If user didn't provide run IDs, list recent runs first

```bash
darkmux lab runs --limit 5
```

This prints the 5 most-recent run IDs with their workload, profile, wall clock, and ok/error status. Surface the table and ask which two to compare.

## Step 1 — Diff

```bash
darkmux lab compare "$ARGUMENTS"
```

(Pass both args separated by a space — clap parses them.)

Output shape:

```
<run-A-id> → <run-B-id>
wall: <Xs> → <Ys> (+/-Ns, +/-N.N%)
turns: <X> → <Y>
compactions: <X> → <Y>
mode: <X> → <Y>     (when applicable)
```

## Step 2 — Interpret

The single most useful framing for the user:

- **If A and B used the same config**: the deltas reflect intrinsic variance in the agent's tool-loop. Check whether the runs landed in the same mode (both fast, both slow, or split).
- **If A and B used different configs (single-variable A/B)**: the deltas attribute to that variable. Significant wall / compaction / turn changes are the signal; small ones may just be noise.
- **If mode flipped from fast → slow** (or vice versa) **between identical configs**: that's the bimodal-distribution finding — same input, different mode. Worth multiple runs to see the distribution shape.

## Step 3 — Report

Tell the user the headline: "B was X% faster/slower than A," then add structural context (turn count, compactions). Avoid editorializing; the numbers are the story.

## Notes

- For multi-run characterization (n=3+) of a single config, use `darkmux-lab-run` with `--runs N` and then summarize all run-ids' inspect outputs. `compare` is for two specific runs, not a distribution.
