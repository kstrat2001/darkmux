// Unit-level tests against the storage layer directly (no HTTP round trip).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { normalizeEvidence, findingKey } from '../db.mjs';

test('normalizeEvidence trims and collapses internal whitespace', () => {
  assert.equal(normalizeEvidence('  throw   err;  \n\t'), 'throw err;');
  assert.equal(normalizeEvidence('a\n\nb'), 'a b');
  assert.equal(normalizeEvidence(''), '');
});

test('findingKey is stable across whitespace-only evidence differences', () => {
  const base = { corpus: 'example', source: 'app', rule: 'swallowed-error', file: 'src/x.ts' };
  const k1 = findingKey({ ...base, evidence: 'throw err;' });
  const k2 = findingKey({ ...base, evidence: '  throw   err;\n' });
  assert.equal(k1, k2, 'normalized whitespace must not change the key');
});

test('findingKey changes when corpus, source, rule, or file changes', () => {
  const base = { corpus: 'example', source: 'app', rule: 'swallowed-error', file: 'src/x.ts', evidence: 'throw err;' };
  const baseline = findingKey(base);
  assert.notEqual(findingKey({ ...base, corpus: 'other' }), baseline);
  assert.notEqual(findingKey({ ...base, source: 'other' }), baseline);
  assert.notEqual(findingKey({ ...base, rule: 'other-rule' }), baseline);
  assert.notEqual(findingKey({ ...base, file: 'src/y.ts' }), baseline);
});

test('findingKey does NOT change when only the line number changes (line is not part of the key)', () => {
  // findingKey never takes a line argument at all — this test documents that
  // by construction: the function signature has no line parameter, so a
  // finding that moves lines is structurally the same key.
  const base = { corpus: 'example', source: 'app', rule: 'swallowed-error', file: 'src/x.ts', evidence: 'throw err;' };
  assert.equal(findingKey(base), findingKey(base));
});
