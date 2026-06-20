# Network-health + config-coherence UX — design proposal

**Status:** proposal (react-to, not build-from)
**Author:** ArchitectUX
**Date:** 2026-06-18
**Scope:** grow `darkmux doctor` from single-machine health into a fleet-spanning coherence + remediation surface, with all guidance generated from live state.

---

## 0. The signal that started this

The engineer who *built* darkmux got confused configuring his own two-machine fleet: "the settings and config is a lot to take in." One concept — "the Redis connection" — is smeared across five places: env vars, `~/.darkmux/config.json`, the macOS Keychain, the Homebrew launchd wrapper, and Redis's own `requirepass`. The failure modes that bit him are *cross-setting* — a stale env var masking config.json, a brew/cargo version split-brain, a rotated password that has to match in 3+ places. None of those are visible to any single existing check.

The agreed direction: **guidance generated from live state, not written as prose.** Docs drift and assume one machine shape; a check computed from *this* machine's real state can't drift and adapts per machine. This proposal designs the UX for that.

---

## 1. Current-surface audit (cited)

darkmux already has every primitive this feature needs. The gap is not missing data — it's that the data is split across three commands that don't talk to each other, presented as flat lists, with no cross-setting reasoning and no fleet-spanning verdict.

### 1.1 `darkmux doctor` — the finding/fix structure already exists

- The `Check` struct is exactly the remediation seed we want: `name` / `status` (Pass/Warn/Fail) / `message` / `hint: Option<String>` — `crates/darkmux-doctor/src/lib.rs:39-45`.
- `DoctorReport::worst_status()` already rolls the worst severity up — `lib.rs:53-63`. This is the seed of a single verdict.
- The renderer is a **flat list of every check** with a one-line summary footer — `print_report`, `lib.rs:2555-2596`. Today that's ~27 checks (`run()`, `lib.rs:85-122`) plus the eureka rules. Each prints `marker name message` then dimmed `→ hint` lines.
- `try_fix` exists but currently wires exactly **one** auto-fix handler (`ctx-window-mismatch`) — `lib.rs:2484-2503`. The framework for operator-confirmed remediation is there; it's barely populated.

**Where it breaks down:** doctor is single-machine and *intra-check* — every check looks at one setting in isolation. There is no check that says "your `DARKMUX_REDIS_URL` env var is masking `config.redis` in config.json," even though both values are individually readable. There is no brew-vs-cargo split-brain check. The provenance hints are good in places (`check_machine_id_resolution` names "from env" vs "from hostname", `lib.rs:901-922`; `check_redis_config` names "password from Keychain", `lib.rs:1021-1032`) but inconsistent — most checks don't show where the value resolved from. And 27+ flat rows with no grouping is precisely the "a lot to take in" the operator named.

### 1.2 `darkmux flow status` — Redis reachability + schema skew already computed

- `collect_status()` already probes Redis (XLEN, oldest/newest, round-trip ms, reachability error), reads disk day-files, and detects **schema skew across the live fleet** — `crates/darkmux-flow/src/status.rs:105-203`. Skew is keyed on schemas observed in the *live Redis stream* (`live_foreign`, `status.rs:140-157`), deliberately ignoring historical disk schemas. This is already a fleet-coherence signal.
- It redacts credentials in the URL before display — `redact_url_creds`, `status.rs:266-280`.
- The doctor already folds this in via `check_flow_sink_health` (`lib.rs:1041-1091`) — one rolled-up row that points at `darkmux flow status` for detail.

**Where it breaks down:** schema skew is detected but framed as a flow-sink warning (`schema_skew_detected` string, `status.rs:156`), not as the *fleet* question "do all my machines agree on version + schema?" The data is right; the framing is single-machine-centric.

### 1.3 `darkmux fleet status [--deep]` — peer reachability + specs, but no coherence

