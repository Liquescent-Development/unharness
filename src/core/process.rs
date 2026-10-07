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

/// `cmd` in the sandbox, in a session of its own, on pipes.
fn spawn_child(
    cmd: std::process::Command,
    sandbox: &Sandbox,
    input: bool,
) -> Result<tokio::process::Child> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut wrapped = sandbox.wrap(cmd)?;
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
    Ok(cmd.spawn()?)
}

/// Kills a [`LineProcess`] ([`LineProcess::stopper`]); once it is gone,
/// or its owner has ended it and let go, this does nothing.
#[derive(Clone)]
pub struct Stopper(mpsc::WeakUnboundedSender<Stop>);

impl Stopper {
    pub fn kill(&self) {
        if let Some(stop) = self.0.upgrade() {
            let _ = stop.send(Stop::Now);
        }
    }
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
        let cmd = cmd.into_std();
        let program = cmd.get_program().to_os_string();
        let failed = || format!("failed to spawn {program:?}");
        // Through the reaper, inside the sandbox like the CLI.
        #[cfg(target_os = "linux")]
        let mut child = match super::reaper::wrap(&cmd).with_context(failed)? {
            Some((reaper, reaped)) => match spawn_child(reaper, sandbox, input) {
                Ok(child) => {
                    reaped.started().with_context(failed)?;
                    child
                }
                // The reaper could not be run here: the CLI goes bare.
                Err(_) => spawn_child(cmd, sandbox, input).with_context(failed)?,
            },
            None => spawn_child(cmd, sandbox, input).with_context(failed)?,
        };
        #[cfg(not(target_os = "linux"))]
        let mut child = spawn_child(cmd, sandbox, input).with_context(failed)?;
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
            let (mut out_task, mut err_task) = (out_task, err_task);
            let drain = async {
                let _ = (&mut out_task).await;
                let _ = (&mut err_task).await;
            };
            if tokio::time::timeout(DRAIN_TIMEOUT, drain).await.is_err() {
                // Something out of reach (a double fork, a session of its
                // own when the child exited by itself) holds the pipes:
                // they are closed on it, and it gets EPIPE from now on.
                out_task.abort();
                err_task.abort();
                let _ = out_task.await;
                let _ = err_task.await;
            }
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

    /// What kills this from outside its owner, also while the owner is
    /// stuck. It does not keep it alive: dropped, this is killed as before.
    pub fn stopper(&self) -> Stopper {
        Stopper(self.stop_tx.downgrade())
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

/// The child. Dropped before it was reaped (the runtime went away while
/// the watcher waited), it kills the child's whole tree.
struct Owned {
    child: Child,
    pid: Option<u32>,
    /// Readable once the child has exited, while it is not reaped yet.
    #[cfg(target_os = "linux")]
    exit: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
    reaped: bool,
}

impl Owned {
    fn new(child: Child) -> Self {
        live_children().send_modify(|n| *n += 1);
        let pid = child.id();
        unreaped().extend(pid);
        Owned {
            #[cfg(target_os = "linux")]
            exit: pid.and_then(pidfd),
            pid,
            child,
            reaped: false,
        }
    }

    /// Wait for the child to exit. Where it can be seen (a pidfd, Linux
    /// 5.3 and later) it is not reaped, and `None` comes back: its pid,
    /// and so its group's id, can then name no other process. Elsewhere,
    /// or when the pidfd cannot be waited on, it is reaped, and its status
    /// comes back.
    async fn exited(&mut self) -> Option<std::io::Result<std::process::ExitStatus>> {
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.exit
            && let Ok(mut ready) = fd.readable().await
        {
            ready.retain_ready();
            return None;
        }
        Some(self.child.wait().await)
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(pid) = self.pid
            && !self.reaped
        {
            kill_tree(pid);
            forget(pid);
        }
        live_children().send_modify(|n| *n -= 1);
    }
}

/// A pidfd for the child `pid`, which must not be reaped yet.
#[cfg(target_os = "linux")]
fn pidfd(pid: u32) -> Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    // SAFETY: a plain syscall; the fd it returns is owned from here on.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as libc::c_int) };
    tokio::io::unix::AsyncFd::with_interest(fd, tokio::io::Interest::READABLE).ok()
}

/// The pids of the `LineProcess` children not reaped yet, which name no
/// other process while they are listed: a child leaves before it is
/// reaped, except where its exit cannot be seen without reaping it.
static UNREAPED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

fn unreaped() -> std::sync::MutexGuard<'static, Vec<u32>> {
    UNREAPED.lock().unwrap_or_else(|e| e.into_inner())
}

fn forget(pid: u32) {
    unreaped().retain(|p| *p != pid);
}

/// How many `LineProcess` children are not reaped yet.
fn live_children() -> &'static tokio::sync::watch::Sender<usize> {
    static LIVE: std::sync::OnceLock<tokio::sync::watch::Sender<usize>> =
        std::sync::OnceLock::new();
    LIVE.get_or_init(|| tokio::sync::watch::channel(0).0)
}

