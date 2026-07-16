---
name: darkmux-bootstrap
description: First-time darkmux setup walkthrough. Detects the operator's hardware tier, helps author + confirm the profile registry and orchestrator declaration, and validates end state with `darkmux doctor`. Read + propose pattern — operator runs the commands; the skill confirms each step took effect. Run this once on a fresh machine, or any time `darkmux doctor` surfaces structural drift.
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

## Step 1 — Detect hardware tier

```bash
darkmux doctor 2>&1 | grep -E "platform|machine_id"
```

Look for:
- `platform` line names the operator's hardware tier (`m-series-128`, `m-series-64`, `m-series-32`, `generic`)
- `machine_id` line names the operator's machine identifier

If doctor errors on the platform check, stop and surface the error. Otherwise report the tier, then help the operator get a profile in place for it. Model selection is theirs; they pick from what LMStudio offers. Two ways forward:

- **Scan + suggest**: run `/darkmux-scan-and-suggest` (sibling skill), which walks through the `lms ls` catalog and proposes profile shapes for each downloaded model.
- **Draft a profile manually**: pick a model from `lms ls`, then run `darkmux profile draft <name> -m <model_key> -t mid` to generate a starter profile JSON. Copy the output into `~/.darkmux/profiles.json` and tune.

Continue with the operator's tier.

## Step 2 — Check what models are downloaded

See what's on disk so the operator can pick from real options when authoring a profile:

```bash
lms ls | head -50
```

If the models the operator wants aren't present, they download them via the LMStudio UI (or `lms get <model>`). Re-run `lms ls` to confirm they appear.

## Step 3 — Profile registry

Check whether `~/.darkmux/profiles.json` has profiles:

```bash
darkmux profile list 2>&1 | head -20
```

Outcomes:

**A. Profiles already exist.** Confirm with the operator that a profile suited to their tier is present, then move on.

**B. No profiles file.** The operator hasn't run `darkmux init` yet. Propose:
> Your profile registry is empty. The default `darkmux init` would create a starter `~/.darkmux/profiles.json` with placeholder profiles. Want to run it?
>
> ```bash
> darkmux init
> ```

**C. Profiles file exists but doesn't cover the model the operator wants.** Propose `darkmux profile draft <profile-name> -m <model-id>` (pick a model id from `lms ls`) and ask the operator to add it to `~/.darkmux/profiles.json`.

## Step 4 — Validate the dispatch path

Once profiles + models are in place, confirm a dispatch can load them and run. A dispatch loads whatever models its profile declares, under the resident RAM budget:

```bash
darkmux lab run quick-q
```

This runs a single-turn smoke prompt through the internal runtime, loading the active profile's models on the way.

Three outcomes:

**A. The run completes.** The dispatch path is wired up. Move on.

**B. It errors on a missing model.** Back to Step 2: download the model first.

**C. It errors on RAM headroom.** Trim the profile's context length so the loadout fits the resident budget, then re-run.

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
