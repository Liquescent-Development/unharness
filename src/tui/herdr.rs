//! Tells herdr, the terminal multiplexer for coding agents, what the
//! session in its pane is doing.
//!
//! herdr hands every pane `HERDR_ENV=1`, `HERDR_SOCKET_PATH` and
//! `HERDR_PANE_ID`, and takes `pane.report_agent` requests on that socket:
//! one JSON object per line, answered with one line (`result` or `error`).
//! The state is `working`, `blocked` or `idle`; herdr shows an `idle` pane
//! whose tab was not looked at since the work ended as `done` by itself.
//! `pane.release_agent` takes the pane back when unharness quits.
//!
//! Reports go out on a task of their own, newest first: a state that is
//! replaced before it was sent is never sent. Nothing here waits on herdr
//! or fails the TUI; what went wrong is appended to `herdr.log` in the state
//! directory, once per kind of failure.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::core::{PermissionKind, Question};

/// What the reports are filed under, and the agent name herdr shows.
const SOURCE: &str = "unharness";
const AGENT: &str = "unharness";
/// How long one request may take before the connection is given up.
const TIMEOUT: Duration = Duration::from_millis(1000);
/// How long quitting waits for the release to go out.
const RELEASE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub state: State,
    /// What the user is asked, while blocked.
    pub message: Option<String>,
}

impl Report {
    /// The state of a session that is `busy` (a turn or a subagent running)
    /// and may be `waiting_on` the user. Waiting comes first: a turn held on
    /// a permission answer does nothing until it is given.
    pub fn of(busy: bool, waiting_on: Option<String>) -> Report {
        match waiting_on {
            Some(message) => Report {
                state: State::Blocked,
                message: Some(message),
            },
            None => Report {
                state: if busy { State::Working } else { State::Idle },
                message: None,
            },
        }
    }
}

/// What a harness request asks of the user, in a few words.
pub fn request_message(kind: &PermissionKind) -> String {
    match kind {
        PermissionKind::ToolUse { tool, .. } => format!("allow {tool}?"),
        PermissionKind::Question { questions } => questions
            .first()
            .map(question_label)
            .unwrap_or_else(|| "question".to_string()),
        PermissionKind::Confirm { title, .. }
        | PermissionKind::Select { title, .. }
        | PermissionKind::Input { title, .. } => title.clone(),
        PermissionKind::PlanApproval { .. } => "approve the plan?".to_string(),
    }
}

/// A question by its header, or its text when it has none.
pub fn question_label(q: &Question) -> String {
    if q.header.is_empty() {
        q.text.clone()
    } else {
        q.header.clone()
    }
}

/// The pane unharness runs in, from herdr's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub socket: PathBuf,
    pub id: String,
}

impl Pane {
    /// `None` outside herdr: all three variables must be set, as herdr's
    /// own integrations require.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Option<Pane> {
        if var("HERDR_ENV").as_deref() != Some("1") {
            return None;
        }
        let socket = var("HERDR_SOCKET_PATH").filter(|s| !s.is_empty())?;
        let id = var("HERDR_PANE_ID").filter(|s| !s.is_empty())?;
        Some(Pane {
            socket: PathBuf::from(socket),
            id,
        })
    }
}

#[cfg(unix)]
pub type Proxy = super::herdr_proxy::Proxy;
#[cfg(not(unix))]
pub type Proxy = ();

/// Start the proxy in front of `pane`'s socket and have the processes
/// unharness starts from now on use it, or, failing that, get none of
/// herdr's variables. For a run of unharness that is what runs in the
/// pane: a report a CLI's integration makes there outlives the run.
pub fn start_proxy(pane: &Pane) -> Option<Proxy> {
    use crate::core::sandbox::{HerdrEnv, set_herdr_env};
    #[cfg(unix)]
    match super::herdr_proxy::Proxy::start(pane.socket.clone(), default_log()) {
        Ok(proxy) => {
            set_herdr_env(HerdrEnv::Proxy(proxy.path().to_path_buf()));
            return Some(proxy);
        }
        Err(e) => {
            if let Some(log) = default_log() {
                log_line(&log, &format!("proxy: not started: {e}"));
            }
        }
    }
    let _ = pane;
    set_herdr_env(HerdrEnv::Strip);
    None
}

fn report_request(pane: &str, report: &Report, seq: u64) -> Value {
    json!({
        "id": format!("{SOURCE}:{seq}"),
        "method": "pane.report_agent",
        "params": {
            "pane_id": pane,
            "source": SOURCE,
            "agent": AGENT,
            "state": report.state.as_str(),
            "message": report.message,
            "seq": seq,
        },
    })
}

