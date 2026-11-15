#!/usr/bin/env python3
"""(#3560) A fake `tailscale` for darkmux's tests. Nothing here reaches the
real tool or any tailnet.

It keeps its state beside itself: `status.json` (its `status --json`),
`serve.json` (its serve configuration), and `args.log` (every argument list,
one per line). Its `serve` changes follow Tailscale 1.102.4's own code
(`cmd/tailscale/cli/serve_v2.go`, `ipn/serve.go`):

* the host is always this machine's CURRENT name, `Self.DNSName`, never an
  argument, so an entry under an earlier name cannot be removed;
* `serve --bg --https=<port> [--set-path=<m>] <target>` adds a handler at
  mount `<m>` (default `/`) on `<host>:<port>` and `TCP.<port>.HTTPS`;
* `serve --https=<port> off` with NO `--set-path` removes EVERY handler on
  `<host>:<port>` (the CLI asks first, and with no terminal its prompt
  answers yes); with `--set-path=<m>` it removes only `<m>`; either way,
  once no handler is left, the web key, the port's TCP record and its Funnel
  go too (`RemoveWebHandler`); no handler to remove is an error.

`whois --json <addr>` answers from `whois.json` (address -> the answer
Tailscale 1.102.4's `apitype.WhoIsResponse` would be), and an address not in
it the way the real tool refuses one no device holds; `whois_fails` makes
every `whois` fail as it does with the daemon down. Read-only, like the
real command.

Switches, each a file beside it: `refuse` (every change fails), `record_path`
(holds a path that must exist before a publish), `publish_as.json` (the
handler a publish stores instead of `{"Proxy": <target>}`),
`break_on_publish` (a publish leaves `serve.json` unreadable and fails),
`status_fails` (`serve status` fails), and `race_mount.json` (handlers
merged into `<host>:<port>` just before a removal: an operator's mount
added between darkmux's read and its remove).
"""
import json
import os
import sys

D = os.path.dirname(os.path.abspath(__file__))


def path(name):
    return os.path.join(D, name)


def has(name):
    return os.path.exists(path(name))


def load(name):
    with open(path(name)) as f:
        return json.load(f)


def fail(msg, code=1):
    sys.stderr.write(msg + "\n")
    sys.exit(code)


args = sys.argv[1:]
with open(path("args.log"), "a") as f:
    f.write(" ".join(args) + "\n")

if args == ["version"]:
    print("1.102.4")
    print("  tailscale commit: fake")
    sys.exit(0)
if args == ["status", "--json"]:
    sys.stdout.write(open(path("status.json")).read())
    sys.exit(0)
if len(args) == 3 and args[:2] == ["whois", "--json"]:
    if has("whois_fails"):
        fail("failed to connect to local tailscaled; it doesn't appear to be running")
    answers = load("whois.json") if has("whois.json") else {}
    if args[2] not in answers:
        fail("peer not found")
    print(json.dumps(answers[args[2]]))
    sys.exit(0)
if args == ["serve", "status", "--json"]:
    if has("status_fails"):
        fail("serve status: permission denied")
    sys.stdout.write(open(path("serve.json")).read())
    sys.exit(0)
if not args or args[0] != "serve":
    fail("fake tailscale: unexpected " + " ".join(args), 2)
if has("refuse"):
    fail("Access denied: serve config denied")

flags, rest = {}, []
for a in args[1:]:
    if a.startswith("--"):
        k, _, v = a[2:].partition("=")
        flags[k] = v if v else True
    else:
        rest.append(a)
port = flags.get("https")
if not isinstance(port, str):
    fail("fake tailscale: no --https=<port>", 2)
host = load("status.json")["Self"]["DNSName"].rstrip(".")
hp = "%s:%s" % (host, port)
sc = load("serve.json") or {}
mount = flags.get("set-path") or "/"

if rest == ["off"]:
    if has("race_mount.json"):
        sc.setdefault("Web", {}).setdefault(hp, {"Handlers": {}})["Handlers"].update(load("race_mount.json"))
    handlers = sc.get("Web", {}).get(hp, {}).get("Handlers", {})
    if "set-path" in flags:
        mounts = [mount] if mount in handlers else []
    else:
        mounts = list(handlers)
    if not mounts:
        fail("error: failed to remove web serve: handler does not exist")
    for m in mounts:
        del handlers[m]
    if not handlers:
        sc.get("Web", {}).pop(hp, None)
        sc.get("TCP", {}).pop(port, None)
        sc.get("AllowFunnel", {}).pop(hp, None)
    with open(path("serve.json"), "w") as f:
        json.dump(sc, f)
    sys.exit(0)

if flags.get("bg") is True and len(rest) == 1:
    if has("record_path") and not os.path.exists(open(path("record_path")).read().strip()):
        fail("fake tailscale: published before darkmux recorded it", 3)
    if has("break_on_publish"):
        with open(path("serve.json"), "w") as f:
            f.write("not json")
        fail("fake tailscale: the publish broke the serve config")
    handler = load("publish_as.json") if has("publish_as.json") else {"Proxy": rest[0]}
    sc.setdefault("TCP", {})[port] = {"HTTPS": True}
    sc.setdefault("Web", {}).setdefault(hp, {"Handlers": {}})["Handlers"][mount] = handler
    with open(path("serve.json"), "w") as f:
        json.dump(sc, f)
    sys.exit(0)

fail("fake tailscale: unexpected " + " ".join(args), 2)
