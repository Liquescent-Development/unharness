//! A child process with line-oriented stdio, used by every long-lived transport.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use super::sandbox::Sandbox;

/// Maximum bytes accepted for a single stdout/stderr line before truncation.
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Capacity of the raw line channel; sends await so backpressure reaches the child.
pub const LINE_CHANNEL_CAPACITY: usize = 1024;
/// After the child exits, how long to wait for the pipes to drain. Grandchild
/// processes can keep the pipes open indefinitely, so this is bounded.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawLine {
    Stdout(String),
    Stderr(String),
    /// The child exited and its output was drained. Always the last line.
    Exited(Option<i32>),
}

pub struct LineProcess {
    stdin: Option<ChildStdin>,
    stop_tx: mpsc::UnboundedSender<Stop>,
    pid: Option<u32>,
    pub lines: mpsc::Receiver<RawLine>,
}

/// How long a CLI asked to end ([`LineProcess::end`]) has to exit by
/// itself before what is left of it is killed.
pub const END_GRACE: Duration = Duration::from_secs(3);

/// What the watcher is asked to do with the child.
enum Stop {
    /// Kill it now.
    Now,
    /// Let it exit by itself until then, and kill it after.
    Within(Duration),
}

impl LineProcess {
    /// Spawn `cmd` with piped stdio and start reader tasks. It leads a
    /// session of its own, so it has no controlling terminal (a Ctrl+C in
    /// the terminal is unharness's to pass on, not the CLI's) and what it
    /// starts is in its process group. Killing it, also by dropping this,
    /// kills that group and, on Linux, every process descended from it,
    /// so a crashed TUI never leaves an orphaned agent behind.
    ///
    /// The command is confined by `sandbox` first; taking it as an argument
    /// keeps a transport from spawning an unconfined agent by omission.
    pub fn spawn(cmd: Command, sandbox: &Sandbox) -> Result<Self> {
        Self::start(cmd, sandbox, true)
    }

    /// Spawn a command the user typed, as [`spawn`](Self::spawn) does but
    /// with no input (stdin is `/dev/null`).
    pub fn spawn_no_input(cmd: Command, sandbox: &Sandbox) -> Result<Self> {
        Self::start(cmd, sandbox, false)
    }

