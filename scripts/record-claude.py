#!/usr/bin/env python3
"""Record a Claude Code stream-json session as a parser fixture.

Drives `claude -p --input-format stream-json --output-format stream-json ...`
with one or more prompts, auto-answers permission requests, and writes every
line in both directions to the fixture file:

  >> {...}   lines we sent on stdin
  {...}      lines claude emitted on stdout
  !! text    lines claude emitted on stderr

Usage:
  scripts/record-claude.py OUT.jsonl [--model haiku] [--allow|--deny] PROMPT [PROMPT...]

Run it from a scratch directory; the agent will act in the cwd.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import threading
import time
import uuid


def redact(line: str, cwd: str) -> str:
    home = os.path.expanduser("~")
    line = line.replace(cwd, "/WORKSPACE")
    if home:
        line = line.replace(home, "/HOME")
    line = re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "user@example.com", line)
    line = re.sub(r"sk-ant-[A-Za-z0-9_-]+", "sk-ant-REDACTED", line)
    return line


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("prompts", nargs="+")
    ap.add_argument("--model", default="haiku")
    ap.add_argument("--deny", action="store_true", help="deny permission requests instead of allowing")
    ap.add_argument("--no-initialize", action="store_true")
    ap.add_argument("--extra", default="", help="extra CLI args (space separated)")
    ap.add_argument("--idle-timeout", type=float, default=120.0)
    ap.add_argument("--image", help="attach this image to the first prompt")
    args = ap.parse_args()

    cwd = os.getcwd()
    cmd = [
        "claude", "-p",
        "--input-format", "stream-json",
        "--output-format", "stream-json",
        "--verbose", "--include-partial-messages",
        "--permission-prompts", "host",
        "--model", args.model,
    ] + (args.extra.split() if args.extra else [])

    env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
    out = open(args.out, "w")
    lock = threading.Lock()

    def record(prefix: str, line: str) -> None:
        with lock:
            out.write(prefix + redact(line.rstrip("\n"), cwd) + "\n")
            out.flush()

    def send(obj: dict) -> None:
        line = json.dumps(obj)
        record(">> ", line)
        proc.stdin.write(line + "\n")
        proc.stdin.flush()

    def pump_stderr() -> None:
        for line in proc.stderr:
            record("!! ", line)
            sys.stderr.write("[stderr] " + line)

    threading.Thread(target=pump_stderr, daemon=True).start()

    if not args.no_initialize:
        send({"type": "control_request", "request_id": str(uuid.uuid4()),
              "request": {"subtype": "initialize"}})

    prompts = list(args.prompts)
    first = prompts.pop(0)
    if args.image:
        import base64
        import mimetypes
        with open(args.image, "rb") as f:
            data = base64.b64encode(f.read()).decode()
        first = [
            {"type": "text", "text": first},
            {"type": "image", "source": {
                "type": "base64",
                "media_type": mimetypes.guess_type(args.image)[0] or "image/png",
                "data": data,
            }},
        ]
    send({"type": "user", "message": {"role": "user", "content": first}})

    last_activity = time.time()
    while True:
        if proc.poll() is not None:
            break
        line = proc.stdout.readline()
        if not line:
            if time.time() - last_activity > args.idle_timeout:
                sys.stderr.write("idle timeout\n")
                break
            time.sleep(0.05)
            continue
        last_activity = time.time()
        record("", line)
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = obj.get("type")
        if t == "control_request":
            req = obj.get("request", {})
            rid = obj.get("request_id")
            if req.get("subtype") == "can_use_tool":
                tool = req.get("tool_name")
                inp = req.get("input", {})
                sys.stderr.write(f"[permission] {tool} {json.dumps(inp)[:120]}\n")
                if tool == "AskUserQuestion":
                    answers = {}
                    for q in inp.get("questions", []):
                        opts = q.get("options", [])
                        answers[q["question"]] = opts[0]["label"] if opts else "yes"
                    resp = {"behavior": "allow", "updatedInput": {**inp, "answers": answers}}
                elif args.deny:
                    resp = {"behavior": "deny", "message": "recording: denied by user"}
                else:
                    resp = {"behavior": "allow", "updatedInput": inp}
                send({"type": "control_response",
                      "response": {"subtype": "success", "request_id": rid, "response": resp}})
            else:
                sys.stderr.write(f"[control_request] {json.dumps(req)[:200]}\n")
        elif t == "result":
            if prompts:
                send({"type": "user", "message": {"role": "user", "content": prompts.pop(0)}})
            else:
                proc.stdin.close()
                # drain remaining output
                for rest in proc.stdout:
                    record("", rest)
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
