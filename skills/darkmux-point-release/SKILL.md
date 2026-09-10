---
name: darkmux-point-release
description: Cut a routine darkmux point release (patch or minor) and ship it to the Homebrew tap. Use when work has merged to main since the last tag and you want `brew upgrade darkmux` to pick it up — e.g. "release the new features", "cut a point release", "should we update the tap". MAINTAINER skill (releasing darkmux itself), not an end-user feature; not shipped to brew installs. Do NOT use for a major (X.0.0) bump or a launch — those need the operator's launch-readiness halt, not this routine.
user_invocable: true
allowed-tools: "Bash(git:*), Bash(gh:*), Bash(cargo:*), Bash(curl:*), Bash(shasum:*), Bash(python3:*), Bash(grep:*), Bash(sed:*), Bash(jq:*), Bash(mktemp:*), Bash(bash:*), Read, Edit, Write"
---

# darkmux point release

The standardized "routine point release" dance, so it never has to be re-derived. Cut a **patch** or **minor** release of already-merged, already-gated work and ship it to the Homebrew tap. The pipeline is the one proven by the v1.0.0 cut; this skill encodes it.

**Scope guard — when NOT to use this skill:** a **major** bump (`X.0.0`), a first public launch, or anything needing a launch-readiness review. Those are operator-gated halts, not routine. If the version math below says major, STOP and hand to the operator.

## 0. Preconditions

```bash
git checkout main && git pull --ff-only     # or a worktree off origin/main —
git status --short                          # NEVER mutate a shared checkout
git describe --tags --abbrev=0              # the latest tag, e.g. v1.0.0

# main's CI must be green BEFORE you branch — this is a gate, not a glance:
gh run list --branch main --limit 3 --json workflowName,conclusion,headSha \
  -q '.[] | "\(.conclusion)  \(.workflowName)  \(.headSha[0:7])"'
```

A prose reminder gets skipped under momentum; the command above is the gate. If
any run on main's tip is not `success`, stop — you are about to tag a red main
and only find out at the formula step.

Also confirm there are **zero open PRs you intend to include**. A release cut
while a feature PR is still in flight ships a version number that does not mean
what the changelog says it means.

## 1. Decide the version (semver — stability began at 1.0.0)

List what's merged since the last tag and classify:
```bash
LAST=$(git describe --tags --abbrev=0)
git log "$LAST"..HEAD --oneline --no-merges | cat
```
- **fixes only** (`fix(...)`) → **patch** (`x.y.Z+1`)
- **any additive feature** (`feat(...)`, backward-compatible) → **minor** (`x.Y+1.0`)
- **any breaking change** (rename/remove/retype a public surface, a required field) → **major → STOP**, hand to the operator.

Check the data-shape schemas — a bump there is worth calling out and (if cross-machine) a schema-lock note in the release notes:
```bash
grep 'FLOW_SCHEMA_VERSION: &str' crates/darkmux-flow/src/schema.rs
grep 'RULES_SCHEMA_VERSION' crates/darkmux-eureka/src/lib.rs
```
Pick and **export** `NEW` — the two blocks in step 2 are meant to be saved as
scripts and run with `bash <file>`, a fresh process that inherits nothing
from this shell except its exported environment:
```bash
export NEW=x.y.z   # tag will be vNEW
```
A plain `NEW=x.y.z` (no export) is invisible to `bash <file>`. Each block
below asserts `$NEW` is set before touching anything else, but the assert
only catches a forgotten export here — it can't supply the value.

## 2. Version PR

Bump every workspace crate manifest + the lockfile, write the CHANGELOG section from the merged PRs, and update doc version references.

