// Schema + all SQL for the crawl-tracker reference receiver (darkmux #1959).
// The server has no inline SQL: everything that touches the database goes
// through the TrackerDB class below, so `node --test test/db.test.mjs` can
// exercise the storage layer directly, without an HTTP round trip.
//
// The inbound event IS a darkmux flow record (see crates/darkmux-flow/src/schema.rs,
// FLOW_SCHEMA_VERSION "1.22.0"), not a bespoke crawler-only shape. The tracker
// is one more flow-stream consumer: `action` (not `event`) names the record kind,
// and every crawl-specific field (corpus, unit, source, sha, rule, file, line,
// evidence, ...) lives inside `payload`. `session_id`, `mission_id`,
// `machine_id`, and `ts` are top-level flow-record fields.
//
// There is no "night": a crawl is continuous, not a time-bounded batch. The
// grouping key for "where are we" is the top-level `mission_id` (a crawl run
// is a darkmux mission; each unit is a task within it) and, orthogonally, the
// (source, sha, rule) triple a unit targets — see getCoverage()/getMissions().

import { DatabaseSync } from 'node:sqlite';
import { createHash } from 'node:crypto';

const VALID_STATUSES = new Set(['new', 'confirmed', 'rejected', 'deferred']);

export class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

const SCHEMA = `
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  received_at TEXT NOT NULL,
  action TEXT NOT NULL,
  corpus TEXT,
  unit TEXT,
  source TEXT,
  sha TEXT,
  rule TEXT,
  session_id TEXT,
  mission_id TEXT,
  machine_id TEXT,
  ts TEXT,
  record TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_mission_id ON events(mission_id);
CREATE INDEX IF NOT EXISTS idx_events_action ON events(action);
CREATE INDEX IF NOT EXISTS idx_events_corpus ON events(corpus);
CREATE INDEX IF NOT EXISTS idx_events_coverage ON events(source, sha, rule);

CREATE TABLE IF NOT EXISTS findings (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  finding_key TEXT UNIQUE NOT NULL,
  corpus TEXT NOT NULL,
  source TEXT NOT NULL,
  rule TEXT NOT NULL,
  file TEXT NOT NULL,
  line INTEGER,
  evidence TEXT NOT NULL,
  why TEXT,
  context TEXT,
  context_start INTEGER,
  context_end INTEGER,
  evidence_mismatch INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'new',
  note TEXT,
  first_seen TEXT NOT NULL,
  last_seen TEXT NOT NULL,
  times_seen INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_findings_corpus ON findings(corpus);
CREATE INDEX IF NOT EXISTS idx_findings_status ON findings(status);
CREATE INDEX IF NOT EXISTS idx_findings_rule ON findings(rule);
CREATE INDEX IF NOT EXISTS idx_findings_last_seen ON findings(last_seen);

CREATE TABLE IF NOT EXISTS sightings (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  finding_id INTEGER NOT NULL REFERENCES findings(id),
  mission_id TEXT,
  sha TEXT,
  unit TEXT,
  session_id TEXT,
  line INTEGER,
  ts TEXT,
  event_id INTEGER
);

CREATE INDEX IF NOT EXISTS idx_sightings_finding ON sightings(finding_id);
CREATE INDEX IF NOT EXISTS idx_sightings_mission_id ON sightings(mission_id);

CREATE TABLE IF NOT EXISTS exclusions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  exclusion_key TEXT NOT NULL,
  corpus TEXT NOT NULL,
  source TEXT NOT NULL,
  rule TEXT NOT NULL,
  file TEXT NOT NULL,
  line INTEGER,
  evidence TEXT NOT NULL,
  reason TEXT,
  mission_id TEXT,
  sha TEXT,
  unit TEXT,
  session_id TEXT,
  ts TEXT,
  event_id INTEGER
);

CREATE INDEX IF NOT EXISTS idx_exclusions_mission_id ON exclusions(mission_id);
CREATE INDEX IF NOT EXISTS idx_exclusions_key ON exclusions(exclusion_key);

CREATE VIRTUAL TABLE IF NOT EXISTS findings_fts USING fts5(
  file, evidence, why, note, rule, source
);
`;

