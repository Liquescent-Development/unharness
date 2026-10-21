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

With --send-now TEXT the recorder sends TEXT at once while the first tool
call runs, as unharness's /now does on a CLI that advertises
`interrupt_send_now_v1`: the message, then `interrupt` with `send_now` and the
message's uuid (--send-now-plain: a plain `interrupt`, then the message).
--send-now-cancel takes it back at once (`interrupt` with `cancel_queued`).
--wait-after keeps reading that long after the last `result`, for a turn the
CLI starts by itself.

With --remote-control the session is put on Remote Control before the first
prompt, and after the last local prompt the recorder keeps the session open
for what arrives from claude.ai/code or the Claude app, until --until-file
exists or nothing has come for --idle-timeout seconds.

Run it from a scratch directory; the agent will act in the cwd.
"""
import argparse
import json
import os
import queue
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
    # The account's organization, named in initialize's answer: a name
    # that is not an email address would pass the line above.
    line = re.sub(r'"(organization|orgName)":"(?:[^"\\]|\\.)*"', r'"\1":"Example Org"', line)
    # Remote Control's environment and bridge session ids, also inside URLs.
    line = re.sub(r"\b(env|session|cse)_[A-Za-z0-9]{12,}", r"\1_REDACTED", line)
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
    ap.add_argument("--send-now", metavar="TEXT",
                    help="send this at once (interrupting the turn) while the first tool call runs")
    ap.add_argument("--send-now-after", type=float, default=3.0, metavar="SECONDS",
                    help="how long after the first tool call starts --send-now goes")
    ap.add_argument("--send-now-plain", action="store_true",
                    help="send --send-now as a plain interrupt and then the message")
    ap.add_argument("--send-now-cancel", action="store_true",
                    help="right after --send-now, take it back (interrupt with cancel_queued)")
    ap.add_argument("--interrupt-after", type=float, metavar="SECONDS",
                    help="send a plain interrupt this long after the first tool call starts")
    ap.add_argument("--wait-after", type=float, default=0.0, metavar="SECONDS",
                    help="after the last result, read on until nothing has come for this long")
    ap.add_argument("--answer-delay", type=float, default=0.0,
                    help="answer permission requests this many seconds late, as a person would")
    ap.add_argument("--compact", action="store_true", help="compact the context after the last prompt")
    ap.add_argument("--get-usage", action="store_true",
                    help="ask for the account's usage (get_usage, as /status does) before the "
                         "first prompt and after the last")
    ap.add_argument("--rewind", action="store_true",
                    help="after the second prompt, rewind to before it, then send the remaining prompts")
    ap.add_argument("--stop-tasks", action="store_true",
                    help="when a turn ends with subagents still running, stop each one (stop_task)")
    ap.add_argument("--interrupt-tasks", action="store_true",
                    help="when a turn ends with subagents still running, interrupt (which stops them)")
    ap.add_argument("--set-mode", help="send set_permission_mode with this mode before the first prompt")
    ap.add_argument("--plan-approve", metavar="MODE",
                    help="approve ExitPlanMode after sending set_permission_mode MODE, as unharness does")
    ap.add_argument("--plan-reject", metavar="FEEDBACK",
                    help="deny the first ExitPlanMode with this message (later ones are approved)")
    ap.add_argument("--remote-control", nargs="?", const="", metavar="NAME",
                    help="enable Remote Control (optionally named) before the first prompt")
    ap.add_argument("--remote-off", action="store_true",
                    help="with --remote-control, disable it before closing the session")
    ap.add_argument("--until-file", metavar="PATH",
                    help="with --remote-control, close the session once this file exists")
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
    # Lines go to stdin from timers too: one at a time, in the order recorded.
    send_lock = threading.Lock()

    def record(prefix: str, line: str) -> None:
        with lock:
            out.write(prefix + redact(line.rstrip("\n"), cwd) + "\n")
            out.flush()

    def send(obj: dict) -> None:
        line = json.dumps(obj)
        with send_lock:
            record(">> ", line)
            proc.stdin.write(line + "\n")
            proc.stdin.flush()

    def pump_stderr() -> None:
        for line in proc.stderr:
            record("!! ", line)
            sys.stderr.write("[stderr] " + line)

    threading.Thread(target=pump_stderr, daemon=True).start()

    # Read on a thread, so that waiting for remote turns can time out.
    lines = queue.Queue()

    def pump_stdout() -> None:
        for line in proc.stdout:
            lines.put(line)
        lines.put(None)

    threading.Thread(target=pump_stdout, daemon=True).start()

    def drain() -> None:
        while True:
            try:
                rest = lines.get(timeout=15)
            except queue.Empty:
                return
            if rest is None:
                return
            record("", rest)

    if not args.no_initialize:
        send({"type": "control_request", "request_id": str(uuid.uuid4()),
              "request": {"subtype": "initialize"}})

    remote_id = None
    if args.remote_control is not None:
        remote_id = str(uuid.uuid4())
        request = {"subtype": "remote_control", "enabled": True}
        if args.remote_control:
            request["name"] = args.remote_control
        send({"type": "control_request", "request_id": remote_id, "request": request})

    if args.set_mode:
        send({"type": "control_request", "request_id": str(uuid.uuid4()),
              "request": {"subtype": "set_permission_mode", "mode": args.set_mode}})

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

    def get_usage():
        request_id = str(uuid.uuid4())
        send({"type": "control_request", "request_id": request_id,
              "request": {"subtype": "get_usage", "skip_behaviors": True}})
        return request_id

    if args.get_usage:
        get_usage()
    send_turn(first)
    # The get_usage after the last prompt: its answer ends the recording.
    final_usage = None

    rewind_id = None
    # Subagent tasks that have started and not yet reported an end.
    running_tasks = []
    tasks_stopped = False
    steered = False
    # The first tool call has started: what is timed from it is armed.
    tool_seen = False
    compacted = False
    rewound = False
    plan_rejected = False
    # Local prompts are done; waiting for what Remote Control brings.
    remote_waiting = False

    def close_remote() -> None:
        if args.remote_off:
            send({"type": "control_request", "request_id": str(uuid.uuid4()),
                  "request": {"subtype": "remote_control", "enabled": False}})
            time.sleep(3)
        proc.stdin.close()
        drain()

    last_activity = time.time()
    # Prompts are done; closing once nothing has come for --wait-after.
    finishing = False
    while True:
        try:
            line = lines.get(timeout=0.2)
        except queue.Empty:
            line = ""
        if line is None:
            break
        if not line:
            if finishing and time.time() - last_activity > args.wait_after:
                proc.stdin.close()
                drain()
                break
            if remote_waiting and args.until_file and os.path.exists(args.until_file):
                sys.stderr.write("until-file found\n")
                close_remote()
                break
            if time.time() - last_activity > args.idle_timeout:
                sys.stderr.write("idle timeout\n")
                if remote_waiting:
                    close_remote()
                break
            continue
        last_activity = time.time()
        record("", line)
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        t = obj.get("type")
        if (final_usage and t == "control_response"
                and obj.get("response", {}).get("request_id") == final_usage):
            proc.stdin.close()
            drain()
            break
        if (args.steer and not steered and t == "assistant"
                and any(b.get("type") == "tool_use" for b in obj.get("message", {}).get("content", []))):
            steered = True
            send({"type": "user", "message": {"role": "user", "content": args.steer}, "priority": "next"})
        if (not tool_seen and t == "assistant"
                and any(b.get("type") == "tool_use" for b in obj.get("message", {}).get("content", []))):
            tool_seen = True
            if args.send_now:
                def send_now():
                    message_id = str(uuid.uuid4())
                    message = {"type": "user", "uuid": message_id,
                               "message": {"role": "user", "content": args.send_now}}
                    if args.send_now_plain:
                        send({"type": "control_request", "request_id": str(uuid.uuid4()),
                              "request": {"subtype": "interrupt"}})
                        send(message)
                    else:
                        send(message)
                        send({"type": "control_request", "request_id": str(uuid.uuid4()),
                              "request": {"subtype": "interrupt", "send_now": True,
                                          "message_uuid": message_id}})
                    if args.send_now_cancel:
                        send({"type": "control_request", "request_id": str(uuid.uuid4()),
                              "request": {"subtype": "interrupt", "cancel_queued": True}})
                timer = threading.Timer(args.send_now_after, send_now)
                timer.daemon = True
                timer.start()
            if args.interrupt_after is not None:
                timer = threading.Timer(args.interrupt_after, lambda: send(
                    {"type": "control_request", "request_id": str(uuid.uuid4()),
                     "request": {"subtype": "interrupt"}}))
                timer.daemon = True
                timer.start()
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
                elif tool == "ExitPlanMode" and args.plan_reject and not plan_rejected:
                    plan_rejected = True
                    # Worded as Claude's own interface words a rejection with feedback.
                    resp = {"behavior": "deny", "message": (
                        "The user doesn't want to proceed with this tool use. The tool use was "
                        "rejected (eg. if it was a file edit, the new_string was NOT written to "
                        "the file). To tell you how to proceed, the user said:\n" + args.plan_reject)}
                elif tool == "ExitPlanMode" and args.plan_approve:
                    send({"type": "control_request", "request_id": str(uuid.uuid4()),
                          "request": {"subtype": "set_permission_mode", "mode": args.plan_approve}})
                    resp = {"behavior": "allow", "updatedInput": inp}
                elif args.deny:
                    resp = {"behavior": "deny", "message": "recording: denied by user"}
                else:
                    resp = {"behavior": "allow", "updatedInput": inp}
                answer = {"type": "control_response",
                          "response": {"subtype": "success", "request_id": rid, "response": resp}}
                if args.answer_delay:
                    def late(answer=answer):
                        # Claude may have cancelled the request and ended already.
                        try:
                            send(answer)
                        except (ValueError, BrokenPipeError):
                            pass
                    timer = threading.Timer(args.answer_delay, late)
                    # Not one to wait for once the session has ended.
                    timer.daemon = True
                    timer.start()
                else:
                    send(answer)
            else:
                sys.stderr.write(f"[control_request] {json.dumps(req)[:200]}\n")
        elif t == "control_response" and remote_id and obj.get("response", {}).get("request_id") == remote_id:
            remote_id = None
            sys.stderr.write(f"[remote_control] {json.dumps(obj.get('response'))}\n")
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
                drain()
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
            elif args.remote_control is not None:
                if not remote_waiting:
                    remote_waiting = True
                    sys.stderr.write("[remote_control] local prompts done; waiting for remote turns\n")
            elif args.wait_after:
                finishing = True
            elif args.get_usage and not final_usage:
                final_usage = get_usage()
            else:
                proc.stdin.close()
                drain()
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
