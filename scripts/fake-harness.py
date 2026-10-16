#!/usr/bin/env python3
"""Replay a recorded fixture as if it were the vendor CLI.

Used by `tests/session_e2e.rs` to exercise the real session drivers without
network access or vendor binaries. The fixture format is the one produced by
the `scripts/record-*.py` recorders:

  >> {...}   a line the client is expected to SEND: the fake blocks here until
             it reads one line from stdin (content is logged, not checked).
             When both carry a `request_id`, later lines that name the
             recorded one name the client's instead, so that an answer
             reaches the request it was for.
  !! text    written to stderr
  # ...      ignored
  {...}      written to stdout

Every stdin line is appended to the file named by $UNHARNESS_FAKE_LOG as it
arrives, also while the replay waits at a `# gate`, so the test can assert on
what the driver sent, and on what it sent too early. All command-line arguments are
ignored (the driver passes vendor flags). The fixture path comes from
$UNHARNESS_FAKE_FIXTURE. Set $UNHARNESS_FAKE_EXIT_CODE to control the exit
status after the fixture is exhausted (default 0; a `# exit=N` line in the
fixture overrides it). Set $UNHARNESS_FAKE_HANG=1
to keep running after the fixture until stdin closes (long-lived protocols).

$UNHARNESS_FAKE_BACKGROUND=<mode>:<path> starts a `sleep` in the background
before the replay and writes "<own pid> <its pid>" to <path>: with mode
`session` it gets a session of its own (as Claude Code runs every command),
with `group` it stays in the fake's process group. With HANG,
$UNHARNESS_FAKE_LINGER=<seconds> waits that long once stdin closes, then
logs `{"ended": true}` and exits (a CLI finishing its own work first).

A `# gate` line in the fixture waits until the file named by
$UNHARNESS_FAKE_GATE exists, so that a test can hold the fake's next line
until the driver has seen something first.

$UNHARNESS_FAKE_ENV names variables (comma-separated) whose values the
fake logs as it starts, as `{"env": {name: value or null}}`, for a test of
what the driver put in the agent's environment. $UNHARNESS_FAKE_ARGV=1
logs its arguments after that, as `{"argv": [...]}`.

$UNHARNESS_FAKE_PROBE makes the fake try the filesystem before it replays,
for the sandbox tests: `write:<path>` and `read:<path>` entries separated by
`;`. Each result is appended to the log as `{"probe", "path", "ok"}`.
"""
import json
import os
import queue
import subprocess
import sys
import threading
import time


def probe(spec, log):
    for entry in filter(None, spec.split(";")):
        kind, _, path = entry.partition(":")
        try:
            if kind == "write":
                with open(path, "w") as f:
                    f.write("probe\n")
            else:
                with open(path) as f:
                    f.read()
            ok = True
        except OSError:
            ok = False
        log.write(json.dumps({"probe": kind, "path": path, "ok": ok}) + "\n")
        log.flush()


def main() -> int:
    fixture = os.environ.get("UNHARNESS_FAKE_FIXTURE")
    if not fixture:
        sys.stderr.write("UNHARNESS_FAKE_FIXTURE not set\n")
        return 2
    log_path = os.environ.get("UNHARNESS_FAKE_LOG")
    log = open(log_path, "a") if log_path else None
    hang = os.environ.get("UNHARNESS_FAKE_HANG") == "1"
    exit_code = int(os.environ.get("UNHARNESS_FAKE_EXIT_CODE", "0"))
    if log and os.environ.get("UNHARNESS_FAKE_ENV"):
        names = filter(None, os.environ["UNHARNESS_FAKE_ENV"].split(","))
        log.write(json.dumps({"env": {n: os.environ.get(n) for n in names}}) + "\n")
        log.flush()
    if log and os.environ.get("UNHARNESS_FAKE_ARGV") == "1":
        log.write(json.dumps({"argv": sys.argv[1:]}) + "\n")
        log.flush()
    if log and os.environ.get("UNHARNESS_FAKE_PROBE"):
        probe(os.environ["UNHARNESS_FAKE_PROBE"], log)
    background = os.environ.get("UNHARNESS_FAKE_BACKGROUND")
    if background:
        mode, _, path = background.partition(":")
        child = subprocess.Popen(
            ["sleep", "300"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=(mode == "session"),
        )
        with open(path, "w") as f:
            f.write(f"{os.getpid()} {child.pid}\n")

    # Read on a thread, and logged as it comes: a line sent while the
    # replay is held at a gate is seen then.
    stdin_lines = queue.Queue()

    def pump_stdin():
        for line in sys.stdin:
            if log:
                log.write(line if line.endswith("\n") else line + "\n")
                log.flush()
            stdin_lines.put(line)
        stdin_lines.put(None)

    threading.Thread(target=pump_stdin, daemon=True).start()

    def read_stdin_line():
        line = stdin_lines.get()
        if line is None:
            # Seen again by whatever reads next.
            stdin_lines.put(None)
        return line

    # Recorded request ids, and the ids the client sent in their place.
    request_ids = {}

    def request_id(text):
        try:
            value = json.loads(text)
        except ValueError:
            return None
        return value.get("request_id") if isinstance(value, dict) else None

    with open(fixture) as f:
        for raw in f:
            line = raw.rstrip("\n")
            for recorded, sent in request_ids.items():
                line = line.replace(f'"{recorded}"', f'"{sent}"')
            if line == "# gate":
                gate = os.environ.get("UNHARNESS_FAKE_GATE")
                while gate and not os.path.exists(gate):
                    time.sleep(0.02)
                continue
            if line.startswith("#"):
                # A recorder's trailing `# exit=N` sets our exit status too.
                if line.startswith("# exit=") and line[7:].strip().isdigit():
                    exit_code = int(line[7:].strip())
                continue
            if not line.strip():
                continue
            if line.startswith(">>"):
                got = read_stdin_line()
                if got is None:
                    break
                recorded, sent = request_id(line[2:]), request_id(got)
                if recorded and sent:
                    request_ids[recorded] = sent
                continue
            if line.startswith("!!"):
                sys.stderr.write(line[2:].lstrip() + "\n")
                sys.stderr.flush()
                continue
            sys.stdout.write(line + "\n")
            sys.stdout.flush()

    if hang:
        while read_stdin_line() is not None:
            pass
        linger = os.environ.get("UNHARNESS_FAKE_LINGER")
        if linger:
            time.sleep(float(linger))
            if log:
                log.write(json.dumps({"ended": True}) + "\n")
                log.flush()
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
