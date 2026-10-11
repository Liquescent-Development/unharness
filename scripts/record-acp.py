#!/usr/bin/env python3
"""Record an Agent Client Protocol (ACP) session as a parser fixture (same
format as the other recorders: `>>` sent lines, `!!` stderr, plain = stdout,
and a closing `# exit=N status=…` line).

Usage: scripts/record-acp.py OUT.jsonl [FLAGS] PROMPT [PROMPT...] -- AGENT [ARGS...]
(--help lists every flag)
  e.g. scripts/record-acp.py out.jsonl "say hi" -- gemini --acp
       scripts/record-acp.py out.jsonl --client-capabilities '{"elicitation":{"form":{}}}' \\
           --elicit '{"q0":"Red"}' "ask me my favourite colour" -- vibe-acp

Run it from a scratch directory; the agent will act in the cwd.

What it sends, in this order:
  1. `initialize` (with --client-capabilities, else `{}`);
  2. `session/new`, or `session/resume` (--resume) or `session/load` (--load);
  3. once the session answers: --mode, and once that has answered (the
     unharness driver holds the prompt until the agent's mode is set,
     #155) --model, then each --config, in the order given, without
     waiting for their answers (as the driver does);
  4. `session/prompt` for each PROMPT, each once the previous one answered
     (--image and --file go with the first); with --cancel-after, a
     `session/cancel` notification that long after the last one was sent,
     if it has not answered by then;
  5. each --ext request, once --ext-after prompts have answered (by default
     all of the original session's), each once the previous one answered;
  6. with --fork, `session/fork` of the session, and the last PROMPT is sent
     to the fork instead (so --fork needs two prompts);
  7. with --close, `session/close` of the current session (the fork, after
     --fork); then stdin is closed and the agent given 15 s to exit.
An error answer to --ext or --close is recorded and the run goes on; any
other error ends it (status=error).

Agent → client requests: `session/request_permission` is answered allow once
(or as --answers / --deny say), `elicitation/create` as --elicit says, and
every other request is refused with -32601, as the unharness driver does.
When a request does not offer the option the answer needs (`allow_once`,
`reject_once`) it is answered `cancelled`; an "always" option is never
picked.

Without the flags added for #169 the lines sent are the ones the recorder
sent before them, so a fixture re-recorded that way keeps its `>>` lines (an
e2e test needs a fixture's `>>` lines to be what the driver sends).

Placeholders in --ext JSON: `$SESSION` is the current session id;
`$USER_MESSAGE` and `$AGENT_MESSAGE` the `messageId` of the newest
`user_message_chunk` / `agent_message_chunk` seen, `$USER_MESSAGE_<n>` and
`$AGENT_MESSAGE_<n>` the n-th distinct one (from 1). A placeholder with no
value yet ends the run before the request is sent.

Do not send the model or thinking level to vibe-acp (2.26.1), with --model
or --config: vibe writes the choice into its config.toml (#164). The
recorder warns when it sees that; run vibe with a scratch VIBE_HOME.
"""
import argparse
import json
import os
import re
import select
import subprocess
import sys
import threading
import time

PLACEHOLDER = re.compile(r"\$(SESSION|USER_MESSAGE(?:_[0-9]+)?|AGENT_MESSAGE(?:_[0-9]+)?)\b")


def redact(line: str, cwd: str) -> str:
    home = os.path.expanduser("~")
    line = line.replace(cwd, "/WORKSPACE")
    if home:
        line = line.replace(home, "/HOME")
    line = re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "user@example.com", line)
    line = re.sub(r"sk-[A-Za-z0-9_-]{8,}", "sk-REDACTED", line)
    return line


def parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        prog="record-acp.py",
        usage=argparse.SUPPRESS,
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("out", help="the fixture to write")
    ap.add_argument("prompts", nargs="+", metavar="PROMPT", help="one session/prompt each, in order")
    ap.add_argument("--client-capabilities", default="{}", metavar="JSON",
                    help="initialize's clientCapabilities, e.g. '{\"elicitation\":{\"form\":{}}}' "
                         "(default {})")
    reattach = ap.add_mutually_exclusive_group()
    reattach.add_argument("--resume", metavar="ID",
                          help="reattach to this session with session/resume instead of creating one")
    reattach.add_argument("--load", metavar="ID",
                          help="reattach to this session with session/load (the agent replays its "
                               "history) instead of creating one")
    ap.add_argument("--mode", metavar="ID", help="set this session mode with session/set_mode")
    ap.add_argument("--mode-option", action="store_true",
                    help="send --mode as session/set_config_option of the session's option of "
                         "category mode instead")
    ap.add_argument("--model", metavar="M",
                    help="select this model through the session's option of category model")
    ap.add_argument("--config", action="append", default=[], metavar="ID=VALUE",
                    help="session/set_config_option of any option (repeatable; an option the "
                         "session lists as boolean takes true or false)")
    ap.add_argument("--fork", action="store_true",
                    help="after the other prompts, session/fork the session and send the last "
                         "PROMPT to the fork")
    ap.add_argument("--close", action="store_true",
                    help="send session/close for the current session before closing stdin")
    ap.add_argument("--cancel-after", type=float, metavar="SECONDS",
                    help="send session/cancel this long after the last prompt, if it is still running")
    ap.add_argument("--deny", action="store_true",
                    help="reject permission requests instead of allowing them (also after --answers)")
    ap.add_argument("--answers", metavar="LIST",
                    help="answer permission requests in order, from a comma-separated list of "
                         "allow, deny and cancel; later requests get the default (allow, or --deny)")
    ap.add_argument("--elicit", action="append", default=[], metavar="JSON|decline|cancel",
                    help="answer elicitation/create: accept with this content object, decline or "
                         "cancel (repeatable: used in order, the last for every later request; "
                         "without it elicitation is refused like any unknown request)")
    ap.add_argument("--ext", action="append", default=[], metavar="METHOD=JSON",
                    help="an extension request, method as written (on the wire ACP extension "
                         "methods start with `_`, e.g. vibe's _rewind/preview); repeatable, in "
                         "order; see the placeholders above")
    ap.add_argument("--ext-after", type=int, metavar="N",
                    help="send the --ext requests once N prompts have answered (default: all of "
                         "the original session's)")
    ap.add_argument("--image", metavar="F", help="attach this image to the first prompt")
    ap.add_argument("--file", metavar="F", help="attach this file to the first prompt (a resource_link)")
    ap.add_argument("--mcp-servers", default="[]", metavar="JSON",
                    help="the session's mcpServers, as a JSON array")
    ap.add_argument("--idle-timeout", type=float, default=120.0, metavar="SECONDS")
    return ap