- `cmd_fleet_status` probes each rostered peer's daemon port (TCP, 300ms budget) and, with `--deep`, fetches `/machine/specs` over HTTP (1s budget) — `src/main.rs:2065-2111`.
- The `--deep` table already surfaces per-peer **RAM-free, OS, darkmux version, loaded models** — `main.rs:2192-2243`.
- It already detects the **shared-token mismatch**: a peer answering 401/403 is rendered `auth?` and a footer names the fix (set `DARKMUX_SERVE_TOKEN`) — `SpecsProbe::AuthRequired`, `main.rs:2235-2237` + `2258-2270`.
- The roster is operator-owned hand-editable JSON at `~/.darkmux/fleet.json` — `FleetRoster`, `roster.rs:53-81`.

**Where it breaks down:** `fleet status --deep` *gathers* the per-peer version and the auth signal but does **no coherence reasoning** on them. It will happily print `studio v1.3.1` and `laptop v1.2.0` in adjacent rows and never say "these disagree — schema break possible." It shows reachability but never answers "does this peer actually reach the *hub's* Redis?" (it probes the peer's *daemon port*, not the peer→hub Redis path). The version split-brain — the exact thing that bit the operator — is sitting in two adjacent table cells with no verdict drawn.

### 1.4 Presence — live fleet membership already exists

- `darkmux:presence:<machine_uid>` keys with TTL give "which machines are live right now," and each beat carries the writer's `schema_version` and `display_name` — `crates/darkmux-flow/src/presence.rs:46-67`. The skew check can key on *live* schema, not stream scraping.
- Daemon endpoints already serve this: `/fleet/machines/live`, `/fleet/sessions/live`, `/flow-status`, `/machine/specs`, `/missions` — `crates/darkmux-serve/src/lib.rs:156-167`.

**Where it breaks down:** presence answers "is the machine here" but isn't cross-referenced against the roster ("you have `mini-1` in your roster but it hasn't beaten in 4 minutes") or against version coherence ("the live beat says `mini-1` is on schema 1.9, you're on 1.10").

### 1.5 Styling primitives — ready

`darkmux_types::style` gives `success`/`warn`/`error`/`accent`/`header`/`dim`/`bold`, TTY+`NO_COLOR`-gated, force-off for `--json` — `crates/darkmux-types/src/style.rs:23-79`. The verdict banner and provenance tags below use these directly; no new styling needed.

### Audit summary

| Need | Already in code | Cited |
|---|---|---|
| Finding + remediation shape | `Check { status, message, hint }` | doctor `lib.rs:39-45` |
| Worst-severity rollup | `worst_status()` | doctor `lib.rs:53-63` |
| Operator-confirmed fix framework | `try_fix` / `FixOutcome` | doctor `lib.rs:2471-2503` |
| Redis reachability + round-trip | `probe_redis` | flow `status.rs:285-397` |
| Fleet schema skew | `live_foreign` skew detect | flow `status.rs:140-157` |
| Credential redaction | `redact_url_creds` | flow `status.rs:266-280` |
| Per-peer version + RAM + models | `fleet status --deep` | main `2192-2243` |
| Shared-token mismatch | `SpecsProbe::AuthRequired` | main `2235-2270` |
| Live membership + live schema | presence beats | presence `46-67` |
| Provenance-naming precedent | machine_id "from env"/"from hostname" | doctor `lib.rs:901-922` |

**Conclusion:** this is a *synthesis + framing* feature, not a data-collection feature. The three layers below mostly re-compose primitives that already exist, add the *cross-setting* and *cross-machine* reasoning that's currently absent, and re-present it under one verdict.

---

## 2. Proposed information architecture

### 2.1 The naming/home decision (answered first — it shapes everything)

**Decision: extend `doctor`. Add a `--fleet` flag. Do not create a new top-level verb.**

Rationale, grounded in the operator's existing mental model:

- The operator already has a consistent `<noun> status` family — `flow status`, `model status`, `mission status`, `fleet status` (`main.rs` dispatch). Each answers "what is the state of X." A *new* `darkmux network` or `darkmux coherence` verb would be a fourth thing to learn and would compete with `fleet status`, violating "do not invent a parallel surface."
- `doctor` is already the established home for "is this set up right + here's the fix." It already folds in flow-sink health (`check_flow_sink_health`) — folding in fleet coherence is the same move one level out. The operator already runs `doctor` as session-start housekeeping (per the `mission status` doctrine in CLAUDE.md).
- `status` *describes*; `doctor` *diagnoses and prescribes*. Coherence + remediation is diagnosis. It belongs under `doctor`, not under a `status` noun.

So:

- `darkmux doctor` — unchanged default: this machine's health, but with a new **Layer-1 coherence** section (cross-setting rules) folded into the existing check list, and the new verdict banner on top.
- `darkmux doctor --fleet` — adds **Layer-2 fleet coherence**: cross-machine version/schema agreement, peer→hub Redis reachability, hub→peer presence, secret-chain match. Reuses `fleet status --deep`'s probe machinery and presence.

`fleet status` stays exactly as-is — the *descriptive* topology table. `doctor --fleet` is the *diagnostic* layer that reasons over the same data. Clean division: `status` = "here's what's there," `doctor` = "here's what's wrong and how to fix it."

`--fix` keeps its current meaning (operator-confirmed auto-apply) and **never crosses a machine boundary** — see §3.4 and the sovereignty constraint.

### 2.2 The verdict — one line that reads at a glance

The hard constraint is "don't drown the operator in dozens of checks." Today `print_report` shows all 27. The fix is a **verdict banner** at the top that collapses everything into one of three states, with a one-line "headline finding" — the single most important thing — and a count. Detail stays available below, but the operator reads the banner first.

```
darkmux doctor — this machine (laptop)

  ╔═══════════════════════════════════════════════════════════════╗
  ║  ⚠  DEGRADED — workable, but 2 things need a look              ║
  ╚═══════════════════════════════════════════════════════════════╝

  Headline:  A stale DARKMUX_REDIS_URL in your shell is masking
             config.json — swaps + flow records go to the OLD Redis.

  18 ok   ·   2 needs-attention   ·   0 broken          (details below ↓)
```

Verdict vocabulary — three words, chosen to map onto the existing `Status` enum without inventing a fourth state:

| Verdict | Maps to | Banner color (style.rs) | Meaning |
|---|---|---|---|
| **HEALTHY** | worst = Pass | `success` (green) | nothing needs you |
| **DEGRADED** | worst = Warn | `warn` (yellow) | works today, but a latent problem will bite (the masking case, the version skew) |
| **BROKEN** | worst = Fail | `error` (red) | won't work end-to-end until fixed |

The **headline** is computed: pick the highest-severity finding, and within a tie, prefer the one with the broadest blast radius (a masking env var or version split-brain affects everything; a missing orchestrator label affects only provenance). This is `f(findings, ranked)` — generated, not written.

`darkmux doctor --quiet` (new) prints **only** the banner + headline + counts — the 4-line glance for the operator who just wants the verdict. The full list is the default; `--quiet` is the at-a-glance-only mode.

### 2.3 The per-finding remediation block — actionable, copy-pasteable, provenance-bearing

This is the heart of "generated, not written." Today a `Check` renders `marker name message` + dimmed hint. The proposal upgrades the *rendering of Warn/Fail findings* to a structured block that always carries three things: **what's true now (with provenance)**, **why it's a problem**, and **the exact command to fix it**. Pass findings stay one-liners (no need to expand what's fine).

The masking case — the exact thing that bit the operator — renders like this:

```
  ⚠  redis coherence            stale env var is masking config.json

     Now:   DARKMUX_REDIS_URL = redis://…@100.64.0.1:6379   ← from shell env
            config.redis.host = 100.64.0.9                  ← from config.json
            (env wins, read live — config.json is being ignored)

     Why:   `darkmux swap` and every flow record go to 100.64.0.1, not the
            100.64.0.9 your config.json names. If that env var is left over
            from an old hub, your records are landing on a machine you think
            you decommissioned.

     Fix:   # if the env var is the stale one, drop it from your shell rc:
            unset DARKMUX_REDIS_URL
            # …or if 100.64.0.1 is correct, move it into config.json so it
            # survives a new shell:  darkmux config set redis.host 100.64.0.1

     Resolved from:  env(DARKMUX_REDIS_URL) > config.json > default
                     ▲ active tier
```

Design rules for the block (all enforce operator sovereignty):

1. **Every value shows its source.** `← from shell env`, `← from config.json`, `← from Keychain (darkmux-redis)`, `← from hostname`. This is the precedent already set by `check_machine_id_resolution` (`lib.rs:901-922`), made universal. "The operator never has to wonder where a decision came from" becomes literal in the output.
2. **The precedence chain is printed when masking is involved**, with the active tier marked. The operator sees *why* one value won. This is the single most-requested clarity from the original signal.
3. **The Fix is copy-pasteable and offers the operator the choice**, never picks for them. The masking fix gives both branches ("if the env var is stale… / if it's correct…") because darkmux *cannot know* which the operator intended — that's a sovereignty call. It suggests; the operator decides.
4. **Secrets are never printed** — redacted via the existing `redact_url_creds` (`status.rs:266-280`). The block shows `redis://…@host`, never the password. Fix commands that store secrets emit the `security add-generic-password …` form with **no value inlined** (matching `check_redis_config`'s hint, `lib.rs:1031`).

The brew/cargo split-brain finding (new Layer-1 rule) renders:

```
  ⚠  install coherence          two darkmux binaries, different versions

     Now:   interactive PATH → ~/.cargo/bin/darkmux      v1.3.1   ← cargo install
            launchd daemon    → /opt/homebrew/bin/darkmux v1.2.0   ← brew (plist)
            (your `darkmux serve` daemon is running the OLDER binary)

     Why:   The daemon writes flow records + serves the viewer on v1.2.0's
            schema while your interactive `darkmux swap` runs v1.3.1. Schema
            skew between them can make the viewer reject the daemon's records.

     Fix:   # pick ONE install method. To make brew match your cargo build:
            brew upgrade darkmux            # → 1.3.1, then restart the daemon
            # …or remove the cargo build and standardize on brew:
            cargo uninstall darkmux
```

This finding is computed by resolving `which -a darkmux`, reading each binary's `--version`, and comparing against the launchd plist's hardcoded path — all live state, no prose.

### 2.4 The fleet-coherence view (`doctor --fleet`)

Layer 2. Reuses `fleet status --deep`'s probe machinery (`fetch_machine_specs`, `main.rs:2287`) and presence (`read_live`, `presence.rs:99`), but *draws verdicts* over the gathered data instead of just tabulating it. Structure: a fleet verdict banner, then a coherence matrix, then per-finding blocks for anything not-green.

```
darkmux doctor --fleet — 2 machines (this: laptop)

  ╔═══════════════════════════════════════════════════════════════╗
  ║  ⚠  FLEET DEGRADED — machines disagree on version              ║
  ╚═══════════════════════════════════════════════════════════════╝

  Headline:  studio is on darkmux v1.2.0, laptop on v1.3.1 — the
             hub is writing an older flow schema than this machine reads.

  Coherence matrix
  ─────────────────────────────────────────────────────────────────
  MACHINE   ROLE   PRESENCE     VERSION   SCHEMA   REACHES HUB REDIS
  laptop    peer   ● live       1.3.1     1.10     ✓ auth ok
  studio    hub    ● live       1.2.0  ⚠  1.9   ⚠  — (is the hub)
  mini-1    peer   ○ 4m silent  ?         ?        ✗ refused
  ─────────────────────────────────────────────────────────────────
  ROLE detected from: studio runs Redis + the always-on daemon → hub.

  Findings ↓

  ⚠  version coherence          hub is behind this machine

     Now:   laptop  1.3.1  (schema 1.10)   ← this machine
            studio  1.2.0  (schema 1.9)    ← from studio's presence beat
            mini-1  unknown                ← no live beat for 4m

     Why:   The hub (studio) writes flow records on schema 1.9; this
            machine's viewer expects 1.10. Records may be rejected or
            rendered as `unknown`. A major-schema gap can break the stream.

     Fix:   # on studio (the hub), bring it up to match — run THERE:
            brew upgrade darkmux && darkmux serve --restart
            # darkmux can't push this for you; run it on studio yourself.

  ✗  peer presence              mini-1 in roster but silent

     Now:   roster has `mini-1` → 100.64.0.7   ← from fleet.json
            last presence beat:  none in the last 4m
            daemon port probe:   ✗ connection refused

     Why:   mini-1 is declared but not reachable. Either its daemon is
            down, it's offline, or the tailnet route changed.

     Fix:   # check mini-1 is up and its daemon is running (run on mini-1):
            darkmux serve
            # …or if mini-1 is gone for good, drop it from the roster:
            darkmux fleet remove mini-1
```

The matrix is the fleet analog of the per-machine verdict — five columns, each a coherence axis (presence / version / schema / hub-reachability / auth), each cell green/yellow/red at a glance. The matrix is the "don't drown me" surface; the finding blocks below are the drill-down. **darkmux's machine role (hub vs peer) is detected, not configured** — a machine running Redis + the always-on daemon is the hub. The matrix names how it decided ("ROLE detected from…") so the inference is auditable.

Critically, **every fleet fix names the machine it must run on and states darkmux won't run it remotely** ("run THERE," "darkmux can't push this for you"). That's the operator-sovereignty line drawn in the UX itself.

### 2.5 The peer-networking flow — state-driven, not a static recipe

The original `always-on-hub.html` / `fleet.html` is being replaced precisely because a static recipe drifts and assumes one machine shape. The replacement is `darkmux doctor --fleet` run on a *new* peer that isn't joined yet: doctor reads the machine's *current* state and prints **only the next missing step**, then re-running it advances. This is the `darkmux-add-machine` skill's read-and-propose pattern, made into a live state machine instead of a linear doc.

State 0 — fresh peer, nothing configured:

```
darkmux doctor --fleet — this machine (macbook-2) is NOT in a fleet yet

  ╔═══════════════════════════════════════════════════════════════╗
  ║  ○  STANDALONE — this machine isn't joined to a hub            ║
  ╚═══════════════════════════════════════════════════════════════╝

  To join an existing fleet, darkmux needs THREE things. You have 0/3.
  Run the step below, then re-run `darkmux doctor --fleet` to advance.

  ☐  1. Point at the hub's Redis        ← NEXT
        You're missing the hub connection. On the hub, find its address
        (it's the machine running `brew services list | grep redis`).
        Then here:
            darkmux config set redis.host <hub-tailnet-addr>
            darkmux config set redis.enabled true

  ☐  2. Share the hub's bearer token    (after step 1)
  ☐  3. Give this machine a fleet name  (after step 1)
```

State 1 — Redis pointed, but auth fails (re-run advances + diagnoses):

```
  ◑  JOINING — 1/3, and the hub is refusing this machine

  ☑  1. Redis host set → 100.64.0.9     ← from config.json
  ☐  2. Share the hub's bearer token    ← NEXT
        Connected to the hub's Redis address, but it refused auth
        (NOAUTH / WRONGPASS). The hub has a password this machine isn't
        sending. Get the SAME password the hub uses and store it here:
            security add-generic-password -U -a "$USER" -s darkmux-redis -w
        (paste the hub's Redis password at the prompt — it's never echoed)

  ☐  3. Give this machine a fleet name  (after step 2)
```

State 3 — all green, with a live smoke confirmation:

```
  ●  JOINED — macbook-2 is live in the fleet (3/3)

  ☑  1. Redis host → 100.64.0.9          ← from config.json
  ☑  2. Bearer token present             ← from Keychain (darkmux-redis)
  ☑  3. Fleet name → macbook-2           ← from DARKMUX_MACHINE_ID

  Smoke test:  wrote a presence beat → the hub sees `macbook-2` live ✓
               wrote a test flow record → landed on the hub's stream ✓

  You're in. `darkmux doctor --fleet` from the hub now shows this machine.
```

Why state-driven beats a recipe: the checkboxes are computed from live state each run, so the doc *can't* drift (there's no doc), and it adapts to *this* machine (a peer that already has a name skips step 3; a machine that's actually the hub gets a different state-0 entirely — "this machine looks like it should be the hub; here's how to make it always-on"). The "NEXT" marker means the operator never has to figure out ordering — the state machine sequences it. Each step's fix is copy-pasteable and provenance-bearing, same rules as §2.3.

---

## 3. How this composes with darkmux doctrine

### 3.1 Operator sovereignty
- **Diagnose + suggest, never push.** Every fleet fix names the machine it runs on and says darkmux won't run it remotely (§2.4). No config is ever written to a remote machine; no secret is ever moved. `--fix` (§3.4) is local-only.
- **Provenance on every value.** The `← from …` tags and the printed precedence chain make "where did this come from" literal. Extends the existing `check_machine_id_resolution` precedent (`lib.rs:901-922`) to every finding.
- **Choices, not decisions.** The masking fix offers both branches; darkmux can't know which the operator intended, so it doesn't pick.

### 3.2 KISS / personal-fleet scale
This is "fleet doctor," not a control plane. It *reads* state from a few Macs over a tailnet (reusing the existing 300ms/1s bounded probes, `roster.rs:13` + `main.rs:2303`) and reasons over it. It never orchestrates, never pushes, never holds a daemon of its own. No Ansible, no agent, no central authority — just a smarter read.

### 3.3 Generated-not-written
Every finding is `f(live state, versioned rules)`. The rules live where the eureka rules already live (`RULES_SCHEMA_VERSION`, `crates/darkmux-eureka/src/lib.rs`) so they version with the code and track it. There is no prose artifact to drift — the replaced `always-on-hub.html` recipe becomes the state-driven §2.5 flow. New cross-setting/cross-machine rules are *minor* bumps to the rules schema (additive — a new rule a future consumer can ignore), per the versioning table in CLAUDE.md.

### 3.4 `--fix` stays local and confirmed
`try_fix` (`lib.rs:2484`) only ever touches *this* machine's state, only for rules with a registered handler, only after the operator passes `--fix`. Fleet findings (§2.4) are **never** auto-fixable — they require a command on another machine, which crosses the sovereignty line. They render with a Fix block but no `--fix` handler. This must be explicit in the UX: a fleet finding's Fix block omits the "`darkmux doctor --fix` can apply this" affordance that a local finding shows.

### 3.5 Web viewer (secondary)
The daemon already serves `/flow-status`, `/fleet/machines/live`, `/fleet/sessions/live`, `/missions` (`serve/lib.rs:156-167`), and the viewer already has a live fleet view + missions lens. **Recommendation: add a "coherence" lens to the existing fleet view, not a new page.** It renders the §2.4 matrix from a new `/fleet/coherence` endpoint (which is just the fleet-layer verdict computed server-side and returned as JSON — the same data `doctor --fleet --json` produces). The viewer is read-only by nature, which fits the diagnose-don't-push constraint perfectly: it can *show* the version split-brain and the silent peer, but the Fix commands stay copy-only (the operator runs them in a terminal on the named machine). CLI stays the primary surface; the viewer is the ambient "is my fleet coherent right now" glance. This is a follow-on, not part of the first cut.

---

## 4. What's genuinely new vs. re-composed

| Layer | New code | Re-composed |
|---|---|---|
| Verdict banner + headline ranking | banner render + headline picker | `worst_status()`, `style::*` |
| L1: env-masks-config rule | the cross-setting comparison + precedence print | `config_access` resolution tiers (already live) |
| L1: brew/cargo split-brain rule | `which -a` + plist-path read + version compare | `parse_semver`, `--version` |
| L1: provenance on every finding | universal `← from` tagging | `check_machine_id_resolution` precedent |
| L2: fleet verdict + coherence matrix | matrix render + per-axis verdict logic | `fleet status --deep` probes, presence, schema skew |
| L2: peer→hub Redis reachability | probe peer→hub (vs today's peer-daemon probe) | `probe_redis`, bounded-connect wrapper |
| L2: role detection (hub vs peer) | the heuristic + "detected from" provenance | presence beats, Redis presence |
| Peer-networking state machine | the 0/1/2/3-state computed checklist | `darkmux-add-machine` skill logic, config_access |
| Viewer coherence lens | `/fleet/coherence` endpoint + lens | existing fleet view, `/flow-status` |

The bulk is re-composition. The genuinely new work is the *cross-setting* and *cross-machine reasoning* — which is exactly the gap the operator's confusion exposed.

---

## 5. Open questions / decisions for the operator

1. **Verdict vocabulary.** Proposed HEALTHY / DEGRADED / BROKEN (maps cleanly to Pass/Warn/Standalone+Fail). Alternatives: "ok / needs-attention / broken" to match the existing flow-status `ok/warn/fail` lowercase markers (`status.rs:472-476`). Which reads better to you — the punchier all-caps, or consistency with flow status's existing vocabulary?

2. **Default output verbosity.** Proposed: full check list stays the default, `--quiet` gives banner-only. Or flip it — make the **banner + only-the-not-green findings** the default, and require `--verbose` to see all 27 passing rows? The flip is arguably more KISS (the operator usually only cares about what's wrong), but it hides the reassuring "everything's fine" wall. Your call on which is the better default.

3. **`doctor --fleet` reach for cross-machine version/schema.** Two ways to learn a peer's version: (a) the live **presence beat** (already carries `schema_version`, `presence.rs:56` — cheap, but only "version per schema," and needs the peer beating into Redis), or (b) the **`/machine/specs` HTTP probe** (already carries `darkmux_version`, `main.rs:2206` — richer, but needs the daemon reachable + the shared token). Presence is the lighter path and degrades gracefully (a silent peer reads `unknown`). Prefer presence-first with specs as enrichment, or always do the deep HTTP probe?

4. **Role detection vs. operator declaration.** Proposed: detect hub-vs-peer from "runs Redis + always-on daemon." But that's an inference, and §3.1 says don't infer what the operator can declare. Should "this machine is the hub" instead be an explicit `config.json` field the operator sets (with detection as a fallback + a doctor nudge when they disagree)? This is the one place the proposal might be over-inferring — worth your read.

5. **Scope of the first cut.** Recommend shipping **Layer 1 (machine coherence) + the verdict banner** first — it directly answers the operator's own confusion (the masking case, the split-brain) and needs no cross-machine probing. Layer 2 (fleet) and the viewer lens follow once Layer 1's finding-block format is proven. Agree with that sequencing, or do you want the fleet layer in the first cut because the two-machine setup is the live pain?

6. **Where does `darkmux config set` come from?** Several fix blocks suggest `darkmux config set redis.host …`. Does that verb exist yet? If not, the fixes either (a) tell the operator to hand-edit `~/.darkmux/config.json` (honest, fits the "operators hand-edit JSON" doctrine), or (b) this feature motivates adding a `config set` verb. Flag — the proposal assumes hand-edit is acceptable as the fallback.
