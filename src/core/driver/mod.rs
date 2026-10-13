//! The loop that drives a session's CLI, so that a harness writes only its protocol.
//!
//! A long-lived harness (one process for the session, turns over its
//! stdin) implements [`StreamProtocol`]: what it sends first, what a
//! command becomes, what it makes of a line, and how its CLI is ended.
//! [`start`] spawns the CLI and runs the loop, which owns the rest: the
//! [`LineProcess`] and the session's [`ProcessSlot`], a restart the
//! protocol asks for, ending the CLI the way the protocol says (what it
//! is sent first, stdin closed, killed after `END_GRACE`, or killed at
//! once mid-turn), reading its output until it has exited, what happens
//! when the TUI drops the handle, and the answer to a command the harness
//! cannot carry out: one `Notice`, worded from its [`Capabilities`].
//!
//! A protocol over JSON-RPC (Codex app-server, ACP) sends its requests
//! through the loop ([`Io::request`]), which numbers and frames them and
//! routes each answer back to [`StreamProtocol::on_answer`] by its id, where
//! its line falls in the CLI's output. What a request was for, and which are
//! still open, stays the protocol's; the loop only routes.
//!
//! A harness whose CLI runs once per turn (Codex `exec`) implements
//! [`per_turn::PerTurnProtocol`] instead, the per-turn flavour.

pub mod per_turn;

use std::collections::HashSet;
use std::future::Future;

use anyhow::Result;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::caps::Capabilities;
use super::event::AgentEvent;
use super::ids::HarnessId;
use super::jsonrpc::{self, RequestId, RpcMessage};
use super::parser::Parser;
use super::process::{LineProcess, RawLine};
use super::sandbox::Sandbox;
use super::session::{
    ProcessModel, ProcessSlot, SessionCommand, SessionHandle, SessionInfo, shutdown_queued,
};

/// What a protocol acts through: the CLI's stdin, the TUI's events, and the
/// ids of its JSON-RPC requests.
pub struct Io<'a> {
    proc: &'a mut LineProcess,
    events: &'a mpsc::Sender<AgentEvent>,
    requests: &'a mut Requests,
}

/// The TUI dropped the session's handle while it was being told
/// something: the protocol stops there, and the loop ends the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gone;

impl<'a> Io<'a> {
    fn new(
        proc: &'a mut LineProcess,
        events: &'a mpsc::Sender<AgentEvent>,
        requests: &'a mut Requests,
    ) -> Self {
        Io {
            proc,
            events,
            requests,
        }
    }

    /// Write `line` to the CLI's stdin.
    pub async fn write(&mut self, line: &str) -> Result<()> {
        self.proc.write_line(line).await
    }

    /// Send the JSON-RPC request `method` (with `params`, none if `Null`)
    /// under the next number: the first a CLI is sent is 1. Its answer
    /// comes to [`StreamProtocol::on_answer`] under that id, which this
    /// returns with how the write went: the id is taken also when the line
    /// could not be written, and the next request is numbered after it.
    pub async fn request(&mut self, method: &str, params: Value) -> (u64, Result<()>) {
        let id = self.requests.next();
        let sent = self
            .proc
            .write_line(&jsonrpc::request(id, method, params))
            .await;
        (id, sent)
    }

    /// Send the JSON-RPC request `method` under the id `name` instead of a
    /// number, so that the numbered ones keep theirs (Codex's
    /// `skills/list`). Every answer under that name comes to
    /// [`StreamProtocol::on_answer`].
    pub async fn request_named(&mut self, name: &str, method: &str, params: Value) -> Result<()> {
        self.requests.names.insert(name.to_string());
        self.proc
            .write_line(&jsonrpc::request(name, method, params))
            .await
    }

    /// Tell the TUI `event`. If it has gone, nothing more: the next event
    /// [`forward`](Self::forward)ed finds that out.
    pub async fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event).await;
    }

    /// Pass on an event of the CLI's stream; `Err(Gone)` once the TUI has
    /// dropped the handle.
    pub async fn forward(&self, event: AgentEvent) -> Result<(), Gone> {
        self.events.send(event).await.map_err(|_| Gone)
    }
}

/// The answer to a JSON-RPC request the protocol sent through [`Io`].
#[derive(Debug, Clone, PartialEq)]
pub struct Answer<'a> {
    /// The id the request went under.
    pub id: RequestId,
    pub result: Option<Value>,
    pub error: Option<Value>,
    /// The line it came on, for the parser.
    pub line: &'a str,
}

/// The ids the protocol's JSON-RPC requests went under on this CLI, so
/// that their answers are routed back. Which are still open, and what each
/// was for, is the protocol's to know.
#[derive(Debug, Default)]
struct Requests {
    /// The number the last request took (none yet: 0).
    last: u64,
    /// The names requests went under ([`Io::request_named`]).
    names: HashSet<String>,
}

impl Requests {
    fn next(&mut self) -> u64 {
        self.last += 1;
        self.last
    }

    /// `line` as the answer to a request sent under an id of ours, if it is
    /// one. Any other line, a request of the CLI's under a number of ours
    /// included, is not; nor is a line before the first request, which is
    /// not looked at.
    fn answer<'l>(&self, line: &'l str) -> Option<Answer<'l>> {
        if self.last == 0 && self.names.is_empty() {
            return None;
        }
        let Some(RpcMessage::Response { id, result, error }) = RpcMessage::parse(line) else {
            return None;
        };
        let id = RequestId::of(&id)?;
        let ours = match &id {
            RequestId::Number(n) => (1..=self.last).contains(n),
            RequestId::Name(name) => self.names.contains(name),
        };
        ours.then_some(Answer {
            id,
            result,
            error,
            line,
        })
    }
}

