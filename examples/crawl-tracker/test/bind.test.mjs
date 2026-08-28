// Spawns the real server process to verify (a) the non-loopback bind
// refusal and (b) graceful SIGTERM handling. (a) needs spawnSync because the
// refusal calls process.exit(1) before the HTTP listener ever opens, so a
// synchronous wait for the whole run is fine. (b) needs an async spawn: a
// spawnSync `timeout` races its own kill against our async shutdown
// (server.close callback + db.close) on macOS and reports a stale ETIMEDOUT
// even when the child exited cleanly, so we drive the signal ourselves once
// we've seen the "listening" line instead of trusting spawnSync's timer.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SERVER_PATH = path.join(__dirname, '..', 'server.mjs');

test('--bind 0.0.0.0 exits non-zero with a clear message, without opening a listener', () => {
  const result = spawnSync(process.execPath, [SERVER_PATH, '--db', ':memory:', '--bind', '0.0.0.0', '--port', '0'], {
    encoding: 'utf8',
    timeout: 5000,
  });
  assert.notEqual(result.status, 0, 'must exit non-zero');
  assert.ok(
    /refus|loopback/i.test(result.stderr),
    `stderr should explain the refusal, got: ${result.stderr}`
  );
});

test('--bind 127.0.0.1 is accepted, and SIGTERM closes the db cleanly', async () => {
  const child = spawn(process.execPath, [SERVER_PATH, '--db', ':memory:', '--bind', '127.0.0.1', '--port', '0'], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });

  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr += chunk.toString();
  });

  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`server never logged "listening"; stderr so far: ${stderr}`)), 5000);
    const check = () => {
      if (/listening/i.test(stderr)) {
        clearTimeout(timer);
        resolve();
      }
    };
    child.stderr.on('data', check);
    check();
  });

  assert.ok(/listening/i.test(stderr));

  const exitCode = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`server never exited after SIGTERM; stderr so far: ${stderr}`)), 5000);
    child.on('exit', (code) => {
      clearTimeout(timer);
      resolve(code);
    });
    child.kill('SIGTERM');
  });

  assert.equal(exitCode, 0, 'graceful SIGTERM handling should exit 0');
  assert.ok(/received SIGTERM, closing/i.test(stderr), `expected graceful-shutdown log, got: ${stderr}`);
});
