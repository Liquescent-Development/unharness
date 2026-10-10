//! A socket that stands in for herdr's for the processes unharness starts.
//!
//! herdr hands its pane's variables to what runs there, and an agent CLI's
//! own herdr integration (`herdr integration install claude`, pi's
//! extension) reports the pane's state with them. On herdr 0.8.2 one
//! `pane.report_agent_session` from `herdr:claude` made herdr ignore every
//! later `pane.report_agent` from unharness, in that process and in the
//! next one in the pane, and nothing unharness could send undid it. So
//! where unharness is what runs in the pane (the TUI, `--print`), the CLIs
//! get `HERDR_SOCKET_PATH` naming this socket instead. A request that
//! reports for a pane or gives one up is answered here the way herdr
//! answers it and goes no further; everything else goes to herdr's socket,
//! and its answers come back line by line. herdr takes one JSON object per
//! line and refuses an array, a method spelled otherwise and a duplicate
//! key, so there is no other way to send a report through.
//!
//! An answer given here may overtake one of herdr's on a connection that
//! carries several requests; every client seen sends one per connection.
//!
//! The socket is `herdr.sock` in a directory of its own (0700) under the
//! runtime directory, removed when the proxy is dropped, or by the next
//! proxy when unharness was killed. A process that connects to herdr's
//! socket by its default path gets past it: this keeps integrations from
//! reporting over unharness, it does not keep anything from herdr (the
//! sandbox does not confine connecting to a socket).

use std::collections::HashSet;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
const MAX_LINE: usize = 16 << 20;

/// How many distinct lines the log takes: a client chooses what it says.
const MAX_NOTES: usize = 64;

/// How often a client that stopped sending is looked at, while herdr's
/// side of its connection is still open.
const HANGUP_CHECK: Duration = Duration::from_millis(500);

/// What a directory of the proxy's is called, before its random part.
const DIR_PREFIX: &str = "unharness-herdr-";

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
        .map_or_else(|| "no source".to_string(), shown);
    Verdict::Answer {
        reply: format!("{}\n", json!({"id": id, "result": {"type": "ok"}})),
        what: format!("kept from herdr: {method} from {source}"),
    }
}

/// `text` as it may go in the log: on one line, and not long.
fn shown(text: &str) -> String {
    let short: String = text.chars().take(64).collect();
    let mut out = short.escape_debug().to_string();
    if short.len() < text.len() {
        out.push('…');
    }
    out
}

/// Writes each distinct line once to `herdr.log`, the first `MAX_NOTES`
/// of them: a CLI reports on every turn.
struct Log {
    path: Option<PathBuf>,
    seen: Mutex<HashSet<String>>,
}

impl Log {
    fn once(&self, what: &str) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        if seen.len() >= MAX_NOTES || !seen.insert(what.to_string()) {
            return;
        }
        if let Some(path) = &self.path {
            super::herdr::log_line(path, what);
            if seen.len() == MAX_NOTES {
                super::herdr::log_line(path, "proxy: nothing more is noted");
            }
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
        sweep(parent);
        // Short: a socket's path must fit in `sun_path` (104 bytes on
        // macOS, whose temporary directory is long already).
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = parent.join(format!("{DIR_PREFIX}{}", &id[..12]));
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

/// Remove what a proxy of a killed unharness left in `parent`: a directory
/// of the user's, not a link, holding only a socket nothing listens on.
fn sweep(parent: &Path) {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    // SAFETY: `geteuid` cannot fail and touches no memory.
    let uid = unsafe { libc::geteuid() };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(DIR_PREFIX) {
            continue;
        }
        let dir = entry.path();
        let ours = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && m.uid() == uid);
        let sock = dir.join("herdr.sock");
        let dead = std::fs::symlink_metadata(&sock).is_ok_and(|m| m.file_type().is_socket())
            && std::os::unix::net::UnixStream::connect(&sock)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionRefused);
        if ours && dead {
            let _ = std::fs::remove_file(&sock);
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

async fn accept(listener: UnixListener, upstream: Arc<PathBuf>, log: Arc<Log>) {
    let mut connections = JoinSet::new();
    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                while connections.try_join_next().is_some() {}
                connections.spawn(serve(client, upstream.clone(), log.clone()));
            }
            // Out of descriptors, or a client gone before it was taken:
            // the next one may fare better.
            Err(e) => {
                log.once(&format!("proxy: accept: {e}"));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// One client: its lines in order, each answered here or written to
/// herdr's socket, which is connected at the first line it gets.
async fn serve(client: UnixStream, upstream: Arc<PathBuf>, log: Arc<Log>) {
    let fd = client.as_raw_fd();
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
        if n == 0 {
            break;
        }
        if line.len() > MAX_LINE {
            log.once(&format!(
                "proxy: a request over {} MiB ended its connection",
                MAX_LINE >> 20
            ));
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
    // answers (a subscription's events) is passed on until it closes, or
    // until the client is gone altogether, which an idle subscription
    // would not show otherwise.
    if let Some(mut to) = herdr {
        let _ = to.shutdown().await;
    }
    let mut check = tokio::time::interval(HANGUP_CHECK);
    loop {
        tokio::select! {
            done = copying.join_next() => if done.is_none() { break },
            _ = check.tick() => if hung_up(fd) { break },
        }
    }
}

/// Whether the peer of `fd` closed the connection, not only its sending
/// side (a client that did `shutdown(SHUT_WR)` may still read). Linux
/// says so with `POLLHUP` whatever is asked for. macOS's `poll` watches
/// only what is asked for, and says `POLLHUP` for the end of either side:
/// asked for `POLLOUT`, it does once this side cannot send, which the
/// peer's close makes so and its `shutdown(SHUT_WR)` does not.
fn hung_up(fd: RawFd) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one `pollfd` on the stack, no timeout; `fd` is held open by
    // the caller.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLHUP | libc::POLLERR) != 0
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

    #[test]
    fn what_a_client_names_goes_in_the_log_on_one_short_line() {
        let req = json!({
            "id": 1,
            "method": "pane.report_agent",
            "params": {"source": format!("evil\n1791660179 forged line{}", "x".repeat(200))},
        });
        let Verdict::Answer { what, .. } = classify(req.to_string().as_bytes()) else {
            panic!("forwarded");
        };
        assert!(!what.contains('\n'), "{what}");
        assert!(what.contains("evil\\n1791660179"), "{what}");
        assert!(what.ends_with('…') && what.len() < 140, "{what}");
    }

    #[test]
    fn the_log_takes_only_so_many_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("herdr.log");
        let log = Log {
            path: Some(path.clone()),
            seen: Mutex::new(HashSet::new()),
        };
        for i in 0..MAX_NOTES + 10 {
            log.once(&format!("kept from herdr: pane.report_agent from s{i}"));
        }
        log.once("kept from herdr: pane.report_agent from s0");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), MAX_NOTES + 1, "{text}");
        assert!(
            text.lines()
                .last()
                .unwrap()
                .ends_with("nothing more is noted")
        );
    }

