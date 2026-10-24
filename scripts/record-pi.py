#!/usr/bin/env python3
"""Record a pi `--mode rpc` session as a parser fixture (same format as
record-claude.py: `>>` sent lines, `!!` stderr, plain = stdout).

Usage: scripts/record-pi.py OUT.jsonl [--provider P --model M] [--no-models] PROMPT [PROMPT...]
       scripts/record-pi.py OUT.jsonl --resume ID [--no-models] [PROMPT...]

With `--resume ID` pi is started on that stored session as unharness
resumes one (`--session-id`, the session saved), and the session's user
messages are listed (`get_fork_messages`) and its stats asked for before
the first prompt, as the driver does; the prompts may then be left out.

With `--extension src/harness/pi/gate.ts --select Allow --select Deny` the
gate's dialogs are answered in that order (the last answer repeats);
`--abort-on-select` sends `abort` instead of answering the first one.
`--no-extensions`, `--no-context-files`, `--no-skills`, `--no-prompt-templates`,
`--skill F` and `--prompt-template F` keep the user's own resources (and
their text) out of the recording.

What is recorded leaves out the system prompt: the system message's
`message_start`/`message_end` lines and `agent_end` lines are not
written, and `get_entries` answers have their system messages (and a
compaction's `systemMessage`) emptied. `--redact OLD=NEW` replaces text
in every line (a provider's name, a host).
"""
import argparse
import json
import os
import re
import subprocess
import sys
import threading
import time


def without_system_prompt(line: str):
    """The line as it is recorded, `None` to leave it out (see above)."""
    try:
        obj = json.loads(line)
    except json.JSONDecodeError:
        return line
    if not isinstance(obj, dict):
        return line
    t = obj.get("type")
    if t == "agent_end":
        return None
    if t in ("message_start", "message_end") and (obj.get("message") or {}).get("role") == "system":
        return None
    if obj.get("command") == "get_entries" and obj.get("success"):
        omitted = lambda m: {"role": "system", "content": "[system prompt omitted]",
                             "timestamp": (m or {}).get("timestamp")}
        for e in (obj.get("data") or {}).get("entries") or []:
            if (e.get("message") or {}).get("role") == "system":
                e["message"] = omitted(e["message"])
            if e.get("systemMessage"):
                e["systemMessage"] = omitted(e["systemMessage"])
        return json.dumps(obj, separators=(",", ":"), ensure_ascii=False)
    return line


