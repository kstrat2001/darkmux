---
name: darkmux-bootstrap
description: First-time darkmux setup walkthrough. Detects the operator's hardware tier, looks up the bake-off-validated recommendation, surfaces what needs downloading, helps confirm the profile registry + orchestrator declaration, and validates end state with `darkmux doctor`. Read + propose pattern — operator runs the commands; the skill confirms each step took effect. Run this once on a fresh machine, or any time `darkmux doctor` surfaces structural drift.
user_invocable: true
allowed-tools: "Bash(darkmux:*),Bash(lms:*),Read"
---

# Darkmux bootstrap

This skill walks an operator through getting a fresh darkmux installation to a clean state on their machine. It works alongside the operator's frontier session: the skill reads state, names what's missing, and asks the operator before they run any mutating command. **Operator-sovereignty:** every change runs at the operator's hand, not the skill's.

The skill assumes darkmux is installed (`darkmux --version` works). If not, point the operator at the Quick Start in the README.

## Step 0 — Confirm darkmux + LMStudio are alive

```bash
darkmux --version
lms --version
darkmux doctor | head -40
```

If any of the three errors:

- `darkmux --version`: stop the skill. Re-run after installing via `cargo install --path .` from the source tree (see README Quick Start).
- `lms --version`: LMStudio CLI isn't on PATH. Tell the operator to install LMStudio (https://lmstudio.ai/) and verify `lms ls` works before re-running this skill.
- `darkmux doctor` errors before producing any output: the binary built but env is misconfigured. Ask the operator to share the error and pause the skill.

**Report to operator**: which of the three are alive, which are missing. Don't continue past this step unless `darkmux doctor` returns at least a partial report.

## Step 1 — Detect hardware tier + look up the bake-off recommendation

```bash
darkmux doctor 2>&1 | grep -E "platform|machine_id|recommendation drift"
```

Look for:
- `platform` line names the operator's hardware tier (`m-series-128`, `m-series-64`, `m-series-32`, `generic`)
- `machine_id` line names the operator's machine identifier
- `recommendation drift` line names what darkmux thinks should be loaded for this tier

Three outcomes:

**A. Tier has a validated recommendation** (currently `m-series-128`; check `darkmux recommendations show <tier>` for the live list). Pull the registry's pick and report to the operator:

```bash
darkmux recommendations show <tier>
```

Then report:
> Your tier is `<tier>`. The recommendation registry has a validated pick for this hardware class — a primary model and a compactor model selected through a documented bake-off (head-to-head comparison with evaluation criteria recorded before the runs). I'll walk through making sure both are downloaded + profile-registered.

**B. Tier has a `pending-bake-off` status** (currently `m-series-64` / `m-series-32` / `generic`). Tell the operator:
> Your tier is `<tier>`. The recommendation registry hasn't validated this hardware class yet — no head-to-head comparison has been run on this RAM band ([#117](https://github.com/kstrat2001/darkmux/issues/117) tracks the gap). I'll help you set up *something* sensible, but the model selection is yours: you'll be picking from what LMStudio offers, not from a validated list.
>
> Two ways forward:
> - **Scan + suggest**: run `/darkmux-scan-and-suggest` (sibling skill) — it walks through your `lms ls` catalog and proposes profile shapes for each model you have.
> - **Draft a profile manually**: pick a model from `lms ls`, then `darkmux profile draft <name> -m <model_key> -t mid` to generate a starter profile JSON. Copy the output into `~/.darkmux/profiles.json` and tune.

**C. Doctor errors on the platform check.** Stop and surface the error.

Continue with the operator's tier.

## Step 2 — Verify the recommended models are downloaded (validated tiers only)

For validated tiers, check whether the registry's recommended models are on disk:

```bash
lms ls | head -50
```

