#!/usr/bin/env python3
"""Turn a darkmux pr-reviewer envelope into a GitHub PR review (DEVOPS-466).

Reads the darkmux `--json` envelope (final_assistant carries the model's JSON
findings block) plus the PR diff, and emits a payload for the GitHub review
API with native inline comments. Robust by design:

  * Tolerant JSON extraction (the model may wrap the block in ```json fences
    or add stray prose) — local models malform output, so we never hard-fail.
  * Each finding carries an `anchor` — a verbatim quote of the line it's about,
    not a line number (local models identify the construct reliably but guess
    its coordinate badly). We resolve that quote to a new-side line by matching
    it against the diff, because GitHub rejects an inline comment on a line it
    can't see. Findings whose anchor is null (file-level) or can't be matched to
    exactly one shown line are folded into the summary body, never guessed onto a
    line.
  * If the block won't parse at all, we fall back to posting the raw review as
    a single summary comment.

Outputs to $RUNNER_TEMP/qa/:
  - review.json  + prints `mode=review`   → POST /pulls/:n/reviews --input
  - comment.md   + prints `mode=comment`  → gh pr comment --body-file
"""
import json
import os
import re

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


def norm_path(p):
    """Strip a leading a/ b/ ./ so a model-cited path matches the diff's path."""
    if isinstance(p, str):
        for pre in ("a/", "b/", "./"):
            if p.startswith(pre):
                return p[len(pre):]
    return p


def new_side_index(diff_text):
    """Map path -> {trimmed new-side line content: [line numbers]}.

    The reviewer cites a finding's location by quoting the line verbatim
    (`anchor`), not by emitting a line number — local models identify the
    construct reliably but guess its coordinate badly, so we do the coordinate
    half deterministically here: index every new-side (added/context) line's
    content so an anchor can be resolved to its line.

    `+++ ` is only treated as a file header before the first hunk (gated by
    `in_hunk`, reset at each `diff --git`), so an added content line whose text
    is itself `+++ ...` (a diff snippet in docs) is never misread as a header.
    """
    out, path, newln, in_hunk = {}, None, None, False
    for line in diff_text.splitlines():
        if line.startswith("diff --git "):
            path, newln, in_hunk = None, None, False
        elif not in_hunk and line.startswith("+++ "):
            p = line[4:].strip()
            path = p[2:] if p.startswith("b/") else (None if p == "/dev/null" else p)
            if path:
                out.setdefault(path, {})
        elif line.startswith("@@"):
            m = re.search(r"\+(\d+)", line)
            newln = int(m.group(1)) if m else None
            in_hunk = True
        elif in_hunk and path and newln is not None:
            if line.startswith("+") or line.startswith(" "):
                content = line[1:].strip()
                if content:
                    out[path].setdefault(content, []).append(newln)
                newln += 1
            elif line.startswith("-") or line.startswith("\\"):
                pass  # removed / "No newline" — no new-side position
    return out


def resolve_anchor(path, anchor, index):
    """Resolve a finding's verbatim `anchor` to a new-side line number.

    Returns an int line, or None when the finding is file-level (`anchor` null)
    or its quote can't be matched to exactly one shown new-side line — those
    post as a general comment, never on a guessed line. Takes the first
    non-empty line of a multi-line quote and matches on trimmed text so minor
    whitespace differences in the quote don't block the match.

    The quote is tried **as-is first**, then with one leading `+`/`-`/space
    stripped. `new_side_index` already stored content with the diff marker
    removed, so a line whose *content* legitimately begins with `-`/`+` (a
    markdown bullet, a `+++ ...` diff snippet in docs — common in darkmux's own
    docs) matches as-is; the strip is the fallback for a model that left the
    diff marker on the quote. As-is first avoids double-stripping the content.
    """
    if not isinstance(anchor, str) or not anchor.strip():
        return None
    first = next((ln for ln in anchor.splitlines() if ln.strip()), "")
    table = index.get(norm_path(path), {})
    candidates = [first.strip()]
    if first[:1] in ("+", "-", " "):
        candidates.append(first[1:].strip())
    for key in candidates:
        if not key:
            continue
        hits = table.get(key, [])
        if len(hits) == 1:
            return hits[0]
    return None


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
    index = new_side_index(diff_text)
    summary = str(data.get("summary", "")).strip()
    verdict = str(data.get("verdict", "")).strip().lower()
    findings = data.get("findings") or []

    inline, deferred = [], []
    for f in findings:
        if not isinstance(f, dict):
            continue
        line = resolve_anchor(f.get("path"), f.get("anchor"), index)
        if line is not None:
            inline.append({"path": norm_path(f.get("path")), "line": line, "side": "RIGHT", "body": comment_body(f)})
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
            loc = f"`{norm_path(f.get('path', '?'))}`"
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
