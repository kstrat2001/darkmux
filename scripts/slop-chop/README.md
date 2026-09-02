# slop-chop — wide findings, focused mods

The refactoring-swarm program (#2212): **detection is mechanical, judgment is a
short list, remediation is bounded and mechanically gated.** This directory
holds the mechanical stages and the draft mission that will string them
together. The first rule is `unnamed-predicate` (#2206) — a compound condition
that encodes a domain idea but has no name and cannot be tested on its own.

## What is here

| file | stage | status |
|---|---|---|
| `survey.mjs` | A — AST pass: find `&&`/`\|\|` condition sites, decompose to distinct operands, classify purity, compute the truth table | **runnable**, tested |
| `oracle.mjs` | B — recompute each site's truth table from the ORIGINAL expression text, independently of the survey (#2207) | **runnable**, tested |
| `survey.test.mjs` | `node --test scripts/slop-chop/*.test.mjs` (runs in CI's ui job) | — |
| `slop-chop-mission.draft.json` | the five-phase mission (survey → triage → extract → gate → assemble) | **DRAFT — not registered, not runnable**; its own `_gaps` block says why |

The registered rule lives at `templates/builtin/rules/unnamed-predicate.json`
and is embedded in the binary like every other builtin rule
(`crates/darkmux-crew/src/rules.rs`).

## Running the mechanical stages by hand

```bash
cd ui && bun install            # typescript is resolved from ui/node_modules
node scripts/slop-chop/survey.mjs $(find ui/src -name '*.ts' -o -name '*.tsx') --strip ui/src/ --out /tmp/sites.json
node scripts/slop-chop/oracle.mjs --sites /tmp/sites.json --out /tmp/oracles.json
```

`find` rather than `**`: bash without `globstar` expands `**` as `*` and
silently under-surveys (89 files instead of 163 on `ui/src`).

`survey` prints the site census (by operand count, purity, and the
provably-equivalent clusters) and writes the **qualifying** sites — 3+ distinct
operands, nothing mutating. `oracle` exits non-zero if its reading of any
expression disagrees with the survey's: the two are independent by design.

## Why the draft mission is not registered

Mission configs are embedded from an explicit list, so an unlisted file is
inert; this one stays out of `templates/builtin/mission-configs/` until its
`_gaps` are closed — chiefly that the `extract` phase needs an *agentic*
for-each (N container dispatches for N files known only after survey), and
`dispatch.map` is single-shot. That decision — a new Tier 1 kind versus a
strategy inside the existing one — is the operator's (#1352).

## Where the numbers came from

Measured on darkmux `ui/src` on 2026-08-31, BEFORE #2224 extracted eight of
the sites (92 files, 21,515 lines): 197 prefilter hits → 244 AST sites → 46
qualifying. The same command on 2026-09-02 gives 253 → 39; the drop in
qualifying sites is #2224's work, not a regression. Judgment refusal 2/2 on a dense 27B model,
0/4 on a 35B-A3B MoE — dense parameters, not total parameters, do the
discriminating. One extraction passed a seven-check gate with 340 tests green
and shipped as #2224 (six named predicates from eight sites).
