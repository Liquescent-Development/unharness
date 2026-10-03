#!/usr/bin/env python3
"""Record `codex exec --json` turns as parser fixtures.

Runs the first prompt in a new thread, captures `thread.started.thread_id`,
then resumes that thread for each following prompt. Each turn is written to
OUT-<n>.jsonl in the usual fixture format (`>>` = stdin we sent, `!!` =
stderr, plain = stdout).

Usage: scripts/record-codex-exec.py OUT [--sandbox read-only] [--extra "..."] PROMPT [PROMPT...]
"""
import argparse
import json
import os
import re
import subprocess
import sys


def redact(line: str, cwd: str) -> str:
    home = os.path.expanduser("~")
    line = line.replace(cwd, "/WORKSPACE")
    if home:
        line = line.replace(home, "/HOME")
    line = re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "user@example.com", line)
    return line


def run_turn(out_path: str, argv: list, prompt: str, cwd: str, timeout: float) -> str | None:
    thread_id = None
    with open(out_path, "w") as out:
        out.write(">> " + redact(prompt, cwd) + "\n")
        try:
            proc = subprocess.run(argv, input=prompt, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired as e:
            out.write("# timeout\n")
            stdout, stderr = e.stdout or "", e.stderr or ""
            rc = "timeout"
        else:
            stdout, stderr, rc = proc.stdout, proc.stderr, proc.returncode
        for line in stdout.splitlines():
            out.write(redact(line, cwd) + "\n")
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("type") == "thread.started":
                thread_id = obj.get("thread_id")
        for line in stderr.splitlines():
            out.write("!! " + redact(line, cwd) + "\n")
        out.write(f"# exit={rc}\n")
    print(f"wrote {out_path} (exit={rc}, thread={thread_id})")
    return thread_id


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("prompts", nargs="+")
    ap.add_argument("--sandbox", default="read-only")
    ap.add_argument("--extra", default="")
    ap.add_argument("--timeout", type=float, default=180.0)
    args = ap.parse_args()

    cwd = os.getcwd()
    extra = args.extra.split() if args.extra else []
    base = ["codex", "exec", "--json", "--skip-git-repo-check", "-s", args.sandbox] + extra

    thread_id = run_turn(f"{args.out}-1.jsonl", base + ["-"], args.prompts[0], cwd, args.timeout)
    for i, prompt in enumerate(args.prompts[1:], start=2):
        if not thread_id:
            print("no thread id captured; cannot resume")
            return 1
        argv = ["codex", "exec", "resume", thread_id, "--json", "--skip-git-repo-check"] + extra + ["-"]
        run_turn(f"{args.out}-{i}.jsonl", argv, prompt, cwd, args.timeout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