```bash
git checkout -b release-$NEW
# every workspace Cargo.toml (root + crates/*) version = NEW
python3 - "$NEW" <<'EOF'
import sys, glob
new=sys.argv[1]
import re
old=open('Cargo.toml').read()
cur=re.search(r'^version = "([^"]+)"', old, re.M).group(1)
for f in ['Cargo.toml']+glob.glob('crates/*/Cargo.toml'):
    s=open(f).read()
    if f'version = "{cur}"' in s:
        open(f,'w').write(s.replace(f'version = "{cur}"', f'version = "{new}"', 1))
print('bumped from', cur)
EOF
cargo update --workspace --quiet   # refresh Cargo.lock
./target/release/darkmux --version 2>/dev/null || cargo build --release 2>&1 | tail -1
```

Then by hand (judgment, not scriptable):
- **CHANGELOG.md**: add `## [NEW] - <date>` above `[Unreleased]`/the prior entry. Group merged PRs into Added / Fixed; lead with the headline. End with the `[NEW]: https://github.com/kstrat2001/darkmux/releases/tag/vNEW` link.
- **Doc version refs**: README status banner, `docs/guide/*.html` sample output strings. Final stray-version sweep:
  ```bash
  grep -rn "$LAST_NUM" README.md docs/index.html docs/guide/*.html | grep -v "tag/v$LAST_NUM\|\[$LAST_NUM\]"
  ```
