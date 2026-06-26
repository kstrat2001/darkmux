#!/usr/bin/env python3
"""Turn a darkmux pr-reviewer envelope into a GitHub PR review (DEVOPS-466).

Reads the darkmux `--json` envelope (final_assistant carries the model's JSON
findings block) plus the PR diff, and emits a payload for the GitHub review
API with native inline comments. Robust by design:

  * Tolerant JSON extraction (the model may wrap the block in ```json fences
    or add stray prose) — local models malform output, so we never hard-fail.
  * Each finding's line is validated against the diff's new-side hunks, because
    GitHub rejects an inline comment on a line it can't see. Out-of-range
    findings are folded into the summary body instead of being dropped.
  * If the block won't parse at all, we fall back to posting the raw review as
    a single summary comment.

Outputs to $RUNNER_TEMP/qa/:
  - review.json  + prints `mode=review`   → POST /pulls/:n/reviews --input
  - comment.md   + prints `mode=comment`  → gh pr comment --body-file
"""
import json
import os
import re
import sys

TEMP = os.path.join(os.environ["RUNNER_TEMP"], "qa")
ENVELOPE = os.path.join(TEMP, "envelope.json")
DIFF = os.path.join(TEMP, "pr.diff")

SEV = {"high": "🔴 **HIGH**", "medium": "🟡 **MEDIUM**", "low": "🔵 **LOW**"}
FOOTER = (
    "\n\n---\n<sub>Automated review by darkmux's own `pr-reviewer` role, running "
    "on a local model (no cloud API) via a self-hosted runner — darkmux "
    "dogfooding itself in public. Advisory, not a merge gate.</sub>"
)


def extract_findings(text):
    """Pull the JSON object out of the model's reply. Returns dict or None."""
    if not text:
        return None
    # Prefer a fenced ```json block; fall back to the outermost braces. Greedy
    # `.*` so a `}` inside a suggestion/detail string doesn't truncate the object.
    m = re.search(r"```(?:json)?\s*(\{.*\})\s*```", text, re.DOTALL)
    candidate = m.group(1) if m else None
    if candidate is None:
        start = text.find("{")
        end = text.rfind("}")
        candidate = text[start : end + 1] if start != -1 and end > start else None
    if candidate is None:
        return None
    try:
        return json.loads(candidate)
    except json.JSONDecodeError:
        return None


def valid_lines(diff_text):
    """Map path -> set of new-side line numbers that are valid comment anchors.

    `+++ ` is only treated as a file header in the pre-hunk header section
    (gated by `in_hunk`, reset at each `diff --git`). That way an *added
    content line* whose text starts with `++ ` — full diff line `+++ ...`,
    e.g. a markdown rule or a diff snippet in docs — is never misread as a
    header, which would otherwise silently drop the rest of the hunk's anchors.
    """
    out, path, newln, in_hunk = {}, None, None, False
    for line in diff_text.splitlines():
        if line.startswith("diff --git "):
            path, newln, in_hunk = None, None, False
        elif not in_hunk and line.startswith("+++ "):
            p = line[4:].strip()
            path = p[2:] if p.startswith("b/") else (None if p == "/dev/null" else p)
            if path:
                out.setdefault(path, set())
        elif line.startswith("@@"):
            m = re.search(r"\+(\d+)", line)
            newln = int(m.group(1)) if m else None
            in_hunk = True
        elif in_hunk and path and newln is not None:
            if line.startswith("+") or line.startswith(" "):
                out[path].add(newln)
                newln += 1
            elif line.startswith("-") or line.startswith("\\"):
                pass  # removed / "No newline" — no new-side position
    return out


def comment_body(f):
    sev = SEV.get(str(f.get("severity", "")).lower(), "**NOTE**")
    parts = [f"{sev} — {f.get('title', '').strip()}", "", f.get("detail", "").strip()]
    advice = f.get("advice")
    if isinstance(advice, str) and advice.strip():
        parts += ["", f"**Fix:** {advice.strip()}"]
    # `suggestion` is the exact one-line replacement (or null) — render it as a
    # one-click GitHub suggestion block. `advice` above is the prose how-to-fix
    # that's always present; the suggestion block is the bonus clean-apply path.
    sug = f.get("suggestion")
    if isinstance(sug, str) and sug.strip():
        parts += ["", "```suggestion", sug.rstrip("\n"), "```"]
    return "\n".join(parts).strip()


def write_comment_fallback(text):
    with open(os.path.join(TEMP, "comment.md"), "w") as fh:
        fh.write("### 🤖 darkmux PR review\n\n")
        fh.write((text or "_The reviewer returned no parseable output._").strip())
        fh.write(FOOTER)
    print("mode=comment")


def main():
    try:
        env = json.load(open(ENVELOPE))
    except (OSError, json.JSONDecodeError):
        write_comment_fallback("_The review dispatch produced no envelope._")
        return
    reply = env.get("final_assistant") or ""
    data = extract_findings(reply)
    if not isinstance(data, dict) or "findings" not in data:
        # Unparseable — post whatever the model said as a summary, don't lose it.
        write_comment_fallback(reply)
        return

    diff_text = open(DIFF).read() if os.path.exists(DIFF) else ""
    anchors = valid_lines(diff_text)
    summary = str(data.get("summary", "")).strip()
    verdict = str(data.get("verdict", "")).strip().lower()
    findings = data.get("findings") or []

    inline, deferred = [], []
    for f in findings:
        if not isinstance(f, dict):
            continue
        path, line = f.get("path"), f.get("line")
        if path in anchors and isinstance(line, int) and line in anchors[path]:
            inline.append({"path": path, "line": line, "side": "RIGHT", "body": comment_body(f)})
        else:
            deferred.append(f)

    body = ["### 🤖 darkmux PR review (local model)", ""]
    if summary:
        body += [summary, ""]
    body.append(f"**Verdict: {verdict or 'n/a'}** · {len(inline)} inline, {len(deferred)} general")
    if deferred:
        body += ["", "**Findings not anchored to a diff line:**"]
        for f in deferred:
            sev = SEV.get(str(f.get("severity", "")).lower(), "NOTE")
            loc = f"`{f.get('path', '?')}:{f.get('line', '?')}`"
            line = f"- {sev} {loc} — {f.get('title', '').strip()}: {f.get('detail', '').strip()}"
            advice = f.get("advice")
            if isinstance(advice, str) and advice.strip():
                line += f" _Fix: {advice.strip()}_"
            body.append(line)
    review = {"event": "COMMENT", "body": "\n".join(body) + FOOTER, "comments": inline}
    with open(os.path.join(TEMP, "review.json"), "w") as fh:
        json.dump(review, fh)
    print("mode=review")


if __name__ == "__main__":
    main()
