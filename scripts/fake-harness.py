#!/usr/bin/env python3
"""Replay a recorded fixture as if it were the vendor CLI.

Used by `tests/session_e2e.rs` to exercise the real session drivers without
network access or vendor binaries. The fixture format is the one produced by
the `scripts/record-*.py` recorders:

  >> {...}   a line the client is expected to SEND: the fake blocks here until
             it reads one line from stdin (content is logged, not checked)
  !! text    written to stderr
  # ...      ignored
  {...}      written to stdout

Every stdin line is appended to the file named by $UNHARNESS_FAKE_LOG so the
test can assert on what the driver sent. All command-line arguments are
ignored (the driver passes vendor flags). The fixture path comes from
$UNHARNESS_FAKE_FIXTURE. Set $UNHARNESS_FAKE_EXIT_CODE to control the exit
status after the fixture is exhausted (default 0; a `# exit=N` line in the
fixture overrides it). Set $UNHARNESS_FAKE_HANG=1
to keep running after the fixture until stdin closes (long-lived protocols).

$UNHARNESS_FAKE_PROBE makes the fake try the filesystem before it replays,
for the sandbox tests: `write:<path>` and `read:<path>` entries separated by
`;`. Each result is appended to the log as `{"probe", "path", "ok"}`.
"""
import json
import os
import sys


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
    if log and os.environ.get("UNHARNESS_FAKE_PROBE"):
        probe(os.environ["UNHARNESS_FAKE_PROBE"], log)

    def read_stdin_line():
        line = sys.stdin.readline()
        if not line:
            return None
        if log:
            log.write(line if line.endswith("\n") else line + "\n")
            log.flush()
        return line

    with open(fixture) as f:
        for raw in f:
            line = raw.rstrip("\n")
            if line.startswith("#"):
                # A recorder's trailing `# exit=N` sets our exit status too.
                if line.startswith("# exit=") and line[7:].strip().isdigit():
                    exit_code = int(line[7:].strip())
                continue
            if not line.strip():
                continue
            if line.startswith(">>"):
                if read_stdin_line() is None:
                    return exit_code
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
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