def main() -> int:
    argv = sys.argv[1:]
    ap = parser()
    if "--" not in argv:
        asked = bool({"-h", "--help"} & set(argv))
        ap.print_help(sys.stdout if asked else sys.stderr)
        return 0 if asked else 2
    split = argv.index("--")
    agent_cmd = argv[split + 1:]
    args = ap.parse_args(argv[:split])
    if not agent_cmd:
        sys.stderr.write("no agent command after --\n")
        return 2
    if args.fork and len(args.prompts) < 2:
        ap.error("--fork sends the last PROMPT to the fork, so it needs at least two prompts")
    if args.mode_option and not args.mode:
        ap.error("--mode-option needs --mode")

    try:
        client_caps = json.loads(args.client_capabilities)
        mcp_servers = json.loads(args.mcp_servers)
    except json.JSONDecodeError as e:
        ap.error(f"bad JSON: {e}")
    configs = []
    for item in args.config:
        key, sep, value = item.partition("=")
        if not sep or not key:
            ap.error(f"--config takes ID=VALUE, not {item!r}")
        configs.append((key, value))
    exts = []
    for item in args.ext:
        method, sep, params = item.partition("=")
        if not sep or not method:
            ap.error(f"--ext takes METHOD=JSON, not {item!r}")
        exts.append((method, params))
    answers = [a.strip() for a in (args.answers or "").split(",") if a.strip()]
    for a in answers:
        if a not in ("allow", "deny", "cancel"):
            ap.error(f"--answers takes allow, deny and cancel, not {a!r}")
    elicits = []
    for item in args.elicit:
        if item in ("decline", "cancel"):
            elicits.append({"action": item})
            continue
        try:
            content = json.loads(item)
        except json.JSONDecodeError as e:
            ap.error(f"--elicit takes a JSON object, decline or cancel: {e}")
        if not isinstance(content, dict):
            ap.error("--elicit content must be a JSON object")
        elicits.append({"action": "accept", "content": content})

    # vibe-acp 2.26.1 writes the model and thinking level it is given into
    # its config.toml (#164).
    vibe_ids = {"model", "thinking"}

    def vibe_warnings():
        if args.model:
            vibe_warning("--model")
        for key, _ in configs:
            if key in vibe_ids:
                vibe_warning(f"--config {key}")

    def vibe_warning(what):
        sys.stderr.write(f"[warning] {what} makes vibe-acp rewrite its config.toml (#164); "
                         "is VIBE_HOME a scratch copy?\n")

    looks_like_vibe = any("vibe" in os.path.basename(a) for a in agent_cmd)
    if looks_like_vibe:
        vibe_warnings()

    cwd = os.getcwd()
    # An agent started from inside another agent's session may behave differently.
    env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE")}
    proc = subprocess.Popen(agent_cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, env=env, text=True, bufsize=1)
    out = open(args.out, "w")
    lock = threading.Lock()
    send_lock = threading.Lock()

    def record(prefix, line):
        with lock:
            out.write(prefix + redact(line.rstrip("\n"), cwd) + "\n")
            out.flush()

    def send(obj):
        line = json.dumps(obj)
        # The --cancel-after timer sends from a thread of its own.
        with send_lock:
            if proc.stdin.closed:
                return
            record(">> ", line)
            try:
                proc.stdin.write(line + "\n")
                proc.stdin.flush()
            except (BrokenPipeError, ValueError):
                pass

    seq = 0
    pending = {}  # request id → what it was for

    def request(method, params, kind):
        nonlocal seq
        seq += 1
        pending[seq] = kind
        send({"jsonrpc": "2.0", "id": seq, "method": method, "params": params})

    def pump_stderr():
        for line in proc.stderr:
            record("!! ", line)
            sys.stderr.write("[stderr] " + line)

    threading.Thread(target=pump_stderr, daemon=True).start()

    request("initialize", {
        "protocolVersion": 1,
        "clientCapabilities": client_caps,
        "clientInfo": {"name": "unharness", "title": "unharness", "version": "0.2.0"},
    }, "initialize")
    status = "init"
    session_id = None
    config_options = []
    prompts = list(args.prompts)
    fork_prompt = prompts.pop() if args.fork else None
    ext_after = args.ext_after if args.ext_after is not None else len(prompts)
    prompts_done = 0
    forked = closed = False
    first = True
    cancel_timer = None
    permissions = 0
    elicitations = 0
    message_ids = {"USER": [], "AGENT": []}

    def finish(new_status):
        nonlocal status
        status = new_status
        with send_lock:
            if not proc.stdin.closed:
                proc.stdin.close()

    def send_cancel(sid):
        sys.stderr.write(f"[cancel] session/cancel after {args.cancel_after}s\n")
        send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}})

    def next_prompt():
        nonlocal first, cancel_timer
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
        last_prompt = not prompts and (fork_prompt is None or forked)
        request("session/prompt", {"sessionId": session_id, "prompt": blocks}, "prompt")
        if last_prompt and args.cancel_after is not None:
            cancel_timer = threading.Timer(args.cancel_after, send_cancel, args=(session_id,))
            cancel_timer.daemon = True
            cancel_timer.start()

    def set_mode():
        """The session exists: send the mode. Its answer is waited for."""
        if args.mode_option:
            option = next((o for o in config_options if o.get("category") == "mode"), None)
            request("session/set_config_option",
                    {"sessionId": session_id, "configId": option["id"] if option else "mode",
                     "value": args.mode}, "mode")
        else:
            request("session/set_mode", {"sessionId": session_id, "modeId": args.mode}, "mode")

    def setup():
        """The mode is set: send the model and options, unanswered."""
        option = next((o for o in config_options if o.get("category") == "model"), None)
        if args.model and option:
            request("session/set_config_option",
                    {"sessionId": session_id, "configId": option["id"], "value": args.model}, "setup")
        for key, value in configs:
            option = next((o for o in config_options if o.get("id") == key), None) or {}
            if looks_like_vibe and key not in vibe_ids \
                    and option.get("category") in ("model", "thought_level", "thinking"):
                vibe_warning(f"--config {key}")
            params = {"sessionId": session_id, "configId": key}
            if option.get("type") == "boolean":
                params["type"] = "boolean"
                params["value"] = value.lower() in ("true", "1", "yes", "on")
            else:
                params["value"] = value
            request("session/set_config_option", params, "setup")

    def placeholder(m):
        name = m.group(1)
        if name == "SESSION":
            if session_id is None:
                raise KeyError(name)
            return session_id
        kind, _, n = name.partition("_MESSAGE")
        ids = message_ids[kind]
        index = int(n[1:]) - 1 if n else len(ids) - 1
        if not 0 <= index < len(ids):
            raise KeyError(name)
        return ids[index]

    def advance():
        """Send whatever comes next, or end the run."""
        if exts and prompts_done >= ext_after:
            method, raw = exts.pop(0)
            try:
                params = json.loads(PLACEHOLDER.sub(placeholder, raw))
            except KeyError as e:
                sys.stderr.write(f"[error] --ext {method}: no value for ${e.args[0]} yet\n")
                return finish("error")
            except json.JSONDecodeError as e:
                sys.stderr.write(f"[error] --ext {method}: bad JSON: {e}\n")
                return finish("error")
            return request(method, params, "ext")
        if prompts:
            return next_prompt()
        if fork_prompt is not None and not forked:
            return request("session/fork",
                           {"sessionId": session_id, "cwd": cwd, "mcpServers": mcp_servers}, "fork")
        if args.close and not closed:
            return request("session/close", {"sessionId": session_id}, "close")
        if exts:
            sys.stderr.write(f"[warning] --ext-after {ext_after}, but only {prompts_done} prompts "
                             f"answered: {len(exts)} --ext requests not sent\n")
        finish("done")

    def stdout_lines():
        """The agent's stdout lines until EOF, also those it wrote just before
        exiting; None once nothing came for --idle-timeout seconds."""
        fd = proc.stdout.fileno()
        buf = b""
        last = time.time()
        while True:
            if not select.select([fd], [], [], 0.5)[0]:
                if time.time() - last > args.idle_timeout:
                    yield None
                    last = time.time()
                continue
            chunk = os.read(fd, 65536)
            if not chunk:
                if buf:
                    yield buf.decode("utf-8", "replace")
                return
            last = time.time()
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                yield line.decode("utf-8", "replace")

    for line in stdout_lines():
        if line is None:
            sys.stderr.write("idle timeout\n")
            break
        record("", line)
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = obj.get("method")
        rid = obj.get("id")

        if method == "session/update":
            update = (obj.get("params") or {}).get("update") or {}
            kind = {"user_message_chunk": "USER", "agent_message_chunk": "AGENT"}.get(update.get("sessionUpdate"))
            mid = update.get("messageId")
            if kind and isinstance(mid, str) and mid not in message_ids[kind]:
                message_ids[kind].append(mid)

        # Agent → client requests.
        if method and rid is not None:
            if method == "session/request_permission":
                options = (obj.get("params") or {}).get("options") or []
                permissions += 1
                if permissions <= len(answers):
                    answer = answers[permissions - 1]
                else:
                    answer = "deny" if args.deny else "allow"
                pick = None
                if answer != "cancel":
                    want = "reject_once" if answer == "deny" else "allow_once"
                    pick = next((o for o in options if o.get("kind") == want), None)
                    if pick is None:
                        sys.stderr.write(f"[warning] the permission request offers no {want}: cancelled\n")
                if pick:
                    send({"jsonrpc": "2.0", "id": rid,
                          "result": {"outcome": {"outcome": "selected", "optionId": pick.get("optionId")}}})
                else:
                    send({"jsonrpc": "2.0", "id": rid, "result": {"outcome": {"outcome": "cancelled"}}})
            elif method == "elicitation/create" and elicits:
                answer = elicits[min(elicitations, len(elicits) - 1)]
                elicitations += 1
                send({"jsonrpc": "2.0", "id": rid, "result": answer})
            else:
                sys.stderr.write(f"[unhandled agent request] {method}\n")
                send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unsupported by unharness"}})
            continue

        # Responses to our requests.
        if rid is not None and method is None:
            kind = pending.pop(rid, None)
            result = obj.get("result") or {}
            if "error" in obj:
                sys.stderr.write(f"[error] {kind}: {json.dumps(obj['error'])[:300]}\n")
                if kind == "ext":
                    advance()
                elif kind == "close":
                    closed = True
                    advance()
                else:
                    finish("error")
            elif kind == "initialize":
                name = str((result.get("agentInfo") or {}).get("name", ""))
                if not looks_like_vibe and "vibe" in name.lower():
                    looks_like_vibe = True
                    vibe_warnings()
                if args.resume or args.load:
                    session_id = args.resume or args.load
                    request("session/resume" if args.resume else "session/load",
                            {"sessionId": session_id, "cwd": cwd, "mcpServers": mcp_servers}, "session")
                else:
                    request("session/new", {"cwd": cwd, "mcpServers": mcp_servers}, "session")
                status = "session"
            elif kind == "session":
                session_id = result.get("sessionId") or session_id
                config_options = result.get("configOptions") or []
                status = "prompt"
                if args.mode:
                    set_mode()
                else:
                    setup()
                    advance()
            elif kind == "mode":
                config_options = result.get("configOptions") or config_options
                setup()
                advance()
            elif kind == "prompt":
                prompts_done += 1
                if cancel_timer:
                    cancel_timer.cancel()
                    cancel_timer = None
                advance()
            elif kind == "ext":
                advance()
            elif kind == "fork":
                session_id = result.get("sessionId") or session_id
                config_options = result.get("configOptions") or config_options
                forked = True
                prompts.append(fork_prompt)
                advance()
            elif kind == "close":
                closed = True
                advance()
            if status in ("done", "error"):
                break

    if cancel_timer:
        cancel_timer.cancel()
    finish(status)
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    record("# ", f"exit={proc.returncode} status={status}")
    out.close()
    print(f"wrote {args.out} (exit={proc.returncode}, status={status}, session={session_id})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