/// What a command became, for the loop to carry out.
pub enum Encoded {
    /// This line goes to the CLI; a failure to write it is told as
    /// [`StreamProtocol::write_failed`] words it.
    Write(String),
    /// Nothing more: what the command needed, if anything, was done
    /// through [`Io`].
    Done,
    /// The harness cannot carry it out: the driver says so, as it does for
    /// a command its `Capabilities` do not offer.
    Unsupported,
    /// Kill the CLI now, with what it started (agy, which has no
    /// interrupt).
    Kill,
    /// Kill the CLI and start `command` in its place, in the session's
    /// sandbox, then take `then` (agy, whose model and policy are flags).
    /// [`StreamProtocol::restarted`] is told how that went. The new CLI is
    /// a new connection: its requests are numbered from 1 again, and no
    /// answer is routed for a request the old one was sent.
    Restart {
        command: Box<Command>,
        then: SessionCommand,
    },
}

/// How a session's CLI is ended, when it is shut down or its handle is
/// dropped behind a `Shutdown`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shutdown {
    /// Write these lines, then close stdin and give the CLI `END_GRACE` to
    /// exit by itself (`LineProcess::end`). Claude is sent `interrupt`,
    /// which stops its background tasks, also between turns.
    End { first: Vec<String> },
    /// Close stdin and give the CLI its grace between turns, kill it at
    /// once mid-turn (`LineProcess::end_or_kill`): a CLI that is sent
    /// nothing to stop the turn could go on with it where nobody sees it.
    EndOrKill { turn_open: bool },
}

/// What one long-lived harness does over its CLI's stdin and stdout; the
/// rest is the loop's ([`start`]).
pub trait StreamProtocol: Send + 'static {
    /// What is sent and told before the first command: the handshake.
    fn start(&mut self, io: &mut Io<'_>) -> impl Future<Output = ()> + Send;

    /// What a command becomes. `Shutdown` never comes here (the loop ends
    /// the CLI as [`shutdown`](Self::shutdown) says), nor a command the
    /// harness's `Capabilities` do not offer (the loop answers it).
    fn encode(
        &mut self,
        cmd: SessionCommand,
        io: &mut Io<'_>,
    ) -> impl Future<Output = Encoded> + Send;

    /// What the TUI is told when the line of an [`Encoded::Write`] could
    /// not be written.
    fn write_failed(&mut self, error: anyhow::Error) -> Vec<AgentEvent>;

    fn parser(&mut self) -> &mut dyn Parser;

    /// One line on the CLI's stdout: parsed, kept books on, and passed on
    /// with [`Io::forward`], stopping at `Err(Gone)`. The answer to a
    /// request of the protocol's goes to [`on_answer`](Self::on_answer)
    /// instead.
    fn on_stdout(
        &mut self,
        line: &str,
        io: &mut Io<'_>,
    ) -> impl Future<Output = Result<(), Gone>> + Send;

    /// The answer to a JSON-RPC request sent through [`Io::request`] or
    /// [`Io::request_named`], told when its line comes, among the others
    /// in the order they came: the CLI may answer requests in any order.
    /// The protocol knows by `answer.id` what the request was for, and
    /// whether it still waits for this (an id answered twice comes twice).
    /// Passed on as in [`on_stdout`](Self::on_stdout), where it goes
    /// unless the protocol takes it here.
    fn on_answer(
        &mut self,
        answer: Answer<'_>,
        io: &mut Io<'_>,
    ) -> impl Future<Output = Result<(), Gone>> + Send {
        async move { self.on_stdout(answer.line, io).await }
    }

    /// What the TUI is told of a line on the CLI's stderr.
    fn on_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
        self.parser().feed_stderr(line)
    }

    /// What the TUI is told when the CLI has exited, before
    /// `ProcessExited` (agy: the end of a turn it left open).
    fn on_exit(&mut self, _code: Option<i32>) -> Vec<AgentEvent> {
        Vec::new()
    }

    /// How the CLI is ended now.
    fn shutdown(&mut self) -> Shutdown;

    /// When the protocol wants [`on_deadline`](Self::on_deadline) called,
    /// if it does (Claude: how long it waits for `initialize`'s answer).
    /// Asked again after everything the loop does.
    fn deadline(&self) -> Option<Instant> {
        None
    }

    /// The [`deadline`](Self::deadline) has come. This must clear it or
    /// move it later: a deadline left in the past is due again at once, and
    /// the loop would call this over and over.
    fn on_deadline(&mut self, _io: &mut Io<'_>) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// How the restart of an [`Encoded::Restart`] went: the new CLI is
    /// running (and the loop goes on with `then`), or it could not be
    /// started (and `then` is dropped).
    fn restarted(&mut self, _result: Result<()>) -> Vec<AgentEvent> {
        Vec::new()
    }
}

/// Spawn `command` in `sandbox` and drive it with `protocol` for the
/// session whose handle this returns. `caps` is what `harness` declares.
pub fn start<P: StreamProtocol>(
    harness: HarnessId,
    caps: &Capabilities,
    command: Command,
    sandbox: &Sandbox,
    protocol: P,
) -> Result<SessionHandle> {
    let proc = LineProcess::spawn(command, sandbox)?;
    let (handle, events, cmds) = SessionHandle::channels(SessionInfo {
        harness,
        process_model: ProcessModel::LongLived,
    });
    let slot = handle.process_slot();
    slot.set(&proc);
    let session = Loop {
        harness,
        caps: caps.clone(),
        protocol,
        proc,
        sandbox: sandbox.clone(),
        slot,
        events,
        cmds,
        requests: Requests::default(),
        shutting_down: false,
    };
    tokio::spawn(session.run());
    Ok(handle)
}