/// Wait for the child. A kill request, or the `LineProcess` being dropped,
/// kills it and its tree; a request to end kills them once its grace is
/// up, unless the child exits first. What a child that exits leaves in
/// its process group goes with it.
async fn watch(owned: &mut Owned, mut stop: mpsc::UnboundedReceiver<Stop>) -> Option<i32> {
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut asking = true;
    let exited = loop {
        let grace_up = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            reaped = owned.exited() => break Some(reaped),
            stop = stop.recv(), if asking => match stop {
                Some(Stop::Within(grace)) => {
                    let at = tokio::time::Instant::now() + grace;
                    deadline = Some(deadline.map_or(at, |d| d.min(at)));
                }
                Some(Stop::Now) => break None,
                // Dropped while it ends by itself: the grace still holds.
                None if deadline.is_some() => asking = false,
                None => break None,
            },
            _ = grace_up => break None,
        }
    };
    let status = match exited {
        // The group's id is the child's pid, which the child gives up when
        // it is reaped: with the group emptied too, a new process could
        // lead a group of that id, as every harness process does. So the
        // group is killed before the reaping where the exit can be seen
        // without it, and right after it elsewhere.
        Some(reaped) => {
            if let Some(pid) = owned.pid {
                kill_group(pid);
                forget(pid);
            }
            match reaped {
                Some(status) => status,
                None => owned.child.wait().await,
            }
        }
        None => {
            if let Some(pid) = owned.pid {
                kill_tree(pid);
                forget(pid);
            }
            let _ = owned.child.start_kill();
            owned.child.wait().await
        }
    };
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

/// Ctrl+Z (SIGTSTP) to unharness, which [`suspend`] answers.
pub struct JobStops {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
}

impl JobStops {
    /// Listen from now on, unless unharness was started with it ignored.
    pub fn listen() -> Self {
        JobStops {
            #[cfg(unix)]
            signal: listen_unless_ignored(libc::SIGTSTP),
        }
    }

    pub async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some(s) = self.signal.as_mut() {
            s.recv().await;
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
    signal_group(id, libc::SIGKILL);
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
        let tree = stop_tree(pid);
        kill_group(pid);
        for p in tree {
            signal(p, libc::SIGKILL);
        }
    }
    #[cfg(not(target_os = "linux"))]
    kill_group(pid);
}

/// SIGSTOP to the child `pid` and every process descended from it, read
/// until no new one turns up. Returns them, `pid` first.
#[cfg(target_os = "linux")]
fn stop_tree(pid: u32) -> Vec<u32> {
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
    tree
}

/// Ctrl+Z, for unharness: the harness processes lead sessions of their
/// own, so the terminal stops only unharness, and a CLI would go on with
/// its turn. They are stopped with their process groups and, on Linux,
/// everything descended from them, then unharness stops itself, and they
/// are continued when it is (`fg`). Where the terminal would not have
/// stopped unharness (its process group is orphaned) nothing stays
/// stopped.
#[cfg(unix)]
pub fn suspend() {
    // Held throughout: none is reaped meanwhile, so no pid names another.
    let live = unreaped();
    #[cfg(target_os = "linux")]
    let mut stopped: Vec<u32> = Vec::new();
    for &pid in live.iter() {
        signal_group(pid, libc::SIGSTOP);
        #[cfg(target_os = "linux")]
        stopped.extend(stop_tree(pid));
    }
    // SAFETY: the disposition is read into, and restored from, a zeroed
    // struct owned here; `raise` stops the process until SIGCONT.
    unsafe {
        let mut stop: libc::sigaction = std::mem::zeroed();
        stop.sa_sigaction = libc::SIG_DFL;
        let mut handler: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGTSTP, &stop, &mut handler);
        libc::raise(libc::SIGTSTP);
        libc::sigaction(libc::SIGTSTP, &handler, std::ptr::null_mut());
    }
    // What was stopped, also what left the tree meanwhile (its parent
    // killed), and what is in it now.
    for &pid in live.iter() {
        signal_group(pid, libc::SIGCONT);
        #[cfg(target_os = "linux")]
        stopped.extend(descendants(pid));
    }
    #[cfg(target_os = "linux")]
    for p in stopped {
        signal(p, libc::SIGCONT);
    }
}

/// `sig` to the process group `id`.
#[cfg(unix)]
fn signal_group(id: u32, sig: libc::c_int) {
    // SAFETY: a plain syscall; a group that is gone is not an error.
    unsafe {
        libc::killpg(id as libc::pid_t, sig);
    }
}

#[cfg(target_os = "linux")]
fn signal(pid: u32, sig: libc::c_int) {
    // SAFETY: a plain syscall; a process that is gone is not an error.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Whether the process `pid` runs now: it exists and is not a zombie.
#[cfg(target_os = "linux")]
pub fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        !s.rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z')
    })
}

/// `pid` and every process below it, held by pidfd: a signal through one
/// reaches the process it was opened for or none, never one that took its
/// pid since. For a child that shares unharness's process group (the
/// prompt's editor, which needs the terminal), where a group signal would
/// reach unharness too.
#[cfg(target_os = "linux")]
pub struct HeldTree(Vec<std::os::fd::OwnedFd>);

