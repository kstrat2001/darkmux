---
name: darkmux-scan-and-suggest
description: Scan LMStudio for downloaded models that aren't yet in any darkmux profile and offer opinionated commentary on which ones are worth adding. Wraps `darkmux scan` with extra context-aware reasoning per model.
---

# Scan + suggest darkmux profiles

## When to use this skill

The user has downloaded one or more new models in LMStudio (or just wants a fresh inventory of what's available locally) and is wondering which ones are worth adding to their darkmux profile registry. They likely don't want to hand-design a profile per model — they want a recommendation pass and then targeted help.

## Step 1 — Run the scan

```bash
darkmux scan
```

This lists every LLM in `lms ls` that isn't already in any profile, with size / params / architecture / suggested task class / a ready-to-run `darkmux profile draft` command.

## Step 2 — Summarize for the user

Don't just dump the raw scan output. Give the user a **prioritized read** in 3 buckets:

- **Worth adding now** — models that fill a gap in their current profile coverage. For example: if they have a `deep` profile on a 35B-A3B but no `fast` profile and they just downloaded a 4B model, the 4B is high-leverage as a `fast` profile.
- **Adjacent / experimental** — models that are interesting but not strictly needed. E.g., a 70B dense model when they already have a 35B-A3B covering most use cases.
- **Probably skip** — models with red flags: not marked `trainedForToolUse`, very low `maxContextLength` (<8K), redundant duplicates of an already-covered model.

Surface the reasoning briefly per model. The user shouldn't have to re-derive *why* you're prioritizing one over another.

## Step 3 — Offer the next step per recommendation

For each model in the "worth adding now" bucket, show the user the exact `darkmux profile draft` command they'd run. Don't run it for them unless they ask. (They might want to tune the name or the task class first.)

Example:

```
Worth adding:
  • Qwen3 1.7B (1.7B dense, 41K maxCtx)
    — Fills the gap for fast/scribe-style single-turn work
    — Run: `darkmux profile draft qwen3-1.7b-fast --model qwen3-1.7b-mlx --task-class fast`
```

## Step 4 — When to call in `darkmux-design-profile`

If the user wants a more bespoke profile for one of the recommended models — *"I want this one tuned specifically for code review"* — hand off to the `/darkmux-design-profile` skill, which gathers description-driven inputs and refines the task class.

## Notes

- **Memory check before bulk-adding.** If the user goes wild and adds 4 new profiles all paired with the 4B compactor, RAM usage at swap time can balloon. Suggest they run `darkmux doctor` after additions to verify RAM headroom.
- **`darkmux scan` doesn't auto-edit anything.** Every recommendation is a print-only suggestion until the user actually edits `~/.darkmux/profiles.json`.
- **Architectural caveats.** MoE models (qwen3_5_moe, gpt_oss) load all expert weights into RAM even though only a fraction activates per token. The footprint is "full size" not "active params." Mention this when the user seems to assume MoE = small RAM.

## Stop conditions

- The scan finds zero uncovered models — say so explicitly and stop. Don't fabricate recommendations for models that don't exist.
- The user has already decided on a model + task class — skip the bucket-summary and go straight to the design-profile flow.
