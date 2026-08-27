#!/usr/bin/env python3
"""Serve the demo world: a real darkmux daemon, with two route families
overridden from fixtures.

    ./serve.py                 # -> http://localhost:8799

Everything the daemon can derive from records it DOES derive — this proxy
forwards `/`, `/flow/*`, `/runs`, `/missions`, `/fleet/*`, the mission graph
and most `/panel/*` untouched, so the screenshots show the real UI reading a
real backend. Only what a probe would answer from THIS machine is replaced:

    /machine/specs|resources|status   the 256 GB M5 Ultra
    /panel/doctor, /panel/machine-status

`generated_at_ms` is restamped on the way out. The machine lens renders a
gather's age, and a fixture frozen at build time would render as increasingly
stale — the one field whose build-time value is wrong by construction.
"""
import argparse, http.server, json, os, pathlib, re, signal, socket, subprocess, sys, time, urllib.error, urllib.request

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent.parent

MACHINE_ROUTES = {"/machine/specs": "specs", "/machine/resources": "resources",
                  "/machine/status": "status"}

# Presence rides the fleet substrate (Redis), which the demo deliberately does
# not run — so the daemon answers these `state: "off"` and the fleet lens shows
# no live machines. Same override rationale as /machine/*: fixture only what a
# probe (or a substrate) would have to answer.
FLEET_ROUTES = {"/fleet/machines/live": "fleet-machines-live.json",
                "/fleet/sessions/live": "fleet-sessions-live.json"}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def make_handler(inner, fx, hero, demo_uids, home):
    class H(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *a):        # quiet; the daemon logs its own
            pass

        def _send(self, code, body, ctype="application/json"):
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(body)

        def _fixture(self, path):
            """A fixture for this route, restamped, or None to proxy."""
            base = self.path.split("?")[0]
            if base in MACHINE_ROUTES:
                f = fx / "machine" / f"{hero}.{MACHINE_ROUTES[base]}.json"
                if f.exists():
                    d = json.loads(f.read_text())
                    d["generated_at_ms"] = int(time.time() * 1000)
                    return json.dumps(d).encode()
            if base in FLEET_ROUTES:
                f = fx / FLEET_ROUTES[base]
                if f.exists():
                    d = json.loads(f.read_text())
                    now = int(time.time() * 1000)
                    # A heartbeat is only meaningful relative to now; a fixture
                    # frozen at build time reads as a fleet that went silent.
                    for m in d.get("machines", []):
                        m["beat_ts_ms"] = now
                    return json.dumps(d).encode()
            if base.startswith("/panel/"):
                pid = base[len("/panel/"):]
                # A width-captured panel (doctor) picks the nearest capture at
                # or below what the client asked for, mirroring what a real
                # daemon does by re-rendering. Without this a phone asking for
                # 42 columns got a 99-column render and the demo showed the
                # product overflowing when it does not.
                widths_f = fx / "panel" / "doctor-widths.json"
                if pid == "doctor" and widths_f.exists():
                    q = self.path.split("?", 1)[1] if "?" in self.path else ""
                    want = 100
                    for part in q.split("&"):
                        if part.startswith("cols="):
                            try:
                                want = int(part[5:])
                            except ValueError:
                                pass
                    widths = sorted(json.loads(widths_f.read_text()))
                    pick = max([w for w in widths if w <= want], default=widths[0])
                    f = fx / "panel" / f"doctor.{pick}.json"
                    if f.exists():
                        d = json.loads(f.read_text())
                        d["captured_ts_ms"] = int(time.time() * 1000)
                        d["age_ms"] = 0
                        return json.dumps(d).encode()
                f = fx / "panel" / f"{pid}.json"
                if f.exists():
                    d = json.loads(f.read_text())
                    d["captured_ts_ms"] = int(time.time() * 1000)
                    d["age_ms"] = 0
                    return json.dumps(d).encode()
            return None

        def _scrub_passthrough(self, data):
            """Rewrite host paths in a PASSTHROUGH response.

            The canned panels go through the importer's scrub at build time,
            but a passthrough panel is rendered live by the daemon and never
            saw it — `lab fixture list` printed the demo home's real absolute
            path (`/Users/<you>/de-projects/.../screenshots/demo-home`) into a
            frame headed for a public docs page. Scrubbing HERE covers every
            passthrough panel, including ones added later, rather than
            requiring each to be remembered.
            """
            try:
                text = data.decode()
            except UnicodeDecodeError:
                return data
            if str(home) not in text and "/Users/" not in text:
                return data
            text = text.replace(str(home), "/home/demo/.darkmux")
            # Backstop for any other host path the daemon may print.
            text = re.sub(r"/Users/[^/\"\s\\]+", "/home/demo", text)
            return text.encode()

        def _filter_flow(self, data):
            """Drop presence records for machines that are not in the demo world.

            The daemon self-emits one `machine.online` at startup
            (`presence_reconciler::emit_machine_online_edge`, called
            unconditionally — there is no knob) carrying THIS host's real
            hardware uid. In the demo flows dir that becomes a fourth machine
            card, duplicating the hero under a second uid. Filtering the READ
            is deliberate over pruning the file: no race with a daemon that is
            still writing, and `/runs` is unaffected either way because a
            presence record carries no session_id and so mints no run.
            """
            try:
                recs = json.loads(data)
            except Exception:                                 # noqa: BLE001
                return data
            if not isinstance(recs, list):
                return data
            kept = [r for r in recs
                    if r.get("source") != "presence-reconciler"
                    or r.get("machine_uid") in demo_uids]
            return json.dumps(kept).encode()

        def do_GET(self):
            # The viewer comes from the WORKING TREE, not the installed
            # binary. The inner daemon serves its own `include_str!`ed copy,
            # so proxying `/` meant a one-line CSS change could only be seen
            # after a full `cargo install` — and, worse, that a shot taken
            # here documented whatever binary happened to be on PATH rather
            # than the code under review. Reading the built asset makes
            # `bun run build` + reload the whole loop.
            if self.path.split("?")[0] in ("/", "/index.html"):
                asset = ROOT / "crates" / "darkmux-serve" / "assets" / "next.html"
                if asset.exists():
                    return self._send(200, asset.read_bytes(), "text/html; charset=utf-8")
            body = self._fixture(self.path)
            if body is not None:
                return self._send(200, body)
            req = urllib.request.Request(inner + self.path)
            # The panel CSRF header must survive the hop or the daemon 403s
            # every passthrough panel (`panel requests require the
            # x-darkmux-panel header`). Forward the request headers the
            # daemon actually keys on rather than inventing a fresh set.
            for k in ("x-darkmux-panel", "accept", "authorization"):
                if self.headers.get(k):
                    req.add_header(k, self.headers[k])
            try:
                with urllib.request.urlopen(req, timeout=120) as r:
                    data, ctype = r.read(), r.headers.get("Content-Type", "application/json")
                    base = self.path.split("?")[0]
                    if base.startswith("/flow/"):
                        data = self._filter_flow(data)
                    elif base.startswith("/panel/"):
                        data = self._scrub_passthrough(data)
                    return self._send(r.status, data, ctype)
            except urllib.error.HTTPError as e:
                return self._send(e.code, e.read(), e.headers.get("Content-Type", "text/plain"))
            except Exception as e:                        # noqa: BLE001
                return self._send(502, str(e).encode(), "text/plain")

        def do_OPTIONS(self):
            self.send_response(204)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Access-Control-Allow-Headers", "x-darkmux-panel, authorization")
            self.send_header("Access-Control-Allow-Methods", "GET, OPTIONS")
            self.send_header("Content-Length", "0")
            self.end_headers()
    return H


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8799)
    ap.add_argument("--dir", default=str(ROOT / "screenshots"))
    a = ap.parse_args()

    out = pathlib.Path(a.dir)
    home, fx = out / "demo-home", out / "fixtures"
    if not (home / "flows").exists():
        sys.exit(f"no demo world at {out} — run ./build.py first")
    hero = (fx / "machine" / "HERO").read_text().strip()
    demo_uids = set(json.loads((fx / "demo-uids.json").read_text()))

    # Claim the OUTER port before spawning anything. The daemon used to be
    # started first, so a port collision failed at `server_bind` with the
    # child already running and no handler left to reap it — one orphaned
    # daemon per failed start, silently holding an isolated home open.
    try:
        claim = socket.socket()
        claim.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        claim.bind(("127.0.0.1", a.port))
    except OSError as e:
        sys.exit(f"port {a.port} is not available ({e}). "
                 f"Stop the other listener, or pass --port.")
    claim.close()

    inner_port = free_port()
    env = dict(os.environ)
    env.update({
        "DARKMUX_HOME": str(home),
        "DARKMUX_FLOWS_DIR": str(home / "flows"),
        "DARKMUX_CREW_DIR": str(home / "crew"),
        "DARKMUX_MACHINE_ID": hero,
        # Isolation, stated twice on purpose: the demo daemon must not reach
        # the operator's real coordination substrate, and must not write into
        # a real audit chain, even if the ambient shell exports both.
        "DARKMUX_REDIS_URL": "",
        "DARKMUX_AUDIT_DIR": "",
    })
    import atexit
    daemon = subprocess.Popen(
        ["darkmux", "serve", "--port", str(inner_port)],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    atexit.register(lambda: daemon.poll() is None and daemon.terminate())

    inner = f"http://127.0.0.1:{inner_port}"
    for _ in range(60):
        try:
            urllib.request.urlopen(inner + "/health", timeout=1).read()
            break
        except Exception:                                  # noqa: BLE001
            time.sleep(0.25)
    else:
        daemon.terminate()
        sys.exit("demo daemon did not come up")

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", a.port),
                                          make_handler(inner, fx, hero, demo_uids, home))
    print(f"demo world serving on http://localhost:{a.port}")
    print(f"  hero machine   {hero}")
    print(f"  daemon         {inner} (isolated home: {home})")
    print(f"  overridden     /machine/*, "
          f"{', '.join('/panel/' + p.stem for p in sorted(fx.glob('panel/*.json')))}")
    print("\n  lenses:")
    for name, frag in [("fleet", ""), ("machine", "#lens=machine"),
                       ("runs", "#lens=runs"), ("console", "#lens=console&panel=doctor")]:
        print(f"    {name:9s} http://localhost:{a.port}/{frag}")
    print("\nctrl-c to stop")

    def bye(*_):
        srv.shutdown(); daemon.terminate(); sys.exit(0)
    signal.signal(signal.SIGINT, bye)
    signal.signal(signal.SIGTERM, bye)
    try:
        srv.serve_forever()
    finally:
        daemon.terminate()


if __name__ == "__main__":
    main()
