// HTTP-level tests against the real server (in-memory db, ephemeral port).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { TrackerDB } from '../db.mjs';
import { buildServer } from '../server.mjs';

async function withServer(fn) {
  const db = new TrackerDB(':memory:');
  const server = buildServer(db);
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  const base = `http://127.0.0.1:${port}`;
  try {
    await fn({ base, db });
  } finally {
    await new Promise((resolve) => server.close(resolve));
    db.close();
  }
}

async function postJson(base, path, body) {
  const res = await fetch(`${base}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: typeof body === 'string' ? body : JSON.stringify(body),
  });
  const json = await res.json().catch(() => null);
  return { status: res.status, json };
}

async function patchJson(base, path, body) {
  const res = await fetch(`${base}${path}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  const json = await res.json().catch(() => null);
  return { status: res.status, json };
}

async function getJson(base, path) {
  const res = await fetch(`${base}${path}`);
  const json = await res.json().catch(() => null);
  return { status: res.status, json };
}

function findingRecord(overrides = {}) {
  const payload = {
    corpus: 'example',
    source: 'app',
    sha: 'sha-aaa',
    rule: 'swallowed-error',
    unit: 'u-0001',
    file: 'src/x.ts',
    line: 42,
    evidence: 'throw err;',
    why: 'error is caught and discarded',
    context: 'lines 37..47',
    context_start: 37,
    context_end: 47,
    evidence_mismatch: false,
    ...overrides.payload,
  };
  return {
    schema_version: '1.22.0',
    action: overrides.action ?? 'crawl.finding',
    ts: overrides.ts ?? new Date().toISOString(),
    session_id: overrides.session_id ?? 'crew-dispatch-crawler-1',
    mission_id: overrides.mission_id ?? 'mission-1',
    model: overrides.model ?? 'qwen3.6-35b-a3b',
    payload,
  };
}

// --- 1. dedup: same key -> seen, times_seen increments, line updates ----

test('POST finding then the same finding again: new -> seen, times_seen increments, line updates', async () => {
  await withServer(async ({ base }) => {
    const first = await postJson(base, '/events', findingRecord());
    assert.equal(first.status, 200);
    assert.equal(first.json.accepted, 1);
    assert.equal(first.json.results[0].status, 'new');
    assert.equal(first.json.results[0].times_seen, 1);
    const findingId = first.json.results[0].finding_id;

    const second = await postJson(
      base,
      '/events',
      findingRecord({
        mission_id: 'mission-2',
        payload: { line: 99 },
      })
    );
    assert.equal(second.status, 200);
    assert.equal(second.json.results[0].status, 'seen');
    assert.equal(second.json.results[0].times_seen, 2);
    assert.equal(second.json.results[0].finding_id, findingId);

    const { json: finding } = await getJson(base, `/findings/${findingId}`);
    assert.equal(finding.line, 99, 'line must be updated to the latest sighting');
    assert.equal(finding.sightings.length, 2, 'two sightings recorded');
  });
});

// --- 2. PATCH rejected sticks; re-seeing keeps it rejected --------------

test('a rejected finding stays rejected when seen again, and times_seen keeps incrementing', async () => {
  await withServer(async ({ base }) => {
    const first = await postJson(base, '/events', findingRecord());
    const findingId = first.json.results[0].finding_id;

    const second = await postJson(base, '/events', findingRecord({ mission_id: 'mission-2' }));
    assert.equal(second.json.results[0].times_seen, 2);

    const patch = await patchJson(base, `/findings/${findingId}`, { status: 'rejected' });
    assert.equal(patch.status, 200);
    assert.equal(patch.json.status, 'rejected');

    const third = await postJson(base, '/events', findingRecord({ mission_id: 'mission-3' }));
    assert.equal(third.json.results[0].status, 'seen');
    assert.equal(third.json.results[0].times_seen, 3);

    const { json: finding } = await getJson(base, `/findings/${findingId}`);
    assert.equal(finding.status, 'rejected', 'status must stay rejected once seen again — dedup is against everything SEEN');
  });
});

// --- 3. FTS search ------------------------------------------------------

test('FTS search matches on why text and on file', async () => {
  await withServer(async ({ base }) => {
    await postJson(
      base,
      '/events',
      findingRecord({ payload: { file: 'src/network/retry.ts', why: 'retries silently swallow the underlying timeout' } })
    );

    const byWhy = await getJson(base, '/findings?q=timeout');
    assert.equal(byWhy.status, 200);
    assert.equal(byWhy.json.total, 1);
    assert.equal(byWhy.json.items[0].file, 'src/network/retry.ts');

    const byFile = await getJson(base, '/findings?q=retry');
    assert.equal(byFile.status, 200);
    assert.equal(byFile.json.total, 1);
  });
});

test('a malformed FTS query returns 400, not 500', async () => {
  await withServer(async ({ base }) => {
    await postJson(base, '/events', findingRecord());
    const res = await getJson(base, `/findings?q=${encodeURIComponent('"unbalanced')}`);
    assert.equal(res.status, 400);
    assert.ok(res.json.error);
  });
});

// --- 4. mixed array of events, unknown action stored + acknowledged -----

test('an array of mixed records is accepted in order with per-record results; unknown actions are stored and acknowledged', async () => {
  await withServer(async ({ base }) => {
    const events = [
      findingRecord({ mission_id: 'mission-4' }),
      {
        schema_version: '1.22.0',
        action: 'crawl.exclusion',
        ts: new Date().toISOString(),
        session_id: 'crew-dispatch-crawler-1',
        mission_id: 'mission-4',
        payload: {
          corpus: 'example',
          source: 'app',
          sha: 'sha-aaa',
          rule: 'swallowed-error',
          unit: 'u-0002',
          file: 'src/y.ts',
          line: 10,
          evidence: 'log.warn(err)',
          reason: 'logged, not swallowed',
        },
      },
      {
        schema_version: '1.22.0',
        action: 'dispatch.start',
        ts: new Date().toISOString(),
        session_id: 'crew-dispatch-crawler-1',
        mission_id: 'mission-4',
        payload: { note: 'not a crawl action, must be stored raw and acked' },
      },
    ];

    const res = await postJson(base, '/events', events);
    assert.equal(res.status, 200);
    assert.equal(res.json.accepted, 3);
    assert.equal(res.json.results[0].action, 'crawl.finding');
    assert.equal(res.json.results[1].action, 'crawl.exclusion');
    assert.ok(Number.isInteger(res.json.results[1].exclusion_id));
    assert.deepEqual(res.json.results[2], { action: 'dispatch.start', stored: true });
  });
});

// --- 5. coverage + missions derivation -----------------------------------

test('GET /coverage and GET /missions derive counts from unit + finding events across two mission_ids', async () => {
  await withServer(async ({ base }) => {
    const unitStarted = (mission, unit) => ({
      schema_version: '1.22.0',
      action: 'crawl.unit.started',
      ts: new Date().toISOString(),
      mission_id: mission,
      payload: { corpus: 'example', unit, source: 'app', sha: 'sha-aaa', rule: 'swallowed-error', kind: 'read', est_tokens: 9000, files: 4 },
    });
    const unitCompleted = (mission, unit, findings) => ({
      schema_version: '1.22.0',
      action: 'crawl.unit.completed',
      ts: new Date().toISOString(),
      mission_id: mission,
      payload: {
        corpus: 'example',
        unit,
        source: 'app',
        sha: 'sha-aaa',
        rule: 'swallowed-error',
        result: 'stop',
        findings,
        exclusions: 1,
        prompt_tokens: 90000,
        completion_tokens: 8000,
        wall_ms: 86000,
      },
    });

    await postJson(base, '/events', unitStarted('mission-a', 'u-1'));
    await postJson(base, '/events', findingRecord({ mission_id: 'mission-a', payload: { unit: 'u-1', file: 'src/a.ts' } }));
    await postJson(base, '/events', unitCompleted('mission-a', 'u-1', 1));

    await postJson(base, '/events', unitStarted('mission-b', 'u-2'));
    await postJson(base, '/events', findingRecord({ mission_id: 'mission-b', payload: { unit: 'u-2', file: 'src/b.ts' } }));
    await postJson(base, '/events', findingRecord({ mission_id: 'mission-b', payload: { unit: 'u-2', file: 'src/c.ts' } }));
    await postJson(base, '/events', unitCompleted('mission-b', 'u-2', 2));

    const coverage = await getJson(base, '/coverage?corpus=example');
    assert.equal(coverage.status, 200);
    assert.equal(coverage.json.items.length, 1, 'both units share the same (source, sha, rule) key');
    const row = coverage.json.items[0];
    assert.equal(row.source, 'app');
    assert.equal(row.sha, 'sha-aaa');
    assert.equal(row.rule, 'swallowed-error');
    assert.equal(row.units_started, 2);
    assert.equal(row.units_completed, 2);
    assert.equal(row.findings, 3);
    assert.equal(row.exclusions, 0);
    assert.ok(row.last_activity);

    const missions = await getJson(base, '/missions?corpus=example');
    assert.equal(missions.status, 200);
    assert.equal(missions.json.items.length, 2);
    const byId = Object.fromEntries(missions.json.items.map((m) => [m.mission_id, m]));
    assert.equal(byId['mission-a'].units_completed, 1);
    assert.equal(byId['mission-a'].findings, 1);
    assert.equal(byId['mission-b'].units_completed, 1);
    assert.equal(byId['mission-b'].findings, 2);
  });
});

// --- 6. malformed JSON -> 400 -------------------------------------------

test('malformed JSON body returns 400', async () => {
  await withServer(async ({ base }) => {
    const res = await fetch(`${base}/events`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: '{ this is not json',
    });
    assert.equal(res.status, 400);
    const json = await res.json();
    assert.ok(json.error);
  });
});

