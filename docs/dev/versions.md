# Harness versions: status notes

Harness versions (`Harness::min_version`, `doctor`): only pi declares
one, 0.78.1, read in its changelog (`--session-id` came in 0.76.0, the
gate's `ctx.mode` in 0.78.1); recordings start at 0.87.1. pi before
0.74 is the deprecated `@mariozechner` package: 0.73.1 refused
`--session-id` ("Error: Unknown option: --session-id", exit 1,
`fixtures/too_old.jsonl`) and, with stdin not a terminal, printed
`--version` on stderr and nothing on stdout (run, with its agent
directory in scratch), which is why `probe_version` takes stderr when
stdout is empty and stderr is a bare version. Only that refused flag
gets the upgrade hint; another says nothing about the version. `pi
update --self` moved it to `@earendil-works` 1.0.4 (reported) and is
still accepted on 1.1.0. A version doctor cannot read is a warning for
every harness but an ACP agent (`reports_version`), whose command may
be a launcher such as `npx`.
