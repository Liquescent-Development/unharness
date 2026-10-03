#!/usr/bin/env python3
"""Record a `codex app-server` JSON-RPC session as a parser fixture.

Performs initialize → initialized → thread/start → turn/start for each
prompt, auto-accepts approval requests, asks for model/list at the end, and
writes both directions to OUT.jsonl (`>>` = sent, `!!` = stderr).

Usage: scripts/record-codex-app-server.py OUT.jsonl [--approval untrusted] [--sandbox read-only] PROMPT [PROMPT...]
"""
import argparse
import json
import os
import re
import subprocess
import sys
import threading
import time


def redact(line: str, cwd: str) -> str:
    home = os.path.expanduser("~")
    line = line.replace(cwd, "/WORKSPACE")
    if home:
        line = line.replace(home, "/HOME")
    line = re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "user@example.com", line)
    return line


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("prompts", nargs="+")
    ap.add_argument("--approval", default="untrusted")
    ap.add_argument("--sandbox", default="read-only")
    ap.add_argument("--model")
    ap.add_argument("--idle-timeout", type=float, default=180.0)
    args = ap.parse_args()

    cwd = os.getcwd()
    proc = subprocess.Popen(["codex", "app-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, bufsize=1)
    out = open(args.out, "w")
    lock = threading.Lock()

    def record(prefix, line):
        with lock:
            out.write(prefix + redact(line.rstrip("\n"), cwd) + "\n")
            out.flush()

    seq = 0

    def request(method, params=None):
        nonlocal seq
        seq += 1
        msg = {"jsonrpc": "2.0", "id": seq, "method": method}
        if params is not None:
            msg["params"] = params
        send(msg)
        return seq

    def notify(method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        send(msg)

    def respond(rid, result):
        send({"jsonrpc": "2.0", "id": rid, "result": result})

    def send(obj):
        line = json.dumps(obj)
        record(">> ", line)
        proc.stdin.write(line + "\n")
        proc.stdin.flush()

    def pump_stderr():
        for line in proc.stderr:
            record("!! ", line)
            sys.stderr.write("[stderr] " + line)

    threading.Thread(target=pump_stderr, daemon=True).start()

    init_id = request("initialize", {"clientInfo": {"name": "unharness", "version": "0.2.0"}})
    thread_id = None
    pending_start = None
    prompts = list(args.prompts)
    phase = "init"
    models_id = None

    last = time.time()
    while proc.poll() is None:
        line = proc.stdout.readline()
        if not line:
            if time.time() - last > args.idle_timeout:
                sys.stderr.write("idle timeout\n")
                break
            time.sleep(0.05)
            continue
        last = time.time()
        record("", line)
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue

        method = obj.get("method")
        rid = obj.get("id")

        # Server → client requests (have both id and method).
        if method and rid is not None:
            if method.endswith("requestApproval"):
                respond(rid, {"decision": "accept"})
            elif method == "item/tool/requestUserInput":
                qs = obj.get("params", {}).get("questions", [])
                answers = {q.get("id"): {"answers": [(q.get("options") or [{}])[0].get("label", "ok")]} for q in qs}
                respond(rid, {"answers": answers})
            else:
                sys.stderr.write(f"[unhandled server request] {method}\n")
                send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unhandled"}})
            continue

        # Responses to our requests.
        if rid is not None and "method" not in obj:
            if rid == init_id:
                notify("initialized")
                params = {"cwd": cwd, "approvalPolicy": args.approval, "sandbox": args.sandbox}
                if args.model:
                    params["model"] = args.model
                pending_start = request("thread/start", params)
                phase = "thread"
            elif rid == pending_start:
                result = obj.get("result") or {}
                thread_id = (result.get("thread") or {}).get("id") or result.get("threadId")
                if "error" in obj or not thread_id:
                    sys.stderr.write(f"[thread/start failed] {json.dumps(obj)[:300]}\n")
                    proc.stdin.close()
                    break
                request("turn/start", {"threadId": thread_id, "input": [{"type": "text", "text": prompts.pop(0)}]})
                phase = "turn"
            elif rid == models_id:
                proc.stdin.close()
                break
            continue

        # Notifications.
        if method == "turn/completed":
            if prompts:
                request("turn/start", {"threadId": thread_id, "input": [{"type": "text", "text": prompts.pop(0)}]})
            else:
                models_id = request("model/list", {})
        elif method == "error":
            sys.stderr.write(f"[error] {json.dumps(obj.get('params'))[:300]}\n")

    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
    record("# ", f"exit={proc.returncode} phase={phase}")
    out.close()
    print(f"wrote {args.out} (exit={proc.returncode}, phase={phase}, thread={thread_id})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