/// The driver task's state.
struct Loop<P> {
    harness: HarnessId,
    /// What the harness declares it can do, which decides what is
    /// unsupported.
    caps: Capabilities,
    protocol: P,
    proc: LineProcess,
    /// The JSON-RPC requests `proc` was sent through [`Io`].
    requests: Requests,
    /// What a restarted CLI is confined by: the session's.
    sandbox: Sandbox,
    slot: ProcessSlot,
    events: mpsc::Sender<AgentEvent>,
    cmds: mpsc::Receiver<SessionCommand>,
    /// The CLI was asked to end: it is not killed when the handle goes.
    shutting_down: bool,
}

impl<P: StreamProtocol> Loop<P> {
    async fn run(mut self) {
        self.protocol
            .start(&mut Io::new(
                &mut self.proc,
                &self.events,
                &mut self.requests,
            ))
            .await;
        loop {
            let deadline = self.protocol.deadline();
            tokio::select! {
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    let mut io = Io::new(&mut self.proc, &self.events, &mut self.requests);
                    self.protocol.on_deadline(&mut io).await;
                }
                cmd = self.cmds.recv() => {
                    // The TUI dropped the handle.
                    let Some(cmd) = cmd else { break };
                    self.command(cmd).await;
                }
                raw = self.proc.lines.recv() => match raw {
                    Some(RawLine::Stdout(line)) => {
                        let answer = self.requests.answer(&line);
                        let mut io = Io::new(&mut self.proc, &self.events, &mut self.requests);
                        let told = match answer {
                            Some(answer) => self.protocol.on_answer(answer, &mut io).await,
                            None => self.protocol.on_stdout(&line, &mut io).await,
                        };
                        if told.is_err() {
                            self.gone().await;
                            return;
                        }
                    }
                    Some(RawLine::Stderr(line)) => {
                        for ev in self.protocol.on_stderr(&line) {
                            let _ = self.events.send(ev).await;
                        }
                    }
                    Some(RawLine::Exited(code)) => {
                        for ev in self.protocol.on_exit(code) {
                            let _ = self.events.send(ev).await;
                        }
                        let _ = self.events.send(AgentEvent::ProcessExited { code }).await;
                        break;
                    }
                    None => break,
                },
            }
        }
        if !self.shutting_down {
            self.proc.kill().await;
        }
    }

    /// Carry out `cmd`, and what it led to.
    async fn command(&mut self, cmd: SessionCommand) {
        if cmd == SessionCommand::Shutdown {
            self.shutting_down = true;
            end(&mut self.proc, self.protocol.shutdown()).await;
            return;
        }
        let mut next = Some(cmd);
        while let Some(cmd) = next.take() {
            let (what, offered) = asks(&cmd, &self.caps);
            if !offered {
                self.cannot(what).await;
                continue;
            }
            let mut io = Io::new(&mut self.proc, &self.events, &mut self.requests);
            let encoded = self.protocol.encode(cmd, &mut io).await;
            match encoded {
                Encoded::Write(line) => {
                    if let Err(e) = self.proc.write_line(&line).await {
                        for ev in self.protocol.write_failed(e) {
                            let _ = self.events.send(ev).await;
                        }
                    }
                }
                Encoded::Done => {}
                Encoded::Unsupported => self.cannot(what).await,
                Encoded::Kill => self.proc.kill().await,
                Encoded::Restart { command, then } => {
                    self.proc.kill().await;
                    let started = match LineProcess::spawn(*command, &self.sandbox) {
                        Ok(proc) => {
                            self.proc = proc;
                            self.slot.set(&self.proc);
                            self.requests = Requests::default();
                            next = Some(then);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    };
                    for ev in self.protocol.restarted(started) {
                        let _ = self.events.send(ev).await;
                    }
                }
            }
        }
    }

    /// Tell the TUI the harness cannot `what`.
    async fn cannot(&mut self, what: &str) {
        let notice = format!("{} cannot {what}", self.harness);
        let _ = self.events.send(AgentEvent::Notice(notice)).await;
    }

    /// The TUI dropped the handle while it was told something. It sends
    /// `Shutdown` and then drops the handle, so a `Shutdown` may be queued
    /// behind what was being told: the CLI is then ended as asked, and
    /// killed otherwise.
    async fn gone(&mut self) {
        if self.shutting_down {
            return;
        }
        if shutdown_queued(&mut self.cmds) {
            end(&mut self.proc, self.protocol.shutdown()).await;
        } else {
            self.proc.kill().await;
        }
    }
}