    fn start(cmd: Command, sandbox: &Sandbox, input: bool) -> Result<Self> {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut wrapped = sandbox.wrap(cmd.into_std())?;
        // After `wrap`: a backend may build a new command around this one.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: `setsid` is async-signal-safe and allocates nothing.
            unsafe {
                wrapped.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut cmd = Command::from(wrapped);
        cmd.stdin(if input { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let program = cmd.as_std().get_program().to_os_string();
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {:?}", program))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("child stdout not piped")?;
        let stderr = child.stderr.take().context("child stderr not piped")?;
        let pid = child.id();

        let (tx, rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = mpsc::unbounded_channel();

        let tx_out = tx.clone();
        let out_task = tokio::spawn(async move {
            let mut reader = BufReader::with_capacity(64 * 1024, stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                // Read to the end when nobody listens, so that a CLI
                // ending by itself does not fail on a closed pipe.
                let _ = tx_out.send(RawLine::Stdout(truncate(line))).await;
            }
        });

        let tx_err = tx.clone();
        let err_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = tx_err.send(RawLine::Stderr(truncate(line))).await;
            }
        });

        // The watcher owns the child: it reaps it on natural exit or on a
        // kill request, then gives the readers a bounded time to drain.
        let mut owned = Owned::new(child);
        tokio::spawn(async move {
            let code = watch(&mut owned, stop_rx).await;
            let drain = async {
                let _ = out_task.await;
                let _ = err_task.await;
            };
            let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
            // What it left running in its group goes with it.
            drop(owned);
            let _ = tx.send(RawLine::Exited(code)).await;
        });

        Ok(LineProcess {
            stdin,
            stop_tx,
            pid,
            lines: rx,
        })
    }

    pub async fn write_line(&mut self, line: &str) -> Result<()> {
        let stdin = self.stdin.as_mut().context("child stdin is closed")?;
        stdin.write_all(line.as_bytes()).await?;
        if !line.ends_with('\n') {
            stdin.write_all(b"\n").await?;
        }
        stdin.flush().await?;
        Ok(())
    }

    pub fn stdin_open(&self) -> bool {
        self.stdin.is_some()
    }

    /// Close stdin, which most protocols treat as an orderly shutdown request.
    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// End the CLI the orderly way: close stdin and give it [`END_GRACE`]
    /// to exit by itself, then kill what is left of it. Dropping this
    /// afterwards does not cut the grace short. `Exited` follows on
    /// `lines`.
    pub fn end(&mut self) {
        self.end_within(END_GRACE);
    }

    /// [`end`](Self::end) between turns, [`kill`](Self::kill) during one:
    /// a CLI given its grace mid-turn could go on with the turn (an edit,
    /// a command) after nobody is reading what it says.
    pub async fn end_or_kill(&mut self, turn_open: bool) {
        if turn_open {
            self.kill().await;
        } else {
            self.end();
        }
    }

    fn end_within(&mut self, grace: Duration) {
        self.stdin.take();
        let _ = self.stop_tx.send(Stop::Within(grace));
    }

    /// Ask the watcher to kill the child. `Exited` follows on `lines`.
    pub async fn kill(&mut self) {
        self.stdin.take();
        let _ = self.stop_tx.send(Stop::Now);
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Wait until every child spawned here is gone and reaped: for a quit,
    /// after each session was asked to end.
    pub async fn all_ended() {
        let mut live = live_children().subscribe();
        let _ = live.wait_for(|n| *n == 0).await;
    }
}

/// The child. Dropped, it kills what the child left running in its
/// process group, and the whole tree if the child was not reaped yet (the
/// runtime went away while the watcher waited).
struct Owned {
    child: Child,
    pid: Option<u32>,
    reaped: bool,
}

impl Owned {
    fn new(child: Child) -> Self {
        live_children().send_modify(|n| *n += 1);
        Owned {
            pid: child.id(),
            child,
            reaped: false,
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        match self.pid {
            Some(pid) if !self.reaped => kill_tree(pid),
            // The group's id stays taken while a member is left, so it
            // names no other.
            Some(pid) => kill_group(pid),
            None => {}
        }
        live_children().send_modify(|n| *n -= 1);
    }
}

/// How many `LineProcess` children are not reaped yet.
fn live_children() -> &'static tokio::sync::watch::Sender<usize> {
    static LIVE: std::sync::OnceLock<tokio::sync::watch::Sender<usize>> =
        std::sync::OnceLock::new();
    LIVE.get_or_init(|| tokio::sync::watch::channel(0).0)
}

/// Wait for the child. A kill request, or the `LineProcess` being dropped,
/// kills it and its tree; a request to end kills them once its grace is
/// up, unless the child exits first.
async fn watch(owned: &mut Owned, mut stop: mpsc::UnboundedReceiver<Stop>) -> Option<i32> {
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut asking = true;
    loop {
        let grace_up = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            status = owned.child.wait() => {
                owned.reaped = true;
                return status.ok().and_then(|s| s.code());
            }
            stop = stop.recv(), if asking => match stop {
                Some(Stop::Within(grace)) => {
                    let at = tokio::time::Instant::now() + grace;
                    deadline = Some(deadline.map_or(at, |d| d.min(at)));
                }
                Some(Stop::Now) => break,
                // Dropped while it ends by itself: the grace still holds.
                None if deadline.is_some() => asking = false,
                None => break,
            },
            _ = grace_up => break,
        }
    }
    if let Some(pid) = owned.pid {
        kill_tree(pid);
    }
    let _ = owned.child.start_kill();
    let status = owned.child.wait().await;
    owned.reaped = true;
    status.ok().and_then(|s| s.code())
}

/// SIGTERM, SIGHUP or SIGQUIT to unharness. The harness processes lead
/// sessions of their own, so none reaches them: unharness ends them
/// instead of dying with them still running.
pub struct EndSignals {
    #[cfg(unix)]
    signals: Vec<tokio::signal::unix::Signal>,
}

impl EndSignals {
    /// Listen from now on.
    pub fn listen() -> Self {
        #[cfg(unix)]
        {
            let signals = [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
                .into_iter()
                .filter_map(listen_unless_ignored)
                .collect();
            EndSignals { signals }
        }
        #[cfg(not(unix))]
        EndSignals {}
    }

    pub async fn recv(&mut self) {
        #[cfg(unix)]
        if !self.signals.is_empty() {
            let waits = self.signals.iter_mut().map(|s| Box::pin(s.recv()));
            futures::future::select_all(waits).await;
            return;
        }
        std::future::pending::<()>().await;
    }
}

/// A handler for `signo`, unless unharness was started with it ignored
/// (`nohup`, a background job's SIGINT): registering one would undo that.
#[cfg(unix)]
pub fn listen_unless_ignored(signo: libc::c_int) -> Option<tokio::signal::unix::Signal> {
    // SAFETY: with a null `act` sigaction only reads the disposition into
    // `old`, which is zeroed and owned here.
    let ignored = unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signo, std::ptr::null(), &mut old) == 0 && old.sa_sigaction == libc::SIG_IGN
    };
    if ignored {
        return None;
    }
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(signo)).ok()
}

