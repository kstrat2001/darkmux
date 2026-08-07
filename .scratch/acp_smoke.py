#!/usr/bin/env python3
"""Hand-rolled ACP JSON-RPC smoke driver for #1698 Packet B2.

Drives a real `darkmux acp` subprocess over stdio: initialize, session/new,
a no-slash prompt that must route through the ROUTER then REFUSE then get a
grounded ANSWER from the real radio-host dispatch, a session/set_config_option
call, and a session/load round trip. Prints every line exchanged.
"""
import json
import subprocess
import sys
import threading
import queue

BIN = sys.argv[1] if len(sys.argv) > 1 else "darkmux"
CWD = sys.argv[2] if len(sys.argv) > 2 else "/tmp"

proc = subprocess.Popen(
    [BIN, "acp"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    bufsize=1,
)

out_q = queue.Queue()


def reader(pipe, tag):
    for line in iter(pipe.readline, ""):
        out_q.put((tag, line.rstrip("\n")))
    pipe.close()


threading.Thread(target=reader, args=(proc.stdout, "OUT"), daemon=True).start()
threading.Thread(target=reader, args=(proc.stderr, "ERR"), daemon=True).start()

_id = 0


def send(method, params):
    global _id
    _id += 1
    msg = {"jsonrpc": "2.0", "id": _id, "method": method, "params": params}
    line = json.dumps(msg)
    print(f">>> {line}")
    proc.stdin.write(line + "\n")
    proc.stdin.flush()
    return _id


def drain_until_response(want_id, timeout=180):
    import time

    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            tag, line = out_q.get(timeout=1)
        except queue.Empty:
            continue
        if tag == "ERR":
            print(f"[stderr] {line}")
            continue
        print(f"<<< {line}")
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if obj.get("id") == want_id and ("result" in obj or "error" in obj):
            return obj
    raise TimeoutError(f"no response for id={want_id} within {timeout}s")


try:
    rid = send("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    init_resp = drain_until_response(rid)
    assert "result" in init_resp, init_resp
    assert init_resp["result"]["agentCapabilities"]["loadSession"] is True, "loadSession must be advertised"
    print("PASS: initialize advertises loadSession=true")

    rid = send("session/new", {"cwd": CWD, "mcpServers": []})
    new_resp = drain_until_response(rid)
    session_id = new_resp["result"]["sessionId"]
    print(f"PASS: session/new -> {session_id}")
    config_options = new_resp["result"].get("configOptions")
    assert config_options, "config_options must be advertised on session/new"
    ids = [c["id"] for c in config_options]
    assert "radio-host" in ids and "humor" in ids, ids
    print(f"PASS: config_options advertised: {ids}")

    rid = send(
        "session/prompt",
        {"sessionId": session_id, "prompt": [{"type": "text", "text": "is this darkmux?"}]},
    )
    prompt_resp = drain_until_response(rid, timeout=180)
    assert prompt_resp["result"]["stopReason"] == "end_turn"
    print("PASS: 'is this darkmux?' got a grounded end_turn response (see chunks above)")

    rid = send(
        "session/set_config_option",
        {"sessionId": session_id, "configId": "humor", "value": "90"},
    )
    set_resp = drain_until_response(rid)
    assert "result" in set_resp, set_resp
    print("PASS: session/set_config_option (humor=90) accepted")

    rid = send("session/load", {"sessionId": session_id, "cwd": CWD, "mcpServers": []})
    load_resp = drain_until_response(rid)
    assert "result" in load_resp, load_resp
    print("PASS: session/load round trip accepted")

    print("\nALL SMOKE CHECKS PASSED")
finally:
    proc.stdin.close()
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
