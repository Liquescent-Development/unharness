#!/usr/bin/env python3
"""Record an Agent Client Protocol (ACP) session as a parser fixture (same
format as the other recorders: `>>` sent lines, `!!` stderr, plain = stdout).

Performs initialize → session/new → session/prompt for each prompt, answers
session/request_permission (allow once, or reject with --deny), and refuses
every other agent → client request, as the unharness driver does.

Usage: scripts/record-acp.py OUT.jsonl [--deny] [--image F] [--file F] [--resume ID] [--model M] PROMPT [PROMPT...] -- AGENT [ARGS...]
  e.g. scripts/record-acp.py out.jsonl "say hi" -- gemini --acp

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


def redact(line: str, cwd: str) -> str:
    home = os.path.expanduser("~")
    line = line.replace(cwd, "/WORKSPACE")
    if home:
        line = line.replace(home, "/HOME")
    line = re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "user@example.com", line)
    line = re.sub(r"sk-[A-Za-z0-9_-]{8,}", "sk-REDACTED", line)
    return line


def main() -> int:
    argv = sys.argv[1:]
    if "--" not in argv:
        sys.stderr.write(__doc__)
        return 2
    split = argv.index("--")
    agent_cmd = argv[split + 1:]
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("prompts", nargs="+")
    ap.add_argument("--deny", action="store_true", help="reject permission requests instead of allowing")
    ap.add_argument("--image", help="attach this image to the first prompt")
    ap.add_argument("--file", help="attach this file to the first prompt (a resource_link)")
    ap.add_argument("--resume", help="reattach to this session id (session/resume) instead of creating one")
    ap.add_argument("--model", help="select this model through the session's model config option")
    ap.add_argument("--idle-timeout", type=float, default=120.0)
    args = ap.parse_args(argv[:split])
    if not agent_cmd:
        sys.stderr.write("no agent command after --\n")
        return 2

    cwd = os.getcwd()
    # An agent started from inside another agent's session may behave differently.
    env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    proc = subprocess.Popen(agent_cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
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

    seq = 0

    def request(method, params):
        nonlocal seq
        seq += 1
        send({"jsonrpc": "2.0", "id": seq, "method": method, "params": params})
        return seq

    def pump_stderr():
        for line in proc.stderr:
            record("!! ", line)
            sys.stderr.write("[stderr] " + line)

    threading.Thread(target=pump_stderr, daemon=True).start()

    init_id = request("initialize", {
        "protocolVersion": 1,
        "clientCapabilities": {},
        "clientInfo": {"name": "unharness", "title": "unharness", "version": "0.2.0"},
    })
    new_id = None
    prompt_id = None
    session_id = None
    prompts = list(args.prompts)
    first = True

    def next_prompt():
        nonlocal prompt_id, first
        blocks = [{"type": "text", "text": prompts.pop(0)}]
        if first and args.image:
            import base64
            import mimetypes
            with open(args.image, "rb") as f:
                data = base64.b64encode(f.read()).decode()
            blocks.append({"type": "image", "data": data,
                           "mimeType": mimetypes.guess_type(args.image)[0] or "image/png"})
        if first and args.file:
            import mimetypes
            import pathlib
            path = pathlib.Path(args.file).resolve()
            link = {"type": "resource_link", "uri": path.as_uri(), "name": path.name}
            mime = mimetypes.guess_type(path.name)[0]
            if mime:
                link["mimeType"] = mime
            blocks.append(link)
        first = False
        prompt_id = request("session/prompt", {"sessionId": session_id, "prompt": blocks})

    last = time.time()
    status = "init"
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

        # Agent → client requests.
        if method and rid is not None:
            if method == "session/request_permission":
                options = (obj.get("params") or {}).get("options") or []
                want = "reject_once" if args.deny else "allow_once"
                pick = next((o for o in options if o.get("kind") == want), options[0] if options else None)
                if pick:
                    send({"jsonrpc": "2.0", "id": rid,
                          "result": {"outcome": {"outcome": "selected", "optionId": pick.get("optionId")}}})
                else:
                    send({"jsonrpc": "2.0", "id": rid, "result": {"outcome": {"outcome": "cancelled"}}})
            else:
                sys.stderr.write(f"[unhandled agent request] {method}\n")
                send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unsupported by unharness"}})
            continue

        # Responses to our requests.
        if rid is not None and method is None:
            if "error" in obj:
                sys.stderr.write(f"[error] {json.dumps(obj['error'])[:300]}\n")
                status = "error"
                proc.stdin.close()
                break
            if rid == init_id:
                if args.resume:
                    session_id = args.resume
                    new_id = request("session/resume", {"sessionId": session_id, "cwd": cwd, "mcpServers": []})
                else:
                    new_id = request("session/new", {"cwd": cwd, "mcpServers": []})
                status = "session"
            elif rid == new_id:
                result = obj.get("result") or {}
                session_id = result.get("sessionId") or session_id
                status = "prompt"
                option = next((o for o in result.get("configOptions") or [] if o.get("category") == "model"), None)
                if args.model and option:
                    request("session/set_config_option",
                            {"sessionId": session_id, "configId": option["id"], "value": args.model})
                next_prompt()
            elif rid == prompt_id:
                if prompts:
                    next_prompt()
                else:
                    status = "done"
                    proc.stdin.close()
                    break

    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
    record("# ", f"exit={proc.returncode} status={status}")
    out.close()
    print(f"wrote {args.out} (exit={proc.returncode}, status={status}, session={session_id})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
