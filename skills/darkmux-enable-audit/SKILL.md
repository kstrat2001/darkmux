---
name: darkmux-enable-audit
description: Guide the operator through enabling AuditFileSink — the BLAKE3 hash-chained append-only sink that runs alongside the casual LocalFileSink. Walks through picking an audit directory, configuring the env var, verifying with `darkmux doctor`, and optionally wiring `darkmux flow integrity-check` into cron. Read + propose pattern; operator runs every state-mutating command. Use when the operator wants flow records on a hash-chained substrate (personal record-keeping, operator-claimed regulatory posture, fleet detection layer).
user_invocable: true
allowed-tools: "Bash(darkmux:*),Bash(lms:*),Bash(echo:*),Bash(ls:*),Bash(env:*),Read"
---

# Darkmux enable audit

This skill walks an operator through turning on **AuditFileSink** alongside the casual `LocalFileSink`. AuditFileSink writes a BLAKE3 hash-chained append-only JSONL log to a separate directory; `darkmux flow integrity-check` walks the chain and **any post-write modification is detectable**. The sink is opt-in via `DARKMUX_AUDIT_DIR`.

The skill is read-and-propose throughout — operator runs every state-mutating command. The decision to enable audit is context-bearing (use case + retention strategy + recovery options); the skill surfaces the options and lets the operator pick.

## Important — what this skill does NOT provide

Before walking through enable, name the boundaries clearly so the operator's mental model lines up with what they're actually getting:

- **Not a compliance certification.** AuditFileSink writes a verifiable hash chain. It does not certify the operator against any regulatory regime (ISO, SOC, HIPAA, AI Act, etc.). Operators in regulated environments compose AuditFileSink with the other layers they need (encryption-at-rest, retention policy, access controls, external timestamping) under their own counsel's guidance. darkmux is MIT-licensed personal OSS; see [DISCLAIMER.md](https://github.com/kstrat2001/darkmux/blob/main/DISCLAIMER.md).
- **Not encrypted at rest.** The audit log lives in plaintext JSONL on disk. Operators who need encryption-at-rest use FileVault (macOS), LUKS (Linux), or a hardware-encrypted volume — that's a filesystem-layer concern, not something darkmux implements.
- **Not tamper-proof.** Hash chaining makes edits *detectable* via `flow integrity-check` — it does not *prevent* them. Append-only filesystem flags (`chflags uappend` on macOS, `chattr +a` on Linux) are the layered defense that prevents writes; they're operator-applied and OS-specific.
- **No retention or rotation policy.** Files accumulate by date indefinitely; the operator owns rotation/purge against their own retention obligations.
- **Not chain-of-custody for legal proceedings.** Court admissibility typically requires external timestamping (RFC 3161 trusted timestamp authority, OpenTimestamps, or equivalent) and custodial controls darkmux does not provide. Detection of tampering is not the same as proof for an adversarial proceeding.

Tell the operator these limits up-front, then ask whether the use case still fits before continuing.

## Step 1 — Confirm prerequisites

```bash
darkmux --version
darkmux doctor 2>&1 | grep -E "flow sink health|audit integrity"
```

The skill needs darkmux installed. (Internally, AuditFileSink uses flow schema 1.5.0 fields; any current darkmux release ships them — `darkmux --version` is informational, no specific version-string match required.) The doctor lines tell us:

- `flow sink health` — is the substrate alive? Should be `✓` regardless of audit being on.
- `audit integrity` — current state. Likely `⚠ no audit files under …` if audit hasn't been enabled yet (this is what we're about to change).

If `flow sink health` is anything other than `✓`, stop the skill and surface the issue — audit composes on top of the casual sink; both need to work.

## Step 2 — Ask about the use case

Three rough buckets shape the audit-dir path recommendation:

- **Personal compliance-curious.** Operator wants the chain for their own peace of mind; no external auditor; no retention requirement. Default `~/.darkmux/audit/` is fine.
- **Regulatory exposure (operator-claimed).** Operator has a real reason — internal audit evidence, vendor due-diligence response (operator's representations, not certification). They probably want the audit dir on encrypted storage (FileVault volume, LUKS partition) AND need to think about retention separately.
- **Fleet deployment.** Multiple machines writing audit independently; the operator may want each machine's audit dir on its own logical volume + an aggregation strategy on top (not in scope for this skill — that's downstream tooling).

Ask the operator which fits. Don't assume. If the operator names a use case the skill doesn't cover (multi-tenant SaaS audit, court-admissible chain-of-custody), pause and tell them this skill won't get them there alone — they need additional layers and counsel review.

## Step 3 — Pick the audit directory path

Based on the use-case answer:

- **Personal** → propose `~/.darkmux/audit/`. No further questions.
- **Regulatory** → ask whether they have an encrypted volume mounted. If yes, propose `<volume-mount>/darkmux-audit/`. If no, propose `~/.darkmux/audit/` AND surface the encryption-at-rest gap as a follow-up they need to address separately.
- **Fleet** → propose `~/.darkmux/audit/` with a `DARKMUX_MACHINE_ID`-suffixed path note: each machine's audit lives under its own dir; aggregation is downstream.

Tell the operator the proposed path. **Wait for confirmation.** If they want a different path, accept it without argument — operator-sovereign.

## Step 4 — Configure the env var

The operator needs `DARKMUX_AUDIT_DIR` set in their shell rc (every shell that runs `darkmux`). The exact incantation depends on their shell:

```bash
# zsh (macOS default):
echo 'export DARKMUX_AUDIT_DIR="$HOME/.darkmux/audit"' >> ~/.zshrc

# bash:
echo 'export DARKMUX_AUDIT_DIR="$HOME/.darkmux/audit"' >> ~/.bashrc

# fish:
echo 'set -gx DARKMUX_AUDIT_DIR "$HOME/.darkmux/audit"' >> ~/.config/fish/config.fish
```

Replace `$HOME/.darkmux/audit` with whatever path Step 3 settled on.

**Tell the operator to run the command themselves** (it appends to their rc file — operator state). After they run it, ask them to start a new shell (or `source` the rc) so the variable is exported in the current session.

Confirm with:

```bash
echo "DARKMUX_AUDIT_DIR=${DARKMUX_AUDIT_DIR:-unset}"
```

Should print the resolved path. If it prints `unset`, the rc didn't reload — ask them to open a fresh terminal tab.

## Step 5 — Trigger a first audit-record write

The audit dir + file are created on first write. Trigger one with a benign flow note:

```bash
darkmux flow note --text "audit substrate enable smoke"
```

Should print one line (the stderr `flow: AuditFileSink enabled — audit_dir=… (hash-chained, flock-serialized)` notice on first invocation of the new sink). Verify the file landed:

```bash
ls "${DARKMUX_AUDIT_DIR}"
# Should show: <YYYY-MM-DD>.jsonl (today's date in UTC)
```

If the file isn't there, the env var isn't propagating to `darkmux` invocations — debug by running `env | grep DARKMUX_AUDIT_DIR` in the same shell.

## Step 6 — Verify the chain validates

```bash
darkmux flow integrity-check
```

Should print:

```
✓ valid  ~/.darkmux/audit/<YYYY-MM-DD>.jsonl  (1 record(s))
```

(Linux fleet operators will see `/home/<user>/.darkmux/audit/…`; macOS shows `/Users/<user>/…`.)

The exit code is `0` on all-chains-valid. Run `echo $?` to confirm if the operator wants to verify they could wire this into a script.

Then re-run doctor:

```bash
darkmux doctor 2>&1 | grep "audit integrity"
```

Should now show `✓ audit integrity   1 file(s), 1 record(s), all chains pass the integrity walk at this check`.

## Step 7 — Optional: wire integrity-check into cron / CI

`darkmux flow integrity-check` exits with **status 2** when any chain is broken. This is the contract that lets cron / CI / monitoring detect tampering without parsing output.

Propose a sample cron entry that alerts on broken chain (operator decides whether to add it):

```bash
# Daily at 06:00 — alerts via mail when any chain breaks.
# Replace `mail -s ...` with whatever notification path fits.
# Replace the darkmux path with what `which darkmux` shows on your machine
# (cargo install puts it at ~/.cargo/bin/darkmux, NOT /usr/local/bin).
0 6 * * * /usr/local/bin/darkmux flow integrity-check >/dev/null 2>&1 || \
    echo "darkmux audit chain broken on $(hostname) at $(date)" | mail -s "darkmux audit alert" you@example.com
```

Tell the operator: this is **optional**, and it runs as the user who owns the crontab — make sure that user has `DARKMUX_AUDIT_DIR` set in their environment (cron doesn't read `.zshrc` by default; either inline the env var into the crontab or source the rc explicitly). Also: cron runs can miss alerts if mail delivery fails — for higher-stakes monitoring use a CI-side check or a monitoring agent that retries on failure.

If the operator runs CI rather than cron, the same exit-code contract works in a GitHub Actions / Jenkins / etc. step — fail the job when `darkmux flow integrity-check` exits non-zero.

## Step 8 — Optional: layered defense (advisory, OS-specific)

For operators who want filesystem-level write prevention (vs detection-only), name the layered options:

- **macOS**: `chflags uappend <file>` makes a file append-only at the FS level. Survives normal `rm` / `mv`; root with `chflags nouappend` can remove. Per-file; would need to apply to each day's audit file as it's created.
- **Linux**: `chattr +a <file>` (requires CAP_LINUX_IMMUTABLE). Same semantics: append-only, root can remove.
- **External**: write the audit dir to a WORM (write-once-read-many) volume — out of scope for darkmux but operator's option.

These are **operator-applied**, not darkmux-managed. Filesystem flags don't compose cleanly with darkmux's `flock`-based concurrent-write strategy; pick one or the other (the chain detection works without the FS flags; the FS flags add prevention but require operator scripting around new-file-creation). The FS-flag layer adds prevention but does not change the detection contract — `flow integrity-check` works either way.

If the operator wants the FS-flag layer, point them at their OS docs — this skill won't write the script for them (it's operator-sovereign + OS-specific enough that a copy-pasteable command would mislead more than help).

## Step 9 — Final report

Report to the operator in one line:

> AuditFileSink enabled — flow records now write to a hash-chained log at `DARKMUX_AUDIT_DIR=<path>`. doctor's `audit integrity` is `✓` at this check; `darkmux flow integrity-check` passes. Run `/darkmux-bootstrap` again any time you want to re-verify the substrate end-to-end.

If they wired cron, add: "Daily integrity check runs at 06:00; if the chain breaks, you'll be notified at the next scheduled run (detection window ~24h)."

Done. The operator has detection coverage on every flow record written from this point forward. Pre-existing records in the casual `LocalFileSink` are unchanged — audit starts from when audit was turned on.
