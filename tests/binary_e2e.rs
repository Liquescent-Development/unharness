//! End-to-end tests of the `unharness` binary, with `scripts/fake-harness.py`
//! standing in for Claude Code: how it ends on a signal and at quit, and
//! what it leaves running.

#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
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
        self.command_in("off", args)
    }

    fn command_in(&self, sandbox: &str, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_unharness"));
        cmd.args(["-H", "claude", "--sandbox", sandbox, "--policy", "bypass"])
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
        // unharness keeps a signal it was started with ignored, as from a
        // background job or `nohup`; these tests send them.
        use std::os::unix::process::CommandExt;
        // SAFETY: `signal` is async-signal-safe and allocates nothing.
        unsafe {
            cmd.pre_exec(|| {
                for signo in [
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGHUP,
                    libc::SIGTERM,
                    libc::SIGTSTP,
                ] {
                    libc::signal(signo, libc::SIG_DFL);
                }
                Ok(())
            });
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
    stop: Arc<std::sync::atomic::AtomicBool>,
    reader: std::thread::JoinHandle<()>,
}

impl Pty {
    fn spawn(mut cmd: Command) -> (Unharness, Pty) {
        // Close-on-exec from the start: another test's child, or this
        // one's, holding the master would keep the terminal from hanging up.
        // SAFETY: plain calls on descriptors opened here, owned below.
        let (master, slave) = unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            assert!(master >= 0, "posix_openpt");
            assert_eq!(libc::grantpt(master), 0, "grantpt");
            assert_eq!(libc::unlockpt(master), 0, "unlockpt");
            let mut name = [0 as libc::c_char; 128];
            assert_eq!(
                libc::ptsname_r(master, name.as_mut_ptr(), name.len()),
                0,
                "ptsname"
            );
            let slave = libc::open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
            );
            assert!(slave >= 0, "open the pty");
            let size = libc::winsize {
                ws_row: 40,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::ioctl(slave, libc::TIOCSWINSZ, &size);
            (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
        };
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
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut file = std::fs::File::from(reader);
            let mut chunk = [0u8; 65536];
            use std::io::Read;
            while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                let mut ready = libc::pollfd {
                    fd: file.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one pollfd, on a descriptor owned here.
                match unsafe { libc::poll(&mut ready, 1, 100) } {
                    0 => continue,
                    1.. => {}
                    _ => break,
                }
                // EIO once the TUI is gone.
                let Ok(n @ 1..) = file.read(&mut chunk) else {
                    break;
                };
                into.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        });
        (
            Unharness(child),
            Pty {
                master,
                screen,
                stop,
                reader,
            },
        )
    }

    /// Close the terminal, as a closed window does: the TUI is hung up.
    fn hang_up(self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.reader.join().unwrap();
        drop(self.master);
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

// A CLI killed while Ctrl+Z had it stopped: what it started is not left
// stopped (below the reaper it goes with the CLI; without, it would be
// continued from the list of what was stopped).
#[test]
fn nothing_is_left_stopped_when_a_stopped_cli_dies_meanwhile() {
    if !python_available() {
        return;
    }
    let setup = Setup::new(SILENT);
    let mut cmd = setup.command(&["--print", "hi"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
    // It is in a session of its own, and outlives the CLI's death.
    struct Reap(u32);
    impl Drop for Reap {
        fn drop(&mut self) {
            // SAFETY: the `sleep` the fake CLI started for this test.
            if running(self.0) {
                unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
    let _child = Reap(child);

    // SAFETY: signals to the processes this test started.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTSTP) };
    for p in [pid, cli, child] {
        wait_until("all of them are stopped", Duration::from_secs(5), || {
            state(p) == Some('T')
        });
    }
    // Its parent gone, the `sleep` is no longer under the CLI.
    unsafe { libc::kill(cli as libc::pid_t, libc::SIGKILL) };
    wait_until("the CLI is gone", Duration::from_secs(5), || {
        state(cli).is_none_or(|s| s == 'Z')
    });
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGCONT) };
    // Continued, or gone with the CLI that left it.
    wait_until(
        "the orphan is not left stopped",
        Duration::from_secs(5),
        || state(child) != Some('T'),
    );
}

// While a run ends, its CLI given its grace: Ctrl+Z still stops both.
#[test]
fn ctrl_z_while_print_ends_stops_the_cli_too() {
    if !python_available() {
        return;
    }
    let setup = Setup::new(SILENT);
    let mut cmd = setup.command(&["--print", "hi"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    let mut unharness = Unharness(cmd.spawn().unwrap());
    let pid = unharness.id();
    wait_until("the turn is sent", Duration::from_secs(20), || {
        std::fs::read_to_string(setup.path("log")).is_ok_and(|l| l.lines().count() >= 2)
    });
    wait_until("unharness listens", Duration::from_secs(5), || {
        catches(pid, libc::SIGTERM) && catches(pid, libc::SIGTSTP)
    });
    let (cli, child) = setup.pids();

    // SAFETY: signals to the child this test started.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    // The CLI, which lingers once its stdin closes, is in its grace.
    wait_until("the CLI is asked to end", Duration::from_secs(5), || {
        std::fs::read_to_string(setup.path("log")).is_ok_and(|l| l.contains("interrupt"))
    });
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTSTP) };
    for p in [pid, cli, child] {
        wait_until("all of them are stopped", Duration::from_secs(5), || {
            state(p) == Some('T')
        });
    }
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGCONT) };
    assert!(
        unharness.exit_within(Duration::from_secs(15)).is_some(),
        "the run did not end"
    );
    for p in [cli, child] {
        wait_until(
            "the CLI and what it started are gone",
            Duration::from_secs(5),
            || !running(p),
        );
    }
}

// The editor has the terminal and unharness waits for it; a quit signal
// ends it, and then the TUI with its CLI.
#[test]
fn sigterm_while_the_prompt_is_in_the_editor_ends_the_editor_and_the_tui() {
    if !python_available() {
        return;
    }
    let setup = Setup::new(SILENT);
    let editor_pid = setup.path("editor-pid");
    let mut cmd = setup.command(&[]);
    // An editor that ignores SIGTERM, below the shell it is started from.
    cmd.env(
        "VISUAL",
        format!(
            "sh -c 'trap \"\" TERM; echo $$ > {}; while :; do sleep 0.1; done'",
            editor_pid.display()
        ),
    );
    let (mut unharness, pty) = Pty::spawn(cmd);
    wait_until("the TUI is up", Duration::from_secs(20), || {
        pty.shows("Welcome to unharness")
    });
    pty.type_keys(b"\x07");
    wait_until("the editor runs", Duration::from_secs(10), || {
        editor_pid.exists()
    });
    let editor: u32 = std::fs::read_to_string(&editor_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    struct Reap(u32);
    impl Drop for Reap {
        fn drop(&mut self) {
            // SAFETY: the editor this test started.
            if running(self.0) {
                unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
    let _reap = Reap(editor);

    // SAFETY: a signal to the child this test started.
    unsafe { libc::kill(unharness.id() as libc::pid_t, libc::SIGTERM) };
    assert!(
        unharness.exit_within(Duration::from_secs(15)).is_some(),
        "the TUI did not exit"
    );
    assert!(
        gone_within(editor, Duration::from_secs(2)),
        "the editor runs on"
    );
}

// A CLI that exits by itself leaves nothing behind, also not what it put
// in a session of its own, which init would otherwise adopt.
#[test]
fn what_a_cli_left_in_a_session_of_its_own_goes_when_it_exits_by_itself() {
    if !python_available() {
        return;
    }
    let setup = Setup::new(SILENT);
    let mut cmd = setup.command(&["--print", "hi"]);
    cmd.env("UNHARNESS_FAKE_HANG", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut unharness = Unharness(cmd.spawn().unwrap());
    wait_until("the CLI started its child", Duration::from_secs(20), || {
        setup.path("pids").exists()
    });
    let (cli, child) = setup.pids();
    struct Reap(u32);
    impl Drop for Reap {
        fn drop(&mut self) {
            // SAFETY: the `sleep` the fake CLI started for this test.
            if running(self.0) {
                unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
            }
        }
    }
    let _child = Reap(child);

    assert!(
        unharness.exit_within(Duration::from_secs(15)).is_some(),
        "the run did not end"
    );
    assert!(gone_within(cli, Duration::from_secs(2)));
    assert!(gone_within(child, Duration::from_secs(2)), "left running");
}

// Reading the directory unharness is in denied (`~/.cargo`, for its
// credentials), a sandboxed CLI still starts below its reaper: the binary
// alone is executable there.
#[test]
fn a_sandboxed_cli_runs_below_its_reaper_where_unharness_s_directory_is_denied() {
    let landlock = std::fs::read_to_string("/sys/kernel/security/lsm")
        .is_ok_and(|lsm| lsm.split(',').any(|m| m.trim() == "landlock"));
    if !python_available() || !landlock {
        return;
    }
    let setup = Setup::new(SILENT);
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_unharness")).parent().unwrap();
    let config = setup.path("config/unharness/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[sandbox]\ndeny_read = [\"{}\"]\n",
        bin_dir.display()
    ));
    std::fs::write(&config, text).unwrap();
    // Where the sandbox lets the fake write.
    let log = setup.path("ws/log");
    let pids = setup.path("ws/pids");
    let mut cmd = setup.command_in("workspace-write", &["--print", "hi"]);
    cmd.env("UNHARNESS_FAKE_LOG", &log)
        .env(
            "UNHARNESS_FAKE_BACKGROUND",
            format!("group:{}", pids.display()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let unharness = Unharness(cmd.spawn().unwrap());
    wait_until("the turn is sent", Duration::from_secs(20), || {
        std::fs::read_to_string(&log).is_ok_and(|l| l.lines().count() >= 2)
    });
    let text = std::fs::read_to_string(&pids).unwrap();
    let cli: u32 = text.split_whitespace().next().unwrap().parse().unwrap();
    let parent = std::fs::read_to_string(format!("/proc/{cli}/stat"))
        .unwrap()
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    let reaper = std::fs::read(format!("/proc/{parent}/cmdline")).unwrap();
    assert!(
        String::from_utf8_lossy(&reaper).contains("__unharness-reap"),
        "{:?}",
        String::from_utf8_lossy(&reaper)
    );
    drop(unharness);
}

// A closed terminal while the prompt is in the editor: the TUI cannot
// take the terminal back, and still ends its session the orderly way.
#[test]
fn a_hangup_while_the_prompt_is_in_the_editor_still_ends_the_session_in_order() {
    if !python_available() {
        return;
    }
    let fixture = repo().join("src/harness/claude/fixtures/basic_turn.jsonl");
    let setup = Setup::new(&std::fs::read_to_string(fixture).unwrap());
    let editor_pid = setup.path("editor-pid");
    let mut cmd = setup.command(&[]);
    cmd.env(
        "VISUAL",
        format!(
            "sh -c 'echo $$ > {}; while :; do sleep 0.1; done'",
            editor_pid.display()
        ),
    );
    let (mut unharness, pty) = Pty::spawn(cmd);
    wait_until("the TUI is up", Duration::from_secs(20), || {
        pty.shows("Welcome to unharness")
    });
    pty.type_keys(b"hi\r");
    wait_until("the turn is over", Duration::from_secs(20), || {
        pty.shows("last turn")
    });
    let (cli, _) = setup.pids();
    pty.type_keys(b"\x07");
    wait_until("the editor runs", Duration::from_secs(10), || {
        editor_pid.exists()
    });
    let editor: u32 = std::fs::read_to_string(&editor_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    pty.hang_up();
    assert!(
        unharness.exit_within(Duration::from_secs(15)).is_some(),
        "the TUI did not exit"
    );
    let sent = std::fs::read_to_string(setup.path("log")).unwrap();
    assert!(sent.contains("interrupt"), "no orderly end: {sent}");
    assert!(gone_within(cli, Duration::from_secs(2)));
    assert!(
        gone_within(editor, Duration::from_secs(2)),
        "the editor runs on"
    );
}
