// Deterministic sanitizer for corpus fixtures recorded from the operator's
// LIVE daemon. The fixtures are committed to a PUBLIC repo (kstrat2001/darkmux),
// so anything client-identifying that came off the live daemon must be
// rewritten to a synthetic equivalent BEFORE a byte touches disk.
//
// FIELD POLICY, NOT TOKEN SCANNING (QA finding, post-0a review). The first
// version of this module was entity-scanning: it rewrote any token
// CONTAINING one of a handful of sentinel words ("finhub", "finsys", ...).
// That measures the wrong thing — everything that DOESN'T happen to spell a
// sentinel word passes verbatim. Real leaks that survived it: `bundle_id`
// values (`someVerb@app/controllers/some_domain_controller.ts` —
// a live client source-tree shape), a 2,900-char client engagement brief in
// a mission's `source_input` field (migration filenames, column names, CI
// narrative), and `sys_NNNN` (underscore form — the old ticket regex only
// matched the hyphen form). None of those contain "finhub" or "finsys"; the
// word-scanner had nothing to catch them on.
//
// The fix inverts the model: **content-bearing and path-bearing fields are
// unsafe BY DEFAULT.** Every string leaf is classified by its FIELD NAME
// against three buckets:
//   - PROSE_FIELDS  → wholesale replacement with generated placeholder prose
//                      of similar length (free-text payloads: description,
//                      source_input, reasoning, ...)
//   - PATH_FIELDS    → format-preserving path replacement, same depth, same
//                      extension, synthetic segment names (bundle_id)
//   - SAFE_FIELDS    → passthrough — but STILL run through the entity-token
//                      scan below as defense-in-depth (enums, ids, numbers,
//                      timestamps, darkmux-internal identifier shapes)
// A field name in none of the three buckets is UNKNOWN, and unknown fields
// are NEVER passed through verbatim — they get a shape-appropriate default
// (see `classify` / `sanitizeString`'s "default" branch). This is the whole
// point: a new field the policy hasn't been told about yet is unsafe until
// someone explicitly allowlists it, not safe until someone notices a leak.
//
// The old entity-token scan (sentinel words, ticket refs, full SHAs) is KEPT
// as a defense-in-depth layer applied to every SAFE field too — it's what
// catches an entity reference riding inside a field that's legitimately
// mostly-safe (e.g. `ansi_text`, the CLI's own rendered output, which mixes
// real mission titles with structural chrome no field-name policy alone
// could safely blank without destroying the golden's value).
//
// Strategy: walk the PARSED JSON value tree (not the raw text — an earlier
// version ran regexes over the raw JSON text and a sentinel word sitting
// directly after a JSON escape marker with no separator let the regex
// consume the escape's letter itself, corrupting valid JSON; walking
// already-unescaped string values sidesteps that whole bug class).
//
// Determinism: every replacement is derived from a SHA-256 digest of the
// ORIGINAL value, never a counter or random source, so re-recording the same
// real content always reproduces the same synthetic output — a corpus diff
// after re-recording shows real changes, not sanitizer entropy. The
// original->synthetic mapping is never written to disk.
//
// Machine names (MacBook-Pro, m1-max-32gb-studio) are deliberately NOT
// touched — operator hardware names aren't client-identifying and the plan
// brief says they may stay. `kstrat2001` (the operator's own public OSS org)
// is likewise left alone where it appears — it's the operator's own public
// identity, not client engagement content.

import { createHash } from "node:crypto";

// The hard tripwire list (case-insensitive substrings) — the sanitizer's own
// active-redaction vocabulary. `tripwire.mjs` uses a BROADER, independent
// canary list (see CANARIES below) for verification; this one drives what
// sanitizeString actively rewrites.
export const SENTINELS = ["FinHero", "finhero", "SYS-", "SYS_", "ExtraGalaxies", "finsys", "finhub"];

// Verification-only canary list (QA finding, post-0a review): broader than
// SENTINELS on purpose. `tripwire.mjs` scans committed output for these as
// an INDEPENDENT check of whether the field-policy actually worked — it does
// not drive what the sanitizer rewrites (that's SENTINELS + the field
// policy below), it drives what the standalone verification script flags.
// Includes underscore/no-separator sentinel forms plus corpus-specific
// canaries observed in the actual leak (borrower/lender/consent domain
// vocabulary, the inertia/pages path prefix, admin_ controller naming).
export const CANARIES = [
  ...SENTINELS,
  "extragalaxies",
  "finsys_",
  "finhub_",
  "borrower",
  "lender",
  "consent",
  "inertia/pages",
  "admin_",
];

