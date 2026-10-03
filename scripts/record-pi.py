#!/usr/bin/env python3
"""Record a pi `--mode rpc` session as a parser fixture (same format as
record-claude.py: `>>` sent lines, `!!` stderr, plain = stdout).

Usage: scripts/record-pi.py OUT.jsonl [--provider P --model M] [--no-models] PROMPT [PROMPT...]
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
    line = re.sub(r"sk-[A-Za-z0-9_-]{8,}", "sk-REDACTED", line)
    return line


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("prompts", nargs="+")
    ap.add_argument("--provider")
    ap.add_argument("--model")
    ap.add_argument("--thinking")
    ap.add_argument("--no-models", action="store_true", help="skip get_available_models")
    ap.add_argument("--idle-timeout", type=float, default=120.0)
    ap.add_argument("--image", help="attach this image to the first prompt")
    args = ap.parse_args()

    cwd = os.getcwd()
    cmd = ["pi", "--mode", "rpc", "--no-session"]
    if args.provider:
        cmd += ["--provider", args.provider]
    if args.model:
        cmd += ["--model", args.model]
    if args.thinking:
        cmd += ["--thinking", args.thinking]

    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, bufsize=1)
    out = open(args.out, "w")
    lock = threading.Lock()

    def record(prefix, line):
        with lock:
            out.write(prefix + redact(line.rstrip("\n"), cwd) + "\n")
            out.flush()

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

    seq = 0

    def next_id():
        nonlocal seq
        seq += 1
        return f"req-{seq}"

    send({"id": next_id(), "type": "get_state"})
    if not args.no_models:
        send({"id": next_id(), "type": "get_available_models"})
    send({"id": next_id(), "type": "get_available_thinking_levels"})

    prompts = list(args.prompts)
    first = {"id": next_id(), "type": "prompt", "message": prompts.pop(0)}
    if args.image:
        import base64
        import mimetypes
        with open(args.image, "rb") as f:
            data = base64.b64encode(f.read()).decode()
        first["images"] = [{
            "type": "image",
            "data": data,
            "mimeType": mimetypes.guess_type(args.image)[0] or "image/png",
        }]
    send(first)

    done = False
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
        t = obj.get("type")
        if t == "extension_ui_request":
            method = obj.get("method")
            rid = obj.get("id")
            if method == "confirm":
                send({"type": "extension_ui_response", "id": rid, "confirmed": True})
            elif method in ("select", "input", "editor"):
                send({"type": "extension_ui_response", "id": rid, "value": "ok"})
        elif t == "agent_settled":
            # The driver asks for context usage after every turn.
            send({"id": next_id(), "type": "get_session_stats"})
            if prompts:
                send({"id": next_id(), "type": "prompt", "message": prompts.pop(0)})
            else:
                done = True
        elif t == "response" and done and obj.get("command") == "get_session_stats":
            proc.stdin.close()
            for rest in proc.stdout:
                record("", rest)
            break
        elif t == "response" and not obj.get("success", True):
            sys.stderr.write(f"[error] {obj.get('command')}: {obj.get('error')}\n")
            if obj.get("command") == "prompt":
                proc.stdin.close()
                break

    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
    record("# ", f"exit={proc.returncode}")
    out.close()
    print(f"wrote {args.out} (exit={proc.returncode})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
