# crawl-tracker

A local Node.js issue tracker for darkmux crawl events (darkmux #1959) that is
also the crawler's **seen-set** (dedup). Zero npm dependencies, Node 24,
`node:sqlite` for storage (including FTS5 search).

This is a standalone example app. It does not require the darkmux Rust
workspace to build or run, and it is not wired into it — see **How darkmux is
expected to point at it** below for what integration would look like; that
side is a separate, not-yet-built packet.

## Run it

```sh
cd examples/crawl-tracker
node server.mjs                       # db: ./tracker.db, port 8790, bind 127.0.0.1
node server.mjs --db :memory:         # ephemeral, nothing persisted
node server.mjs --port 9000 --db ~/crawl-tracker.db
```

Open `http://127.0.0.1:8790/` for the UI (search, filters, a finding detail
pane with status buttons and a note field, a Coverage table, and a Missions
list).

There is no authentication. `--bind` refuses any value that is not loopback
(`127.0.0.1`, `::1`, `localhost`) — see **Security posture** below.

## Test it

```sh
npm test
# or directly:
node --test test/*.test.mjs
```

`node --test test/` (the bare directory form) does not work reliably on this
Node build (v24.16.0) — it fails with a spurious `Cannot find module 'test'`
error. Use the glob form (`test/*.test.mjs`), which is what `npm test` runs.

## The event contract

The tracker receives **darkmux flow records** — the same record shape every
other darkmux flow-stream consumer sees (`crates/darkmux-flow/src/schema.rs`,
`FLOW_SCHEMA_VERSION` currently `"1.22.0"`). It is not a bespoke
crawler-only shape: crawl context rides inside a flow record's `payload`
like any other producer's does.

`POST /events` with `Content-Type: application/json` and a body that is
**one flow record, or a JSON array of them**.

Top-level flow-record fields this tracker reads (all optional except
`action`, and lenient on read — an unrecognized field is ignored, a missing
one is tolerated):

| Field | Used for |
|---|---|
| `action` | **Required.** Routes the record — see Actions below. |
| `ts` | Sighting/event timestamp. |
| `session_id` | Recorded on findings/exclusions as the sighting's session. |
| `mission_id` | The crawl run this record belongs to (a crawl run is a darkmux mission). Used by `GET /missions` and the finding-history sighting list. |
| `machine_id` | Stored on the raw event row. |
| `payload` | **All crawl-specific fields live here** — see Actions below. |

Every other top-level flow-record field (`level`, `category`, `tier`,
`stage`, `handle`, `phase_id`, `source` — note this is a *different* field
than `payload.source` — `model`, `machine_uid`, `work_id`, `attempt`,
`prev_hash`, `hash`) is accepted and ignored.

### Actions

- **`crawl.finding`** — payload: `corpus, source, sha, rule, unit, file,
  line, evidence, why, context, context_start, context_end,
  evidence_mismatch`. Creates or touches a finding (see Dedup below).
- **`crawl.exclusion`** — same payload shape plus `reason`. Never creates a
  finding; recorded for recall auditing.
- **`crawl.unit.started`** — payload: `corpus, unit, source, sha, rule,
  kind, est_tokens, sites|files`. A unit is one bounded crawl task: one
  pattern (`rule`) against one source at one commit (`sha`).
- **`crawl.unit.completed`** — payload: `corpus, unit, source, sha, rule,
  result, findings, exclusions, prompt_tokens, completion_tokens, wall_ms`.
- Any other `action` (e.g. `dispatch.start`, `dispatch.complete` — the
  liveness bookends every darkmux dispatch emits) is **stored raw and
  acknowledged, never rejected.** The tracker is one consumer among several
  on a flow stream; deciding what reaches it is the hook filter's job, on
  the darkmux side, not this receiver's.

A record with no `action` field is rejected with `400`. Malformed JSON is
rejected with `400`. A `crawl.finding` or `crawl.exclusion` record missing
`corpus`, `source`, `rule`, or `payload.file` is rejected with `400`
(everything else in the contract is lenient on read; those four are the
minimum needed to compute an identity).

**There is no "night."** An earlier draft of this contract had a
time-bounded "night" batch concept; that was cut (a crawl is continuous, not
run in nightly batches). The two groupings that replaced it:

- **`mission_id`** (top-level, one per crawl run) — "what happened in this
  run" (`GET /missions`).
- **`(source, sha, rule)`** (all inside `payload`, all three present on
  every unit/finding/exclusion record) — "how covered is this
  source-at-commit against this pattern, across every run that ever touched
  it" (`GET /coverage`). This is the durable one: coverage accumulates
  across missions, because the seen-set does too.

### Response shape

`200 { "ok": true, "accepted": N, "results": [ ... one per record, same order ... ] }`

Per-record result shape:

| `action` | Result |
|---|---|
| `crawl.finding` | `{ "action": "crawl.finding", "finding_id": N, "status": "new" \| "seen", "times_seen": N }` |
| `crawl.exclusion` | `{ "action": "crawl.exclusion", "exclusion_id": N }` |
| anything else | `{ "action": "...", "stored": true }` |

## Dedup — the seen-set

A finding's identity (`finding_key`) is:

```
sha256(corpus | source | rule | file | normalize(evidence))
```

where `normalize` trims the evidence string and collapses internal
whitespace runs to a single space. **Deliberately not the line number** — a
finding whose cited line moved (a file edited above it, a refactor) is
still the same finding; only `evidence` identity matters. See
`normalizeEvidence` / `findingKey` in `db.mjs`.

Every `crawl.finding` record either creates a new `findings` row
(`status: "new"`, `times_seen: 1`) or touches an existing one by key
(`status: "seen"`, `times_seen` incremented, `line`/`why`/`context`
refreshed to the latest sighting). Either way a row is appended to
`sightings` (one per occurrence, carrying `mission_id`, `sha`, `unit`,
`session_id`, `line`, `ts`).

**A `rejected` finding that is seen again stays `rejected`.** `times_seen`
still increments and a new sighting is still recorded — but `status` is
never reset by re-sighting. This is the point of the seen-set: dedup is
against everything the crawler has ever **seen**, not everything a human
has **accepted**. Marking a finding `rejected` or `deferred` is a durable
verdict, not a snooze.

`exclusions` get their own table, keyed the same way (plus `reason`), so
recall can be audited later ("what did the crawler decide NOT to report,
and why") — they never create or touch a `findings` row.

## Read endpoints

- `GET /findings?q=&corpus=&source=&rule=&status=&mission_id=&limit=&offset=`
  — `q` is a full-text search (FTS5) over `file`, `evidence`, `why`, `note`,
  `rule`, `source`. A malformed FTS query (e.g. an unbalanced quote) returns
  `400`, not `500`. Ordered by `last_seen` desc. `limit` defaults to 50,
  capped at 500.
- `GET /findings/:id` — the finding row plus its full `sightings` history.
- `PATCH /findings/:id` — body `{ "status"?: "new"|"confirmed"|"rejected"|"deferred", "note"?: "..." }`. Invalid status → `400`.
- `GET /coverage?corpus=` — per `(source, sha, rule)`: `units_started`,
  `units_completed`, `findings`, `exclusions`, `last_activity`. "Where are
  we" for a crawl that never ends.
- `GET /missions?corpus=` — every `mission_id` seen, with `first_seen`,
  `last_seen`, `units_completed`, `findings`. A mission has no
  start/completion semantics here (unlike the retired "night" concept) — it
  simply has activity or it doesn't.
- `GET /stats` — totals `by_status` and `by_rule`.
- `GET /health` — `{ "ok": true, "db": "<path>", "findings": N }`.
- `GET /` — the UI (`ui.html`, inline CSS/JS, no build step, no CDN).

## How darkmux is expected to point at it (not wired up yet)

This receiver is designed to be one sink named in a corpus manifest's
`hooks` list, the same way any other flow sink is configured — e.g.:

```json
{
  "hooks": [
    { "on": ["crawl.finding", "crawl.exclusion", "crawl.unit.started", "crawl.unit.completed"], "http": "http://127.0.0.1:8790/events" }
  ]
}
```

**The darkmux side of this (actually reading `hooks` from a corpus/config
and POSTing flow records here) is a separate, not-yet-built packet.** This
README describes the contract this receiver implements and will keep
implementing; it does not claim darkmux emits to it today.

## Security posture

No authentication. `--bind` refuses anything that is not loopback
(`127.0.0.1`, `::1`, `localhost`) and the process exits non-zero with a
clear message rather than opening a socket. If you need this reachable from
another machine, put it behind your own reverse proxy with auth — don't
loosen the bind check.

## Files

- `server.mjs` — HTTP server + CLI entry (`--db`, `--port`, `--bind`). No
  inline SQL.
- `db.mjs` — schema + every SQL statement. `TrackerDB` is usable directly
  (no HTTP) for tests or scripts.
- `ui.html` — the single-page UI. Inline CSS + one `<script>` block, no
  inline event handlers (works under a strict CSP), no CDN.
- `test/db.test.mjs` — storage-layer unit tests (dedup key stability).
- `test/server.test.mjs` — HTTP-level tests against the real server on an
  ephemeral port with an in-memory db.
- `test/bind.test.mjs` — spawns the real process to verify the loopback-only
  bind refusal and graceful `SIGTERM` handling.