// Matches a maximal run of identifier/path-safe characters that CONTAINS one
// of the sentinel substrings anywhere inside it — still applied to every
// field (SAFE fields included) as defense-in-depth. `@` is included so
// `org/repo@<sha>` compounds are replaced as one unit.
const SENTINEL_TOKEN_RE = /[A-Za-z0-9_./@-]*(?:extragalaxies|finhero|finsys|finhub)[A-Za-z0-9_./@-]*/gi;

// Jira-shaped ticket refs — both separator forms (`SYS-2590`, `sys_2609`).
const TICKET_RE = /\bSYS[-_]\d+\b/gi;

// A full 40-hex git commit SHA. Deliberately NOT matching shorter abbreviated
// forms (7-12 hex chars) — indistinguishable from plain numeric fields.
const SHA40_RE = /\b[a-f0-9]{40}\b/g;

// Any IPv4-shaped substring, anywhere, in any field — defense-in-depth catch
// for things like `redis_url_redacted`'s embedded tailnet address. Kept
// deliberately permissive (any dotted-quad) rather than scoped to one field
// name, since a leaked IP could show up in a URL, a log line, anywhere.
const IPV4_RE = /\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b/g;

// A Tailscale MagicDNS name — `<machine>.<tailnet>.ts.net`. Same
// defense-in-depth reasoning as IPV4_RE above, and added because the IP rule
// alone was NOT enough: a captured `doctor` golden carried a synthetic IP
// (this scrubber had rewritten it) sitting directly beside the operator's REAL
// MagicDNS hostname, which nothing rewrote. The tailnet component is the
// durable identifier of the two — an address can be reassigned, a tailnet name
// is the same string everywhere it appears.
const MAGICDNS_RE = /\b([a-z0-9-]+)\.([a-z0-9-]+)\.ts\.net\b/gi;

function stableHex(input, salt) {
  return createHash("sha256").update(salt + ":" + input).digest("hex");
}

// Format-preserving substitution: keeps every separator character
// (`. _ - / @`) exactly where it was, and replaces every alnum character with
// a hash-derived character of the SAME class (lowercase stays lowercase,
// uppercase stays uppercase, digit stays digit) — so the synthetic token has
// identical length and identical shape to the original.
function syntheticIdentifier(original) {
  const digest = stableHex(original, "id");
  let digestPos = 0;
  const nextNibble = () => {
    const c = digest[digestPos % digest.length];
    digestPos++;
    return parseInt(c, 16); // 0-15
  };
  const LOWER = "abcdefghijklmnopqrstuvwxyz";
  const UPPER = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
  const DIGIT = "0123456789";
  let out = "";
  for (const ch of original) {
    if (/[._/@-]/.test(ch)) {
      out += ch;
    } else if (/[a-z]/.test(ch)) {
      out += LOWER[(nextNibble() * 16 + nextNibble()) % LOWER.length];
    } else if (/[A-Z]/.test(ch)) {
      out += UPPER[(nextNibble() * 16 + nextNibble()) % UPPER.length];
    } else if (/[0-9]/.test(ch)) {
      out += DIGIT[nextNibble() % DIGIT.length];
    } else {
      out += ch; // shouldn't happen given the source regexes, but never drop a char
    }
  }
  return out;
}

function syntheticHex40(original) {
  return stableHex(original, "sha40").slice(0, 40);
}

function syntheticUuid(original) {
  const digest = stableHex(original, "uuid").toUpperCase();
  return `${digest.slice(0, 8)}-${digest.slice(8, 12)}-${digest.slice(12, 16)}-${digest.slice(16, 20)}-${digest.slice(20, 32)}`;
}

function syntheticIpv4(original) {
  const digest = stableHex(original, "ip");
  const octet = (i) => 1 + (parseInt(digest.slice(i * 2, i * 2 + 2), 16) % 254);
  // Kept in the CGNAT-shaped 100.x.x.x range for plausibility (the real
  // value it replaces is a Tailscale address in that same range) — not load
  // bearing, just avoids a jarring shape change in the fixture.
  return `100.${octet(1)}.${octet(2)}.${octet(3)}`;
}

function scrubIpv4(s) {
  return s.replace(IPV4_RE, (m) => syntheticIpv4(m));
}

// Both components are rewritten, not just the tailnet: a machine name is often
// the operator's hardware model, which is identifying on its own.
function scrubMagicDns(s) {
  return s.replace(MAGICDNS_RE, (_m, host, tailnet) => {
    const h = stableHex(host, "magicdns-host").slice(0, 8);
    const t = stableHex(tailnet, "magicdns-tailnet").slice(0, 10);
    return `host-${h}.tailnet-${t}.ts.net`;
  });
}

