#!/usr/bin/env python3
"""Key a long CI job's green result by the TREE it tested, not the commit (#3137).

Why: a squash merge's commit on main has the same `git rev-parse HEAD^{tree}`
as the PR merge ref that already passed, yet main's push CI used to run the
full macOS suite again on it. On the 5.1.0 release one tree ran that suite at
least five times. A job that uses this script records "tree T passed <kind>"
when it finishes green, and on its next run over an identical tree skips its
heavy steps and names the original run as its evidence.

The record is an artifact named `ci-tree-pass.<kind>.<tree>`, uploaded by the
job's last step (so it exists only when every earlier step passed). Artifacts,
not actions/cache: a cache saved on a PR's `refs/pull/N/merge` is readable only
from that PR, never from main, which is the one place the skip matters most.
Artifacts are repository-wide and listable by name.

The tree is the one the job CHECKED OUT, so a PR records its merge commit's
tree. A PR with a stale base tested a tree main will never have after the
squash, and main's lookup misses; nothing untested can skip.

Who can write a record (the forgery guard, `trusted_evidence()`): any workflow
run uploads artifacts into this repository's list, including a run for a fork
PR, whose workflow file the fork controls. Without a filter, a fork could
upload `ci-tree-pass.build-test-lint.<T>` for the tree its NEXT push will
produce, and that push's checks, then main's, would skip as green. So a record
counts only when the run that produced it:
  - ran in this repository on this repository's own code
    (`head_repository_id == repository_id`: never a fork PR), and
  - ran the expected workflow file (`path`).
  - DEMONSTRABLY TESTED the tree its name claims, recomputed here from the
    run's GitHub-held metadata, never from anything the run wrote
    (`bound_to_tree()`).
The first two are not enough on their own: anyone who can push a branch can
open a throwaway PR whose own workflow (or script, or build.rs) uploads
`ci-tree-pass.build-test-lint.<T>` for ANOTHER PR's merge tree T, computed
locally with `git merge-tree`, without running a test; the artifact outlives
the closed PR. The binding closes that: the verifier computes the tree the
producing run tested and requires it to equal T.
  - push, workflow_dispatch, schedule: the tree of the run's `head_sha`.
  - pull_request: the run checked out GitHub's test merge of its base into
    `head_sha`. The run object does not carry that merge commit, and its
    `pull_requests[].base.sha` is NOT a snapshot (measured: it stays at the
    PR's first base after main moves). So the verifier takes the base branch's
    newest commits as of the run's `created_at` (merge ref is computed before
    the run is created) and requires `git merge-tree <base> <head_sha>` == T
    for one of them. Exactly one associated PR, or a miss.
A record whose name tree differs from its run's tested tree then forges
nothing: to bind, T must be a tree containing the producing head's own
changes, and a head that skipped its tests via an edited workflow carries
that edit into T.

Usage (in a job; GH_TOKEN, GITHUB_REPOSITORY, GITHUB_OUTPUT set):
  ci-tree-pass.py lookup --kind K --workflow .github/workflows/ci.yml
      -> GITHUB_OUTPUT: hit=true|false, evidence=<run url>; a summary line
  ci-tree-pass.py record --kind K --out DIR
      -> writes DIR/record.json; GITHUB_OUTPUT: name=ci-tree-pass.K.<tree>
  ci-tree-pass.py --self-test

Any API or git error, an unknown event, or anything ambiguous is a MISS: the job runs its steps. Failing open to work is safe;
failing open to a skip would not be.
"""
import json
import os
import subprocess
import sys
import urllib.request

PREFIX = "ci-tree-pass"


def artifact_name(kind: str, tree: str) -> str:
    return f"{PREFIX}.{kind}.{tree}"


BASE_CANDIDATES = 3
SHA_EVENTS = ("push", "workflow_dispatch", "schedule")