fn release_request(pane: &str, seq: u64) -> Value {
    json!({
        "id": format!("{SOURCE}:{seq}"),
        "method": "pane.release_agent",
        "params": {
            "pane_id": pane,
            "source": SOURCE,
            "agent": AGENT,
            "seq": seq,
        },
    })
}

/// What herdr answered: `Ok` for a `result`, its message for an `error`.
fn check_response(line: &str) -> Result<(), String> {
    let v: Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("unreadable answer: {e}"))?;
    if v.get("result").is_some() {
        return Ok(());
    }
    match v.get("error") {
        Some(e) => Err(format!(
            "{}: {}",
            e.get("code").and_then(Value::as_str).unwrap_or("error"),
            e.get("message").and_then(Value::as_str).unwrap_or("")
        )),
        None => Err(format!("unexpected answer: {}", line.trim())),
    }
}

/// herdr keeps the report with the highest `seq` per source. Microseconds
/// since the epoch keep a later unharness in the same pane above an
/// earlier one, as herdr's own integrations do.
#[derive(Debug, Default)]
struct Seq(AtomicU64);

impl Seq {
    fn next(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0);
        let mut prev = self.0.load(Ordering::Relaxed);
        loop {
            let next = now.max(prev + 1);
            match self
                .0
                .compare_exchange(prev, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return next,
                Err(p) => prev = p,
            }
        }
    }
}

/// Appends failures to a file, skipping one that repeats the last.
#[derive(Debug)]
struct Log {
    path: Option<PathBuf>,
    last: Option<String>,
}

impl Log {
    fn failed(&mut self, what: &str) {
        if self.last.as_deref() == Some(what) {
            return;
        }
        self.last = Some(what.to_string());
        if let Some(path) = &self.path {
            log_line(path, what);
        }
    }

    fn succeeded(&mut self) {
        self.last = None;
    }
}

