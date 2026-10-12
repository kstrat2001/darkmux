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
Only people with write access can push a branch here, and they can already
push workflow changes, so the guard is exactly as strong as branch access.
An honest record also implies an identical workflow definition: the workflow
file is part of the tree that was hashed.

Usage (in a job; GH_TOKEN, GITHUB_REPOSITORY, GITHUB_OUTPUT set):
  ci-tree-pass.py lookup --kind K --workflow .github/workflows/ci.yml
      -> GITHUB_OUTPUT: hit=true|false, evidence=<run url>; a summary line
  ci-tree-pass.py record --kind K --out DIR
      -> writes DIR/record.json; GITHUB_OUTPUT: name=ci-tree-pass.K.<tree>
  ci-tree-pass.py --self-test

Any API error is a MISS: the job runs its steps. Failing open to work is safe;
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


def trusted_evidence(artifacts, runs_by_id, workflow_path, name):
    """Return the html_url of the newest run whose record counts, else None.

    `artifacts` is the API's `artifacts` list for `name`, newest first;
    `runs_by_id` maps run id -> the API's workflow run object.
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
    try:
        arts = _api(f"repos/{repo}/actions/artifacts?name={name}&per_page=100")["artifacts"]
        runs = {}
        for art in arts:
            rid = (art.get("workflow_run") or {}).get("id")
            if rid is not None and rid not in runs:
                runs[rid] = _api(f"repos/{repo}/actions/runs/{rid}")
        evidence = trusted_evidence(arts, runs, workflow_path, name)
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
        print(f"no trusted record for tree {tree} ({kind}): a miss")


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
NAME = "ci-tree-pass.build-test-lint.abc"


def _art(rid, head=REPO, repo=REPO, expired=False, name=NAME):
    return {"name": name, "expired": expired,
            "workflow_run": {"id": rid, "repository_id": repo, "head_repository_id": head}}


def _run(rid, head=REPO, repo=REPO, path=WF):
    return {"id": rid, "path": path, "html_url": f"run/{rid}",
            "repository": {"id": repo}, "head_repository": {"id": head}}


SELF_TEST_CASES = [
    # (artifacts, runs, expected evidence, why)
    ([_art(1)], {1: _run(1)}, "run/1", "a same-repo record counts"),
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


def self_test() -> int:
    failures = []
    for arts, runs, want, why in SELF_TEST_CASES:
        got = trusted_evidence(arts, runs, WF, NAME)
        if got != want:
            failures.append(f"  want {want!r}, got {got!r}: {why}")
    if failures:
        print("tree-pass self-test FAILED:\n" + "\n".join(failures))
        return 1
    print(f"tree-pass self-test passed: {len(SELF_TEST_CASES)} cases")
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
