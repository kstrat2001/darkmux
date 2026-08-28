#!/usr/bin/env python3
"""Shared identity-scrub machinery for demo-env importers.

Factored out of `import_session.py` (#2032 Packet 1) so `import_mission.py`
can reuse the SAME `FORBIDDEN` patterns and `scrub()` rewrite instead of
maintaining a second copy that drifts. `import_session.py` re-exports
`scrub`/`FORBIDDEN` from here, so `from import_session import scrub,
FORBIDDEN` (used by `build.py::canned_doctor`) keeps working unchanged.

Anything matching `FORBIDDEN` never reaches a committed fixture. The scan is
a BACKSTOP, not the mechanism: `scrub()` removes the known carriers, and
`scan_or_die()` fails the import loudly if an unknown one survives. A
fixture is published to a public docs site, so "probably clean" is not a
standard.
"""
import re
import sys
import pathlib

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))
import lib_vocab as _vocab  # noqa: E402


def _engagement_patterns():
    r"""Engagement-name patterns, built from the ONE file that owns the list.

    Delegates the READ to `scripts/lib_vocab.py`, which fails loud on a
    truncated parse. See that module's doc for the mutation that proved why
    a naive `\[(.*?)\]` regex here is unsafe.

    Ticket prefixes are skipped: the dedicated `\bSYS-\d+\b` rule below
    bounds the digits.
    """
    names = {n.lower() for n in _vocab.sentinels()}
    names = {n for n in names if not n.endswith(("-", "_"))}
    return [(re.compile(r"(?i)\b" + re.escape(n) + r"\b"), "an engagement name")
            for n in sorted(names)]


FORBIDDEN = [
    (re.compile(r"/Users/[^/\"\s]+"),          "a host home directory"),
    (re.compile(r"\b100\.\d{1,3}\.\d{1,3}\.\d{1,3}\b"), "a tailnet IP"),
    # Engagement names are NOT spelled here. `tests/parity/lib/sanitize.mjs`
    # owns that vocabulary (`scripts/engagement-sentinel-guard.py` names it
    # as the sole owner and allowlists only that file), so re-declaring the
    # strings would both leak them into a PUBLIC file — the guard fails the
    # build on exactly that, correctly — and create the second copy that
    # drifts. Read the owner instead; see `_engagement_patterns()`.
    *_engagement_patterns(),
    (re.compile(r"\bSYS-\d+\b"),               "an internal ticket key"),
    # TLD segment required to be alphabetic (2+ letters): a real email's
    # domain always ends that way, but a `<name>@<version>` fixture
    # `satisfies` string (e.g. `tiny-python-suite@1.0`) has the SAME
    # `word@word.word` shape with a NUMERIC final segment — caught as a
    # false positive when `lab fixture list`'s real registry content first
    # flowed through this scan live (#2032 Packet 1's serve.py backstop).
    (re.compile(r"(?i)[\w.+-]+@[\w-]+(?:\.[\w-]+)*\.[a-zA-Z]{2,}"),
     "an email address"),
    (re.compile(r"(?i)\b[\w-]+\.ts\.net\b"),   "a MagicDNS name"),
    # The host's own name. It survives rewrites aimed at the DOMAIN (it is
    # the left-most label), and it is the single most identifying string a
    # `doctor` or `flow status` capture carries.
    (re.compile(r"(?i)\b(?:macbook-pro|kains?-mac(?:book|-studio)?|mac-studio)\b"),
     "the operator's hostname"),
]