def bound_to_tree(run, tree, commit_tree, base_candidates, merge_tree, pr_base_refs):
    """True only when `run` demonstrably tested `tree` (see the docstring).

    The three callables read GitHub-held state: `commit_tree(sha)` is the tree
    of a commit, `base_candidates(ref, until)` the newest commits on `ref` at
    or before `until`, `merge_tree(base, head)` the clean merge tree or None,
    `pr_base_refs(sha)` the base refs of the PRs GitHub associates with a
    commit. That last one is needed because a run's own `pull_requests` is
    EMPTIED once its PR merges (measured on #3223's run 38206242193), which is
    exactly when main's push run looks the record up.
    Any exception they raise is the caller's miss.
    """
    head = run.get("head_sha")
    event = run.get("event")
    if event in SHA_EVENTS:
        return commit_tree(head) == tree
    if event == "pull_request":
        prs = run.get("pull_requests") or []
        refs = [(pr.get("base") or {}).get("ref") for pr in prs] or pr_base_refs(head)
        if len(refs) != 1:
            return False
        base_ref = refs[0]
        created = run.get("created_at")
        if not base_ref or not created:
            return False
        for base in base_candidates(base_ref, created)[:BASE_CANDIDATES]:
            if merge_tree(base, head) == tree:
                return True
        return False
    return False


def trusted_evidence(artifacts, runs_by_id, workflow_path, name, bound=lambda run: False):
    """Return the html_url of the newest run whose record counts, else None.

    `artifacts` is the API's `artifacts` list for `name`, newest first;
    `runs_by_id` maps run id -> the API's workflow run object; `bound(run)`
    says whether that run demonstrably tested the tree in `name`.
    """
    for art in artifacts:
        if art.get("name") != name or art.get("expired"):
            continue
        wr = art.get("workflow_run") or {}
        repo_id = wr.get("repository_id")
        if repo_id is None or wr.get("head_repository_id") != repo_id:
            continue  # a fork PR's run: it controls its own workflow file
        run = runs_by_id.get(wr.get("id"))
        if not run:
            continue
        if (run.get("head_repository") or {}).get("id") != repo_id:
            continue
        if (run.get("repository") or {}).get("id") != repo_id:
            continue
        if run.get("path") != workflow_path:
            continue
        try:
            if not bound(run):
                continue
        except Exception as e:  # noqa: BLE001 -- unverifiable is a miss
            print(f"rejected: run {run.get('html_url')} could not be verified ({e})")
            continue
        return run.get("html_url")
    return None