// finding_key = sha256(corpus | source | rule | file | normalize(evidence)).
// Deliberately NOT the line number: a line that moved is the same finding.
export function normalizeEvidence(evidence) {
  return String(evidence ?? '').trim().replace(/\s+/g, ' ');
}

export function findingKey({ corpus, source, rule, file, evidence }) {
  const input = [corpus, source, rule, file, normalizeEvidence(evidence)].join('|');
  return createHash('sha256').update(input, 'utf8').digest('hex');
}

function exclusionKey({ corpus, source, rule, file, evidence }) {
  return findingKey({ corpus, source, rule, file, evidence });
}

function nowIso() {
  return new Date().toISOString();
}

function asInt(value, fallback) {
  if (value === undefined || value === null || value === '') return fallback;
  const n = Number.parseInt(value, 10);
  return Number.isFinite(n) ? n : fallback;
}

export class TrackerDB {
  constructor(dbPath) {
    this.path = dbPath;
    this.db = new DatabaseSync(dbPath);
    this.db.exec('PRAGMA journal_mode = WAL;');
    this.db.exec(SCHEMA);
    this.#prepare();
  }

  #prepare() {
    this.stmts = {
      insertEvent: this.db.prepare(
        `INSERT INTO events (received_at, action, corpus, unit, source, sha, rule, session_id, mission_id, machine_id, ts, record)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
      ),
      findFindingByKey: this.db.prepare('SELECT * FROM findings WHERE finding_key = ?'),
      findFindingById: this.db.prepare('SELECT * FROM findings WHERE id = ?'),
      insertFinding: this.db.prepare(
        `INSERT INTO findings
           (finding_key, corpus, source, rule, file, line, evidence, why, context,
            context_start, context_end, evidence_mismatch, status, first_seen, last_seen, times_seen)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'new', ?, ?, 1)`
      ),
      touchFinding: this.db.prepare(
        `UPDATE findings SET
           line = ?, why = ?, context = ?, context_start = ?, context_end = ?,
           evidence_mismatch = ?, last_seen = ?, times_seen = times_seen + 1
         WHERE id = ?`
      ),
      patchFindingStatus: this.db.prepare('UPDATE findings SET status = ? WHERE id = ?'),
      patchFindingNote: this.db.prepare('UPDATE findings SET note = ? WHERE id = ?'),
      insertSighting: this.db.prepare(
        `INSERT INTO sightings (finding_id, mission_id, sha, unit, session_id, line, ts, event_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)`
      ),
      sightingsForFinding: this.db.prepare(
        'SELECT * FROM sightings WHERE finding_id = ? ORDER BY id ASC'
      ),
      insertExclusion: this.db.prepare(
        `INSERT INTO exclusions
           (exclusion_key, corpus, source, rule, file, line, evidence, reason, mission_id, sha, unit, session_id, ts, event_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`
      ),
      ftsDelete: this.db.prepare('DELETE FROM findings_fts WHERE rowid = ?'),
      ftsInsert: this.db.prepare(
        `INSERT INTO findings_fts (rowid, file, evidence, why, note, rule, source)
         VALUES (?, ?, ?, ?, ?, ?, ?)`
      ),
    };
  }

  // --- event ingestion -----------------------------------------------
  // `events` here are darkmux flow records: { action, ts, session_id,
  // mission_id, machine_id, payload: { ... crawl fields ... }, ... }.

  insertEvents(records) {
    const list = Array.isArray(records) ? records : [records];
    if (list.length === 0) {
      throw new HttpError(400, 'request body must be a flow record or a non-empty array of records');
    }
    const results = [];
    const run = this.db.exec.bind(this.db);
    run('BEGIN');
    try {
      for (const record of list) {
        results.push(this.#insertOne(record));
      }
      run('COMMIT');
    } catch (err) {
      run('ROLLBACK');
      throw err;
    }
    return results;
  }

  insertEvent(record) {
    return this.insertEvents([record])[0];
  }

  #insertOne(record) {
    if (record === null || typeof record !== 'object' || Array.isArray(record)) {
      throw new HttpError(400, 'each record must be a JSON object');
    }
    const action = record.action;
    if (typeof action !== 'string' || action.length === 0) {
      throw new HttpError(400, 'record.action is required');
    }

    const receivedAt = nowIso();
    const payload = record.payload ?? {};
    const eventRow = this.stmts.insertEvent.run(
      receivedAt,
      action,
      payload.corpus ?? null,
      payload.unit ?? null,
      payload.source ?? null,
      payload.sha ?? null,
      payload.rule ?? null,
      record.session_id ?? null,
      record.mission_id ?? null,
      record.machine_id ?? null,
      record.ts ?? null,
      JSON.stringify(record)
    );
    const eventId = Number(eventRow.lastInsertRowid);

    if (action === 'crawl.finding') {
      return this.#recordFinding(record, payload, eventId);
    }
    if (action === 'crawl.exclusion') {
      return this.#recordExclusion(record, payload, eventId);
    }
    // A record whose action isn't crawl.* is stored raw above and
    // acknowledged, never rejected — the hook filter on the darkmux side
    // decides what gets sent here; the tracker is lenient on read.
    return { action, stored: true };
  }

  #recordFinding(record, payload, eventId) {
    const required = { corpus: payload.corpus, source: payload.source, rule: payload.rule, file: payload.file };
    for (const [k, v] of Object.entries(required)) {
      if (v === undefined || v === null) {
        throw new HttpError(400, `crawl.finding payload missing required field: ${k}`);
      }
    }
    const evidence = payload.evidence ?? '';
    const key = findingKey({ corpus: payload.corpus, source: payload.source, rule: payload.rule, file: payload.file, evidence });
    const existing = this.stmts.findFindingByKey.get(key);

    const line = payload.line ?? null;
    const why = payload.why ?? null;
    const context = payload.context ?? null;
    const contextStart = payload.context_start ?? null;
    const contextEnd = payload.context_end ?? null;
    const evidenceMismatch = payload.evidence_mismatch ? 1 : 0;
    const now = nowIso();

    let findingId;
    let timesSeen;

    if (existing) {
      this.stmts.touchFinding.run(line, why, context, contextStart, contextEnd, evidenceMismatch, now, existing.id);
      findingId = existing.id;
      const refreshed = this.stmts.findFindingById.get(findingId);
      timesSeen = refreshed.times_seen;
      this.#syncFts(findingId, {
        file: payload.file,
        evidence,
        why,
        note: refreshed.note,
        rule: payload.rule,
        source: payload.source,
      });
    } else {
      const inserted = this.stmts.insertFinding.run(
        key,
        payload.corpus,
        payload.source,
        payload.rule,
        payload.file,
        line,
        evidence,
        why,
        context,
        contextStart,
        contextEnd,
        evidenceMismatch,
        now,
        now
      );
      findingId = Number(inserted.lastInsertRowid);
      timesSeen = 1;
      this.#syncFts(findingId, {
        file: payload.file,
        evidence,
        why,
        note: null,
        rule: payload.rule,
        source: payload.source,
      });
    }

    this.stmts.insertSighting.run(
      findingId,
      record.mission_id ?? null,
      payload.sha ?? null,
      payload.unit ?? null,
      record.session_id ?? null,
      line,
      record.ts ?? null,
      eventId
    );

    return {
      action: 'crawl.finding',
      finding_id: findingId,
      status: existing ? 'seen' : 'new',
      times_seen: timesSeen,
    };
  }

  #recordExclusion(record, payload, eventId) {
    const required = { corpus: payload.corpus, source: payload.source, rule: payload.rule, file: payload.file };
    for (const [k, v] of Object.entries(required)) {
      if (v === undefined || v === null) {
        throw new HttpError(400, `crawl.exclusion payload missing required field: ${k}`);
      }
    }
    const evidence = payload.evidence ?? '';
    const key = exclusionKey({ corpus: payload.corpus, source: payload.source, rule: payload.rule, file: payload.file, evidence });
    const inserted = this.stmts.insertExclusion.run(
      key,
      payload.corpus,
      payload.source,
      payload.rule,
      payload.file,
      payload.line ?? null,
      evidence,
      payload.reason ?? null,
      record.mission_id ?? null,
      payload.sha ?? null,
      payload.unit ?? null,
      record.session_id ?? null,
      record.ts ?? null,
      eventId
    );
    return { action: 'crawl.exclusion', exclusion_id: Number(inserted.lastInsertRowid) };
  }

  #syncFts(findingId, { file, evidence, why, note, rule, source }) {
    this.stmts.ftsDelete.run(findingId);
    this.stmts.ftsInsert.run(findingId, file ?? '', evidence ?? '', why ?? '', note ?? '', rule ?? '', source ?? '');
  }

  // --- reads -----------------------------------------------------------

  getFindings({ q, corpus, source, rule, status, mission_id, limit, offset } = {}) {
    const clauses = [];
    const params = [];
    let fromClause = 'FROM findings f';

    if (q) {
      // FTS5 syntax errors (e.g. an unbalanced quote) surface here; the
      // caller (server.mjs) maps SqliteError to a 400 response.
      fromClause += ' JOIN findings_fts fts ON fts.rowid = f.id';
      clauses.push('findings_fts MATCH ?');
      params.push(q);
    }
    if (corpus) {
      clauses.push('f.corpus = ?');
      params.push(corpus);
    }
    if (source) {
      clauses.push('f.source = ?');
      params.push(source);
    }
    if (rule) {
      clauses.push('f.rule = ?');
      params.push(rule);
    }
    if (status) {
      clauses.push('f.status = ?');
      params.push(status);
    }
    if (mission_id) {
      clauses.push('EXISTS (SELECT 1 FROM sightings s WHERE s.finding_id = f.id AND s.mission_id = ?)');
      params.push(mission_id);
    }

    const where = clauses.length ? `WHERE ${clauses.join(' AND ')}` : '';
    const lim = Math.min(Math.max(asInt(limit, 50), 1), 500);
    const off = Math.max(asInt(offset, 0), 0);

    let rows;
    let total;
    try {
      total = this.db
        .prepare(`SELECT COUNT(*) AS n ${fromClause} ${where}`)
        .get(...params).n;
      rows = this.db
        .prepare(
          `SELECT f.id, f.corpus, f.source, f.rule, f.file, f.line, f.status,
                  f.times_seen, f.first_seen, f.last_seen, f.evidence
           ${fromClause} ${where}
           ORDER BY f.last_seen DESC
           LIMIT ? OFFSET ?`
        )
        .all(...params, lim, off);
    } catch (err) {
      // Any sqlite-level failure while a FTS MATCH clause is in play is
      // treated as a bad search query (400), never a 500 — node:sqlite
      // reports FTS5 syntax errors (unbalanced quotes, bad operators, ...)
      // as a generic ERR_SQLITE_ERROR with a driver-specific message, so we
      // key off the error code rather than pattern-matching the message.
      if (q && err.code === 'ERR_SQLITE_ERROR') {
        throw new HttpError(400, `invalid search query: ${err.message}`);
      }
      throw err;
    }

    return { total, items: rows };
  }

  getFinding(id) {
    const finding = this.stmts.findFindingById.get(id);
    if (!finding) return null;
    const sightings = this.stmts.sightingsForFinding.all(id);
    return { ...finding, sightings };
  }

  patchFinding(id, { status, note } = {}) {
    const finding = this.stmts.findFindingById.get(id);
    if (!finding) return null;
    if (status !== undefined) {
      if (!VALID_STATUSES.has(status)) {
        throw new HttpError(400, `invalid status: ${status}`);
      }
      this.stmts.patchFindingStatus.run(status, id);
    }
    if (note !== undefined) {
      this.stmts.patchFindingNote.run(note, id);
    }
    if (status !== undefined || note !== undefined) {
      const refreshed = this.stmts.findFindingById.get(id);
      this.#syncFts(id, {
        file: refreshed.file,
        evidence: refreshed.evidence,
        why: refreshed.why,
        note: refreshed.note,
        rule: refreshed.rule,
        source: refreshed.source,
      });
    }
    return this.getFinding(id);
  }

  // "Where are we" for a crawl that never ends: per (source, sha, rule),
  // how many units have started/completed and what they turned up. Derived
  // entirely from the events table (which already carries the extracted
  // source/sha/rule columns for every crawl.* record).
  getCoverage({ corpus } = {}) {
    const params = [];
    let corpusClause = '';
    if (corpus) {
      corpusClause = 'AND corpus = ?';
      params.push(corpus);
    }
    return this.db
      .prepare(
        `SELECT source, sha, rule,
                COUNT(DISTINCT CASE WHEN action = 'crawl.unit.started' THEN unit END) AS units_started,
                COUNT(DISTINCT CASE WHEN action = 'crawl.unit.completed' THEN unit END) AS units_completed,
                SUM(CASE WHEN action = 'crawl.finding' THEN 1 ELSE 0 END) AS findings,
                SUM(CASE WHEN action = 'crawl.exclusion' THEN 1 ELSE 0 END) AS exclusions,
                MAX(COALESCE(ts, received_at)) AS last_activity
         FROM events
         WHERE source IS NOT NULL AND sha IS NOT NULL AND rule IS NOT NULL
         ${corpusClause}
         GROUP BY source, sha, rule
         ORDER BY last_activity DESC`
      )
      .all(...params);
  }

  // Every mission_id seen, with activity span + rollup. A mission has no
  // start/completion semantics here — it simply has activity or not.
  getMissions({ corpus } = {}) {
    const params = [];
    let corpusClause = '';
    if (corpus) {
      corpusClause = 'AND corpus = ?';
      params.push(corpus);
    }
    return this.db
      .prepare(
        `SELECT mission_id,
                MIN(COALESCE(ts, received_at)) AS first_seen,
                MAX(COALESCE(ts, received_at)) AS last_seen,
                COUNT(DISTINCT CASE WHEN action = 'crawl.unit.completed' THEN unit END) AS units_completed,
                SUM(CASE WHEN action = 'crawl.finding' THEN 1 ELSE 0 END) AS findings
         FROM events
         WHERE mission_id IS NOT NULL
         ${corpusClause}
         GROUP BY mission_id
         ORDER BY last_seen DESC`
      )
      .all(...params);
  }

  getStats() {
    const byStatus = this.db
      .prepare('SELECT status, COUNT(*) AS n FROM findings GROUP BY status')
      .all()
      .reduce((acc, row) => {
        acc[row.status] = row.n;
        return acc;
      }, {});
    const byRule = this.db
      .prepare('SELECT rule, COUNT(*) AS n FROM findings GROUP BY rule')
      .all()
      .reduce((acc, row) => {
        acc[row.rule] = row.n;
        return acc;
      }, {});
    const total = this.db.prepare('SELECT COUNT(*) AS n FROM findings').get().n;
    return { total, by_status: byStatus, by_rule: byRule };
  }

  health() {
    const findings = this.db.prepare('SELECT COUNT(*) AS n FROM findings').get().n;
    return { ok: true, db: this.path, findings };
  }

  close() {
    this.db.close();
  }
}

export function openDatabase(dbPath) {
  return new TrackerDB(dbPath);
}