def scrub(obj):
    """Rewrite the KNOWN identity carriers. Anything left is caught below."""
    if isinstance(obj, dict):
        return {k: scrub(v) for k, v in obj.items()}
    if isinstance(obj, list):
        return [scrub(v) for v in obj]
    if isinstance(obj, str):
        # A sandbox path is real content (the run's workspace), so it is
        # rewritten to an equivalent demo path rather than blanked — the run
        # detail lens renders it, and an empty workspace row reads as a bug.
        s = re.sub(r"/Users/[^/\"\s]+/\.darkmux", "/home/demo/.darkmux", obj)
        s = re.sub(r"/Users/[^/\"\s]+", "/home/demo", s)
        # The username rule leaves the rest of a home path intact, and the
        # rest is identity too: a personal project root, a checkout name, a
        # worktree hash (#2032 review: `/home/demo/de-projects/dm-crawl`
        # reached a committed fixture). Everything under /home/demo that is
        # not the demo's own `.darkmux` or the repo's rewritten `darkmux`
        # root collapses to one placeholder. Nothing downstream reads a
        # workspace path for content.
        s = re.sub(r"/home/demo/(?!\.darkmux(?:/|\b)|darkmux(?:/|\b))[^\"\s]+", "/home/demo/workspace", s)
        # A frontier orchestrator's OWN scratchpad path
        # (`/private/tmp/claude-<uid>/...`) can end up quoted inside a
        # review's findings when the reviewed diff was staged from one (a
        # mangled `-Users-<name>-...` segment lives INSIDE this path, not
        # after a bare `/Users/`, so the rule above never reaches it). #2032
        # Packet 1's own import hit this: a `diff_file` config value pointed
        # at the importing agent's scratchpad. Collapsed to one placeholder
        # rather than a token-preserving rewrite — nothing downstream reads
        # this path for its content, only its presence.
        s = re.sub(r"/private/tmp/claude-[^\"\s]+", "/tmp/demo-scratch", s)
        # The operator's hostname. #2032 Packet 1's import also hit this
        # NOT as live identity but as example text INSIDE a code comment the
        # review was quoting verbatim (`ui/src/lenses/lab/labSeries.ts`
        # discussing its own machine_id normalization) — the string is
        # policy-forbidden either way (see FORBIDDEN below), so it gets
        # rewritten here rather than making every future import of
        # self-referential review content fail. `.local` (mDNS) survives as
        # a harmless suffix on the replacement.
        s = re.sub(r"(?i)\bmacbook-pro\b", "demo-laptop", s)
        s = re.sub(r"(?i)\bkains?-mac(?:book|-studio)?\b", "demo-studio", s)
        s = re.sub(r"(?i)\bmac-studio\b", "demo-studio", s)
        # Network identity. Both carriers name the operator's private
        # tailnet, and both show up in output that otherwise belongs in the
        # docs (`darkmux doctor` prints the daemon's bind host). Rewritten to
        # a demo-domain equivalent rather than blanked, so the surrounding
        # line still reads as the real check output it is.
        # Consume the WHOLE FQDN, host label included. Matching only the
        # `<tailnet>.ts.net` suffix left the host label behind, so
        # `macbook-pro.taild<...>.ts.net` scrubbed to
        # `macbook-pro.demo-hub.internal` — still the operator's real
        # machine name, in a string that goes on a public docs page.
        s = re.sub(r"\b[\w-]+(?:\.[\w-]+)*\.ts\.net\b", "demo-hub.internal", s)
        s = re.sub(r"\b100\.\d{1,3}\.\d{1,3}\.\d{1,3}\b", "10.0.0.21", s)
        return s
    return obj


def scan_or_die(blob, dest_description):
    """Refuse to write `dest_description` if a FORBIDDEN pattern survived.

    `blob` is the full text of everything about to be written — callers
    build it as `"\n".join(json.dumps(r, sort_keys=True) for r in records)`
    or similar, so ONE scan covers every record/file in the batch.
    """
    for pat, what in FORBIDDEN:
        m = pat.search(blob)
        if m:
            sys.exit(f"REFUSING to write {dest_description}: found {what} "
                     f"({m.group(0)!r}).\nAdd a rewrite rule to scrub() — "
                     f"do not weaken the scan.")


def ts_to_ms(ts_str):
    """Parse a flow record's `\"ts\": \"2026-...Z\"` into epoch milliseconds.

    `strptime("%Y-%m-%dT%H:%M:%SZ")` reads the literal `Z` character and
    drops it — it does NOT attach a UTC tzinfo — so the parsed
    `datetime` is naive, and `.timestamp()` on a naive datetime
    interprets it in the LOCAL timezone. On this machine (UTC+8) that
    silently shifted every parsed instant 8 hours off true UTC. It never
    surfaced in `import_session.py`, which only ever diffs two values
    parsed the SAME wrong way (`t_ms = ms(r) - base`) — the error cancels.
    It broke loudly in `import_mission.py`, which compares a parsed `ts`
    against a mission's `created_ts` (a plain UTC unix integer, parsed by
    neither datetime nor a timezone): a record 8 hours inside a mission's
    real window read as 8 hours OUTSIDE it and got silently dropped
    (caught in dev-testing — see that module's `collect_flow_records`
    doc). Attaching `timezone.utc` explicitly is the fix, for both
    callers.
    """
    from datetime import datetime, timezone
    return int(datetime.strptime(ts_str, "%Y-%m-%dT%H:%M:%SZ")
               .replace(tzinfo=timezone.utc).timestamp() * 1000)