def redact(line: str, cwd: str, pairs=()) -> str:
    for old, new in pairs:
        line = line.replace(old, new)
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
    ap.add_argument("prompts", nargs="*")
    ap.add_argument("--provider")
    ap.add_argument("--model")
    ap.add_argument("--thinking")
    ap.add_argument("--no-models", action="store_true", help="skip get_available_models")
    ap.add_argument("--thinking-levels", action="store_true",
                    help="also ask for the thinking levels (the driver only does after a model switch)")
    ap.add_argument("--idle-timeout", type=float, default=120.0)
    ap.add_argument("--image", help="attach this image to the first prompt")
    ap.add_argument("--steer", help="send this text while the first tool call runs")
    ap.add_argument("--compact", action="store_true", help="compact the context after the last prompt")
    ap.add_argument("--after-compact", action="append", default=[], metavar="PROMPT",
                    help="send this prompt once the compaction ended (repeatable; with --compact)")
    ap.add_argument("--approve", action="store_true",
                    help="trust the workspace's .pi for this run (its settings.json, e.g. a low keepRecentTokens)")
    ap.add_argument("--rewind", action="store_true",
                    help="after the second prompt, rewind to before it, then send the remaining prompts")
    ap.add_argument("--extension", action="append", default=[],
                    help="load this extension file (repeatable); others are not discovered")
    ap.add_argument("--no-extensions", action="store_true", help="discover no extensions")
    ap.add_argument("--no-context-files", action="store_true",
                    help="leave out AGENTS.md files (pi's own global one included)")
    ap.add_argument("--no-skills", action="store_true", help="load no skills")
    ap.add_argument("--no-prompt-templates", action="store_true", help="load no prompt templates")
    ap.add_argument("--skill", action="append", default=[],
                    help="load this skill (repeatable); others are not discovered")
    ap.add_argument("--prompt-template", action="append", default=[],
                    help="load this prompt template (repeatable); others are not discovered")
    ap.add_argument("--select", action="append", default=[],
                    help="the answer to the next select dialog (repeatable; default: ok)")
    ap.add_argument("--abort-on-select", action="store_true",
                    help="send abort instead of answering the first select dialog")
    ap.add_argument("--session-id",
                    help="pass --session-id as unharness does (a pi before 0.76.0 refuses it)")
    ap.add_argument("--resume", metavar="ID",
                    help="resume this stored session as the driver does (see above)")
    ap.add_argument("--redact", action="append", default=[], metavar="OLD=NEW",
                    help="replace OLD with NEW in every line recorded (repeatable)")
    ap.add_argument("--keep-session", action="store_true",
                    help="save the session (to resume it later; a sessionDir in the workspace's "
                         ".pi/settings.json keeps it there)")
    args = ap.parse_args()
    if not args.prompts and not args.resume:
        ap.error("a PROMPT is needed (only --resume runs without one)")
    if args.resume and (args.session_id or args.image):
        ap.error("--resume takes neither --session-id nor --image")

    cwd = os.getcwd()
    if args.resume:
        cmd = ["pi", "--mode", "rpc", "--session-id", args.resume]
    else:
        cmd = ["pi", "--mode", "rpc"] + ([] if args.keep_session else ["--no-session"])
    if args.session_id:
        cmd += ["--session-id", args.session_id]
    if args.provider:
        cmd += ["--provider", args.provider]
    if args.model:
        cmd += ["--model", args.model]
    if args.thinking:
        cmd += ["--thinking", args.thinking]
    if args.approve:
        cmd += ["--approve"]
    if args.extension or args.no_extensions:
        cmd += ["--no-extensions"]
        for e in args.extension:
            cmd += ["-e", os.path.abspath(e)]
    if args.no_context_files:
        cmd += ["--no-context-files"]
    if args.skill or args.no_skills:
        cmd += ["--no-skills"]
        for s in args.skill:
            cmd += ["--skill", os.path.abspath(s)]
    if args.prompt_template or args.no_prompt_templates:
        cmd += ["--no-prompt-templates"]
        for t in args.prompt_template:
            cmd += ["--prompt-template", os.path.abspath(t)]

    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, bufsize=1)
    out = open(args.out, "w")
    lock = threading.Lock()

    pairs = [r.split("=", 1) for r in args.redact]

    def record(prefix, line):
        line = line.rstrip("\n")
        if prefix == "":
            line = without_system_prompt(line)
            if line is None:
                return
        with lock:
            out.write(prefix + redact(line, cwd, pairs) + "\n")
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
    send({"id": next_id(), "type": "get_commands"})
    if not args.no_models:
        send({"id": next_id(), "type": "get_available_models"})
    if args.thinking_levels:
        send({"id": next_id(), "type": "get_available_thinking_levels"})

    prompts = list(args.prompts)
    after_compact = list(args.after_compact)
    if args.resume:
        # The prompts follow the listing (below). How full the context is
        # comes after it, as the driver asks.
        send({"id": next_id(), "type": "get_fork_messages"})
        send({"id": next_id(), "type": "get_session_stats"})
        first = None
    else:
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
    if first:
        send(first)

    # The user messages pi last listed (`get_fork_messages`), and what a
    # compaction kept from, until its entries say which turn holds it: as
    # the driver does, asked from the last listed message on, then from the
    # one before, at most ENTRY_TRIES times.
    listed = []
    cut = None
    cursor = 0
    tries = 0
    ENTRY_TRIES = 4

    def ask_entries():
        nonlocal tries
        tries += 1
        send({"id": next_id(), "type": "get_entries", "since": listed[cursor]})

    steered = False
    aborted = False
    compacting = False
    compaction_ended = False
    rewound = False
    turns = 0
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
        if args.steer and not steered and t == "tool_execution_start":
            steered = True
            send({"id": next_id(), "type": "steer", "message": args.steer})
        if t == "extension_ui_request":
            method = obj.get("method")
            rid = obj.get("id")
            if method == "select" and args.abort_on_select and not aborted:
                # The dialog is left open: what pi does with it is the point.
                aborted = True
                send({"id": next_id(), "type": "abort"})
            elif method == "confirm":
                send({"type": "extension_ui_response", "id": rid, "confirmed": True})
            elif method == "select" and args.select:
                answer = args.select.pop(0) if len(args.select) > 1 else args.select[0]
                send({"type": "extension_ui_response", "id": rid, "value": answer})
            elif method in ("select", "input", "editor"):
                send({"type": "extension_ui_response", "id": rid, "value": "ok"})
        elif t == "agent_settled":
            # After every turn the driver asks for context usage and for the
            # user messages (their entry ids are what a rewind targets).
            turns += 1
            send({"id": next_id(), "type": "get_session_stats"})
            send({"id": next_id(), "type": "get_fork_messages"})
        elif t == "response" and obj.get("command") == "get_entries":
            entries = ((obj.get("data") or {}).get("entries") or []) if obj.get("success") else None
            if cut and entries is not None and not any(e.get("id") == cut for e in entries) \
                    and cursor > 0 and tries < ENTRY_TRIES:
                cursor -= 1
                ask_entries()
            else:
                cut = None
        elif t == "response" and obj.get("command") == "get_fork_messages" and obj.get("success", True):
            messages = (obj.get("data") or {}).get("messages") or []
            listed = [m.get("entryId") for m in messages if m.get("entryId")]
            if args.rewind and not rewound and turns == 2 and len(messages) >= 2:
                # Rewind = fork a new session from before the second message.
                rewound = True
                send({"id": next_id(), "type": "fork", "entryId": messages[1]["entryId"]})
            elif prompts:
                send({"id": next_id(), "type": "prompt", "message": prompts.pop(0)})
            elif args.compact and not compacting:
                compacting = True
                send({"id": next_id(), "type": "compact"})
            elif compacting and after_compact:
                send({"id": next_id(), "type": "prompt", "message": after_compact.pop(0)})
            else:
                break
        elif t == "compaction_end":
            if obj.get("reason") == "manual":
                compaction_ended = True
                # A requested compaction is a unit of work of its own: the
                # driver asks what it asks after a turn once it ends.
                send({"id": next_id(), "type": "get_session_stats"})
                send({"id": next_id(), "type": "get_fork_messages"})
            # One that succeeded: the driver asks for the session's entries
            # from the last message pi listed, to tell which turn holds the
            # entry the kept messages start at.
            kept = (obj.get("result") or {}).get("firstKeptEntryId")
            if kept and listed:
                cut, cursor, tries = kept, len(listed) - 1, 0
                ask_entries()
        elif t == "response" and obj.get("command") == "fork":
            # The fork is only in place once it answers (other commands sent
            # meanwhile still see the old session); then re-read the new
            # session's state and message list.
            send({"id": next_id(), "type": "get_state"})
            send({"id": next_id(), "type": "get_fork_messages"})
        elif t == "response" and obj.get("command") == "compact" and not obj.get("success", True):
            sys.stderr.write(f"[error] compact: {obj.get('error')}\n")
            # Refused before it started: nothing more will come.
            if not compaction_ended:
                break
        elif t == "response" and not obj.get("success", True):
            sys.stderr.write(f"[error] {obj.get('command')}: {obj.get('error')}\n")
            if obj.get("command") == "prompt":
                break

    proc.stdin.close()
    for rest in proc.stdout:
        record("", rest)

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
