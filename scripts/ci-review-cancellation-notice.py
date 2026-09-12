#!/usr/bin/env python3
"""Disclose a job-timeout cancellation of `darkmux-review.yml` like a decline. (#2100)

## The bug

`darkmux-review.yml` bounds the whole `review` job at `timeout-minutes: 90`.
A large diff's review can legitimately run that long (#1247's own
concurrent-build contention tax, or just a big diff) — the darkmux dispatch
step is still emitting telemetry, healthy, just slow, when GitHub Actions
marks the JOB `cancelled` and tears it down. The dispatch process is killed
mid-flight; it never reaches its own `deliver.github_review` step, so
`rendered.json` is never written. "Post the review" was gated on the
implicit `success()` a bare `if: steps.diff.outputs.has_diff == 'true'`
carries — which is FALSE once the job itself is cancelled — so that step
never got a turn either. Net result on the PR: nothing. No summary, no
"timed out" line. A reader cannot tell "never ran" from "ran 90 minutes and
was killed". Same disclosure class #1764 closed for bundler declines and
#1605 closed for a dispatch that ends without posting, from a new cause: a
job-level cancel kills the whole process tree, so the posting step never
gets a turn UNLESS its own `if:` survives cancellation (`always()`).

## The fix

`darkmux-review.yml`'s "Post the review" step moves from an implicit
`success()` gate to `always() && steps.diff.outputs.has_diff == 'true'`, so
it still runs in GitHub's post-cancellation grace window (mirroring the
`always()` fix #2550/#2623 made for the mutation gate's own "Report
survivors" step, for the identical failure shape: a reporting step gated on
`!cancelled()`/an implicit `success()` never runs when the JOB — not just
its own preceding step — is cancelled).

That step's existing `[ ! -s rendered.json ]` branch already disclosed a
dispatch that "crashed before its own degraded-mode fallback could run"
with a generic warning. This script gives that SAME branch a second,
MORE SPECIFIC message for a CANCELLED job, naming the elapsed wall clock —
the number that decides whether `timeout-minutes` needs raising, made
visible on the PR instead of inferred from workflow logs nobody opens
(#2100's own "concurrent-build tax... visible instead of inferred").

A cancelled job is NOT always a timed-out job, though: `darkmux-review.yml`
sets `cancel-in-progress: true`, so re-dispatching a review on the same PR
cancels whatever run is already in flight — a real, common cause distinct
from the 90-minute budget expiring. This script's message names which one
it thinks happened (by comparing elapsed time to the configured budget,
within `GRACE_MINUTES` of tolerance) rather than always asserting "timed
out"; see `GRACE_MINUTES`'s own comment for the run-history proof.

## Partial findings are never discarded

If the review DID manage to write a non-empty, parseable `rendered.json`
before the cancel landed (a graceful stop inside the dispatch, or a race
between the dispatch finishing and the job being marked cancelled), this
script's job is to say NOTHING — the workflow's own mode-keyed posting
(`review`/`partial`/`degraded`/`noop`/`comment`) already discloses exactly
what ran and did not, following the same "a load-bearing stage exhausting
is an honest degraded run" doctrine `DeliverScope.errored` already
encodes. This script exists ONLY for the case that pipeline can't cover:
the dispatch was killed before it ever got to write anything at all. A
partial review is not a failed review — this script is careful to never
override real, even incomplete, content with the "nothing produced"
notice; see the "PARTIAL FINDINGS SURVIVE" self-test cases below.

Honestly stated: those self-test cases prove the invariant against
`compute_notice` DIRECTLY, in isolation — they are not proof the real
caller ever hands this function a corrupt or no-mode `rendered_path`. The
bash caller's own `[ ! -s rendered.json ]` gate only ever gets this script
a MISSING or EMPTY path (both handled below); a non-empty-but-corrupt or
no-mode payload passes that gate and reaches the caller's own `jq`-based
mode dispatch instead, which is a separate code path this script cannot
see and does not protect (#2100 CONSIDER 3 hardened that path directly, in
the workflow file, rather than here).

## Never a verdict

The disclosed line states what ran, what did not, and why — never a
judgment on the diff itself (darkmux describes, never adjudicates).

Run `--self-test` to exercise the decision table below before it is
trusted in CI — the same discipline `ci-mutants-summary.py` established
for `quality.yml`'s own cancellation disclosure.
"""

import json
import sys
from pathlib import Path

CANCELLED_STATUS = "cancelled"