If the recommended models are missing, propose the one-command fix. Read the model ids from the recommendation registry (`darkmux recommendations show <tier>` if you haven't already), then:
> The recommendation registry says these models should be downloaded for your tier (primary + compactor). Want me to walk you through downloading them? The command is `darkmux model pull-recommended` — it skips already-downloaded models, so safe to run even if one is already present.

**Wait for operator confirmation.** If they say yes, ask them to run:

```bash
darkmux model pull-recommended
```

This may take several minutes per model (multi-GB downloads). After it finishes, re-run `lms ls` to confirm the models appear.

For non-validated tiers, skip this step; tell the operator to download whatever models they want via LMStudio.

## Step 3 — Profile registry

Check whether `~/.darkmux/profiles.json` has profiles:

```bash
darkmux profile list 2>&1 | head -20
```

Outcomes:

**A. Profiles already exist.** Confirm with the operator that the profile their tier's recommendation expects (check `darkmux recommendations show <tier>` for the expected profile name — `balanced` for `m-series-128` today) is present. Move on.

**B. No profiles file.** The operator hasn't run `darkmux init` yet. Propose:
> Your profile registry is empty. The default `darkmux init` would create a starter `~/.darkmux/profiles.json` with placeholder profiles. Want to run it?
>
> ```bash
> darkmux init
> ```

**C. Profiles file exists but doesn't include the recommended one.** Check `darkmux recommendations show <tier>` for the expected profile name + model id. Propose `darkmux profile draft <profile-name> -m <model-id>` and ask the operator to add it to `~/.darkmux/profiles.json`.

## Step 4 — Validate the swap path

Once profiles + models are in place, ask the operator to run:

```bash
darkmux swap recommended --dry-run
```

This resolves the active hardware tier → validated profile → pre-flight-checks all models are downloaded → shows what the swap would do. **No actual model loads happen with `--dry-run`.**

Three outcomes:

**A. Dry-run succeeds.** The recommendation path is wired up. Ask if the operator wants to swap for real:
> Looks clean. Drop the `--dry-run` flag to actually load the recommended models?

**B. Dry-run errors with rationale.** A non-validated tier — surface the rationale verbatim, ask the operator to pick a profile manually with `darkmux swap <name>`.

**C. Dry-run errors with missing models.** Back to Step 2.

## Step 5 — Optional: declare the orchestrator

Flow records carry an `orchestrator` field that names the frontier session driving the work (e.g., `claude-code`, `antigravity`). Declaring it gives provenance to every flow record. Propose:

> Want flow records to carry the orchestrator name? It's operator-explicit by design — no auto-detection. Export this in your shell rc:
>
> ```bash
> export DARKMUX_ORCHESTRATOR=claude-code
> ```
>
> Replace `claude-code` with the harness driving this session (e.g. `claude-code`, `antigravity`, `cursor`). Doctor will surface a warning until it's declared.

## Step 6 — Optional: enable the compliance substrates

Two opt-in environment variables enable the heavier substrates:

- `DARKMUX_AUDIT_DIR` → enables AuditFileSink (hash-chained tamper-evident log; #163)
- `DARKMUX_REDIS_URL` → enables RedisSink (coordination substrate for cross-machine work; #162 Phase 3)

These are out of scope for first-time bootstrap. **Sibling skills:**

- `/darkmux-enable-audit` (shipped, #177): walks through the compliance/audit posture. The operator can invoke it directly after bootstrap.
- `/darkmux-enable-redis`: tracked in #178; not yet implemented.

After bootstrap, point the operator at `/darkmux-enable-audit` for the audit substrate, and at the README's environment-variables table plus #178 for Redis coordination. Don't suggest invoking `/darkmux-enable-redis` yet; it isn't installed and won't be found.

## Step 7 — Final validation

```bash
darkmux doctor
```

Walk through the output with the operator. Each `⚠` or `✗` line should now have a clear next-step. If everything is `✓` or `ⓘ`, the bootstrap is complete — confirm with the operator and let them get to work.

**Don't auto-mutate state on `⚠` lines.** Surface the hint, propose the command, let the operator decide. This is the same posture as every other step.

## Closing

Report the final state to the operator in one sentence:
> Bootstrap complete — your machine is on tier `<tier>`, profile `<active>`, with `<N>` doctor checks green and `<M>` warnings remaining. Run `darkmux doctor` any time to re-verify, or `/darkmux-status` for a quick look at what's loaded.