/// What `cmd` asks of the harness, as the notice that it cannot says it,
/// and whether its `Capabilities` offer that.
fn asks(cmd: &SessionCommand, caps: &Capabilities) -> (&'static str, bool) {
    match cmd {
        SessionCommand::Steer { .. } => ("steer a running turn", caps.steer),
        SessionCommand::Compact { .. } => ("compact its context on request", caps.compaction),
        SessionCommand::Rewind { .. } => ("rewind its session", caps.rewind.conversation),
        SessionCommand::Interrupt => ("interrupt a turn", caps.interrupt),
        SessionCommand::StopSubagent { .. } => ("stop a subagent", caps.subagents.stop),
        SessionCommand::RespondPermission { .. } => (
            "take an answer to a permission request",
            caps.interactive_permissions,
        ),
        SessionCommand::RemoteControl { .. } => ("go on remote control", caps.remote_control),
        // What every harness takes.
        SessionCommand::SendTurn { .. } => ("take a turn", true),
        SessionCommand::SetModel(_) => ("change its model", true),
        SessionCommand::SetEffort(_) => ("change its effort", true),
        SessionCommand::SetPolicy(_) => ("change its permission policy", true),
        SessionCommand::Shutdown => ("shut down", true),
    }
}

/// Wait until `proc` has exited, what is left of its output unread: once
/// it was ended or killed, it is gone (reaped, its pipes drained or
/// closed) when this returns.
async fn exited(proc: &mut LineProcess) -> Option<i32> {
    while let Some(line) = proc.lines.recv().await {
        if let RawLine::Exited(code) = line {
            return code;
        }
    }
    None
}

