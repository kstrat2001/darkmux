// darkmux (#3561): in a sandbox on this machine, a server that names no
// host listens on the loopback.
//
// darkmux writes this file into its managed Node install and loads it with
// `NODE_OPTIONS=--require <this file>` in every sandbox on the host that
// runs that Node (dispatch, contained commands, client previews). Such a
// sandbox shares this machine's network, and Node's own default for a
// listen with no host is every address (`::`), which darkmux stops (#3559).
//
// Rewritten to 127.0.0.1: `listen()`, `listen(cb)`, `listen(port)`,
// `listen(port, cb)`, `listen(port, backlog, cb)`, `listen(port, undefined
// or null, ...)`, and `listen({ port, ... })` whose options name no
// `host`, `path`, `fd` or `handle`. So `http.createServer().listen(0)`,
// Express's `app.listen(port)` and supertest's `app.listen(0)` listen on
// 127.0.0.1, which `fetch('http://localhost:<port>')` also reaches.
//
// So `listen({ port, ipv6Only: true })`, which names no host, listens on
// 127.0.0.1 too (IPv4 only, like every rewritten listen).
//
// Left alone: any explicit host (`0.0.0.0` and `::` included, which
// darkmux's watch still stops), a unix socket or named pipe, an fd, a handle.
// An empty host (`listen(0, '')`, `{ host: '' }`) is a host string, so it is
// left alone too, and Node binds it to every address (`::`): the watch stops
// that one as well.
'use strict';

const net = require('net');

const LOOPBACK = '127.0.0.1';
const PATCHED = Symbol.for('darkmux.listen-loopback');

// Node's own test for a pipe name: a string that is not a port number.
function isPipeName(s) {
  return typeof s === 'string' && !(Number(s) >= 0);
}

function present(v) {
  return v !== undefined && v !== null;
}

// `args` with the loopback as the host, when they name none.
function withLoopback(args) {
  const first = args[0];
  if (first !== null && typeof first === 'object') {
    const named = present(first.host) || present(first.path) || present(first.fd) || present(first.handle) || first._handle !== undefined;
    return named ? args : [Object.assign({}, first, { host: LOOPBACK })].concat(args.slice(1));
  }
  if (isPipeName(first)) return args;
  if (typeof first === 'function') return [0, LOOPBACK].concat(args);
  if (typeof args[1] === 'string') return args;
  const port = present(first) ? first : 0;
  const rest = args.length > 1 && !present(args[1]) ? args.slice(2) : args.slice(1);
  return [port, LOOPBACK].concat(rest);
}

if (!net.Server.prototype[PATCHED]) {
  const listen = net.Server.prototype.listen;
  net.Server.prototype.listen = function darkmuxListen(...args) {
    return listen.apply(this, withLoopback(args));
  };
  net.Server.prototype[PATCHED] = true;
}
