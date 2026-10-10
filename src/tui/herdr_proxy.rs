//! A socket that stands in for herdr's for the processes unharness starts.
//!
//! herdr hands its pane's variables to what runs there, and an agent CLI's
//! own herdr integration (`herdr integration install claude`, pi's
//! extension) reports the pane's state with them. unharness reports for
//! its pane itself, and on herdr 0.8.2 one `pane.report_agent_session`
//! from `herdr:claude` made herdr ignore every later `pane.report_agent`
//! from unharness, so the CLIs get `HERDR_SOCKET_PATH` naming this socket
//! instead. A request that reports for a pane or gives one up is answered
//! here the way herdr answers it and goes no further; everything else goes
//! to herdr's socket, and its answers come back line by line.
//!
//! The socket is `herdr.sock` in a directory of its own (0700) under the
//! runtime directory, removed when the proxy is dropped. A process that
//! connects to herdr's socket by its default path gets past it: this keeps
//! integrations from reporting over unharness, it does not keep anything
//! from herdr.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::{JoinHandle, JoinSet};

/// The methods by which a client tells herdr what agent is in a pane, or
/// takes that back. herdr answers each with `{"type": "ok"}`, and none of
/// the integrations acts on what the answer says (all 17 of herdr 0.8.2).
const AGENT_METHODS: &[&str] = &[
    "pane.report_agent",
    "pane.report_agent_session",
    "pane.release_agent",
    "pane.clear_agent_authority",
    "pane.report_metadata",
];

/// The longest line a client may send; a longer one ends the connection.
const MAX_LINE: usize = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Answered here: the answer, and what was kept from herdr, for the log.
    Answer {
        reply: String,
        what: String,
    },
    Forward,
}

/// What to do with one line from a client. A report is kept from herdr
/// whatever pane it names: only what unharness started reaches this
/// socket, an integration reports for the pane in its environment, which
/// is unharness's, and herdr may know that pane by another id after a
/// `pane move`.
fn classify(line: &[u8]) -> Verdict {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return Verdict::Forward;
    };
    let Some(method) = v.get("method").and_then(Value::as_str) else {
        return Verdict::Forward;
    };
    if !AGENT_METHODS.contains(&method) {
        return Verdict::Forward;
    }
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let source = v
        .pointer("/params/source")
        .and_then(Value::as_str)
        .unwrap_or("no source");
    Verdict::Answer {
        reply: format!("{}\n", json!({"id": id, "result": {"type": "ok"}})),
        what: format!("kept from herdr: {method} from {source}"),
    }
}

/// Writes each distinct line once to `herdr.log`: a CLI reports on every
/// turn.
struct Log {
    path: Option<PathBuf>,
    seen: Mutex<HashSet<String>>,
}

impl Log {
    fn once(&self, what: &str) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        if !seen.insert(what.to_string()) {
            return;
        }
        if let Some(path) = &self.path {
            super::herdr::log_line(path, what);
        }
    }
}

/// The socket and the task that serves it. Dropping it closes every
/// connection and removes the socket.
pub struct Proxy {
    path: PathBuf,
    task: JoinHandle<()>,
    dir: PathBuf,
}

impl Proxy {
    /// Listen in a new directory under the runtime directory (the
    /// temporary one where there is none) and pass on to `upstream`,
    /// herdr's socket. Must be called on the runtime.
    pub fn start(upstream: PathBuf, log: Option<PathBuf>) -> std::io::Result<Proxy> {
        let parent = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
        Proxy::start_in(&parent, upstream, log)
    }

    fn start_in(parent: &Path, upstream: PathBuf, log: Option<PathBuf>) -> std::io::Result<Proxy> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        // Short: a socket's path must fit in `sun_path` (104 bytes on
        // macOS, whose temporary directory is long already).
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = parent.join(format!("unharness-herdr-{}", &id[..12]));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let path = dir.join("herdr.sock");
        let bound = UnixListener::bind(&path).and_then(|listener| {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(listener)
        });
        let listener = match bound {
            Ok(listener) => listener,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let log = Arc::new(Log {
            path: log,
            seen: Mutex::new(HashSet::new()),
        });
        let task = tokio::spawn(accept(listener, Arc::new(upstream), log));
        Ok(Proxy { path, task, dir })
    }