# (#2100 MUST FIX 1) How close `elapsed` has to sit to the configured job
# budget before this script asserts "this was a timeout" rather than "this
# was cancelled for some other reason". Proven from this workflow's OWN run
# history, not theory: `cancel-in-progress: true` in the concurrency group
# above `jobs.review` means re-dispatching a review on the same PR cancels
# whatever run is already in flight. Of the six cancelled runs this workflow
# has ever had, four ran the full ~90-minute budget and TWO were short
# supersede-cancels — run 30812863146 (PR 1617) was cancelled after 18
# minutes when run 30814081434 was queued for the same PR. A third of the
# sample. Asserting "this is a timed-out run" against an 18-minute (or a
# 2-minute, or a 0-second) elapsed time is simply false, and false in the
# specific way that misleads a maintainer re-dispatching a fix: it reads as
# "raise the timeout" when the real story is "a newer run superseded this
# one, which is working as designed". Named rather than inlined so the
# self-test cases below can assert against the same threshold this function
# uses.
GRACE_MINUTES = 10


def rendered_has_content(rendered_path: str) -> bool:
    """Whether `rendered_path` already holds a real, parseable payload.

    Anything here means the ordinary mode-keyed posting path has (or will
    have) something honest to say about this run — findings survive there,
    not by way of this script. Missing, empty, and unparseable all count as
    "nothing yet": the caller's own `[ ! -s rendered.json ]` bash guard
    already filters out "missing/empty" before this module is ever
    invoked, but this check stays self-contained (not merely inherited from
    the caller's gate) so a future caller — or this file's own self-test —
    can prove the "never clobber real content" invariant directly, without
    relying on bash's `-s` test to have run first.
    """
    path = Path(rendered_path) if rendered_path else None
    if path is None or not path.is_file():
        return False
    try:
        raw = path.read_text()
    except OSError:
        return False
    if not raw.strip():
        return False
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return False
    return isinstance(data, dict) and bool(data.get("mode"))