// Deterministic placeholder prose: same overall length as the original
// (exact), newlines preserved at their original offsets (so a multi-
// paragraph brief still LOOKS like a multi-paragraph brief in the fixture,
// with zero real words surviving), everything else replaced by a
// hash-selected word from a small fixed lorem vocabulary. This is what
// "generated placeholder prose of similar length, deterministic from a hash
// of the original" means in the QA brief — re-recording the same real brief
// always produces the same placeholder.
const LOREM_WORDS = [
  "lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "do",
  "eiusmod", "tempor", "incididunt", "ut", "labore", "et", "dolore", "magna", "aliqua", "enim",
  "minim", "veniam", "quis", "nostrud", "exercitation", "ullamco", "laboris", "nisi", "aliquip", "ex",
  "ea", "commodo", "consequat", "duis", "aute", "irure", "in", "reprehenderit", "voluptate", "velit",
  "esse", "cillum", "fugiat", "nulla", "pariatur", "excepteur", "sint", "occaecat", "cupidatat", "non",
  "proident", "sunt", "culpa", "qui", "officia", "deserunt", "mollit", "anim", "id", "est", "laborum",
];
function placeholderProse(original) {
  if (!original) return original;
  const digest = stableHex(original, "prose");
  let idx = 0;
  const nextWord = () => {
    const byte = parseInt(digest.slice((idx * 2) % (digest.length - 1), (idx * 2) % (digest.length - 1) + 2), 16) || 0;
    idx++;
    return LOREM_WORDS[byte % LOREM_WORDS.length];
  };
  let out = "";
  while (out.length < original.length) out += (out.length ? " " : "") + nextWord();
  out = out.slice(0, original.length);
  const chars = out.split("");
  for (let i = 0; i < original.length; i++) if (original[i] === "\n") chars[i] = "\n";
  return chars.join("");
}

// --- field policy -----------------------------------------------------

// Content-bearing free-text fields — wholesale placeholder-prose
// replacement, no attempt to scrub-in-place. Anything that is genuinely
// operator-authored prose about darkmux's own internal work (not client
// content) still gets replaced: the cost is a slightly less "real-looking"
// golden for the (currently zero) goldens that render this text; the
// alternative is trusting a regex to have caught everything, which is
// exactly the failure mode this rewrite exists to close.
const PROSE_FIELDS = new Set([
  "description",
  "source_input",
  "reasoning",
  "reasoning_text",
  "reason",
  "degenerate",
  "short_circuit",
  "attribution_note",
  "detail",
  "prompt",
  "source_text",
  "stderr_tail",
]);

// Path-shaped fields — run through syntheticIdentifier (segment/extension/
// depth preserving) rather than prose replacement.
const PATH_FIELDS = new Set(["bundle_id"]);

// Hardware UUID fields — synthetic stable UUID, not a character-preserving
// scramble (a scrambled-but-same-shape UUID would still look like a real
// hardware identifier; a hash-derived UUID is unambiguously synthetic).
const UUID_FIELDS = new Set(["machine_uid"]);

// Known-safe fields: identifier shapes (darkmux-generated, not client
// content), enums, numbers-as-strings, timestamps, versions. Still subject
// to the universal entity-token/ticket/SHA/IP scan below. Curated from an
// exhaustive survey of every string field observed across a real recorded
// corpus (2026-08-09) — see the field-policy review that added this list.
const SAFE_FIELDS = new Set([
  "_type", "action", "ansi_text", "args", "argv", "attribution", "build",
  "captured_date", "captured_prev_date", "case_ids", "category", "command",
  "config_id", "cpu_brand", "crew", "daemon_url", "darkmux_version", "date",
  "decision", "dir", "display_name", "endpoint", "event", "exec_mode",
  "extra", "file", "finish_reason", "first_date", "first_ts",
  "flow_schema_version", "handle", "http_status", "id", "identifier",
  "image", "inputs_fingerprint", "kind", "label", "last_date", "last_ts",
  "level", "limit_source", "machine", "machine_id", "machines",
  "mission_id", "mission_status", "missions", "model", "model_key", "name",
  "orchestrator", "origin", "os", "owner", "panel", "parentId", "path",
  "phase_id", "phase_ids", "profile", "recorded_at_iso",
  "redis_url_redacted", "reasoning_format", "result_class", "role",
  "role_id", "route", "ruling", "runtime", "schema_version", "served_model",
  "session_id", "size", "source", "stage", "state", "status", "step_id",
  "surface", "target", "task_ids", "tier", "ts", "url", "version",
  "workspace",
]);
// (#1868 packet 1) `label`, `mission_status`, `parentId`, `target` added for
// /mission/:id/graph.json, the mission-graph parity fixture's node/edge
// shape (crates/darkmux-serve/src/mission_graph.rs). Short structural
// display strings and graph-linkage ids, the same character as `handle`/
// `phase_id`/`step_id` already on this list, not free-text prose. Verified
// unused by any other currently-recorded endpoint when these were added
// (`grep -l '"label"' corpus/*.json` etc. all came back empty), so this
// doesn't loosen coverage anywhere already recorded.
//
// One caveat worth carrying forward (review finding, #1868): `label` is
// NOT purely structural the way the other three are. `mission_graph.rs`
// resolves it to a task's/phase's `display_name` (falling back to the id),
// which is operator-authored config text, the same character as
// `display_name` already on this list. It is safe for the CURRENT fixture
// because every built-in mission config labels its work with structural
// verbs (Investigate, Bundle, Verify). Anyone recording a SECOND graph
// fixture from a hand-authored engagement mission must read its labels
// before committing the corpus file. (`description`, which for coder-phase
// doubles as the coder's dispatch brief, is correctly in PROSE_FIELDS.)

