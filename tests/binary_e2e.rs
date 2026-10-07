//! End-to-end tests of the `unharness` binary, with `scripts/fake-harness.py`
//! standing in for Claude Code: how it ends on a signal and at quit, and
//! what it leaves running.

#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use unharness::core::process::running;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A home, config and state of its own, and a fake Claude Code that
/// replays the fixture `lines`, starts a `sleep` in a session of its own and, once
/// its stdin closes, goes on for a minute.
struct Setup {
    tmp: tempfile::TempDir,
}

impl Setup {
    fn new(lines: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fixture = root.join("fixture.jsonl");
        std::fs::write(&fixture, lines).unwrap();
        for dir in ["home", "config/unharness", "state", "data", "cache", "ws"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        // What unharness asks the CLI outside a session is answered here;
        // the model list does not start the background child.
        let fake = root.join("claude");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n\
                 case \"$1\" in auth) echo '{{\"loggedIn\": true}}'; exit 0;; esac\n\
                 case \"$*\" in\n\
                 *--version*) echo '2.1.292 (Claude Code)'; exit 0;;\n\
                 *--safe-mode*) unset UNHARNESS_FAKE_BACKGROUND;;\n\
                 esac\n\
                 export UNHARNESS_FAKE_FIXTURE='{}'\n\
                 exec python3 '{}' \"$@\"\n",
                fixture.display(),
                repo().join("scripts/fake-harness.py").display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            root.join("config/unharness/config.toml"),
            format!("[harnesses.claude]\nbinary = \"{}\"\n", fake.display()),
        )
        .unwrap();
        Setup { tmp }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_unharness"));
        cmd.args(["-H", "claude", "--sandbox", "off", "--policy", "bypass"])
            .args(args)
            .current_dir(self.path("ws"))
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("TERM", "xterm-256color")
            .env("UNHARNESS_FAKE_HANG", "1")
            .env("UNHARNESS_FAKE_LINGER", "60")
            .env(
                "UNHARNESS_FAKE_BACKGROUND",
                format!("session:{}", self.path("pids").display()),
            )
            .env("UNHARNESS_FAKE_LOG", self.path("log"));
        for (key, _) in std::env::vars() {
            if key.starts_with("HERDR_") || key.starts_with("CLAUDE_") {
                cmd.env_remove(key);
            }
        }
        cmd
    }

    /// The fake's pid and its background child's.
    fn pids(&self) -> (u32, u32) {
        let text = std::fs::read_to_string(self.path("pids")).unwrap();
        let (cli, child) = text.trim().split_once(' ').unwrap();
        (cli.parse().unwrap(), child.parse().unwrap())
    }
}

fn wait_until(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn gone_within(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while running(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Whether `pid` has a handler for `signo`.
fn catches(pid: u32, signo: libc::c_int) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            let mask = s.lines().find_map(|l| l.strip_prefix("SigCgt:"))?;
            u64::from_str_radix(mask.trim(), 16).ok()
        })
        .is_some_and(|mask| mask & (1 << (signo - 1)) != 0)
}

/// unharness, ended with SIGTERM if a test fails while it runs, so that
/// it takes its CLI along.
struct Unharness(Child);

impl Unharness {
    fn id(&self) -> u32 {
        self.0.id()
    }

