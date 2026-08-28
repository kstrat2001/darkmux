#!/usr/bin/env node
// The crawl-tracker reference receiver (darkmux #1959).
//
// node server.mjs [--db ./tracker.db] [--port 8790] [--bind 127.0.0.1]
//
// Zero npm dependencies. node:http + node:sqlite (via db.mjs) only.
// There is no authentication, so only loopback binds are accepted — see
// refuseNonLoopbackBind() below.

import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { TrackerDB, HttpError } from './db.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const UI_PATH = path.join(__dirname, 'ui.html');

const LOOPBACK_HOSTS = new Set(['127.0.0.1', '::1', 'localhost']);

export function refuseNonLoopbackBind(bind) {
  if (!LOOPBACK_HOSTS.has(bind)) {
    throw new Error(
      `refusing to bind to ${bind}: crawl-tracker has no authentication, so only loopback ` +
        `(127.0.0.1, ::1, localhost) is a safe posture. Put it behind your own reverse proxy ` +
        `with auth if you need remote access.`
    );
  }
}

function sendJson(res, status, body) {
  const text = JSON.stringify(body);
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(text);
}

function sendError(res, err) {
  if (err instanceof HttpError) {
    sendJson(res, err.status, { error: err.message });
    return;
  }
  // eslint-disable-next-line no-console
  console.error(err);
  sendJson(res, 500, { error: 'internal error' });
}

async function readBody(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  if (chunks.length === 0) return undefined;
  const text = Buffer.concat(chunks).toString('utf8');
  try {
    return JSON.parse(text);
  } catch {
    throw new HttpError(400, 'malformed JSON body');
  }
}

function queryParams(url) {
  const out = {};
  for (const [k, v] of url.searchParams.entries()) out[k] = v;
  return out;
}

async function handlePostEvents(req, res, db) {
  const body = await readBody(req);
  if (body === undefined) {
    throw new HttpError(400, 'request body is required');
  }
  const results = db.insertEvents(body);
  sendJson(res, 200, { ok: true, accepted: results.length, results });
}

function handleGetFindings(req, res, db, url) {
  const q = queryParams(url);
  const result = db.getFindings(q);
  sendJson(res, 200, result);
}

function handleGetFindingById(req, res, db, id) {
  const finding = db.getFinding(id);
  if (!finding) {
    throw new HttpError(404, `finding ${id} not found`);
  }
  sendJson(res, 200, finding);
}

async function handlePatchFinding(req, res, db, id) {
  const body = await readBody(req);
  if (body === undefined || typeof body !== 'object' || Array.isArray(body)) {
    throw new HttpError(400, 'request body must be a JSON object');
  }
  const existing = db.getFinding(id);
  if (!existing) {
    throw new HttpError(404, `finding ${id} not found`);
  }
  const updated = db.patchFinding(id, { status: body.status, note: body.note });
  sendJson(res, 200, updated);
}

function handleGetCoverage(req, res, db, url) {
  const q = queryParams(url);
  sendJson(res, 200, { items: db.getCoverage(q) });
}

function handleGetMissions(req, res, db, url) {
  const q = queryParams(url);
  sendJson(res, 200, { items: db.getMissions(q) });
}

function handleGetStats(req, res, db) {
  sendJson(res, 200, db.getStats());
}

function handleGetHealth(req, res, db) {
  sendJson(res, 200, db.health());
}

async function handleGetUi(req, res) {
  const html = await readFile(UI_PATH, 'utf8');
  res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
  res.end(html);
}

export function buildServer(db) {
  return http.createServer((req, res) => {
    Promise.resolve()
      .then(async () => {
        const url = new URL(req.url, 'http://internal.invalid');
        const parts = url.pathname.split('/').filter(Boolean);

        if (req.method === 'POST' && url.pathname === '/events') {
          return handlePostEvents(req, res, db);
        }
        if (req.method === 'GET' && url.pathname === '/findings') {
          return handleGetFindings(req, res, db, url);
        }
        if (req.method === 'GET' && parts[0] === 'findings' && parts.length === 2) {
          const id = Number.parseInt(parts[1], 10);
          if (!Number.isFinite(id)) throw new HttpError(400, 'finding id must be numeric');
          return handleGetFindingById(req, res, db, id);
        }
        if (req.method === 'PATCH' && parts[0] === 'findings' && parts.length === 2) {
          const id = Number.parseInt(parts[1], 10);
          if (!Number.isFinite(id)) throw new HttpError(400, 'finding id must be numeric');
          return handlePatchFinding(req, res, db, id);
        }
        if (req.method === 'GET' && url.pathname === '/coverage') {
          return handleGetCoverage(req, res, db, url);
        }
        if (req.method === 'GET' && url.pathname === '/missions') {
          return handleGetMissions(req, res, db, url);
        }
        if (req.method === 'GET' && url.pathname === '/stats') {
          return handleGetStats(req, res, db);
        }
        if (req.method === 'GET' && url.pathname === '/health') {
          return handleGetHealth(req, res, db);
        }
        if (req.method === 'GET' && url.pathname === '/') {
          return handleGetUi(req, res);
        }
        throw new HttpError(404, `no route for ${req.method} ${url.pathname}`);
      })
      .catch((err) => sendError(res, err));
  });
}

// --- CLI entry ---------------------------------------------------------

export function parseArgs(argv) {
  const args = { db: './tracker.db', port: 8790, bind: '127.0.0.1' };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--db') args.db = argv[++i];
    else if (arg === '--port') args.port = Number.parseInt(argv[++i], 10);
    else if (arg === '--bind') args.bind = argv[++i];
    else throw new Error(`unrecognized argument: ${arg}`);
  }
  return args;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));

  try {
    refuseNonLoopbackBind(args.bind);
  } catch (err) {
    console.error(err.message);
    process.exit(1);
  }

  const db = new TrackerDB(args.db);
  const server = buildServer(db);

  const shutdown = (signal) => {
    console.error(`crawl-tracker: received ${signal}, closing`);
    server.close(() => {
      db.close();
      process.exit(0);
    });
  };
  process.on('SIGINT', () => shutdown('SIGINT'));
  process.on('SIGTERM', () => shutdown('SIGTERM'));

  server.listen(args.port, args.bind, () => {
    const addr = server.address();
    console.error(`crawl-tracker listening on http://${args.bind}:${addr.port} (db: ${args.db})`);
  });
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1]);
if (isMain) {
  main();
}
