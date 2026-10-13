# herdr: status notes

herdr (`tui/herdr.rs`), checked on herdr 0.8.2 (socket protocol 20)
in a named test session with Claude Code 2.1.289 and agy 1.2.17: the
TUI sends `pane.report_agent` (`source` and `agent` both `unharness`,
`seq` in microseconds since the epoch) over `HERDR_SOCKET_PATH`, one
JSON line answered by one line, on a task of its own that sends only
the newest state; `pane.release_agent` on quit. `herdr agent get`
showed `working` on a prompt, `blocked` at a Claude permission modal
and at the policy picker held for agy under `ask`, `idle` after the
turn and `done` when the turn ended with another tab focused (herdr
derives `done`; a report is only `idle`, `working`, `blocked` or
`unknown`), and no agent once unharness quit. herdr's own detection
took the pane for `claude` (the child process) until the first report
arrived, about 1.5 s after launch, and the report wins after that.
Unverified: where herdr shows the `message` of a blocked report (no
API view returns it), whether the release or the exit cleared the
pane. A vendor integration (`herdr integration install claude`)
reports for the same pane under its own `source`, and one
`pane.report_agent_session` from `herdr:claude` made herdr ignore
every later `pane.report_agent` from unharness (stuck on `working`;
`pane.clear_agent_authority` did not undo it). It outlives the CLI: a
`--print` run's hook (Claude's integration, version 8, sends only
`report_agent_session`, `seq` in nanoseconds) held the pane for the
TUI started there next, and neither a release nor a clear from
`herdr:claude` with a later `seq`, nor a session report from
unharness, let go of it. So in the TUI and `--print` what unharness
starts gets every `HERDR_*` variable with `HERDR_SOCKET_PATH` naming
a proxy (`tui/herdr_proxy.rs`, a socket in
a 0700 directory under the runtime directory): `pane.report_agent`,
`pane.report_agent_session`, `pane.release_agent`,
`pane.clear_agent_authority` and `pane.report_metadata` are answered
there with `{"type": "ok"}` (what herdr answers; all 17 built-in
integrations of 0.8.2 send only the first two, one request per
connection, and act on no answer), for any pane id, and everything
else goes to herdr's socket (`Sandbox::wrap`, `HerdrEnv`). herdr
refuses an array, another spelling of a method and a duplicate key, so
a report has no other way through; an answer given in the proxy can
overtake one of herdr's on a connection with several requests. A
client that has closed its connection ends herdr's side (`POLLHUP`, so
that an idle subscription does not hold it), and a killed
unharness's proxy directory is removed by the next. Without the
pane id an agent's `herdr pane current --current` answered with the
UI-focused pane, and `pane get --current` with `pane_not_found` (#67);
hiding the variables was never a boundary, since herdr finds its
default socket by itself, and that still gets past the proxy (Landlock
is not asked to confine connecting to a socket: ABI 9's
`LANDLOCK_ACCESS_FS_RESOLVE_UNIX` is not handled, so herdr's, tmux's
or Docker's socket leads out of the sandbox). Under `--no-tui` and
`herdr = false` the variables pass as they are; where the proxy fails
to start they are removed. Checked in the TUI against an isolated herdr server
(scratch `XDG_CONFIG_HOME`, tmux, Landlock, Claude Code 2.1.296 with
its herdr hook installed, focus on another pane): the agent saw all six
variables, `herdr pane current --current` named its own pane, `herdr
pane split --current --no-focus` split its own pane, the hook's
`pane.report_agent_session` was kept from herdr (no `agent_session` on
the pane) and the pane went `working`, `idle` as unharness reported;
`/quit` released the pane and removed the proxy's directory. A
`--print` run went through the proxy, left the pane without an
`agent_session`, and a TUI started there next went `idle`, `working`,
`idle`; the directory a SIGKILLed unharness left was removed by the
next run. `herdr pane get` takes only a pane id (0.8.2): `--current`
there is `pane_not_found` in any pane. Checked before with a
prototype: layout, run, wait-output, agent list and wait through it.
Unverified: `events.subscribe` live (only against a stand-in), a
client that sends several requests on one connection. On macOS
`poll` asked for nothing never said `POLLHUP` (the test failed on
GitHub's runner): it registers a kqueue filter only for what is asked
for, so `hung_up` asks for `POLLOUT`, whose filter ends once the
socket cannot send, set by the peer's close and not by its
`shutdown(SHUT_WR)` (read in XNU's `poll_nocancel`, `filt_sowrite`,
`unp_disconnect`, `unp_shutdown`), and the test passed there with
it. Seen with the variables inherited,
before the proxy: pi's herdr extension reports
only when `ctx.mode` is `tui`, so it is silent in unharness's `rpc`
sessions; pi's A2A extension (`herdr-a2a`) fails with "client session
readiness timed out" because its broker answers `POST /v1/register`
with 403 `verification_failed` ("Herdr could not verify this pane")
until its 5 s deadline. The same 403 came back for `herdr-a2a
client-session` run unsandboxed from a pane herdr knows as `claude`,
so it is not the sandbox; that the broker wants herdr to know the pane
as the `pi` of that session is inferred from the binary's strings, not
read in its source.
