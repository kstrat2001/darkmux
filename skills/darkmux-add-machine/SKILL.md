---
name: darkmux-add-machine
description: Walk an operator through joining a new Mac to an existing darkmux fleet. Read + propose pattern — operator runs every command. Configures DARKMUX_MACHINE_ID + DARKMUX_REDIS_URL on the new machine, registers it in the operator's other-machines' rosters, and runs a smoke test to confirm cross-fleet flow records land. Sibling to darkmux-bootstrap (first-time setup); use this when the operator already has a fleet running and is adding the Nth machine. (#176)
user_invocable: true
allowed-tools: "Bash(darkmux:*),Bash(lms:*),Bash(echo:*),Bash(env:*),Bash(ls:*),Bash(ping:*),Bash(nc:*),Read"
---

# Darkmux add machine

This skill walks the operator through adding a new Mac to an existing darkmux fleet. It's the sibling of `darkmux-bootstrap` (first-time setup) — use this skill when:

- The operator already has 1+ machines running darkmux successfully
- A coordinator (Redis-running machine) is reachable on the operator's tailnet
- The operator wants this new machine to participate in cross-fleet dispatch / observability

The skill is **read-and-propose** throughout — operator runs every state-mutating command. The skill surfaces the implications and validates the configuration.

## Important — what this skill assumes about the operator's fleet

Before walking through join, confirm the operator's mental model lines up with what's about to happen:

- **Per-machine roster.** Each machine has its OWN `~/.darkmux/fleet.json`. Adding this new machine to the fleet means: (a) configure this machine's env vars + roster to know about its peers, AND (b) run `darkmux machine add <this-machine-id>` on EACH of the operator's other existing machines so they see it too. Cross-machine roster replication is filed as [#280](https://github.com/kstrat2001/darkmux/issues/280) but not yet shipped — for now, the per-machine roster is the operator's hand-managed reality.
- **Tailnet trust boundary.** darkmux assumes everyone reachable on the same `DARKMUX_REDIS_URL` is the same operator. No per-machine auth beyond the mesh VPN (Tailscale, etc.). See [README — "Who darkmux is for"](https://github.com/kstrat2001/darkmux#who-darkmux-is-for).
- **Single global work stream (#590).** All fleet work routes onto one stream (`darkmux:work`); the first available runner claims any job. There's no machine-capacity tier to declare — a `--machine <id>` hint is advisory only.

Tell the operator these points up-front, then continue.

## Step 1 — Confirm prerequisites on this new machine

```bash
darkmux --version
lms --version
darkmux doctor 2>&1 | head -30
```

What we need:

- `darkmux --version` returns. If not, point the operator at the README Quick Start; pause this skill until `cargo install --path .` produces a working binary.
- `lms --version` returns. If not, install LMStudio first.
- `darkmux doctor` runs and prints a banner. It will likely report several `⚠` lines (Redis not configured, etc.) — that's expected pre-join state. We're about to fix those.

Report the state back to the operator; don't continue until darkmux + lms are both alive.

## Step 2 — Get the fleet coordinator details from the operator

Ask: *"Which machine is your darkmux fleet's hub (the Redis-running machine)?"*

Need from the operator:

- **Coordinator's reachable address** — usually a tailnet IP (`100.x.y.z`) or a Tailscale Magic DNS name (e.g. `studio.your-tailnet.ts.net`).
- **Redis URL** — typically `redis://default:<password>@<coord-addr>:6379`. The operator should have this from their bootstrap on the coordinator; encourage them to use the existing value verbatim.
- **Existing fleet machine ids** — so we can pick a non-colliding id for this new machine. Operator can run `darkmux machine list` on their coordinator to print these; or if this skill has reachable Redis, `XRANGE darkmux:flow - + COUNT 1000` would show recent provenance fields.

If the coordinator is reachable from this machine right now:

```bash
ping -c 2 <coord-addr>
nc -zvw1 <coord-addr> 6379
```

Both should succeed. If `nc` says "refused" or times out — the operator's tailnet routing isn't set up correctly for this machine. Pause the skill; this is a tailnet config issue, not a darkmux one.

## Step 3 — Pick this machine's machine_id

A logical fleet name — short, distinguishable, operator-readable in flow records and the topology UI. Examples: `studio`, `laptop`, `mini-1`, `m5-max`, `office-pi`.

Ask the operator for their proposed id. Cross-check against the existing fleet:

```bash
# If we set DARKMUX_REDIS_URL in this session (don't persist yet), we can
# read recent provenance to see what ids are already in use:
DARKMUX_REDIS_URL=<from-step-2> darkmux flow status 2>&1 | head -20
```

(Or operator runs `darkmux machine list` on their coordinator and reads the existing ids.)

If the proposed id collides with an existing machine — pause; ask for a different one.

## Step 4 — Configure env vars in this machine's shell rc

Tell the operator to add these to `~/.zshrc` (or `~/.bashrc`):

```bash
# darkmux fleet membership — added by /darkmux-add-machine
export DARKMUX_MACHINE_ID=<picked-in-step-3>
export DARKMUX_REDIS_URL=redis://default:<password>@<coord-addr>:6379
# Optional: name this frontier session in flow records
export DARKMUX_ORCHESTRATOR=<harness-name (e.g. claude-code / antigravity / cursor)>
```

`DARKMUX_AUDIT_DIR` is intentionally NOT set here — audit substrate is the `/darkmux-enable-audit` skill's territory. Only run that if the operator wants the compliance posture.

After editing, the operator reloads the shell + verifies the vars are set (presence-only — do NOT print the values, which would expose the Redis password to shell history and screen-share workflows):

```bash
source ~/.zshrc   # or open a new terminal
[ -n "${DARKMUX_MACHINE_ID:-}" ]  && echo "machine_id set" || echo "machine_id UNSET"
[ -n "${DARKMUX_REDIS_URL:-}" ]   && echo "redis_url set" || echo "redis_url UNSET"
```

Both should report "set." Resist the temptation to `env | grep DARKMUX` or `echo $DARKMUX_REDIS_URL` to verify — those commands enter shell history with the Redis password embedded and expose it on screen-share / screen-grab. The full value-bearing verification happens at Step 5 below via `darkmux doctor`, which prints the URL with the password **redacted**.

## Step 5 — Verify with `darkmux doctor`

```bash
darkmux doctor
```

Look for these lines in the output (now `✓` after step 4):

- `machine_id` — should report your id, sourced from `DARKMUX_MACHINE_ID`
- `flow sink health` — Redis sink should now be `✓` (the test connect to the coordinator succeeded; this is the check that covers Redis reachability)

If `flow sink health` is `⚠` — re-check the Redis URL. The most common errors: wrong password, wrong host, tailnet not routing.

## Step 6 — Add this machine to the local roster

```bash
darkmux machine add <this-machine-id> --address 127.0.0.1:8765
```

This registers the new machine in its OWN roster (the daemon on this machine listens on `:8765` by default; the address points at the local daemon).

Verify:

```bash
darkmux machine list
```

Should show one entry — this machine.

## Step 7 — Tell the OTHER machines about this new one

This is the hand-coordinated step the cross-machine state issue ([#280](https://github.com/kstrat2001/darkmux/issues/280)) will close. For now: on EACH of the operator's existing machines, run:

```bash
darkmux machine add <new-machine-id> --address <new-machine-tailnet-addr>:8765
```

The `<new-machine-tailnet-addr>` is THIS machine's Tailscale IP / Magic DNS name (operator can find via `tailscale ip -4` on this machine).

Surface this clearly to the operator:

> Adding a peer to a fleet currently requires running `fleet add` on every existing fleet member's machine. Cross-machine roster replication is filed as #280 and will close that loop. For now, walk over to each of your other Macs and run `fleet add <this-id> --address <addr>:8765` once.

## Step 8 — Smoke test: cross-fleet flow record

Confirm this machine writes flow records that the rest of the fleet sees:

```bash
darkmux flow note --text "hello from $(echo $DARKMUX_MACHINE_ID)"
```

(If `darkmux flow note` doesn't exist on the operator's darkmux version, use `darkmux doctor` instead — any darkmux command that writes a flow record works for the smoke.)

Then on the operator's coordinator (or any other fleet member):

```bash
# That machine's daemon Redis-aggregates the whole fleet, so the new
# machine's records show up here (needs `darkmux serve` running there):
curl -s "http://127.0.0.1:8765/flow/$(date +%F)"
```

The new machine's `machine_id` should appear in the recent flow records. If yes — the new machine is joined. Run `darkmux flow tail --session <id>` to follow it live, or read `~/.darkmux/flows/$(date +%F).jsonl` directly.

## Step 9 — Optional: start the daemon as a service

For machines that should run `darkmux serve` (the daemon) continuously — typically the always-on member:

- Operator-managed via `launchd` on macOS (the operator's call; not opinionated by darkmux)
- Or: just `darkmux serve` in a foreground terminal for transient use

The skill does NOT auto-create the launchd plist — operator-sovereignty; system services are the operator's territory.

## Idempotency note

If the operator re-runs this skill on an already-joined machine, the early checks (`darkmux doctor` showing `machine_id ✓`, `flow sink health ✓`, etc.) will surface the existing state. The skill should recognize this and ask:

> This machine looks already joined as `<existing-id>`. Did you mean to re-configure, or is this a different machine being added to the same fleet?

If re-configuring, continue from the step the operator names. If a different machine, the operator misread the prereqs — pause and clarify.

## What this skill does NOT do

- **Install darkmux** — that's the Quick Start in the README; this skill picks up after `darkmux --version` works.
- **Set up Tailscale or any other mesh VPN** — that's the operator's tailnet, separate from darkmux. The skill assumes the operator already has a tailnet that reaches the coordinator.
- **Replicate operator state across machines** — profiles, missions, phases, audit logs all stay local. Cross-machine state is the [#280](https://github.com/kstrat2001/darkmux/issues/280) work.
- **Configure LMStudio or download models** — that's a sibling story; the operator does this from their normal LMStudio workflow.

## Composes with

- `darkmux-bootstrap` — first-time setup; use that on the operator's FIRST machine; this skill on every subsequent one
- `darkmux-enable-audit` — opt-in compliance posture (after this skill completes)
- [#280](https://github.com/kstrat2001/darkmux/issues/280) — cross-machine roster replication (will eliminate step 7's manual fanout)
- [#590](https://github.com/kstrat2001/darkmux/issues/590) — single-stream fleet dispatch + the capability-routing successor (replaces the retired tier-aware routing)