def _api(path):
    req = urllib.request.Request(
        f"https://api.github.com/{path}",
        headers={
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)


def _tree():
    return subprocess.check_output(["git", "rev-parse", "HEAD^{tree}"], text=True).strip()


def _merge_tree(base, head):
    """The clean merge tree of `base` into `head`, or None (conflict, error).

    Fetches both commits with enough history to find their merge base; a
    shallow cut that hides it makes git refuse or disagree, which is a miss.
    """
    subprocess.run(
        ["git", "fetch", "-q", "--no-tags", "--depth=50", "origin", base, head],
        check=True, timeout=120,
    )
    out = subprocess.run(
        ["git", "merge-tree", "--write-tree", base, head],
        capture_output=True, text=True, timeout=120,
    )
    if out.returncode != 0:
        return None
    return out.stdout.splitlines()[0].strip()


def _output(**kv):
    with open(os.environ["GITHUB_OUTPUT"], "a") as f:
        for k, v in kv.items():
            f.write(f"{k}={v}\n")


def _summary(line):
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a") as f:
            f.write(line + "\n")
    print(line)


def lookup(kind, workflow_path):
    tree = _tree()
    name = artifact_name(kind, tree)
    repo = os.environ["GITHUB_REPOSITORY"]
    arts = []

    def bound(run):
        ok = bound_to_tree(
            run, tree,
            commit_tree=lambda sha: _api(f"repos/{repo}/git/commits/{sha}")["tree"]["sha"],
            base_candidates=lambda ref, until: [
                c["sha"] for c in _api(
                    f"repos/{repo}/commits?sha={ref}&until={until}&per_page={BASE_CANDIDATES}")
            ],
            merge_tree=_merge_tree,
            pr_base_refs=lambda sha: [
                pr["base"]["ref"] for pr in _api(f"repos/{repo}/commits/{sha}/pulls")
            ],
        )
        if not ok:
            print(f"rejected: run {run.get('html_url')} ({run.get('event')}) did not test tree {tree}")
        return ok

    try:
        arts = _api(f"repos/{repo}/actions/artifacts?name={name}&per_page=100")["artifacts"]
        runs = {}
        for art in arts:
            rid = (art.get("workflow_run") or {}).get("id")
            if rid is not None and rid not in runs:
                runs[rid] = _api(f"repos/{repo}/actions/runs/{rid}")
        evidence = trusted_evidence(arts, runs, workflow_path, name, bound=bound)
    except Exception as e:  # noqa: BLE001 -- any failure means: run the steps
        print(f"::warning::tree-pass lookup failed ({e}); treating it as a miss")
        evidence = None
    if evidence:
        _output(hit="true", evidence=evidence)
        _summary(
            f"**{kind}: skipped.** Tree `{tree}` already passed `{kind}` in "
            f"{evidence}; that run is this job's evidence (#3137)."
        )
    else:
        _output(hit="false", evidence="")
        print(f"{len(arts)} record(s) named for tree {tree} ({kind}), none from a run "
              "that demonstrably tested it: a miss")


def record(kind, out_dir):
    tree = _tree()
    sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    os.makedirs(out_dir, exist_ok=True)
    run_url = (
        f"{os.environ.get('GITHUB_SERVER_URL', 'https://github.com')}/"
        f"{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ.get('GITHUB_RUN_ID', '')}"
    )
    with open(os.path.join(out_dir, "record.json"), "w") as f:
        json.dump(
            {"kind": kind, "tree": tree, "commit": sha, "run": run_url,
             "event": os.environ.get("GITHUB_EVENT_NAME", "")},
            f,
        )
    _output(name=artifact_name(kind, tree))


# ---- self-test ---------------------------------------------------------------

REPO = 100
FORK = 200
WF = ".github/workflows/ci.yml"
T = "abc"
NAME = "ci-tree-pass.build-test-lint.abc"


def _art(rid, head=REPO, repo=REPO, expired=False, name=NAME):
    return {"name": name, "expired": expired,
            "workflow_run": {"id": rid, "repository_id": repo, "head_repository_id": head}}


def _run(rid, head=REPO, repo=REPO, path=WF):
    return {"id": rid, "path": path, "html_url": f"run/{rid}",
            "repository": {"id": repo}, "head_repository": {"id": head}}


def _yes(_run):
    return True


SELF_TEST_CASES = [
    # (artifacts, runs, expected evidence, why) -- repository/path/name guards,
    # with the tree binding held true so each guard is tested on its own.
    ([_art(1)], {1: _run(1)}, "run/1", "a same-repo, bound record counts"),
    ([], {}, None, "no record is a miss"),
    ([_art(1, head=FORK)], {1: _run(1, head=FORK)}, None,
     "a fork PR's record never counts (it controls its workflow file)"),
    ([_art(1, head=FORK)], {1: _run(1)}, None,
     "the artifact's own head repo is checked, not only the run's"),
    ([_art(1)], {1: _run(1, head=FORK)}, None,
     "the run's head repo is checked, not only the artifact's"),
    ([_art(1)], {1: _run(1, repo=FORK)}, None, "a run in another repository never counts"),
    ([_art(1)], {1: _run(1, path=".github/workflows/other.yml")}, None,
     "a record from another workflow file never counts"),
    ([_art(1, expired=True)], {1: _run(1)}, None, "an expired record is a miss"),
    ([_art(1, name="ci-tree-pass.build-test-lint.abcd")], {1: _run(1)}, None,
     "a record for another tree never counts"),
    ([_art(1)], {}, None, "a record whose run cannot be read is a miss"),
    ([_art(1, head=None, repo=None)], {1: _run(1, head=None, repo=None)}, None,
     "a record that names no repository is a miss, not a match of two blanks"),
    ([_art(1, head=FORK), _art(2)], {1: _run(1, head=FORK), 2: _run(2)}, "run/2",
     "a forged record does not hide a real one behind it"),
]


# Tree binding. Commits: H_OK has tree T; H_EVIL (a branch whose own workflow
# uploaded a record NAMED for T) has tree "evil". Merges: base B1 into H_OK is
# T; anything into H_EVIL is "evil+..." (its own edit rides along).
COMMIT_TREES = {"H_OK": T, "H_EVIL": "evil"}
MERGES = {("B1", "H_OK"): T, ("B0", "H_OK"): "older", ("B1", "H_EVIL"): "evil+b1"}


def _bt(**run):
    run.setdefault("created_at", "2026-10-12T05:00:00Z")
    return run


def _pr(head, base_ref="main", n=1):
    return _bt(event="pull_request", head_sha=head,
               pull_requests=[{"base": {"ref": base_ref, "sha": "STALE"}}] * n)


def _raise(*_a):
    raise RuntimeError("api down")


BIND_CASES = [
    # (run, base candidates, expected, why)
    (_bt(event="push", head_sha="H_OK"), ["B1"], True, "an honest push run that tested T binds"),
    (_bt(event="schedule", head_sha="H_OK"), ["B1"], True, "a scheduled run binds by its head's tree"),
    (_bt(event="push", head_sha="H_EVIL"), ["B1"], False,
     "(a) a record named for T from a run whose head tree is not T is a miss"),
    (_bt(event="workflow_dispatch", head_sha="H_EVIL"), ["B1"], False,
     "(b) a dispatch on another branch whose head tree is not T is a miss"),
    (_bt(event="workflow_dispatch", head_sha="H_OK"), ["B1"], True,
     "a dispatch whose head tree is T binds"),
    (_pr("H_OK"), ["B1", "B0"], True, "(c) an honest PR run: its base merged into its head is T"),
    (_pr("H_OK"), ["B2", "B1"], True, "the base may be one of the newest few at run creation"),
    (_pr("H_OK"), ["B0"], False, "a PR run whose merge with every candidate base is not T is a miss"),
    (_pr("H_EVIL"), ["B1"], False,
     "(a) a throwaway PR naming another PR's merge tree is a miss"),
    (_pr("H_OK", n=0), ["B1"], True,
     "a merged PR's run (its pull_requests emptied) binds through the commit's PR"),
    (_pr("H_OK", n=2), ["B1"], False, "a PR run with two associated PRs is ambiguous: a miss"),
    (dict(_pr("H_OK", n=0), _assoc=[]), ["B1"], False,
     "a PR run with no PR on the run or the commit is a miss"),
    (dict(_pr("H_OK", n=0), _assoc=["main", "ui/port"]), ["B1"], False,
     "a commit in two PRs is ambiguous: a miss"),
    (_pr("H_OK", base_ref=""), ["B1"], False, "a PR run with no base ref is a miss"),
    (_bt(event="pull_request_target", head_sha="H_OK"), ["B1"], False, "an unknown event is a miss"),
    (_bt(event="push"), ["B1"], False, "a run with no head sha is a miss"),
    (_bt(event="pull_request", head_sha="H_OK", pull_requests=[{"base": {"ref": "main"}}],
         created_at=""), ["B1"], False, "a PR run with no creation time is a miss"),
    (_pr("H_OK"), ["B0", "B0", "B0", "B1"], False,
     "only the newest few bases are candidates, not all of history"),
]


def self_test() -> int:
    failures = []
    for arts, runs, want, why in SELF_TEST_CASES:
        got = trusted_evidence(arts, runs, WF, NAME, bound=_yes)
        if got != want:
            failures.append(f"  want {want!r}, got {got!r}: {why}")
    for run, bases, want, why in BIND_CASES:
        got = bound_to_tree(
            run, T,
            commit_tree=lambda sha: COMMIT_TREES.get(sha),
            base_candidates=lambda ref, until, b=bases: list(b),
            merge_tree=lambda base, head: MERGES.get((base, head)),
            pr_base_refs=lambda sha, r=run: r.get("_assoc", ["main"]),
        )
        if got != want:
            failures.append(f"  want {want!r}, got {got!r}: {why}")
    # Unbound and unverifiable records are misses end to end.
    for bound, why in [(lambda r: False, "an unbound record is a miss"),
                       (_raise, "a verifier error is a miss, never a skip")]:
        got = trusted_evidence([_art(1)], {1: _run(1)}, WF, NAME, bound=bound)
        if got is not None:
            failures.append(f"  want None, got {got!r}: {why}")
    total = len(SELF_TEST_CASES) + len(BIND_CASES) + 2
    if failures:
        print("tree-pass self-test FAILED:\n" + "\n".join(failures))
        return 1
    print(f"tree-pass self-test passed: {total} cases")
    return 0


def _arg(flag):
    if flag not in sys.argv or sys.argv.index(flag) + 1 >= len(sys.argv):
        sys.exit(f"{flag} requires a value")
    return sys.argv[sys.argv.index(flag) + 1]


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.exit(self_test())
    if len(sys.argv) > 1 and sys.argv[1] == "lookup":
        lookup(_arg("--kind"), _arg("--workflow"))
    elif len(sys.argv) > 1 and sys.argv[1] == "record":
        record(_arg("--kind"), _arg("--out"))
    else:
        sys.exit("usage: ci-tree-pass.py lookup --kind K --workflow PATH | record --kind K --out DIR | --self-test")