    /// The socket to hand to the processes unharness starts.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // Dropping the task's `JoinSet` aborts every connection with it.
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn accept(listener: UnixListener, upstream: Arc<PathBuf>, log: Arc<Log>) {
    let mut connections = JoinSet::new();
    while let Ok((client, _)) = listener.accept().await {
        while connections.try_join_next().is_some() {}
        connections.spawn(serve(client, upstream.clone(), log.clone()));
    }
}

/// One client: its lines in order, each answered here or written to
/// herdr's socket, which is connected at the first line it gets.
async fn serve(client: UnixStream, upstream: Arc<PathBuf>, log: Arc<Log>) {
    let (read, write) = client.into_split();
    // Answers given here and lines from herdr go through one writer, a
    // whole line at a time.
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let mut read = BufReader::new(read);
    let mut herdr: Option<OwnedWriteHalf> = None;
    // Its answers on their way back; dropped with the connection's task.
    let mut copying = JoinSet::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = match (&mut read)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 || line.len() > MAX_LINE {
            break;
        }
        match classify(&line) {
            Verdict::Answer { reply, what } => {
                log.once(&what);
                if write
                    .lock()
                    .await
                    .write_all(reply.as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Verdict::Forward => {
                if herdr.is_none() {
                    match UnixStream::connect(&*upstream).await {
                        Ok(s) => {
                            let (from, to) = s.into_split();
                            copying.spawn(copy_back(from, write.clone()));
                            herdr = Some(to);
                        }
                        Err(e) => {
                            log.once(&format!("proxy: connect {}: {e}", upstream.display()));
                            break;
                        }
                    }
                }
                let to = herdr.as_mut().expect("connected above");
                if to.write_all(&line).await.is_err() {
                    break;
                }
            }
        }
    }
    // The client is done sending: so is herdr's side, and what herdr still
    // answers is passed on until it closes.
    if let Some(mut to) = herdr {
        let _ = to.shutdown().await;
    }
    while copying.join_next().await.is_some() {}
}

/// herdr's lines to the client until either closes.
async fn copy_back(from: OwnedReadHalf, to: Arc<tokio::sync::Mutex<OwnedWriteHalf>>) {
    let mut from = BufReader::new(from);
    let mut line = Vec::new();
    loop {
        line.clear();
        match from.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if to.lock().await.write_all(&line).await.is_err() {
            return;
        }
    }
    let _ = to.lock().await.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    fn agent_reports_are_answered_here_with_their_id() {
        for method in AGENT_METHODS {
            for (id, want) in [
                (json!("cli:1"), json!("cli:1")),
                (json!(7), json!(7)),
                (Value::Null, Value::Null),
            ] {
                let mut req = json!({
                    "method": method,
                    "params": {"pane_id": "w1:p2", "source": "herdr:claude"},
                });
                if !id.is_null() {
                    req["id"] = id;
                }
                let Verdict::Answer { reply, what } = classify(format!("{req}\n").as_bytes())
                else {
                    panic!("{method} was forwarded");
                };
                assert!(reply.ends_with('\n'));
                let reply: Value = serde_json::from_str(&reply).unwrap();
                assert_eq!(reply, json!({"id": want, "result": {"type": "ok"}}));
                assert_eq!(what, format!("kept from herdr: {method} from herdr:claude"));
            }
        }
    }

    #[test]
    fn everything_else_goes_to_herdr() {
        for line in [
            &b"{\"id\":\"cli:pane:current\",\"method\":\"pane.current\",\"params\":{}}\n"[..],
            b"{\"id\":1,\"method\":\"events.subscribe\"}\n",
            b"{\"id\":1}\n",
            b"not json\n",
            b"{\"method\":\"pane.report_agent\"",
        ] {
            assert_eq!(
                classify(line),
                Verdict::Forward,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    /// A stand-in for herdr's socket: answers a line with `answers` lines
    /// of its own, and keeps what it was sent.
    async fn fake_herdr(path: &Path, answers: usize) -> mpsc::UnboundedReceiver<Value> {
        let listener = UnixListener::bind(path).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut s = BufReader::new(s);
                    let mut line = String::new();
                    while s.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        line.clear();
                        for i in 0..answers {
                            let answer = json!({"id": req["id"], "result": {"n": i}});
                            let _ = s
                                .get_mut()
                                .write_all(format!("{answer}\n").as_bytes())
                                .await;
                        }
                        let _ = tx.send(req);
                    }
                });
            }
        });
        rx
    }

    async fn ask(socket: &Path, request: Value, answers: usize) -> Vec<Value> {
        let s = UnixStream::connect(socket).await.unwrap();
        let mut s = BufReader::new(s);
        s.get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut got = Vec::new();
        for _ in 0..answers {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), s.read_line(&mut line))
                .await
                .expect("an answer")
                .unwrap();
            got.push(serde_json::from_str(&line).unwrap());
        }
        got
    }

    #[tokio::test]
    async fn a_vendor_report_is_answered_and_a_query_passed_on() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr_sock = tmp.path().join("herdr.sock");
        let mut seen = fake_herdr(&herdr_sock, 1).await;
        let log = tmp.path().join("herdr.log");
        let proxy = Proxy::start_in(tmp.path(), herdr_sock, Some(log.clone())).unwrap();

        let report = json!({
            "id": "herdr:claude:1",
            "method": "pane.report_agent_session",
            "params": {"pane_id": "w1:p2", "source": "herdr:claude", "agent": "claude"},
        });
        let got = ask(proxy.path(), report.clone(), 1).await;
        assert_eq!(
            got,
            [json!({"id": "herdr:claude:1", "result": {"type": "ok"}})]
        );
        // Twice, logged once.
        ask(proxy.path(), report, 1).await;

        let query = json!({"id": "cli:pane:current", "method": "pane.current", "params": {}});
        let got = ask(proxy.path(), query.clone(), 1).await;
        assert_eq!(got, [json!({"id": "cli:pane:current", "result": {"n": 0}})]);
        assert_eq!(
            seen.recv().await.unwrap(),
            query,
            "only the query reached herdr"
        );
        assert!(seen.try_recv().is_err());

        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains("kept from herdr: pane.report_agent_session from herdr:claude"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn every_line_of_a_streamed_answer_comes_through() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr_sock = tmp.path().join("herdr.sock");
        let _seen = fake_herdr(&herdr_sock, 3).await;
        let proxy = Proxy::start_in(tmp.path(), herdr_sock, None).unwrap();
        let got = ask(
            proxy.path(),
            json!({"id": 1, "method": "events.subscribe"}),
            3,
        )
        .await;
        let ns: Vec<_> = got.iter().map(|v| v["result"]["n"].clone()).collect();
        assert_eq!(ns, [json!(0), json!(1), json!(2)]);
    }

    #[tokio::test]
    async fn without_herdr_the_client_is_closed_and_the_failure_logged() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("herdr.log");
        let proxy = Proxy::start_in(
            tmp.path(),
            tmp.path().join("absent.sock"),
            Some(log.clone()),
        )
        .unwrap();
        let mut s = UnixStream::connect(proxy.path()).await.unwrap();
        s.write_all(b"{\"id\":1,\"method\":\"pane.list\"}\n")
            .await
            .unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
            .await
            .expect("closed")
            .unwrap();
        assert!(rest.is_empty());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("proxy: connect"), "{text}");
    }

    #[tokio::test]
    async fn an_overlong_line_ends_the_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr_sock = tmp.path().join("herdr.sock");
        let mut seen = fake_herdr(&herdr_sock, 1).await;
        let proxy = Proxy::start_in(tmp.path(), herdr_sock, None).unwrap();
        let mut s = UnixStream::connect(proxy.path()).await.unwrap();
        let long = vec![b'x'; MAX_LINE + 10];
        // The proxy may close before all of it is written.
        let _ = s.write_all(&long).await;
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest))
            .await
            .expect("closed");
        assert!(rest.is_empty());
        assert!(seen.try_recv().is_err(), "nothing reached herdr");
    }

    #[tokio::test]
    async fn the_socket_is_the_user_s_alone_and_goes_with_the_proxy() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let proxy = Proxy::start_in(tmp.path(), tmp.path().join("absent.sock"), None).unwrap();
        let path = proxy.path().to_path_buf();
        let dir = path.parent().unwrap().to_path_buf();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        drop(proxy);
        assert!(!dir.exists());
    }
}