/// A short-lived child asked one thing on stdin, for a model or provider
/// list: confined by `sandbox`, since the CLI reads its own configuration,
/// which a sandboxed session can write and which may name commands to run
/// (Claude's `apiKeyHelper`). It leads a process group of its own, and
/// the whole group is killed and reaped when this is dropped, so a helper
/// it started does not outlive it.
pub struct ProbeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    lines: std::sync::mpsc::Receiver<String>,
}

impl ProbeProcess {
    pub fn spawn(cmd: std::process::Command, sandbox: &Sandbox) -> Result<Self> {
        let mut cmd = sandbox.wrap(cmd)?;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().context("spawn")?;
        live_probes().push(child.id());
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("stdout")?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(ProbeProcess {
            child,
            stdin,
            lines,
        })
    }

    pub fn write_line(&mut self, line: &str) -> Result<()> {
        use std::io::Write;
        let stdin = self.stdin.as_mut().context("stdin closed")?;
        writeln!(stdin, "{line}")?;
        stdin.flush()?;
        Ok(())
    }

    /// The next stdout line, waiting until `deadline` at most.
    pub fn next_line(
        &self,
        deadline: std::time::Instant,
    ) -> std::result::Result<String, std::sync::mpsc::RecvTimeoutError> {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        self.lines.recv_timeout(left)
    }
}

impl ProbeProcess {
    /// Kill every probe still running, for a quit that does not wait for
    /// the threads asking them.
    pub fn kill_all() {
        for id in live_probes().drain(..) {
            kill_group(id);
        }
    }
}

