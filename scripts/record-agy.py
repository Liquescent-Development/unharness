#!/usr/bin/env python3
"""Record an Antigravity stream-json session as a parser fixture.

Drives `agy --print= --input-format stream-json --output-format stream-json`,
sends each prompt as one NDJSON line, and waits for the `result` event between
prompts. A line on stdin is `{"event":"user","message":{"content":"..."}}`
(agy 1.2.17 names the missing `event` field when sent anything else).

Usage: scripts/record-agy.py OUT.jsonl [--extra=...] PROMPT [PROMPT...]
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


def encode(text: str) -> dict:
    return {"event": "user", "message": {"content": text}}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("prompts", nargs="+")
    ap.add_argument("--extra", default="")
    ap.add_argument("--idle-timeout", type=float, default=90.0)
    args = ap.parse_args()

    cwd = os.getcwd()
    # `--print=` (empty value) enables print mode while the prompts arrive on stdin;
    # agy 1.2.x treats `-p --input-format` as a prompt named "--input-format".
    cmd = ["agy", "--print=", "--input-format", "stream-json", "--output-format", "stream-json",
           "--print-timeout", "0"]
    cmd += args.extra.split() if args.extra else []

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

    prompts = list(args.prompts)
    send(encode(prompts.pop(0)))

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
        if obj.get("event") == "result" or obj.get("type") == "result":
            if prompts:
                send(encode(prompts.pop(0)))
            else:
                proc.stdin.close()
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
