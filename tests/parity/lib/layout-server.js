// The layout suites' web server: the staged bundle as a static file, plus a
// `/flow/<date>/stream` that answers with SSE headers and then stays open, so
// a live page's EventSource reaches `open` and the page reads as connected.
// (Python's `http.server`, which the sibling suites use, cannot hold a
// response open; a live run page that never connects reads "no signal".)
//
// Every other request is answered by the suite's own `page.route` handlers
// (`lib/layout-fixture.js`) before it reaches this server.
const http = require("http");
const fs = require("fs");
const path = require("path");

const [, , dir, port] = process.argv;
const TYPES = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml" };

http
  .createServer((req, res) => {
    const p = new URL(req.url, "http://x").pathname;
    if (/^\/flow\/\d{4}-\d{2}-\d{2}\/stream$/.test(p)) {
      res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "keep-alive" });
      res.write(": layout harness\n\n");
      const keep = setInterval(() => res.write(": keepalive\n\n"), 15_000);
      req.on("close", () => clearInterval(keep));
      return;
    }
    const file = path.join(dir, p === "/" ? "index.html" : p);
    if (!file.startsWith(dir) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
      res.writeHead(404);
      return res.end();
    }
    res.writeHead(200, { "content-type": TYPES[path.extname(file)] ?? "application/octet-stream" });
    fs.createReadStream(file).pipe(res);
  })
  .listen(Number(port), "127.0.0.1");