impl Drop for ProbeProcess {
    fn drop(&mut self) {
        self.stdin.take();
        kill_group(self.child.id());
        live_probes().retain(|id| *id != self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The process groups of the probes running now.
static LIVE_PROBES: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

fn live_probes() -> std::sync::MutexGuard<'static, Vec<u32>> {
    LIVE_PROBES.lock().unwrap_or_else(|e| e.into_inner())
}

/// SIGKILL to the process group `id`. A group that is already gone is not
/// an error.
fn kill_group(id: u32) {
    #[cfg(unix)]
    // SAFETY: a plain syscall on the group created for this child.
    unsafe {
        libc::killpg(id as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = id;
}

/// SIGKILL to the child `pid`, which leads a process group, to that group,
/// and on Linux to every process descended from it: a group misses what
/// moved to a session of its own, as every command Claude Code runs does.
/// The child must not be reaped yet, so that its pid names no other.
fn kill_tree(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        // Stopped first, so that nothing forks while the tree is read.
        let mut tree = vec![pid];
        signal(pid, libc::SIGSTOP);
        for _ in 0..16 {
            let found: Vec<u32> = descendants(pid)
                .into_iter()
                .filter(|p| !tree.contains(p))
                .collect();
            if found.is_empty() {
                break;
            }
            for &p in &found {
                signal(p, libc::SIGSTOP);
            }
            tree.extend(found);
        }
        kill_group(pid);
        for p in tree {
            signal(p, libc::SIGKILL);
        }
    }
    #[cfg(not(target_os = "linux"))]
    kill_group(pid);
}

#[cfg(target_os = "linux")]
fn signal(pid: u32, sig: libc::c_int) {
    // SAFETY: a plain syscall; a process that is gone is not an error.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Every process below `root`, read from `/proc`.
#[cfg(target_os = "linux")]
fn descendants(root: u32) -> Vec<u32> {
    let mut parents: Vec<(u32, u32)> = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        // `pid (comm) state ppid ...`, where `comm` may hold anything.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse::<u32>().ok());
        if let Some(ppid) = ppid {
            parents.push((pid, ppid));
        }
    }
    let mut found = Vec::new();
    let mut next = vec![root];
    while let Some(parent) = next.pop() {
        for &(pid, ppid) in &parents {
            if ppid == parent && !found.contains(&pid) {
                found.push(pid);
                next.push(pid);
            }
        }
    }
    found
}

fn truncate(mut line: String) -> String {
    if line.len() > MAX_LINE_BYTES {
        let mut cut = MAX_LINE_BYTES;
        while !line.is_char_boundary(cut) {
            cut -= 1;
        }
        line.truncate(cut);
        line.push_str("…[truncated]");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_probe_takes_its_helpers_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("helper.pid");
        let mut cmd = std::process::Command::new("sh");
        // A helper in the background, then an answer, then a long wait.
        cmd.arg("-c").arg(format!(
            "sleep 30 & echo $! > {}; read line; echo \"got $line\"; sleep 30",
            pid_file.display()
        ));
        let mut probe = ProbeProcess::spawn(cmd, &Sandbox::off()).unwrap();
        probe.write_line("hi").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        assert_eq!(probe.next_line(deadline).unwrap(), "got hi");
        let helper: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        drop(probe);
        // SAFETY: signal 0 only asks whether the process exists.
        let alive = |pid| unsafe { libc::kill(pid, 0) } == 0;
        let gone_by = std::time::Instant::now() + Duration::from_secs(5);
        while alive(helper) && std::time::Instant::now() < gone_by {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(helper));
    }

    async fn drain(p: &mut LineProcess) -> Vec<RawLine> {
        let mut got = Vec::new();
        while let Some(l) = p.lines.recv().await {
            let exited = matches!(l, RawLine::Exited(_));
            got.push(l);
            if exited {
                break;
            }
        }
        got
    }

    #[tokio::test]
    async fn spawn_echo_and_read_lines() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("cat; echo done >&2; exit 3");
        let mut p = LineProcess::spawn(cmd, &Sandbox::off()).unwrap();
        p.write_line("hello").await.unwrap();
        p.write_line("world\n").await.unwrap();
        p.close_stdin();

        let got = drain(&mut p).await;
        assert!(got.contains(&RawLine::Stdout("hello".into())));
        assert!(got.contains(&RawLine::Stdout("world".into())));
        assert!(got.contains(&RawLine::Stderr("done".into())));
        assert_eq!(got.last(), Some(&RawLine::Exited(Some(3))));
    }

    #[tokio::test]
    async fn kill_terminates_even_with_grandchild_holding_pipes() {
        // `sh` is killed but its `sleep` child keeps stdout open; Exited must
        // still arrive within the drain timeout.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30");
        let mut p = LineProcess::spawn(cmd, &Sandbox::off()).unwrap();
        assert!(p.pid().is_some());
        p.kill().await;
        let got = tokio::time::timeout(Duration::from_secs(5), drain(&mut p))
            .await
            .unwrap();
        assert!(matches!(got.last(), Some(RawLine::Exited(_))));
    }

    /// Whether the process `pid` runs now (not gone, not a zombie).
    #[cfg(target_os = "linux")]
    fn running(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            !s.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
    }

    /// Whether the process `pid` is still alive a moment after it was
    /// killed.
    #[cfg(target_os = "linux")]
    fn alive(pid: &str) -> bool {
        for _ in 0..50 {
            if !running(pid) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[tokio::test]
    async fn a_group_reads_no_input_and_its_background_ends_with_it() {
        // `cat` reads an empty stdin, not the terminal. The background
        // `sleep` holds no pipe, and outlives the shell only until the
        // group is killed.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("cat; sleep 30 >/dev/null 2>&1 & echo $!; echo err >&2");
        let mut p = LineProcess::spawn_no_input(cmd, &Sandbox::off()).unwrap();
        assert!(!p.stdin_open());
        let got = tokio::time::timeout(Duration::from_secs(5), drain(&mut p))
            .await
            .unwrap();
        assert!(got.contains(&RawLine::Stderr("err".into())));
        assert_eq!(got.last(), Some(&RawLine::Exited(Some(0))));
        let Some(RawLine::Stdout(_sleeper)) = got.first() else {
            panic!("{got:?}");
        };
        #[cfg(target_os = "linux")]
        assert!(!alive(_sleeper));
    }

    #[tokio::test]
    async fn killing_a_group_kills_what_it_started() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & echo $!; wait");
        let mut p = LineProcess::spawn_no_input(cmd, &Sandbox::off()).unwrap();
        let Some(RawLine::Stdout(_sleeper)) = p.lines.recv().await else {
            panic!("no pid");
        };
        #[cfg(target_os = "linux")]
        assert!(alive(&_sleeper));
        p.kill().await;
        let got = tokio::time::timeout(Duration::from_secs(5), drain(&mut p))
            .await
            .unwrap();
        // Killed by a signal: no exit code.
        assert_eq!(got.last(), Some(&RawLine::Exited(None)));
        #[cfg(target_os = "linux")]
        assert!(!alive(&_sleeper));
    }

    #[tokio::test]
    async fn an_ended_child_exits_by_itself() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("cat; exit 4");
        let mut p = LineProcess::spawn(cmd, &Sandbox::off()).unwrap();
        p.end();
        let got = tokio::time::timeout(Duration::from_secs(5), drain(&mut p))
            .await
            .unwrap();
        assert_eq!(got.last(), Some(&RawLine::Exited(Some(4))));
    }

    // Threads of its own: the watcher runs while this one waits.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_ended_child_is_killed_once_its_grace_is_up_also_when_dropped() {
        // Deaf to the end of its input, as Claude is while a task runs.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo $$; trap '' TERM; while :; do sleep 1; done");
        let mut p = LineProcess::spawn(cmd, &Sandbox::off()).unwrap();
        let Some(RawLine::Stdout(_pid)) = p.lines.recv().await else {
            panic!("no pid");
        };
        p.end_within(Duration::from_millis(300));
        drop(p);
        #[cfg(target_os = "linux")]
        {
            std::thread::sleep(Duration::from_millis(100));
            assert!(running(&_pid), "killed before its grace was up");
            assert!(!alive(&_pid));
        }
    }

    /// `pid` prints itself and then runs in a session of its own, as Claude
    /// Code runs every command.
    #[cfg(target_os = "linux")]
    const OWN_SESSION: &str = "setsid sh -c 'echo $$; exec sleep 30' & wait";

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn killing_reaches_what_moved_to_a_session_of_its_own() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(OWN_SESSION);
        let mut p = LineProcess::spawn(cmd, &Sandbox::off()).unwrap();
        let Some(RawLine::Stdout(sleeper)) = p.lines.recv().await else {
            panic!("no pid");
        };
        assert!(alive(&sleeper));
        p.kill().await;
        let got = tokio::time::timeout(Duration::from_secs(5), drain(&mut p))
            .await
            .unwrap();
        assert_eq!(got.last(), Some(&RawLine::Exited(None)));
        assert!(!alive(&sleeper));
    }

