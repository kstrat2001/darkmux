"""Tiny log-line parser. Synthetic fixture for darkmux lab quickstart.

Self-contained: stdlib only, single function, intentionally small so
the demo-quickstart workload exercises the COW-clone + per-run-sandbox
+ verify-loop without touching any external tooling beyond `python3`.
"""

from typing import Optional


def parse_line(line: str) -> Optional[dict]:
    """Parse a `<timestamp>  <LEVEL>  <message>` log line.

    Whitespace separation requires 2+ spaces between fields. Returns
    None for malformed input rather than raising — callers filter.
    """
    stripped = line.strip()
    if not stripped:
        return None
    parts = []
    current = stripped
    for _ in range(2):
        idx = current.find("  ")
        if idx == -1:
            return None
        parts.append(current[:idx].strip())
        current = current[idx:].strip()
    parts.append(current)
    if len(parts) != 3:
        return None
    timestamp, level, message = parts
    if not timestamp or not level or not message:
        return None
    return {"timestamp": timestamp, "level": level, "message": message}