def elapsed_minutes(started_epoch, now_epoch) -> "int | None":
    """Whole minutes between `started_epoch` and `now_epoch`, or `None`.

    `None` (never a raised exception) whenever either bound is missing or
    not a real number — a malformed/absent timestamp degrades the message's
    wording, it never crashes the disclosure the message exists to make.
    """
    if started_epoch is None or now_epoch is None:
        return None
    try:
        started = float(started_epoch)
        now = float(now_epoch)
    except (TypeError, ValueError):
        return None
    return max(0, int((now - started) // 60))


def compute_notice(
    job_status: str,
    rendered_path: str,
    timeout_minutes,
    started_epoch=None,
    now_epoch=None,
    run_url: str = "",
) -> str:
    """The disclosure body to post, or `""` when no notice is needed.

    `""` covers two distinct, deliberately-not-distinguished-further cases:
    the job did not get cancelled (a clean pass, an ordinary non-timeout
    failure — those already have their own disclosure paths), and a job
    that WAS cancelled but already has real rendered content to show for
    itself. Either way, this script has nothing to add.
    """
    if job_status != CANCELLED_STATUS:
        return ""
    if rendered_has_content(rendered_path):
        return ""

    minutes = elapsed_minutes(started_epoch, now_epoch)
    try:
        budget = int(timeout_minutes)
    except (TypeError, ValueError):
        budget = None

    # (#2100 MUST FIX 1) Three distinct causes, three distinct claims — never
    # collapse "cancelled" into "timed out" just because a job timeout is
    # the ONE cancellation cause this workflow can name a budget for. See
    # `GRACE_MINUTES` above for why the threshold exists at all. Either
    # bound missing/unparseable means the comparison itself can't be made —
    # that's the UNKNOWN case, never a guess in either direction.
    if minutes is None or budget is None:
        cause = (
            f"was cancelled (job `timeout-minutes: {timeout_minutes}`) — the "
            "elapsed run time could not be determined, so whether this was a "
            "timeout, a superseding re-dispatch, or a manual cancel is unknown."
        )
    elif minutes >= budget - GRACE_MINUTES:
        cause = (
            f"ran {minutes} minute(s) before being cancelled — at or near its "
            f"`timeout-minutes: {timeout_minutes}` job budget, most likely its own "
            "timeout (though a manual cancel this close to the budget can also "
            "cause this)."
        )
    else:
        cause = (
            f"was cancelled after only {minutes} minute(s) — well short of its "
            f"`timeout-minutes: {timeout_minutes}` job budget, so this is NOT a "
            "timeout. Most likely a newer dispatch on this PR superseded it "
            "(this workflow cancels an in-progress run for the same PR), or "
            "someone cancelled it manually."
        )

    lines = [
        f":no_entry: darkmux self-review {cause} The review never finished, "
        "so no automated review was produced; treat it as needing manual "
        "review.",
    ]
    if run_url:
        lines.append(f"See the run for details: {run_url}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------


def _write(tmp_path: Path, name: str, content: str) -> str:
    p = tmp_path / name
    p.write_text(content)
    return str(p)


SELF_TEST_CASES = [
    {
        "name": "a clean success run never gets a notice",
        "job_status": "success",
        "rendered": None,
        "expect_empty": True,
    },
    {
        "name": "an ordinary (non-cancelled) failure never gets THIS notice — "
        "the caller's own generic 'produced no rendered payload' warning "
        "covers it",
        "job_status": "failure",
        "rendered": None,
        "expect_empty": True,
    },
    {
        "name": "cancelled AT its budget (elapsed within GRACE_MINUTES of timeout): "
        "the loud, distinct TIMEOUT notice",
        "job_status": "cancelled",
        "rendered": None,
        "timeout_minutes": 90,
        "started_epoch": 1000.0,
        "now_epoch": 1000.0 + 91 * 60,
        "run_url": "https://github.com/kstrat2001/darkmux/actions/runs/123",
        "expect_contains": [
            "cancelled",
            "91 minute(s)",
            "most likely its own",
            "timeout-minutes: 90",
            "no automated review was produced",
            "https://github.com/kstrat2001/darkmux/actions/runs/123",
        ],
        "expect_not_contains": ["NOT a timeout"],
    },
    {
        "name": "cancelled WELL SHORT of its budget (18-minute PR-1617 shape, run "
        "30812863146): must NOT claim a timeout — a supersede-cancel, per "
        "GRACE_MINUTES's own run-history proof",
        "job_status": "cancelled",
        "rendered": None,
        "timeout_minutes": 90,
        "started_epoch": 0.0,
        "now_epoch": 18 * 60.0,
        "expect_contains": [
            "cancelled",
            "18 minute(s)",
            "NOT a timeout",
            "superseded",
            "no automated review was produced",
        ],
        "expect_not_contains": ["most likely its own"],
    },
    {
        "name": "cancelled with a TWO-MINUTE elapsed epoch — the exact reviewer "
        "repro: must not read as a timed-out run",
        "job_status": "cancelled",
        "rendered": None,
        "timeout_minutes": 90,
        "started_epoch": 0.0,
        "now_epoch": 2 * 60.0,
        "expect_contains": ["2 minute(s)", "NOT a timeout"],
        "expect_not_contains": ["this is a timed-out run", "most likely its own"],
    },
    {
        "name": "cancelled with a ZERO-SECOND elapsed epoch — must not read as a "
        "timed-out run either",
        "job_status": "cancelled",
        "rendered": None,
        "timeout_minutes": 90,
        "started_epoch": 0.0,
        "now_epoch": 0.0,
        "expect_contains": ["0 minute(s)", "NOT a timeout"],
        "expect_not_contains": ["most likely its own"],
    },
    {
        "name": "cancelled with an EMPTY rendered.json file — same as no file at all",
        "job_status": "cancelled",
        "rendered_content": "",
        "timeout_minutes": 90,
        "started_epoch": 0.0,
        "now_epoch": 60.0,
        "expect_contains": ["cancelled", "no automated review was produced"],
    },
    {
        "name": "cancelled with a CORRUPT rendered.json — treated as nothing produced, "
        "never crashes",
        "job_status": "cancelled",
        "rendered_content": "{not valid json at all",
        "timeout_minutes": 90,
        "expect_contains": ["cancelled", "no automated review was produced"],
    },
    {
        "name": "cancelled with a rendered.json that parses but has no mode field — "
        "still treated as nothing produced",
        "job_status": "cancelled",
        "rendered_content": json.dumps({"review": None}),
        "timeout_minutes": 90,
        "expect_contains": ["no automated review was produced"],
    },
    {
        "name": "PARTIAL FINDINGS SURVIVE: cancelled but rendered.json already carries "
        "a real degraded payload — this script stays silent so the ordinary "
        "mode-keyed posting path delivers it",
        "job_status": "cancelled",
        "rendered_content": json.dumps(
            {
                "mode": "degraded",
                "fallback_comment": "review ran: 1 of 3 rules reviewed before the job "
                "timeout — src/a.rs:12 — the lock is dropped before the write.",
            }
        ),
        "expect_empty": True,
    },
    {
        "name": "PARTIAL FINDINGS SURVIVE (review mode, not just degraded): cancelled but "
        "a full review payload already exists",
        "job_status": "cancelled",
        "rendered_content": json.dumps({"mode": "review", "review": {"event": "COMMENT"}}),
        "expect_empty": True,
    },
    {
        "name": "no elapsed-time inputs available — the message says the cause is "
        "UNKNOWN rather than guessing either way",
        "job_status": "cancelled",
        "rendered": None,
        "timeout_minutes": 90,
        "started_epoch": None,
        "now_epoch": None,
        "expect_contains": [
            "timeout-minutes: 90",
            "elapsed run time could not be determined",
            "no automated review was produced",
        ],
        "expect_not_contains": ["most likely its own", "NOT a timeout"],
    },
    {
        "name": "malformed elapsed-time inputs degrade gracefully, never raise, and "
        "still say UNKNOWN rather than guessing",
        "job_status": "cancelled",
        "rendered": None,
        "timeout_minutes": 90,
        "started_epoch": "not-a-number",
        "now_epoch": "also-not-a-number",
        "expect_contains": ["elapsed run time could not be determined"],
    },
    {
        "name": "malformed timeout_minutes degrades gracefully (budget unparseable) — "
        "even with a real elapsed time, cause is UNKNOWN rather than guessed",
        "job_status": "cancelled",
        "rendered": None,
        "timeout_minutes": "ninety",
        "started_epoch": 0.0,
        "now_epoch": 30 * 60.0,
        "expect_contains": ["elapsed run time could not be determined"],
        "expect_not_contains": ["most likely its own", "NOT a timeout"],
    },
    {
        "name": "no run URL supplied — the message still renders without a link line",
        "job_status": "cancelled",
        "rendered": None,
        "timeout_minutes": 90,
        "run_url": "",
        "expect_contains": ["no automated review was produced"],
        "expect_not_contains": ["See the run for details"],
    },
]


def rendered_has_content_self_test() -> list:
    failures = []
    if rendered_has_content(""):
        failures.append("empty path string must read as no content")
    if rendered_has_content("/does/not/exist/rendered.json"):
        failures.append("a missing file must read as no content")
    return failures


def self_test() -> int:
    failures = []
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        tmp_path = Path(tmp)
        for i, case in enumerate(SELF_TEST_CASES):
            rendered = case.get("rendered", "__unset__")
            if rendered == "__unset__":
                if "rendered_content" in case:
                    rendered = _write(tmp_path, f"rendered-{i}.json", case["rendered_content"])
                else:
                    rendered = str(tmp_path / f"missing-{i}.json")

            result = compute_notice(
                job_status=case["job_status"],
                rendered_path=rendered,
                timeout_minutes=case.get("timeout_minutes", 90),
                started_epoch=case.get("started_epoch"),
                now_epoch=case.get("now_epoch"),
                run_url=case.get("run_url", ""),
            )

            label = case["name"]
            if case.get("expect_empty"):
                if result != "":
                    failures.append(f"{label}: expected empty notice, got: {result!r}")
                continue
            for needle in case.get("expect_contains", []):
                if needle not in result:
                    failures.append(f"{label}: expected {needle!r} in notice, got: {result!r}")
            for needle in case.get("expect_not_contains", []):
                if needle in result:
                    failures.append(f"{label}: expected {needle!r} ABSENT from notice, got: {result!r}")

    failures.extend(rendered_has_content_self_test())

    total = len(SELF_TEST_CASES) + 2
    if failures:
        print("ci-review-cancellation-notice self-test FAILED:\n" + "\n".join(failures))
        return 1
    print(f"ci-review-cancellation-notice self-test passed: {total} cases")
    return 0


USAGE = (
    "usage: ci-review-cancellation-notice.py --job-status <success|failure|cancelled> "
    "--rendered <path> --timeout-minutes <N> [--started-epoch <epoch>] "
    "[--now-epoch <epoch>] [--run-url <url>]\n"
    "       ci-review-cancellation-notice.py --self-test"
)


def _take_flag(args: list, flag: str):
    """Remove `flag <value>` from `args` (in place) and return `value`, or
    `None` if `flag` is absent. Exits with a usage error if `flag` is
    present with no following value — a caller passing a flag with nothing
    after it is a workflow-authoring bug, not a runtime condition to
    tolerate silently.
    """
    if flag not in args:
        return None
    i = args.index(flag)
    if i + 1 >= len(args):
        print(f"{flag} requires a value", file=sys.stderr)
        sys.exit(2)
    value = args[i + 1]
    del args[i : i + 2]
    return value


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())

    argv = sys.argv[1:]
    job_status = _take_flag(argv, "--job-status")
    rendered = _take_flag(argv, "--rendered")
    timeout_minutes = _take_flag(argv, "--timeout-minutes")
    started_epoch = _take_flag(argv, "--started-epoch")
    now_epoch = _take_flag(argv, "--now-epoch")
    run_url = _take_flag(argv, "--run-url") or ""

    if job_status is None or rendered is None or timeout_minutes is None:
        print(USAGE, file=sys.stderr)
        sys.exit(2)

    if now_epoch is None:
        import time

        now_epoch = time.time()

    print(
        compute_notice(
            job_status=job_status,
            rendered_path=rendered,
            timeout_minutes=timeout_minutes,
            started_epoch=started_epoch,
            now_epoch=now_epoch,
            run_url=run_url,
        )
    )
    sys.exit(0)