    // SIGUSR2 is set to be ignored for the whole test process: no other
    // test uses it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_signal_started_ignored_stays_ignored() {
        unsafe { libc::signal(libc::SIGUSR2, libc::SIG_IGN) };
        assert!(listen_unless_ignored(libc::SIGUSR2).is_none());
        assert!(listen_unless_ignored(libc::SIGUSR1).is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_tree_goes_with_the_runtime() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (sleeper, _p) = rt.block_on(async {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(OWN_SESSION);
            let mut p = LineProcess::spawn(cmd, &Sandbox::off()).unwrap();
            let Some(RawLine::Stdout(sleeper)) = p.lines.recv().await else {
                panic!("no pid");
            };
            (sleeper, p)
        });
        assert!(alive(&sleeper));
        drop(rt);
        assert!(!alive(&sleeper));
    }

    #[tokio::test]
    async fn spawn_missing_binary_errors() {
        let cmd = Command::new("/definitely/not/a/binary");
        assert!(LineProcess::spawn(cmd, &Sandbox::off()).is_err());
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "é".repeat(MAX_LINE_BYTES);
        let t = truncate(s);
        assert!(t.ends_with("…[truncated]"));
        assert!(t.len() <= MAX_LINE_BYTES + "…[truncated]".len());
    }
}