    #[tokio::test]
    async fn what_a_killed_unharness_left_is_swept_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        // A socket nobody listens on any more.
        let stale = parent.join(format!("{DIR_PREFIX}stale"));
        std::fs::create_dir(&stale).unwrap();
        drop(std::os::unix::net::UnixListener::bind(stale.join("herdr.sock")).unwrap());
        // One that is in use, another unharness's.
        let live = Proxy::start_in(parent, parent.join("absent.sock"), None).unwrap();
        // Not ours to take: other names, a link, a directory with more in it.
        let other = parent.join("other");
        std::fs::create_dir(&other).unwrap();
        drop(std::os::unix::net::UnixListener::bind(other.join("herdr.sock")).unwrap());
        std::os::unix::fs::symlink(&other, parent.join(format!("{DIR_PREFIX}link"))).unwrap();
        let full = parent.join(format!("{DIR_PREFIX}full"));
        std::fs::create_dir(&full).unwrap();
        drop(std::os::unix::net::UnixListener::bind(full.join("herdr.sock")).unwrap());
        std::fs::write(full.join("keep"), "k").unwrap();

        let next = Proxy::start_in(parent, parent.join("absent.sock"), None).unwrap();
        assert!(!stale.exists());
        assert!(live.path().exists());
        assert!(other.join("herdr.sock").exists());
        assert!(parent.join(format!("{DIR_PREFIX}link")).exists());
        assert!(full.join("keep").exists());
        drop(next);
    }

    /// herdr's side of a subscription: takes one request and then neither
    /// answers nor closes. Says when the proxy closed its connection.
    async fn subscribed_herdr(path: &Path) -> tokio::sync::oneshot::Receiver<()> {
        let listener = UnixListener::bind(path).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let fd = s.as_raw_fd();
            let mut s = BufReader::new(s);
            let mut line = String::new();
            s.read_line(&mut line).await.unwrap();
            while !hung_up(fd) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let _ = tx.send(());
            drop(s);
        });
        rx
    }

    #[tokio::test]
    async fn a_client_gone_ends_an_idle_subscription() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr_sock = tmp.path().join("herdr.sock");
        let closed = subscribed_herdr(&herdr_sock).await;
        let proxy = Proxy::start_in(tmp.path(), herdr_sock, None).unwrap();
        let mut s = UnixStream::connect(proxy.path()).await.unwrap();
        s.write_all(b"{\"id\":1,\"method\":\"events.subscribe\"}\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(s);
        tokio::time::timeout(HANGUP_CHECK * 4, closed)
            .await
            .expect("the proxy let go of herdr")
            .unwrap();
    }

    #[tokio::test]
    async fn a_client_that_stopped_sending_still_gets_herdrs_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let herdr_sock = tmp.path().join("herdr.sock");
        let listener = UnixListener::bind(&herdr_sock).unwrap();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let mut s = BufReader::new(s);
            let mut line = String::new();
            s.read_line(&mut line).await.unwrap();
            // Past a check or two of the client.
            tokio::time::sleep(HANGUP_CHECK * 2).await;
            for i in 0..3 {
                let event = json!({"id": 1, "result": {"n": i}});
                s.get_mut()
                    .write_all(format!("{event}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let proxy = Proxy::start_in(tmp.path(), herdr_sock, None).unwrap();
        let mut s = UnixStream::connect(proxy.path()).await.unwrap();
        s.write_all(b"{\"id\":1,\"method\":\"events.subscribe\"}\n")
            .await
            .unwrap();
        s.shutdown().await.unwrap();
        let mut got = String::new();
        tokio::time::timeout(HANGUP_CHECK * 6, s.read_to_string(&mut got))
            .await
            .expect("herdr's answers and the end")
            .unwrap();
        assert_eq!(got.lines().count(), 3, "{got}");
    }
}
