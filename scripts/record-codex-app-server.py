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
    line = re.sub(r'"serverName":"[^"]*"', '"serverName":"user"', line)
    line = re.sub(r'"installationId":"[^"]*"', '"installationId":"REDACTED"', line)
    return line


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("prompts", nargs="+")
    ap.add_argument("--approval", default="untrusted")
    ap.add_argument("--sandbox", default="read-only")
    ap.add_argument("--model")
    ap.add_argument("--idle-timeout", type=float, default=180.0)
    ap.add_argument("--image", help="attach this image to the first prompt")
    ap.add_argument("--steer", help="send this text while the first tool call runs")
    ap.add_argument("--compact", action="store_true", help="compact the context after the last prompt")
    ap.add_argument("--fork-thread", help="branch this existing thread (thread/fork) instead of starting one")
    ap.add_argument("--rewind", action="store_true",
                    help="after the second prompt, rewind to before it, then send the remaining prompts")
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
    turn_id = None
    steered = False
    compact_id = None
    turn_ids = []
    revert_id = None
    rewound = False

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
                if args.fork_thread:
                    pending_start = request("thread/fork", {"threadId": args.fork_thread, "excludeTurns": True})
                else:
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
                items = [{"type": "text", "text": prompts.pop(0)}]
                if args.image:
                    items.append({"type": "localImage", "path": os.path.abspath(args.image)})
                request("turn/start", {"threadId": thread_id, "input": items})
                phase = "turn"
            elif rid == models_id:
                proc.stdin.close()
                break
            elif rid == revert_id:
                revert_id = None
                request("turn/start", {"threadId": thread_id, "input": [{"type": "text", "text": prompts.pop(0)}]})
            continue

        params = obj.get("params") or {}
        if method == "turn/started" and params.get("threadId") == thread_id:
            turn_id = (params.get("turn") or {}).get("id")
            if compact_id is None:
                turn_ids.append(turn_id)
        if (args.steer and not steered and method == "item/started"
                and (params.get("item") or {}).get("type") == "commandExecution"):
            steered = True
            request("turn/steer", {"threadId": thread_id, "expectedTurnId": turn_id,
                                   "input": [{"type": "text", "text": args.steer}]})

        # Notifications.
        # Sub-agent threads report on the same stream; only the main thread's
        # turn ends ours.
        if method == "turn/completed" and params.get("threadId") != thread_id:
            continue
        if method == "turn/completed":
            if args.rewind and len(turn_ids) == 2 and prompts and not rewound:
                rewound = True
                revert_id = request("thread/revert", {"threadId": thread_id, "beforeTurnId": turn_ids[1]})
            elif prompts:
                request("turn/start", {"threadId": thread_id, "input": [{"type": "text", "text": prompts.pop(0)}]})
            elif args.compact and compact_id is None:
                # Compaction runs as a turn of its own and ends with turn/completed.
                compact_id = request("thread/compact/start", {"threadId": thread_id})
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