function classifyField(fieldName) {
  if (PROSE_FIELDS.has(fieldName)) return "prose";
  if (PATH_FIELDS.has(fieldName)) return "path";
  if (UUID_FIELDS.has(fieldName)) return "uuid";
  if (SAFE_FIELDS.has(fieldName)) return "safe";
  return "unknown"; // never verbatim — see sanitizeString's default branch
}

/** The universal defense-in-depth pass: sentinel-token / ticket / SHA / IPv4 scrub. Applied to every field regardless of classification. */
function entityScan(s, matched) {
  let out = s.replace(SENTINEL_TOKEN_RE, (m) => {
    matched.identifiers++;
    return syntheticIdentifier(m);
  });
  out = out.replace(TICKET_RE, (m) => {
    matched.tickets++;
    return syntheticIdentifier(m);
  });
  out = out.replace(SHA40_RE, (m) => {
    matched.shas++;
    return syntheticHex40(m);
  });
  const beforeIp = out;
  out = scrubIpv4(out);
  if (out !== beforeIp) matched.ips++;
  const beforeDns = out;
  out = scrubMagicDns(out);
  if (out !== beforeDns) matched.magicdns++;
  return out;
}

/** Sanitize one already-unescaped JS string value, given the JSON field name it was found under. */
function sanitizeString(s, fieldName, matched) {
  const cls = classifyField(fieldName);
  if (cls === "prose") {
    matched.prose++;
    return placeholderProse(s);
  }
  if (cls === "path") {
    matched.paths++;
    return syntheticIdentifier(s);
  }
  if (cls === "uuid") {
    matched.uuids++;
    return syntheticUuid(s);
  }
  if (cls === "safe") {
    return entityScan(s, matched);
  }
  // Unknown field: never pass through verbatim. Guess a shape-appropriate
  // default from the VALUE (not the field name, which by definition told us
  // nothing) — prose-like (contains whitespace, reasonably long) gets
  // placeholder prose; everything else gets the identifier scramble. Either
  // way it's recorded so the transcript can name which unclassified fields
  // showed up, for the next explicit-allowlist pass.
  matched.unknownFields.add(fieldName);
  matched.unknown++;
  if (s.length > 24 && /\s/.test(s)) return placeholderProse(s);
  return syntheticIdentifier(entityScan(s, matched));
}

/** Recursively walk a parsed JSON value, sanitizing every string leaf by its enclosing field name. */
function walk(value, matched, fieldName) {
  if (typeof value === "string") return sanitizeString(value, fieldName, matched);
  if (Array.isArray(value)) return value.map((v) => walk(v, matched, fieldName));
  if (value && typeof value === "object") {
    const out = {};
    for (const [k, v] of Object.entries(value)) out[k] = walk(v, matched, k);
    return out;
  }
  return value; // number / boolean / null pass through untouched
}

/**
 * Sanitize a raw JSON response body (string). Parses, walks every string
 * leaf under the field-name policy above, and re-serializes pretty-printed.
 * Returns the sanitized text plus a report of what was rewritten (counts
 * only — never the original values, and `unknownFields` names FIELD NAMES
 * never field VALUES) for the record script's transcript. Throws if `text`
 * is not valid JSON.
 */
export function sanitizeText(text) {
  const parsed = JSON.parse(text);
  const matched = {
    identifiers: 0, tickets: 0, shas: 0, ips: 0, magicdns: 0,
    prose: 0, paths: 0, uuids: 0, unknown: 0,
    unknownFields: new Set(),
  };
  const sanitized = walk(parsed, matched, "");
  return {
    text: JSON.stringify(sanitized, null, 2) + "\n",
    matched: { ...matched, unknownFields: [...matched.unknownFields] },
  };
}

/** Case-insensitive scan for any surviving CANARY substring (broader than SENTINELS — see module doc). Returns the list of hits, empty when clean. */
export function scanForSentinels(text) {
  const hits = [];
  for (const s of CANARIES) {
    const re = new RegExp(s.replace(/[-/\\^$*+?.()|[\]{}]/g, "\\$&"), "gi");
    const m = text.match(re);
    if (m && m.length) hits.push({ sentinel: s, count: m.length });
  }
  return hits;
}