#[cfg(target_os = "linux")]
impl HeldTree {
    pub fn of(pid: u32) -> Self {
        use std::os::fd::FromRawFd;
        let mut pids = vec![pid];
        pids.extend(descendants(pid));
        let fds = pids
            .into_iter()
            .filter_map(|p| {
                // SAFETY: a plain syscall; the descriptor is owned below.
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, p as libc::pid_t, 0) };
                // SAFETY: a new descriptor nothing else owns.
                (fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
            })
            .collect();
        HeldTree(fds)
    }

    pub fn signal(&self, sig: libc::c_int) {
        use std::os::fd::AsRawFd;
        for fd in &self.0 {
            // SAFETY: a plain syscall; one that has exited is not an error.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }

    /// Whether one of them has not exited yet.
    pub fn any_running(&self) -> bool {
        use std::os::fd::AsRawFd;
        self.0.iter().any(|fd| {
            let mut exited = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd, on a descriptor owned here; readable
            // once the process has exited.
            unsafe { libc::poll(&mut exited, 1, 0) == 0 }
        })
    }
}

/// Everything below `root`, which is not touched: stopped, so that none
/// starts another meanwhile, then killed (the reaper, for what its CLI
/// left).
#[cfg(target_os = "linux")]
pub(crate) fn kill_below(root: u32) {
    let mut below: Vec<u32> = Vec::new();
    for _ in 0..16 {
        let found: Vec<u32> = descendants(root)
            .into_iter()
            .filter(|p| !below.contains(p))
            .collect();
        if found.is_empty() {
            break;
        }
        for &p in &found {
            signal(p, libc::SIGSTOP);
        }
        below.extend(found);
    }
    for p in below {
        signal(p, libc::SIGKILL);
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

    /// Whether the process `pid` is still alive a moment after it was
    /// killed.
    #[cfg(target_os = "linux")]
    fn alive(pid: &str) -> bool {
        for _ in 0..50 {
            if !running(pid.parse().unwrap()) {
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

    // Holding the output, the background `sleep` kept the drain waiting
    // for its whole timeout, and was killed after it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn what_a_child_leaves_in_its_group_goes_when_it_exits() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & echo $!");
        let mut p = LineProcess::spawn_no_input(cmd, &Sandbox::off()).unwrap();
        let started = std::time::Instant::now();
        let got = tokio::time::timeout(Duration::from_secs(5), drain(&mut p))
            .await
            .unwrap();
        assert!(
            started.elapsed() < DRAIN_TIMEOUT / 2,
            "{:?}",
            started.elapsed()
        );
        assert_eq!(got.last(), Some(&RawLine::Exited(Some(0))));
        let Some(RawLine::Stdout(sleeper)) = got.first() else {
            panic!("{got:?}");
        };
        assert!(!alive(sleeper));
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

    // The grandchild left the child's tree and session: nothing kills
    // it, but it no longer has anyone reading its output.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_pipes_are_closed_on_what_escaped_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name| dir.path().join(name).display().to_string();
        let (out, go, failed) = (file("out"), file("go"), file("failed"));
        let escaped = format!(
            "trap '' PIPE; touch {out}; while [ ! -e {go} ]; do sleep 0.05; done; echo late || touch {failed}"
        );
        let mut cmd = Command::new("sh");
        // The child waits until it is out, or the group kill would catch it.
        cmd.arg("-c").arg(format!(
            "(setsid sh -c \"{escaped}\" &); while [ ! -e {out} ]; do sleep 0.05; done"
        ));
        let mut p = LineProcess::spawn_no_input(cmd, &Sandbox::off()).unwrap();
        let got = tokio::time::timeout(Duration::from_secs(10), drain(&mut p)).await;
        // Let it go first whatever happened: it ends after one line.
        std::fs::write(&go, "").unwrap();
        assert_eq!(got.unwrap().last(), Some(&RawLine::Exited(Some(0))));
        let failed = std::path::Path::new(&failed);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !failed.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(failed.exists(), "its output was still read");
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

    #[tokio::test]
    async fn a_stopper_kills_but_does_not_keep_alive() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exec sleep 30");
        let mut p = LineProcess::spawn(cmd, &Sandbox::off()).unwrap();
        p.stopper().kill();
        let got = tokio::time::timeout(Duration::from_secs(5), drain(&mut p))
            .await
            .unwrap();
        assert!(matches!(got.last(), Some(RawLine::Exited(_))), "{got:?}");

        // Dropped with a stopper around, it is killed as before.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo $$; exec sleep 30");
        let mut p = LineProcess::spawn(cmd, &Sandbox::off()).unwrap();
        let Some(RawLine::Stdout(pid)) = p.lines.recv().await else {
            panic!("no pid");
        };
        let stopper = p.stopper();
        drop(p);
        let pid: u32 = pid.trim().parse().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while running(pid) {
            assert!(tokio::time::Instant::now() < deadline, "kept alive");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stopper.kill();
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
        p.end_within(Duration::from_secs(1));
        drop(p);
        #[cfg(target_os = "linux")]
        {
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                running(_pid.parse().unwrap()),
                "killed before its grace was up"
            );
            std::thread::sleep(Duration::from_millis(900));
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
