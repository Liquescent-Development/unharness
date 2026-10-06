//! A child process with line-oriented stdio, used by every long-lived transport.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

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
    kill_tx: Option<oneshot::Sender<()>>,
    pid: Option<u32>,
    pub lines: mpsc::Receiver<RawLine>,
}

impl LineProcess {
    /// Spawn `cmd` with piped stdio and start reader tasks. `kill_on_drop` is
    /// set so a crashed TUI never leaves an orphaned agent behind.
    ///
    /// The command is confined by `sandbox` first; taking it as an argument
    /// keeps a transport from spawning an unconfined agent by omission.
    pub fn spawn(cmd: Command, sandbox: &Sandbox) -> Result<Self> {
        Self::start(cmd, sandbox, false)
    }

    /// Spawn a command the user typed: no input (stdin is `/dev/null`), and
    /// a session of its own, so it has no controlling terminal to read
    /// from or draw on, and everything it starts is in one process group.
    /// `kill`, dropping this, and the command's own end each kill that
    /// whole group, so what it left running in the background goes too.
    pub fn spawn_group(cmd: Command, sandbox: &Sandbox) -> Result<Self> {
        Self::start(cmd, sandbox, true)
    }

    fn start(cmd: Command, sandbox: &Sandbox, group: bool) -> Result<Self> {
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut wrapped = sandbox.wrap(cmd.into_std())?;
        // After `wrap`: a backend may build a new command around this one.
        #[cfg(unix)]
        if group {
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
        cmd.stdin(if group { Stdio::null() } else { Stdio::piped() })
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
        // The session leader's pid is its group's id.
        let group_id = pid.filter(|_| group);

        let (tx, rx) = mpsc::channel(LINE_CHANNEL_CAPACITY);
        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        let tx_out = tx.clone();
        let out_task = tokio::spawn(async move {
            let mut reader = BufReader::with_capacity(64 * 1024, stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if tx_out.send(RawLine::Stdout(truncate(line))).await.is_err() {
                    break;
                }
            }
        });

        let tx_err = tx.clone();
        let err_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if tx_err.send(RawLine::Stderr(truncate(line))).await.is_err() {
                    break;
                }
            }
        });

        // The watcher owns the child: it reaps it on natural exit or on a
        // kill request, then gives the readers a bounded time to drain.
        tokio::spawn(async move {
            let code = watch(child, kill_rx, group_id).await;
            let drain = async {
                let _ = out_task.await;
                let _ = err_task.await;
            };
            let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
            if let Some(id) = group_id {
                kill_group(id);
            }
            let _ = tx.send(RawLine::Exited(code)).await;
        });

        Ok(LineProcess {
            stdin,
            kill_tx: Some(kill_tx),
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

    /// Ask the watcher to kill the child. `Exited` follows on `lines`.
    pub async fn kill(&mut self) {
        self.stdin.take();
        if let Some(tx) = self.kill_tx.take() {
            let _ = tx.send(());
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
}

/// Wait for the child. A kill request, or the `LineProcess` being dropped,
/// kills it, and its process group when it has one.
async fn watch(
    mut child: Child,
    kill_rx: oneshot::Receiver<()>,
    group: Option<u32>,
) -> Option<i32> {
    tokio::select! {
        status = child.wait() => status.ok().and_then(|s| s.code()),
        _ = kill_rx => {
            if let Some(id) = group {
                kill_group(id);
            }
            let _ = child.start_kill();
            child.wait().await.ok().and_then(|s| s.code())
        }
    }
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

    /// Whether the process `pid` is still alive (not gone, not a zombie)
    /// a moment after it was killed.
    #[cfg(target_os = "linux")]
    fn alive(pid: &str) -> bool {
        let running = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
                !s.rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z')
            })
        };
        for _ in 0..50 {
            if !running() {
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
        let mut p = LineProcess::spawn_group(cmd, &Sandbox::off()).unwrap();
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
        let mut p = LineProcess::spawn_group(cmd, &Sandbox::off()).unwrap();
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