- **⚠ Do NOT touch `packaging/homebrew/darkmux.rb` in this PR — not even its comment block.** Merging anything under `packaging/homebrew/` fires the tap-sync workflow, which would publish a pin that is still pointing at the PREVIOUS tag, racing the real pin from step 4. The formula is touched exactly once per release, in its own PR, *after* the tag exists — it cannot be pinned earlier anyway, because the `sha256` comes from the tag's tarball. (Learned the hard way on a previous release; the sweep above deliberately excludes the formula.)
- **Every release**, regenerate the published demo world (#2032 packet 3 — this is the decided cadence: at release, not continuous, so a demo rebuild never recaptures screenshots for unshipped work): `cd scripts/demo-env && ./build.py && ./serve.py --port <free-port> &` then, once it's up, `./export_static.py --base http://127.0.0.1:<port>`, then from the repo root `bash scripts/build-demo.sh`. This is broader than "only if the viewer changed" — `demo-flow.jsonl`/`demo-runs.json` read as how long ago the demo world was last active, which drifts every release regardless of whether the UI changed, so the fixture regen runs every time and `build-demo.sh`'s narrower index.html regen rides along with it. Full command sequence, what each step writes, and the pre-commit proof checklist live in `scripts/demo-env/README.md`'s own "Refreshing the published demo" section — follow that, don't re-derive it here.

Verify, then ship the PR (mechanical release-prep → external QA skipped, named; CI gates).
**Both blocks below are meant to be saved as a script and run with `bash
<file>`, not pasted line-by-line into an interactive shell** — each wraps its
body in a `( ... )` subshell specifically so a failure path's `exit` ends the
subshell, not the terminal session it's running in; run the block itself
(not its individual lines) so that containment holds:
```bash
(
  set -eu
  # $NEW must be exported from step 1 — this block runs as `bash <file>`, a
  # fresh process, so a plain `NEW=x.y.z` typed into the parent shell never
  # reaches it. Checked with an explicit `if`/`exit` rather than `set -u` or
  # `${NEW:?...}` for TWO reasons, only one of which is the trap swallow:
  #   1. An exported-but-EMPTY `NEW` passes `set -u` cleanly and would build a
  #      `release-` branch. That is this block's live reason — this block now
  #      carries no `trap`, so nounset alone WOULD propagate here.
  #   2. In the merge block below (which does carry traps), a nounset abort
  #      inside a subshell with an EXIT trap is silently swallowed to exit 0 on
  #      bash 3.2 — proven by running that exact shape. `${NEW:?...}` swallows
  #      identically. An ordinary `set -e` failure with the same trap still
  #      propagates, so this is nounset-specific.

  # Bound: 30s x 120 attempts = 60 minutes. Measured fresh against this
  # repo's own last ~60 completed runs per workflow (createdAt -> updatedAt,
  # which includes queue time — the same wall clock the poll experiences):
  # CI's median is ~8 min with a max of ~59 min; quality's median is ~15 min
  # with a max of ~44 min. 60 minutes is roughly 1x the slowest run actually
  # observed, NOT the ~2.7x an earlier version of this comment claimed
  # (#2589 round 3) — the bound itself is still the right call (it sits on
  # the safe side of the slowest real run, and a timeout blocks a merge
  # rather than permitting one), but say so honestly. Raise it if a future
  # release routinely queues past an hour.
  POLL_SECONDS=30
  MAX_ATTEMPTS=120
  ATTEMPTS=0
  while :; do
    set +e
    RESULT=$(gh pr checks release-$NEW --json bucket --jq '
      . as $all
      | if ($all | length) == 0 then "no-checks"
        elif (($all | map(select(.bucket=="pass" or .bucket=="fail" or .bucket=="skipping" or .bucket=="cancel")) | length) == ($all | length)) then "done"
        else "pending"
        end
    ' 2>"$ERRF")
    STATUS=$?
    set -e
    if [ "$STATUS" -ne 0 ]; then
      if grep -qF "no checks reported on" "$ERRF"; then
        RESULT="no-checks"
      else
        echo "gh pr checks failed (exit $STATUS) — not retrying blindly:"
        cat "$ERRF"
        exit 1
      fi
    fi
    # "no-checks" (nothing registered yet) and "pending" both mean keep
    # waiting. "done" requires EVERY reported bucket to be one of the known
    # terminal values (pass/fail/skipping/cancel) — inverted from the old
    # "done unless a bucket literally equals pending" test, which read an
    # unrecognized or MISSING bucket field as done by default. gh's own
    # fallback-to-pending only covers unmapped *state* values; it does not
    # cover a renamed or absent "bucket" key in some future gh version, and
    # that reads here as JSON `null`, which must not be waved through as
    # done (#2589 round 3).
    if [ "$RESULT" = "done" ]; then
      break
    fi
    ATTEMPTS=$((ATTEMPTS + 1))
    if [ "$ATTEMPTS" -ge "$MAX_ATTEMPTS" ]; then
      # (MAX_ATTEMPTS - 1) sleeps actually elapse: the last attempt breaks
      # before sleeping, so quote the real bound rather than the nominal one.
      echo "timed out after ~$(( (MAX_ATTEMPTS - 1) * POLL_SECONDS ))s waiting for checks on release-$NEW"
      exit 1
    fi
    sleep "$POLL_SECONDS"
  done

  # This verdict read is deliberately a SEPARATE call from the poll above,
  # not unified onto one source. TWO divergences, and the second is the
  # bigger one the earlier note omitted:
  #   * CONTENT — `gh pr checks` dedupes re-run check names and folds in
  #     legacy commit statuses; the raw `check-runs` API below does neither.
  #   * COMMIT — the poll keys on the PR's own head (whatever GitHub thinks
  #     the branch tip is), while this read keys on the LOCAL `git rev-parse
  #     HEAD`. A stale local sha yields `no-verdicts` or the older commit's
  #     failures, and both of those block, so no false merge was constructible
  #     — but the two calls are not asking about the same commit by
  #     construction, only by the operator standing on the branch they pushed.
  # Not exploitable on this repo today (#2589 round 4).
  #
  # `per_page=100` plus an explicit count assertion, not the bare default of
  # 30: the check-runs endpoint paginates, a re-run only ever ADDS rows, and
  # a silently truncated read would report success over a subset that
  # happens to all be green while a real failure sits on a page never
  # fetched. This repo's PRs carry ~10 check runs today; 100 is headroom,
  # and the assertion below still catches it if that ever stops being true
  # (#2589 round 3).
  set +e
  RAW=$(gh api "repos/kstrat2001/darkmux/commits/$(git rev-parse HEAD)/check-runs?per_page=100" 2>"$ERRF")
  API_STATUS=$?
  set -e
  if [ "$API_STATUS" -ne 0 ]; then
    echo "gh api check-runs failed (exit $API_STATUS):"
    cat "$ERRF"
    exit 1
  fi
  TOTAL=$(echo "$RAW" | jq '.total_count')
  GOT=$(echo "$RAW" | jq '.check_runs | length')
  if [ "$TOTAL" != "$GOT" ]; then
    echo "check-runs response truncated: total_count=$TOTAL but only $GOT rows returned (per_page=100 wasn't enough) — refusing to judge a partial set"
    exit 1
  fi

  # A nightly-only job (mutation full sweep) legitimately reads "skipped" on
  # every PR run, so the OLD `unique|join(",")` always included it and
  # `[ "$C" = "success" ]` was never true on this repo — a silent no-op
  # before this change, a hard failure on every real release after it
  # (#2589). Filter out conclusions that aren't a verdict (skipped/neutral)
  # before judging — but do NOT filter `null`: a check run's conclusion is
  # null exactly while it's still running (status != "completed"), so
  # dropping it is what let two successes plus a still-in-flight check read
  # as "success" and merge, and — compounded with the poll-side bug above —
  # let a run with five successes, one skipped, and four still-running
  # checks merge as well (#2589 round 3). Map null to a readable
  # "in_progress" instead, so it still fails the all-success test and shows
  # up by name in the non-matching output. Genuine bad conclusions
  # (failure/cancelled/timed_out) are never filtered, so they still block
  # the merge as before. Refuse to merge if nothing verdict-bearing is left
  # either (a run with only skipped jobs proves nothing, so it's not
  # "success").
  C=$(echo "$RAW" | jq -r '
    ([.check_runs[].conclusion]
      | map(select(. != "skipped" and . != "neutral"))
      | map(if . == null then "in_progress" else . end)
    ) as $v
    | if ($v | length) == 0 then "no-verdicts"
      elif ($v | all(. == "success")) then "success"
      else ($v | unique | join(","))
      end
  ')

  if [ "$C" = "success" ]; then
    gh pr merge release-$NEW --squash --delete-branch
    # Separate statements, not `&&` — see the note in the version-PR block.
    git checkout main
    git pull --ff-only
  else
    echo "checks did not all succeed (non-skipped conclusions: $C) — not merging"
    exit 1
  fi
)
```

## 2.5. The dogfood gate — verify the FEATURES on the build you are about to tag

**Operator mandate: no release is cut until darkmux runs REAL AI dispatches
showing this release's FEATURES work** — not merely that the dispatch path
runs. `cargo test` + CI + a trivial smoke are necessary and NOT sufficient:
they exercise the pieces, never the live invocation, and never the behavior.

This step was missing from this skill until 2026-08-13, while CLAUDE.md said it
belonged here — so a release cut by following this file alone skipped it.

```bash
cargo install --path .        # from the RELEASE COMMIT, not your working tree
darkmux --version             # must equal NEW
```

**Make a runtime image available first.** The versioned GHCR image publishes
*at* release, so a pre-release dogfood cannot pull it. If `runtime/` changed,
`docker build -t darkmux-runtime:latest runtime/` from the release commit. If it
did not, the prior release's image is byte-identical — `docker tag <prev>
darkmux-runtime:latest`, and drop the tag afterwards.

**A trivial message is the FLOOR, not the gate.** It proves the container ran
and the loop executed. It does NOT prove the release's features work — #1135
shipped a `dispatch --profile` that silently loaded at LMStudio's 4096 default
instead of the profile's `n_ctx`, and a trivial smoke FITS 4096, so it looked
perfectly healthy while the feature was broken.

**For each feature-bearing change, run a dispatch that EXERCISES it** and check
the expected behavior against ground truth — `lms ps` for the loaded model and
context, the flow records for emitted fields, the envelope for output shape, the
served viewer, `darkmux doctor` for a check firing. Viewer-only and failure-path
features that a happy dispatch cannot reach get their own targeted reproduction
rather than a pass.

**Standard coverage pair**: a coder long-agentic dispatch (`darkmux lab run
long-agentic` on the default coder profile) plus a PR review. Together those
exercise dispatch → container spawn → tool loop → compaction → verify, and the
reviewer → render → post path. A coder run that does not *converge* is a model
finding, not a path failure — the gate is that the machinery ran end to end and
produced an envelope.

**Name the loaded model.** A garbage *response* is a model finding; the path
passed if the container ran and the loop executed.

Why this is load-bearing: **v1.3.x–1.4.0 shipped a completely broken internal
runtime** (`docker docker run`, exit 125 — #975). Every unit test asserted the
docker argv vector; nothing ever constructed and ran the real `Command`, so the
break sailed through four releases of green CI. One dogfood dispatch would have
caught it on the first try.

## 3. Tag + GitHub release

```bash
git tag vNEW $(git rev-parse HEAD) && git push origin vNEW
gh release create vNEW --title "darkmux NEW" --notes-file <(...)   # notes distilled from the CHANGELOG section
```
(The release event triggers the GHCR runtime-image publish workflow — verify in step 6.)

## 4. Formula stable-pin PR (needs the tag's tarball)

```bash
SHA=$(curl -sL "https://github.com/kstrat2001/darkmux/archive/refs/tags/vNEW.tar.gz" | shasum -a 256 | cut -d' ' -f1)
git checkout -b formula-$NEW
# in packaging/homebrew/darkmux.rb: url → .../tags/vNEW.tar.gz ; sha256 → $SHA ; comment vLAST → vNEW
git commit -am "feat(homebrew): pin formula to stable vNEW (url + sha256)"
git push -u origin formula-$NEW
gh pr create --title "feat(homebrew): pin formula to stable vNEW" --body "Stable-pin for vNEW. sha256 from the live tarball. Merge triggers the tap-sync workflow."
# CI-green-gate + merge (same conclusion==SUCCESS pattern as step 2)
```

## 5. Sync the tap

Merging the formula PR fires `.github/workflows/sync-homebrew-tap.yml`, which opens a PR on `kstrat2001/homebrew-darkmux`. Merge it:
```bash
gh run list --workflow sync-homebrew-tap.yml --limit 1 --json status,conclusion
gh pr list --repo kstrat2001/homebrew-darkmux --json number,title
gh pr merge <N> --repo kstrat2001/homebrew-darkmux --squash
```

## 6. Verify

```bash
# formula serves the new tag
gh api repos/kstrat2001/homebrew-darkmux/contents/Formula/darkmux.rb --jq .content | base64 -d | grep -E "url \"|sha256"
# GHCR runtime image published on the release
gh run list --workflow "Publish runtime image" --limit 1 --json status,conclusion
# local dev box: reinstall from source so it matches the tag
cargo install --path . --quiet && darkmux --version
```
On a brew machine (e.g. the Studio): `brew upgrade darkmux` then `darkmux --version` = NEW.

**If no brew machine is reachable, the release still ships — but say what is
unverified.** Everything above (the tap's formula content, the tag tarball's
`sha256`, the GHCR image) can be checked from any machine. What ONLY a brew
machine proves is that the formula actually *builds and runs* from the tap.

Do not substitute a `brew install` on the dev laptop: it collides with the
`cargo install`ed binary on PATH and breaks the setup everything else was
tested against. Instead:

```bash
darkmux machine list          # is the brew machine reachable at all?
```

Defer the check, name it explicitly in the release wrap-up, and record it as
a follow-up task — an unverified step that is *stated* is a known gap; one
that is silently skipped is how a broken formula reaches a user.

## Notes

- **No bare `cargo fmt`** on this repo (not rustfmt-clean to stable; churns ~75 files) — match style by hand, verify via build/clippy/test.
- If the `runtime/` crate changed: `cargo clippy --manifest-path runtime/Cargo.toml --all-targets -- -D warnings` (it's outside the workspace).
- The carve-out for skipping external QA is mechanical release-prep (version strings + changelog prose). Anything with real logic in the batch should already have been QA'd on its own PR.
- This skill releases darkmux itself; it is intentionally NOT in `EMBEDDED_SKILLS` (not shipped to brew end users).