test('a record with no action is rejected with 400', async () => {
  await withServer(async ({ base }) => {
    const res = await postJson(base, '/events', { schema_version: '1.22.0', payload: {} });
    assert.equal(res.status, 400);
  });
});

// --- 7. GET / returns the UI page ----------------------------------------

test('GET / returns text/html', async () => {
  await withServer(async ({ base }) => {
    const res = await fetch(`${base}/`);
    assert.equal(res.status, 200);
    assert.ok(res.headers.get('content-type').includes('text/html'));
    const body = await res.text();
    assert.ok(body.includes('<'), 'body looks like HTML');
  });
});

test('GET /health reports ok and a findings count', async () => {
  await withServer(async ({ base }) => {
    await postJson(base, '/events', findingRecord());
    const res = await getJson(base, '/health');
    assert.equal(res.status, 200);
    assert.equal(res.json.ok, true);
    assert.equal(res.json.findings, 1);
  });
});

test('PATCH with an invalid status is rejected with 400', async () => {
  await withServer(async ({ base }) => {
    const first = await postJson(base, '/events', findingRecord());
    const findingId = first.json.results[0].finding_id;
    const res = await patchJson(base, `/findings/${findingId}`, { status: 'not-a-real-status' });
    assert.equal(res.status, 400);
  });
});
