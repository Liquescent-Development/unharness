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
    ap.add_argument("--file", help="attach this PDF or text file to the first prompt")
    ap.add_argument("--steer", help="send this text while the first tool call runs")
    ap.add_argument("--compact", action="store_true", help="compact the context after the last prompt")
    ap.add_argument("--rewind", action="store_true",
                    help="after the second prompt, rewind to before it, then send the remaining prompts")
    ap.add_argument("--stop-tasks", action="store_true",
                    help="when a turn ends with subagents still running, stop each one (stop_task)")
    ap.add_argument("--interrupt-tasks", action="store_true",
                    help="when a turn ends with subagents still running, interrupt (which stops them)")
    args = ap.parse_args()

    cwd = os.getcwd()
    cmd = [
        "claude", "-p",
        "--input-format", "stream-json",
        "--output-format", "stream-json",
        "--verbose", "--include-partial-messages",
        "--permission-prompts", "host",
        # As unharness runs it; without it there is no AskUserQuestion.
        "--permission-prompt-tool", "stdio",
        "--model", args.model,
    ] + (args.extra.split() if args.extra else [])

    # Not a parent session's identity; the provider switches stay.
    env = {k: v for k, v in os.environ.items()
           if not k.startswith("CLAUDE") or k.startswith("CLAUDE_CODE_USE_")}
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
    if args.file:
        import base64
        if isinstance(first, str):
            first = [{"type": "text", "text": first}]
        name = os.path.basename(args.file)
        if args.file.lower().endswith(".pdf"):
            with open(args.file, "rb") as f:
                source = {"type": "base64", "media_type": "application/pdf",
                          "data": base64.b64encode(f.read()).decode()}
        else:
            with open(args.file) as f:
                source = {"type": "text", "media_type": "text/plain", "data": f.read()}
        first.append({"type": "document", "source": source, "title": name})
    turn_ids = []

    def send_turn(content):
        # Each turn carries a uuid of ours; rewind_conversation targets it.
        turn_ids.append(str(uuid.uuid4()))
        send({"type": "user", "uuid": turn_ids[-1], "message": {"role": "user", "content": content}})

    send_turn(first)

    rewind_id = None
    # Subagent tasks that have started and not yet reported an end.
    running_tasks = []
    tasks_stopped = False
    steered = False
    compacted = False
    rewound = False
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
        if (args.steer and not steered and t == "assistant"
                and any(b.get("type") == "tool_use" for b in obj.get("message", {}).get("content", []))):
            steered = True
            send({"type": "user", "message": {"role": "user", "content": args.steer}, "priority": "next"})
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
        elif t == "control_response" and rewind_id and obj.get("response", {}).get("request_id") == rewind_id:
            rewind_id = None
            send_turn(prompts.pop(0))
        elif t == "system" and obj.get("subtype") == "task_started" and obj.get("task_type") == "local_agent":
            running_tasks.append(obj.get("task_id"))
        elif t == "system" and obj.get("subtype") == "task_notification":
            if obj.get("task_id") in running_tasks:
                running_tasks.remove(obj.get("task_id"))
            if args.interrupt_tasks and tasks_stopped and not running_tasks:
                # An interrupt between turns ends the subagents without a turn
                # to report it, so no `result` will come.
                time.sleep(2)
                proc.stdin.close()
                for rest in proc.stdout:
                    record("", rest)
                break
        elif t == "result":
            if args.stop_tasks and running_tasks and not tasks_stopped:
                # Claude reports a stopped task in a turn of its own, whose
                # `result` then ends the recording.
                tasks_stopped = True
                for task_id in running_tasks:
                    send({"type": "control_request", "request_id": str(uuid.uuid4()),
                          "request": {"subtype": "stop_task", "task_id": task_id}})
            elif args.interrupt_tasks and running_tasks and not tasks_stopped:
                # Leave stdin open: whatever the interrupt causes should be recorded.
                tasks_stopped = True
                send({"type": "control_request", "request_id": str(uuid.uuid4()),
                      "request": {"subtype": "interrupt"}})
            elif args.rewind and len(turn_ids) == 2 and prompts and not rewound:
                rewound = True
                rewind_id = str(uuid.uuid4())
                send({"type": "control_request", "request_id": rewind_id,
                      "request": {"subtype": "rewind_conversation", "target_message_uuid": turn_ids[1]}})
            elif prompts:
                send_turn(prompts.pop(0))
            elif args.compact and not compacted:
                compacted = True
                send({"type": "user", "message": {"role": "user", "content": "/compact"}})
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
