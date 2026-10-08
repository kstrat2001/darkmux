# Rules schema versioning

Claude Code loads this file when it works in this directory. It holds the rules for the code here, moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line pointer to it.

## Versioning — rules schema

The `eureka` rules engine versions its emitted definitions (`RuleDef`s) with plain semver applied to the rules **data shape** (not to darkmux itself). `RULES_SCHEMA_VERSION` lives in `crates/darkmux-eureka/src/lib.rs` as a single constant.

**Scope today: engine-internal + `darkmux doctor`.** The RuleDefs are consumed in-process and surfaced by `darkmux doctor`. There is **no viewer consumer yet**: the `instruments.jsonl` sidecar was retired (#557), the flow-stream transport that would carry RuleDefs to the viewer is unbuilt (#657), and the viewer-side rules validation is unbuilt (#12). So there is currently **no viewer-blocking behavior and no `EXPECTED_RULES_SCHEMA_MAJOR` constant**. The semver discipline below governs the data shape for when that transport lands.

| Bump | Meaning |
|---|---|
| **Patch** (`1.0.0` → `1.0.1`) | Fully backward-compatible — a message fix, a threshold tweak that doesn't change semantics, a typo in a `fix_hint`. |
| **Minor** (`1.0` → `1.1`) | Additive — a new rule `kind`, a new optional field on `RuleDef`. A future consumer can SAFELY IGNORE what it can't yet evaluate. |
| **Major** (`1.x` → `2.0`) | Breaking — rename/retype a field, change the `RuleKind` enum encoding, a new required field. |

Rule of thumb when changing the schema:

- Adding a new rule? **Minor bump.**
- Renaming or retyping a field on `RuleDef`? **Major bump.**
- Fixing a typo in `fix_hint`? **Patch bump.**

When the viewer consumer lands (#657 transport + #12 viewer rules validation), this section is where the major-bump UI contract (block stale data, prompt to update) gets defined and the viewer-side version gate gets added in the same PR. Until then there is nothing on the viewer side to bump.