/// Append `what` to the log at `path`, after the time in seconds since
/// the epoch.
pub(super) fn log_line(path: &Path, what: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = append(path, &format!("{secs} {what}\n"));
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// Where failures are written: beside unharness's other state.
pub fn default_log() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("unharness").join("herdr.log"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Message {
    Report(Report),
    Release,
}

/// Sends reports for one pane. Dropping it stops the task without a
/// release; `release` is the orderly end.
pub struct Reporter {
    tx: watch::Sender<Option<Message>>,
    last: Option<Report>,
    task: JoinHandle<()>,
}

impl Reporter {
    /// Start the task on the current runtime.
    pub fn start(pane: Pane, log: Option<PathBuf>) -> Reporter {
        let (tx, rx) = watch::channel(None);
        let task = tokio::spawn(run(
            pane,
            rx,
            Log {
                path: log,
                last: None,
            },
        ));
        Reporter {
            tx,
            last: None,
            task,
        }
    }

    /// Have `report` sent, unless it is what was sent last.
    pub fn update(&mut self, report: Report) {
        if self.last.as_ref() == Some(&report) {
            return;
        }
        self.last = Some(report.clone());
        self.tx.send_replace(Some(Message::Report(report)));
    }

    /// Give the pane back to herdr, waiting a moment at most.
    pub async fn release(mut self) {
        self.tx.send_replace(Some(Message::Release));
        // Dropping `self` afterwards aborts the task if it is still at it.
        let _ = tokio::time::timeout(RELEASE_TIMEOUT, &mut self.task).await;
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(pane: Pane, mut rx: watch::Receiver<Option<Message>>, mut log: Log) {
    let seq = Arc::new(Seq::default());
    let mut conn = Connection::new(pane.socket.clone());
    while rx.changed().await.is_ok() {
        let Some(message) = rx.borrow_and_update().clone() else {
            continue;
        };
        let request = match &message {
            Message::Report(r) => report_request(&pane.id, r, seq.next()),
            Message::Release => release_request(&pane.id, seq.next()),
        };
        match conn.send(&request).await {
            Ok(()) => log.succeeded(),
            Err(e) => log.failed(&e),
        }
        if message == Message::Release {
            return;
        }
    }
}

/// One connection to the socket, opened when needed and dropped after a
/// failure.
struct Connection {
    socket: PathBuf,
    #[cfg(unix)]
    stream: Option<tokio::io::BufReader<tokio::net::UnixStream>>,
}

impl Connection {
    fn new(socket: PathBuf) -> Self {
        Connection {
            socket,
            #[cfg(unix)]
            stream: None,
        }
    }

    /// Send one request and read its answer. A connection kept from an
    /// earlier request may have been closed by herdr since, so a failure on
    /// one is tried once more on a new one.
    async fn send(&mut self, request: &Value) -> Result<(), String> {
        let line = format!("{request}\n");
        let reused = self.has_stream();
        match tokio::time::timeout(TIMEOUT, self.exchange(&line)).await {
            Ok(Ok(answer)) => return check_response(&answer),
            Ok(Err(_)) | Err(_) if reused => self.drop_stream(),
            Ok(Err(e)) => {
                self.drop_stream();
                return Err(e);
            }
            Err(_) => {
                self.drop_stream();
                return Err(format!("no answer within {}ms", TIMEOUT.as_millis()));
            }
        }
        match tokio::time::timeout(TIMEOUT, self.exchange(&line)).await {
            Ok(Ok(answer)) => check_response(&answer),
            Ok(Err(e)) => {
                self.drop_stream();
                Err(e)
            }
            Err(_) => {
                self.drop_stream();
                Err(format!("no answer within {}ms", TIMEOUT.as_millis()))
            }
        }
    }

    #[cfg(unix)]
    fn has_stream(&self) -> bool {
        self.stream.is_some()
    }

    #[cfg(unix)]
    fn drop_stream(&mut self) {
        self.stream = None;
    }

    #[cfg(unix)]
    async fn exchange(&mut self, line: &str) -> Result<String, String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        if self.stream.is_none() {
            let s = tokio::net::UnixStream::connect(&self.socket)
                .await
                .map_err(|e| format!("connect {}: {e}", self.socket.display()))?;
            self.stream = Some(BufReader::new(s));
        }
        let stream = self.stream.as_mut().expect("connected above");
        stream
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write: {e}"))?;
        let mut answer = String::new();
        let n = stream
            .read_line(&mut answer)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("herdr closed the connection".to_string());
        }
        Ok(answer)
    }

    #[cfg(not(unix))]
    fn has_stream(&self) -> bool {
        false
    }

    #[cfg(not(unix))]
    fn drop_stream(&mut self) {}

    #[cfg(not(unix))]
    async fn exchange(&mut self, _line: &str) -> Result<String, String> {
        Err(format!(
            "{}: herdr's socket is not supported on this platform",
            self.socket.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ToolAction;
    use std::collections::HashMap;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn waiting_on_the_user_is_blocked_even_mid_turn() {
        let r = Report::of(true, Some("allow Bash?".into()));
        assert_eq!(r.state, State::Blocked);
        assert_eq!(r.message.as_deref(), Some("allow Bash?"));
        assert_eq!(
            Report::of(false, Some("x".into())).state,
            State::Blocked,
            "a policy picker holds a session that is not running"
        );
    }

    #[test]
    fn busy_is_working_and_otherwise_idle() {
        assert_eq!(Report::of(true, None).state, State::Working);
        assert_eq!(Report::of(false, None).state, State::Idle);
        assert_eq!(Report::of(true, None).message, None);
    }

    #[test]
    fn off_unless_herdr_set_up_the_pane() {
        assert_eq!(Pane::from_env(env(&[])), None);
        let full = [
            ("HERDR_ENV", "1"),
            ("HERDR_SOCKET_PATH", "/run/herdr.sock"),
            ("HERDR_PANE_ID", "w1:p2"),
        ];
        assert_eq!(
            Pane::from_env(env(&full)),
            Some(Pane {
                socket: "/run/herdr.sock".into(),
                id: "w1:p2".into()
            })
        );
        for missing in 0..full.len() {
            let mut vars = full.to_vec();
            vars.remove(missing);
            assert_eq!(
                Pane::from_env(env(&vars)),
                None,
                "without {}",
                full[missing].0
            );
        }
        let mut vars = full.to_vec();
        vars[0] = ("HERDR_ENV", "0");
        assert_eq!(Pane::from_env(env(&vars)), None);
        vars[0] = ("HERDR_ENV", "1");
        vars[2] = ("HERDR_PANE_ID", "");
        assert_eq!(Pane::from_env(env(&vars)), None);
    }

    #[test]
    fn requests_have_the_fields_herdr_requires() {
        let r = report_request("w1:p2", &Report::of(false, Some("allow Bash?".into())), 7);
        assert_eq!(r["method"], "pane.report_agent");
        assert_eq!(r["params"]["pane_id"], "w1:p2");
        assert_eq!(r["params"]["source"], "unharness");
        assert_eq!(r["params"]["agent"], "unharness");
        assert_eq!(r["params"]["state"], "blocked");
        assert_eq!(r["params"]["message"], "allow Bash?");
        assert_eq!(r["params"]["seq"], 7);
        let idle = report_request("w1:p2", &Report::of(false, None), 8);
        assert_eq!(idle["params"]["state"], "idle");
        assert!(idle["params"]["message"].is_null());

        let rel = release_request("w1:p2", 9);
        assert_eq!(rel["method"], "pane.release_agent");
        assert_eq!(rel["params"]["agent"], "unharness");
        assert_eq!(rel["params"]["seq"], 9);
    }

    #[test]
    fn answers_are_read_as_herdr_sends_them() {
        assert_eq!(
            check_response("{\"id\":\"x\",\"result\":{\"type\":\"ok\"}}\n"),
            Ok(())
        );
        assert_eq!(
            check_response(
                "{\"id\":\"x\",\"error\":{\"code\":\"not_found\",\"message\":\"pane not found\"}}"
            ),
            Err("not_found: pane not found".to_string())
        );
        assert!(check_response("garbage").is_err());
    }

    #[test]
    fn seq_only_goes_up() {
        let seq = Seq::default();
        let a = seq.next();
        let b = seq.next();
        assert!(b > a);
        seq.0.store(u64::MAX - 10, Ordering::Relaxed);
        assert_eq!(seq.next(), u64::MAX - 9, "ahead of the clock stays ahead");
    }

    #[test]
    fn request_messages_name_what_is_asked() {
        let tool = PermissionKind::ToolUse {
            tool: "Bash".into(),
            input: Value::Null,
            action: ToolAction::Opaque,
            description: None,
        };
        assert_eq!(request_message(&tool), "allow Bash?");
        let q = PermissionKind::Question {
            questions: vec![Question {
                id: "1".into(),
                header: String::new(),
                text: "Which one?".into(),
                options: vec![],
                allow_other: true,
                multi: false,
            }],
        };
        assert_eq!(request_message(&q), "Which one?");
        let c = PermissionKind::Confirm {
            title: "Proceed?".into(),
            message: None,
        };
        assert_eq!(request_message(&c), "Proceed?");
    }

    #[test]
    fn a_repeated_failure_is_logged_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/herdr.log");
        let mut log = Log {
            path: Some(path.clone()),
            last: None,
        };
        log.failed("connect: refused");
        log.failed("connect: refused");
        log.succeeded();
        log.failed("connect: refused");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().all(|l| l.ends_with(" connect: refused")));
    }

    /// A stand-in for herdr's socket: answers every line, and keeps what
    /// it was sent.
    #[cfg(unix)]
    async fn fake_herdr(
        path: &Path,
        answer: &'static str,
    ) -> tokio::sync::mpsc::UnboundedReceiver<Value> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::UnixListener::bind(path).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut s = BufReader::new(s);
                    let mut line = String::new();
                    while s.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let _ = tx.send(serde_json::from_str(&line).unwrap());
                        line.clear();
                        let _ = s.get_mut().write_all(answer.as_bytes()).await;
                    }
                });
            }
        });
        rx
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_go_out_in_order_and_end_with_a_release() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("herdr.sock");
        let mut seen = fake_herdr(&sock, "{\"id\":\"x\",\"result\":{\"type\":\"ok\"}}\n").await;
        let pane = Pane {
            socket: sock,
            id: "w1:p2".into(),
        };
        let log = tmp.path().join("herdr.log");
        let mut r = Reporter::start(pane, Some(log.clone()));

        r.update(Report::of(true, None));
        let first = seen.recv().await.unwrap();
        assert_eq!(first["params"]["state"], "working");
        // The same state again is not sent again.
        r.update(Report::of(true, None));
        r.update(Report::of(true, Some("allow Bash?".into())));
        let second = seen.recv().await.unwrap();
        assert_eq!(second["params"]["state"], "blocked");
        assert!(second["params"]["seq"].as_u64() > first["params"]["seq"].as_u64());
        r.release().await;
        let last = seen.recv().await.unwrap();
        assert_eq!(last["method"], "pane.release_agent");
        assert!(!log.exists(), "nothing failed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn without_herdr_failures_are_logged_and_nothing_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let pane = Pane {
            socket: tmp.path().join("absent.sock"),
            id: "w1:p2".into(),
        };
        let log = tmp.path().join("herdr.log");
        let mut r = Reporter::start(pane, Some(log.clone()));
        r.update(Report::of(true, None));
        r.update(Report::of(false, None));
        let started = std::time::Instant::now();
        r.release().await;
        assert!(started.elapsed() < RELEASE_TIMEOUT + Duration::from_millis(200));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("connect"), "{text}");
        assert_eq!(text.lines().count(), 1, "one line for one kind of failure");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_error_from_herdr_is_logged() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("herdr.sock");
        let mut seen = fake_herdr(
            &sock,
            "{\"id\":\"x\",\"error\":{\"code\":\"not_found\",\"message\":\"pane not found\"}}\n",
        )
        .await;
        let log = tmp.path().join("herdr.log");
        let mut r = Reporter::start(
            Pane {
                socket: sock,
                id: "w9:p9".into(),
            },
            Some(log.clone()),
        );
        r.update(Report::of(true, None));
        seen.recv().await.unwrap();
        r.release().await;
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("not_found: pane not found"), "{text}");
    }
}