    fn exit_within(&mut self, within: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Unharness {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // SAFETY: a signal to the child this test started.
            // (And SIGCONT, in case it was left stopped.)
            unsafe {
                libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
                libc::kill(self.0.id() as libc::pid_t, libc::SIGCONT);
            }
            if self.exit_within(Duration::from_secs(10)).is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
}

/// Takes the turn and never answers it.
const SILENT: &str = ">> {\"subtype\": \"initialize\"}\n>> {\"type\": \"user\"}\n";

// The CLI is in a session of its own, so no signal to unharness reaches
// it: unharness ends it, and what it started, before going.
#[test]
fn print_ends_on_sigterm_sighup_and_sigquit_with_the_cli_and_what_it_started() {
    if !python_available() {
        return;
    }
    let runs: Vec<_> = [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .map(|signo| {
            let setup = Setup::new(SILENT);
            let mut cmd = setup.command(&["--print", "hi"]);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            (signo, setup, Unharness(cmd.spawn().unwrap()))
        })
        .collect();
    let signalled: Vec<_> = runs
        .into_iter()
        .map(|(signo, setup, unharness)| {
            let pid = unharness.id();
            wait_until("the turn is sent", Duration::from_secs(20), || {
                std::fs::read_to_string(setup.path("log")).is_ok_and(|l| l.lines().count() >= 2)
            });
            wait_until("unharness listens", Duration::from_secs(5), || {
                catches(pid, signo)
            });
            let pids = setup.pids();
            // SAFETY: a signal to the child this test started.
            unsafe { libc::kill(pid as libc::pid_t, signo) };
            (signo, pids, unharness)
        })
        .collect();
    for (signo, (cli, child), mut unharness) in signalled {
        let status = unharness
            .exit_within(Duration::from_secs(15))
            .unwrap_or_else(|| panic!("signal {signo} did not end the run"));
        assert_eq!(status.code(), Some(130), "signal {signo}");
        assert!(gone_within(cli, Duration::from_secs(2)), "signal {signo}");
        assert!(gone_within(child, Duration::from_secs(2)), "signal {signo}");
    }
}

/// A terminal for the TUI: what it draws is collected, keys are written.
struct Pty {
    master: OwnedFd,
    screen: Arc<Mutex<Vec<u8>>>,
}

impl Pty {
    fn spawn(mut cmd: Command) -> (Unharness, Pty) {
        let (mut master, mut slave) = (0, 0);
        let size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: out-pointers to locals; the fds returned are owned below.
        let ok = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        };
        assert_eq!(ok, 0, "openpty");
        // SAFETY: both are new descriptors nothing else owns.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        cmd.stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        use std::os::unix::process::CommandExt;
        // SAFETY: async-signal-safe calls only.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        drop(slave);
        let screen = Arc::new(Mutex::new(Vec::new()));
        let reader = master.try_clone().unwrap();
        let into = screen.clone();
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(reader);
            let mut chunk = [0u8; 65536];
            use std::io::Read;
            // EIO once the TUI is gone.
            while let Ok(n @ 1..) = file.read(&mut chunk) {
                into.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
        (Unharness(child), Pty { master, screen })
    }

    fn shows(&self, text: &str) -> bool {
        String::from_utf8_lossy(&self.screen.lock().unwrap()).contains(text)
    }

    fn type_keys(&self, keys: &[u8]) {
        // SAFETY: a write from a live buffer to an fd owned here.
        let n = unsafe { libc::write(self.master.as_raw_fd(), keys.as_ptr().cast(), keys.len()) };
        assert_eq!(n, keys.len() as isize);
    }
}

#[test]
fn quitting_the_tui_says_it_waits_for_the_cli_and_ctrl_c_stops_waiting() {
    if !python_available() {
        return;
    }
    let fixture = repo().join("src/harness/claude/fixtures/basic_turn.jsonl");
    let setup = Setup::new(&std::fs::read_to_string(fixture).unwrap());
    let (mut unharness, pty) = Pty::spawn(setup.command(&[]));
    wait_until("the TUI is up", Duration::from_secs(20), || {
        pty.shows("Welcome to unharness")
    });
    pty.type_keys(b"hi\r");
    wait_until("the turn is over", Duration::from_secs(20), || {
        pty.shows("last turn")
    });
    let (cli, child) = setup.pids();

    // Between turns the CLI has its grace, which it does not use to exit.
    pty.type_keys(b"\x04");
    let quit = Instant::now();
    wait_until("the wait is shown", Duration::from_secs(5), || {
        pty.shows("Waiting for the agent to exit")
    });
    pty.type_keys(b"\x03");
    assert!(
        unharness.exit_within(Duration::from_secs(15)).is_some(),
        "the TUI did not exit"
    );
    let grace = unharness::core::process::END_GRACE;
    assert!(quit.elapsed() < grace, "{:?}", quit.elapsed());
    assert!(gone_within(cli, Duration::from_secs(2)));
    assert!(gone_within(child, Duration::from_secs(2)));
}

/// The state letter in `/proc/<pid>/stat` (`T` when stopped).
fn state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

// The CLI, in a session of its own, does not get the terminal's SIGTSTP.
#[test]
fn ctrl_z_in_print_stops_the_cli_with_unharness_and_fg_continues_it() {
    if !python_available() {
        return;
    }
    let setup = Setup::new(SILENT);
    let mut cmd = setup.command(&["--print", "hi"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A group of its own, as a shell's job: one whose parent is in
    // another group of the session, which the kernel lets stop.
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let unharness = Unharness(cmd.spawn().unwrap());
    let pid = unharness.id();
    wait_until("the turn is sent", Duration::from_secs(20), || {
        std::fs::read_to_string(setup.path("log")).is_ok_and(|l| l.lines().count() >= 2)
    });
    wait_until("unharness listens", Duration::from_secs(5), || {
        catches(pid, libc::SIGTSTP)
    });
    let (cli, child) = setup.pids();

    // SAFETY: signals to the child this test started.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTSTP) };
    for p in [pid, cli, child] {
        wait_until("all of them are stopped", Duration::from_secs(5), || {
            state(p) == Some('T')
        });
    }
    // `fg`: the shell continues unharness's group, which is unharness.
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGCONT) };
    for p in [pid, cli, child] {
        wait_until("all of them run again", Duration::from_secs(5), || {
            state(p).is_some_and(|s| s != 'T')
        });
    }
}