/// End `proc` as `how` says. `Exited` follows on its lines.
async fn end(proc: &mut LineProcess, how: Shutdown) {
    match how {
        Shutdown::End { first } => {
            for line in first {
                let _ = proc.write_line(&line).await;
            }
            proc.end();
        }
        Shutdown::EndOrKill { turn_open } => proc.end_or_kill(turn_open).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::StopReason;
    use crate::core::process::END_GRACE;
    use serde_json::json;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Every line is a `TextDelta`, `done` ends the turn, and a line on
    /// stderr is a `Notice`.
    struct Echo;

    impl Parser for Echo {
        fn feed(&mut self, line: &str) -> Vec<AgentEvent> {
            if line == "done" {
                vec![AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Done,
                }]
            } else {
                vec![AgentEvent::TextDelta(line.to_string())]
            }
        }

        fn feed_stderr(&mut self, line: &str) -> Vec<AgentEvent> {
            vec![AgentEvent::Notice(line.to_string())]
        }
    }

    /// A protocol of plain lines: `hello` is the handshake, a turn is its
    /// text, `SetModel` starts `restart` in the CLI's place and then sends
    /// `again`, and `Interrupt` kills the CLI.
    struct Toy {
        parser: Echo,
        turn_open: bool,
        /// Ended with `End` and these lines first, else with `EndOrKill`.
        end_with: Option<Vec<String>>,
        restart: Option<fn() -> Command>,
        /// `on_deadline` says so once, then.
        deadline: Option<Instant>,
    }

    impl Toy {
        fn new() -> Self {
            Toy {
                parser: Echo,
                turn_open: false,
                end_with: None,
                restart: None,
                deadline: None,
            }
        }
    }

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    impl StreamProtocol for Toy {
        async fn start(&mut self, io: &mut Io<'_>) {
            let _ = io.write("hello").await;
        }

        async fn encode(&mut self, cmd: SessionCommand, _io: &mut Io<'_>) -> Encoded {
            match cmd {
                SessionCommand::SendTurn { text, .. } => {
                    self.turn_open = true;
                    Encoded::Write(text)
                }
                SessionCommand::SetModel(_) => match self.restart {
                    Some(command) => Encoded::Restart {
                        command: Box::new(command()),
                        then: SessionCommand::turn("again"),
                    },
                    None => Encoded::Done,
                },
                SessionCommand::Interrupt => Encoded::Kill,
                SessionCommand::Compact { .. } => Encoded::Unsupported,
                _ => Encoded::Done,
            }
        }

        fn write_failed(&mut self, error: anyhow::Error) -> Vec<AgentEvent> {
            self.turn_open = false;
            vec![AgentEvent::Error(format!("toy: {error}"))]
        }

        fn parser(&mut self) -> &mut dyn Parser {
            &mut self.parser
        }

        async fn on_stdout(&mut self, line: &str, io: &mut Io<'_>) -> Result<(), Gone> {
            for ev in self.parser.feed(line) {
                if matches!(ev, AgentEvent::TurnCompleted { .. }) {
                    self.turn_open = false;
                }
                io.forward(ev).await?;
            }
            Ok(())
        }

        fn on_exit(&mut self, _code: Option<i32>) -> Vec<AgentEvent> {
            if std::mem::take(&mut self.turn_open) {
                vec![AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Interrupted,
                }]
            } else {
                Vec::new()
            }
        }

        fn shutdown(&mut self) -> Shutdown {
            match &self.end_with {
                Some(first) => Shutdown::End {
                    first: first.clone(),
                },
                None => Shutdown::EndOrKill {
                    turn_open: self.turn_open,
                },
            }
        }

        fn deadline(&self) -> Option<Instant> {
            self.deadline
        }

        async fn on_deadline(&mut self, io: &mut Io<'_>) {
            self.deadline = None;
            io.emit(AgentEvent::Notice("waited".into())).await;
        }

        fn restarted(&mut self, result: Result<()>) -> Vec<AgentEvent> {
            vec![AgentEvent::Notice(match result {
                Ok(()) => "restarted".into(),
                Err(e) => format!("restart failed: {e}"),
            })]
        }
    }

    /// A protocol of JSON-RPC requests. With `at_start` it sends `a` and
    /// `b` numbered and `c` under the name `n` first; a turn is a numbered
    /// request of its text, and `SetModel` restarts the CLI as `restart` and
    /// sends `again`. An answer is told as `<method> <- <result>` (`?` for
    /// one it does not wait for), and the one to `a` sends `d`; any other
    /// line is told as it is.
    struct Rpc {
        parser: Echo,
        /// The protocol's own book: what each open request is for.
        open: HashMap<RequestId, String>,
        at_start: bool,
        restart: Option<fn() -> Command>,
    }

    impl Rpc {
        fn new() -> Self {
            Rpc {
                parser: Echo,
                open: HashMap::new(),
                at_start: false,
                restart: None,
            }
        }

        /// A numbered request of `method`, told as an `Error` with its id
        /// if it could not be written.
        async fn send(&mut self, method: &str, io: &mut Io<'_>) {
            let (id, sent) = io.request(method, json!({"k": method})).await;
            self.open.insert(RequestId::Number(id), method.to_string());
            if let Err(e) = sent {
                io.emit(AgentEvent::Error(format!("{method} ({id}): {e}")))
                    .await;
            }
        }
    }

    impl StreamProtocol for Rpc {
        async fn start(&mut self, io: &mut Io<'_>) {
            if !self.at_start {
                return;
            }
            self.send("a", io).await;
            self.send("b", io).await;
            io.request_named("n", "c", Value::Null).await.unwrap();
            self.open.insert(RequestId::Name("n".into()), "c".into());
        }

        async fn encode(&mut self, cmd: SessionCommand, io: &mut Io<'_>) -> Encoded {
            match cmd {
                SessionCommand::SendTurn { text, .. } => {
                    self.send(&text, io).await;
                    Encoded::Done
                }
                SessionCommand::SetModel(_) => match self.restart {
                    Some(command) => Encoded::Restart {
                        command: Box::new(command()),
                        then: SessionCommand::turn("again"),
                    },
                    None => Encoded::Done,
                },
                _ => Encoded::Done,
            }
        }

        fn write_failed(&mut self, error: anyhow::Error) -> Vec<AgentEvent> {
            vec![AgentEvent::Error(error.to_string())]
        }

        fn parser(&mut self) -> &mut dyn Parser {
            &mut self.parser
        }

        async fn on_stdout(&mut self, line: &str, io: &mut Io<'_>) -> Result<(), Gone> {
            io.forward(AgentEvent::TextDelta(line.to_string())).await
        }

        async fn on_answer(&mut self, answer: Answer<'_>, io: &mut Io<'_>) -> Result<(), Gone> {
            let method = self.open.remove(&answer.id).unwrap_or_else(|| "?".into());
            let said = match (answer.error, answer.result) {
                (Some(e), _) => format!("error {}", e["message"]),
                (None, Some(r)) => r.to_string(),
                (None, None) => "nothing".into(),
            };
            io.forward(AgentEvent::TextDelta(format!("{method} <- {said}")))
                .await?;
            if method == "a" {
                self.send("d", io).await;
            }
            Ok(())
        }

        fn shutdown(&mut self) -> Shutdown {
            Shutdown::EndOrKill { turn_open: false }
        }

        // The old CLI's requests went with it.
        fn restarted(&mut self, _result: Result<()>) -> Vec<AgentEvent> {
            self.open.clear();
            Vec::new()
        }
    }

    /// Everything a command can need.
    fn every() -> Capabilities {
        let mut caps = Capabilities {
            steer: true,
            compaction: true,
            interrupt: true,
            interactive_permissions: true,
            remote_control: true,
            ..Default::default()
        };
        caps.rewind.conversation = true;
        caps.subagents.stop = true;
        caps
    }

    fn run<P: StreamProtocol>(script: &str, protocol: P) -> SessionHandle {
        let toy_harness = HarnessId::intern("toy");
        start(toy_harness, &every(), sh(script), &Sandbox::off(), protocol).unwrap()
    }

    async fn next(h: &mut SessionHandle) -> AgentEvent {
        tokio::time::timeout(Duration::from_secs(10), h.events.recv())
            .await
            .expect("no event")
            .expect("the driver is gone")
    }

    /// Events until `ProcessExited`, which is the last.
    async fn until_exited(h: &mut SessionHandle) -> Vec<AgentEvent> {
        let mut got = Vec::new();
        loop {
            let ev = next(h).await;
            let exited = matches!(ev, AgentEvent::ProcessExited { .. });
            got.push(ev);
            if exited {
                return got;
            }
        }
    }

    /// The pid the CLI printed first.
    async fn pid(h: &mut SessionHandle) -> u32 {
        match next(h).await {
            AgentEvent::TextDelta(pid) => pid.trim().parse().unwrap(),
            other => panic!("no pid: {other:?}"),
        }
    }

    /// Whether the process `pid` is gone within `within`.
    #[cfg(target_os = "linux")]
    async fn gone_within(pid: u32, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while crate::core::process::running(pid) {
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }

    fn opus() -> SessionCommand {
        SessionCommand::SetModel(crate::core::ModelRef::new(
            HarnessId::CLAUDE,
            "anthropic",
            "opus",
        ))
    }

    /// Prints its pid, echoes its input and ends on EOF; `hello` is the
    /// handshake.
    const ECHO: &str = "echo $$; while read l; do echo \"$l\"; done";
    /// Prints its pid and never ends by itself: deaf to its input closing
    /// and to SIGTERM.
    const DEAF: &str = "echo $$; trap '' TERM; while :; do sleep 1; done";

    #[tokio::test]
    async fn a_turn_goes_out_and_its_lines_come_back_until_the_cli_has_exited() {
        let mut h = run(&format!("{ECHO}; echo oops >&2"), Toy::new());
        pid(&mut h).await;
        assert_eq!(next(&mut h).await, AgentEvent::TextDelta("hello".into()));
        h.send(SessionCommand::turn("hi")).await.unwrap();
        assert_eq!(next(&mut h).await, AgentEvent::TextDelta("hi".into()));
        h.send(SessionCommand::turn("done")).await.unwrap();
        assert!(matches!(
            next(&mut h).await,
            AgentEvent::TurnCompleted {
                stop_reason: StopReason::Done
            }
        ));
        h.send(SessionCommand::Shutdown).await.unwrap();
        assert_eq!(
            until_exited(&mut h).await,
            [
                AgentEvent::Notice("oops".into()),
                AgentEvent::ProcessExited { code: Some(0) }
            ]
        );
    }

    #[tokio::test]
    async fn the_protocol_is_woken_at_its_deadline() {
        let toy = Toy {
            deadline: Some(Instant::now() + Duration::from_millis(50)),
            ..Toy::new()
        };
        let mut h = run("cat >/dev/null", toy);
        assert_eq!(next(&mut h).await, AgentEvent::Notice("waited".into()));
        h.send(SessionCommand::Shutdown).await.unwrap();
        assert_eq!(
            until_exited(&mut h).await,
            [AgentEvent::ProcessExited { code: Some(0) }]
        );
    }

    #[tokio::test]
    async fn a_line_that_cannot_be_written_is_told_as_the_protocol_words_it() {
        let mut h = run("echo $$; cat >/dev/null; sleep 0.3", Toy::new());
        pid(&mut h).await;
        h.send(SessionCommand::Shutdown).await.unwrap();
        h.send(SessionCommand::turn("late")).await.unwrap();
        assert_eq!(
            until_exited(&mut h).await,
            [
                AgentEvent::Error("toy: child stdin is closed".into()),
                AgentEvent::ProcessExited { code: Some(0) }
            ]
        );
    }

    // What it is sent first reaches it, and it finishes what it does once
    // its input closes: its output is read until it has exited.
    #[tokio::test]
    async fn an_ended_cli_is_sent_what_it_ends_with_and_given_its_grace() {
        let toy = Toy {
            end_with: Some(vec!["bye".into()]),
            ..Toy::new()
        };
        let mut h = run(&format!("{ECHO}; sleep 0.5; echo late"), toy);
        pid(&mut h).await;
        h.send(SessionCommand::Shutdown).await.unwrap();
        assert_eq!(
            until_exited(&mut h).await,
            [
                AgentEvent::TextDelta("hello".into()),
                AgentEvent::TextDelta("bye".into()),
                AgentEvent::TextDelta("late".into()),
                AgentEvent::ProcessExited { code: Some(0) }
            ]
        );
    }

    // Between turns it is given its grace, also once the handle is
    // dropped, and killed when that is up.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_ended_cli_is_killed_once_its_grace_is_up_also_when_the_handle_is_dropped() {
        let mut h = run(DEAF, Toy::new());
        let cli = pid(&mut h).await;
        h.send(SessionCommand::Shutdown).await.unwrap();
        drop(h);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(crate::core::process::running(cli), "cut short");
        assert!(gone_within(cli, END_GRACE + Duration::from_secs(2)).await);
    }

    // Given its grace mid-turn, it could go on with the turn unseen.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_cli_ended_mid_turn_is_killed_at_once() {
        let mut h = run(DEAF, Toy::new());
        let cli = pid(&mut h).await;
        h.send(SessionCommand::turn("go")).await.unwrap();
        h.send(SessionCommand::Shutdown).await.unwrap();
        assert!(gone_within(cli, END_GRACE / 2).await);
        assert_eq!(
            until_exited(&mut h).await,
            [
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Interrupted
                },
                AgentEvent::ProcessExited { code: None }
            ]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_handle_kills_the_cli() {
        let mut h = run(DEAF, Toy::new());
        let cli = pid(&mut h).await;
        drop(h);
        assert!(gone_within(cli, END_GRACE / 2).await);
    }

    // The driver is still telling the TUI something when the handle goes:
    // it finds the `Shutdown` queued behind it, and ends the CLI as that
    // says instead of killing it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_shutdown_queued_behind_an_event_ends_the_cli_as_asked() {
        let dir = tempfile::tempdir().unwrap();
        let got = dir.path().join("got");
        let capacity = crate::core::session::EVENT_CHANNEL_CAPACITY;
        let script = format!(
            "echo $$; i=0; while [ $i -lt {} ]; do echo x; i=$((i+1)); done; \
             cat > {}; trap '' TERM; while :; do sleep 1; done",
            capacity + 100,
            got.display()
        );
        let toy = Toy {
            end_with: Some(vec!["bye".into()]),
            ..Toy::new()
        };
        let mut h = run(&script, toy);
        let cli = pid(&mut h).await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.events.len() < capacity {
            assert!(Instant::now() < deadline, "the events never filled");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        h.send(SessionCommand::Shutdown).await.unwrap();
        drop(h);

        let deadline = Instant::now() + Duration::from_secs(5);
        while std::fs::read_to_string(&got).unwrap_or_default() != "hello\nbye\n" {
            assert!(Instant::now() < deadline, "not sent what it ends with");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(crate::core::process::running(cli), "killed, not ended");
        assert!(gone_within(cli, END_GRACE + Duration::from_secs(2)).await);
    }

    // The same, with no `Shutdown` behind the event: killed at once.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_handle_dropped_while_an_event_is_told_kills_the_cli() {
        let capacity = crate::core::session::EVENT_CHANNEL_CAPACITY;
        let script = format!(
            "echo $$; i=0; while [ $i -lt {} ]; do echo x; i=$((i+1)); done; \
             trap '' TERM; while :; do sleep 1; done",
            capacity + 100
        );
        let mut h = run(&script, Toy::new());
        let cli = pid(&mut h).await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while h.events.len() < capacity {
            assert!(Instant::now() < deadline, "the events never filled");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(h);
        assert!(gone_within(cli, END_GRACE / 2).await);
    }

    // Ended and let go, it writes far more than its pipe and the line
    // channel hold: its output is still read to the end, so it finishes
    // within its grace instead of blocking on a full pipe until killed.
    #[tokio::test]
    async fn an_ended_cli_is_read_to_the_end_after_the_handle_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let done = dir.path().join("done");
        let line = "0123456789".repeat(4);
        let script = format!(
            "cat >/dev/null; i=0; while [ $i -lt 20000 ]; do echo {line}; i=$((i+1)); done; \
             touch {}",
            done.display()
        );
        let h = run(&script, Toy::new());
        h.send(SessionCommand::Shutdown).await.unwrap();
        drop(h);
        // Killed when its grace is up, it would never write the file: the
        // wait can be long without weakening what it shows.
        let deadline = Instant::now() + END_GRACE + Duration::from_secs(10);
        while !done.exists() {
            assert!(
                Instant::now() < deadline,
                "cut short: its output was not read"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // The session's process is the new one: killed through the slot, it
    // goes, and the one it replaced went with the restart.
    #[tokio::test]
    async fn a_restarted_cli_is_named_in_the_slot_again() {
        let toy = Toy {
            restart: Some(|| sh(ECHO)),
            ..Toy::new()
        };
        let mut h = run(DEAF, toy);
        let _old = pid(&mut h).await;
        h.send(opus()).await.unwrap();
        assert_eq!(next(&mut h).await, AgentEvent::Notice("restarted".into()));
        let _new = pid(&mut h).await;
        // The new one is sent the command the restart was for.
        assert_eq!(next(&mut h).await, AgentEvent::TextDelta("again".into()));
        #[cfg(target_os = "linux")]
        assert!(gone_within(_old, Duration::from_secs(2)).await);
        h.process_slot().kill();
        assert_eq!(
            until_exited(&mut h).await,
            [
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Interrupted
                },
                AgentEvent::ProcessExited { code: None }
            ]
        );
        #[cfg(target_os = "linux")]
        assert!(gone_within(_new, Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn a_restart_that_fails_drops_its_command() {
        let toy = Toy {
            restart: Some(|| Command::new("/definitely/not/a/binary")),
            ..Toy::new()
        };
        let mut h = run(DEAF, toy);
        pid(&mut h).await;
        h.send(opus()).await.unwrap();
        let Some(AgentEvent::Notice(said)) = until_exited(&mut h).await.first().cloned() else {
            panic!("the failure was not told");
        };
        assert!(said.starts_with("restart failed: "), "{said}");
    }

    // What the harness does not offer never reaches its protocol, and
    // what the protocol refuses is said the same way: one notice each.
    #[tokio::test]
    async fn a_command_the_harness_cannot_carry_out_is_answered_with_one_notice() {
        let caps = Capabilities {
            compaction: true,
            ..Default::default()
        };
        let mut h = start(
            HarnessId::intern("toy"),
            &caps,
            sh(ECHO),
            &Sandbox::off(),
            Toy::new(),
        )
        .unwrap();
        pid(&mut h).await;
        assert_eq!(next(&mut h).await, AgentEvent::TextDelta("hello".into()));
        for cmd in [
            SessionCommand::Steer {
                text: "x".into(),
                attachments: Vec::new(),
            },
            SessionCommand::Rewind { anchor: "a".into() },
            SessionCommand::Interrupt,
            SessionCommand::StopSubagent { id: "s".into() },
            SessionCommand::RespondPermission {
                id: "r".into(),
                decision: crate::core::PermissionDecision::Deny {
                    reason: "no".into(),
                },
            },
            SessionCommand::RemoteControl {
                enabled: true,
                name: None,
            },
            SessionCommand::Compact { instructions: None },
            SessionCommand::turn("after"),
        ] {
            h.send(cmd).await.unwrap();
        }
        let mut said = Vec::new();
        loop {
            match next(&mut h).await {
                AgentEvent::Notice(n) => said.push(n),
                // Nothing went to the CLI before the turn: it echoes all.
                ev => {
                    assert_eq!(ev, AgentEvent::TextDelta("after".into()));
                    break;
                }
            }
        }
        assert_eq!(
            said,
            [
                "toy cannot steer a running turn",
                "toy cannot rewind its session",
                "toy cannot interrupt a turn",
                "toy cannot stop a subagent",
                "toy cannot take an answer to a permission request",
                "toy cannot go on remote control",
                "toy cannot compact its context on request",
            ]
        );
    }

    /// The line the CLI printed next, as JSON.
    async fn line(h: &mut SessionHandle) -> Value {
        match next(h).await {
            AgentEvent::TextDelta(line) => serde_json::from_str(&line).unwrap(),
            other => panic!("no line: {other:?}"),
        }
    }

    fn said(text: &str) -> AgentEvent {
        AgentEvent::TextDelta(text.into())
    }

    /// Reads three requests and prints them back, answers them out of
    /// order between lines that are no answers of ours, then reads one
    /// more and answers it.
    const ANSWERS_OUT_OF_ORDER: &str = r#"
        read -r a; read -r b; read -r n
        printf '%s\n%s\n%s\n' "$a" "$b" "$n"
        echo '{"jsonrpc":"2.0","id":"n","result":"third"}'
        echo '{"jsonrpc":"2.0","id":7,"result":"never asked"}'
        echo '{"jsonrpc":"2.0","id":1,"method":"ask"}'
        echo '{"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"no"}}'
        echo '{"jsonrpc":"2.0","id":1,"result":"first"}'
        read -r d; printf '%s\n' "$d"
        echo '{"jsonrpc":"2.0","id":3,"result":"fourth"}'
        echo '{"jsonrpc":"2.0","id":1,"result":"again"}'
        cat >/dev/null
    "#;

    // Each answer reaches the protocol under the id its request went with,
    // where its line falls among the others; what is not an answer to a
    // request of ours is a line like any other.
    #[tokio::test]
    async fn answers_are_routed_back_by_id_in_the_order_they_come() {
        let rpc = Rpc {
            at_start: true,
            ..Rpc::new()
        };
        let mut h = run(ANSWERS_OUT_OF_ORDER, rpc);
        // Numbered from 1, and the named one under its name.
        assert_eq!(
            line(&mut h).await,
            json!({"jsonrpc": "2.0", "id": 1, "method": "a", "params": {"k": "a"}})
        );
        assert_eq!(
            line(&mut h).await,
            json!({"jsonrpc": "2.0", "id": 2, "method": "b", "params": {"k": "b"}})
        );
        assert_eq!(
            line(&mut h).await,
            json!({"jsonrpc": "2.0", "id": "n", "method": "c"})
        );
        assert_eq!(next(&mut h).await, said(r#"c <- "third""#));
        // An id never given out, and a request of the CLI's under one that
        // was: neither is an answer.
        assert_eq!(
            line(&mut h).await,
            json!({"jsonrpc": "2.0", "id": 7, "result": "never asked"})
        );
        assert_eq!(
            line(&mut h).await,
            json!({"jsonrpc": "2.0", "id": 1, "method": "ask"})
        );
        assert_eq!(next(&mut h).await, said(r#"b <- error "no""#));
        assert_eq!(next(&mut h).await, said(r#"a <- "first""#));
        // Sent from that answer: numbered after the others.
        assert_eq!(
            line(&mut h).await,
            json!({"jsonrpc": "2.0", "id": 3, "method": "d", "params": {"k": "d"}})
        );
        assert_eq!(next(&mut h).await, said(r#"d <- "fourth""#));
        // Answered twice: the protocol knows it no longer waits for it.
        assert_eq!(next(&mut h).await, said(r#"? <- "again""#));
        h.send(SessionCommand::Shutdown).await.unwrap();
        assert_eq!(
            until_exited(&mut h).await,
            [AgentEvent::ProcessExited { code: Some(0) }]
        );
    }

    // A restarted CLI is a new connection: its requests are numbered from
    // 1 again, and an answer is routed for what it was sent.
    #[tokio::test]
    async fn a_restarted_cli_numbers_its_requests_from_one_again() {
        let rpc = Rpc {
            restart: Some(|| {
                sh(r#"read -r l; printf '%s\n' "$l"
                      echo '{"jsonrpc":"2.0","id":1,"result":"x"}'; cat >/dev/null"#)
            }),
            ..Rpc::new()
        };
        // Prints back what it reads, and answers nothing.
        let mut h = run(r#"while read -r l; do printf '%s\n' "$l"; done"#, rpc);
        h.send(SessionCommand::turn("one")).await.unwrap();
        h.send(SessionCommand::turn("two")).await.unwrap();
        assert_eq!(line(&mut h).await["id"], 1);
        assert_eq!(line(&mut h).await["id"], 2);
        h.send(opus()).await.unwrap();
        let again = line(&mut h).await;
        assert_eq!(
            (&again["id"], &again["method"]),
            (&json!(1), &json!("again"))
        );
        assert_eq!(next(&mut h).await, said(r#"again <- "x""#));
    }

    // Its line could not be written, and it took its number all the same.
    #[tokio::test]
    async fn a_request_that_cannot_be_written_takes_its_number() {
        let mut h = run("echo $$; cat >/dev/null; sleep 0.3", Rpc::new());
        pid(&mut h).await;
        h.send(SessionCommand::Shutdown).await.unwrap();
        h.send(SessionCommand::turn("late")).await.unwrap();
        h.send(SessionCommand::turn("later")).await.unwrap();
        assert_eq!(
            until_exited(&mut h).await,
            [
                AgentEvent::Error("late (1): child stdin is closed".into()),
                AgentEvent::Error("later (2): child stdin is closed".into()),
                AgentEvent::ProcessExited { code: Some(0) }
            ]
        );
    }

    #[tokio::test]
    async fn a_command_can_kill_the_cli() {
        let mut h = run(DEAF, Toy::new());
        pid(&mut h).await;
        h.send(SessionCommand::turn("go")).await.unwrap();
        h.send(SessionCommand::Interrupt).await.unwrap();
        assert_eq!(
            until_exited(&mut h).await,
            [
                AgentEvent::TurnCompleted {
                    stop_reason: StopReason::Interrupted
                },
                AgentEvent::ProcessExited { code: None }
            ]
        );
    }
}
