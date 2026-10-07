//! A harness CLI's own reaper (Linux). unharness's binary, started in the
//! CLI's place, makes itself the subreaper of everything below it, runs the
//! CLI on the same pipes, and once the CLI exits kills what is left under
//! it. Without it, what the CLI put in a session of its own, or forked
//! away twice, is adopted by init when the CLI exits by itself, out of
//! every tree unharness can find, and outlives the session.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The first argument that makes unharness a reaper.
pub const ARG: &str = "__unharness-reap";

/// The descriptor a reaper reports on whether it started the CLI.
#[cfg(target_os = "linux")]
const STATUS_FD: libc::c_int = 3;

static EXE: OnceLock<PathBuf> = OnceLock::new();

/// Start CLIs through this binary from now on: unharness's `main` does.
/// A test binary is not unharness, so its CLIs are started bare. The
/// child execs the image it was forked from, also once an update has
/// replaced the file it came from, so the reaper is always this version.
pub fn enable() {
    #[cfg(target_os = "linux")]
    if std::fs::metadata("/proc/self/exe").is_ok() {
        let _ = EXE.set(PathBuf::from("/proc/self/exe"));
    }
}

/// Start CLIs through `exe`, an unharness binary: for a test binary, which
/// cannot be a reaper itself.
pub fn enable_with(exe: PathBuf) {
    #[cfg(target_os = "linux")]
    let _ = EXE.set(exe);
    #[cfg(not(target_os = "linux"))]
    let _ = exe;
}

/// The file the running unharness was started from, once the reaper is
/// enabled: the sandbox lets a reaper execute it also where reading its
/// directory is denied.
pub fn image() -> Option<PathBuf> {
    std::fs::canonicalize(EXE.get()?).ok()
}

/// What tells, once a reaper from [`wrap`] is spawned, whether it started
/// the CLI.
#[cfg(target_os = "linux")]
pub struct Reaped {
    status: std::os::fd::OwnedFd,
    report: Option<std::os::fd::OwnedFd>,
}

/// `cmd` run through the reaper, once that is enabled. Its program, its
/// arguments, its changes to the environment and its directory are kept;
/// nothing else set on it is.
#[cfg(target_os = "linux")]
pub fn wrap(
    cmd: &std::process::Command,
) -> std::io::Result<Option<(std::process::Command, Reaped)>> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    let Some(exe) = EXE.get() else {
        return Ok(None);
    };
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 returns.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both were just opened and are owned here alone.
    let (status, report) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

    let mut outer = std::process::Command::new(exe);
    // As `ps` shows it.
    outer.arg0("unharness");
    outer
        .arg(ARG)
        .arg("--")
        .arg(cmd.get_program())
        .args(cmd.get_args());
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => outer.env(key, value),
            None => outer.env_remove(key),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        outer.current_dir(dir);
    }
    let fd = report.as_raw_fd();
    // SAFETY: dup2 and fcntl are async-signal-safe and allocate nothing.
    unsafe {
        outer.pre_exec(move || {
            let moved = if fd == STATUS_FD {
                libc::fcntl(fd, libc::F_SETFD, 0)
            } else {
                libc::dup2(fd, STATUS_FD)
            };
            if moved == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(Some((
        outer,
        Reaped {
            status,
            report: Some(report),
        },
    )))
}

#[cfg(target_os = "linux")]
impl Reaped {
    /// Once the reaper is spawned: whether it started the CLI, with the
    /// error its spawn gave otherwise (the reaper then exits).
    pub fn started(mut self) -> std::io::Result<()> {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        drop(self.report.take());
        let mut poll = libc::pollfd {
            fd: self.status.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // A reaper that says nothing in time (stopped, say) is taken to
        // have started it; its exit is reported as the CLI's.
        // SAFETY: one pollfd, owned here.
        if unsafe { libc::poll(&mut poll, 1, 5000) } <= 0 {
            return Ok(());
        }
        let mut said = Vec::new();
        let _ = std::fs::File::from(self.status).read_to_end(&mut said);
        if said.len() < 4 {
            return Ok(());
        }
        let errno = i32::from_ne_bytes([said[0], said[1], said[2], said[3]]);
        Err(std::io::Error::from_raw_os_error(errno))
    }
}

/// Run as the reaper: `<exe> __unharness-reap -- <program> <args>…`. Called
/// before anything else in `main`, while this is the only thread, and
/// never returns: it exits as the CLI did.
#[cfg(target_os = "linux")]
pub fn main() -> ! {
    use std::ffi::{OsStr, OsString};
    use std::os::unix::process::CommandExt;
    use std::process::exit;

    let mut args = std::env::args_os().skip(2);
    if args.next().as_deref() != Some(OsStr::new("--")) {
        exit(2);
    }
    let Some(program) = args.next() else { exit(2) };
    let rest: Vec<OsString> = args.collect();

    // SAFETY: plain syscalls on this process, before any other thread.
    let (set, status) = unsafe {
        // Not passed on to the CLI; written to only if it cannot start.
        let status = libc::fcntl(STATUS_FD, libc::F_SETFD, libc::FD_CLOEXEC) != -1;
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in FORWARDED.iter().chain(&[libc::SIGCHLD]) {
            libc::sigaddset(&mut set, *sig);
        }
        // Taken one at a time below.
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        (set, status)
    };
    let mut command = std::process::Command::new(&program);
    command.args(&rest);
    // SAFETY: sigemptyset and pthread_sigmask are async-signal-safe and
    // allocate nothing.
    unsafe {
        // The mask set above is inherited across exec; the CLI and what
        // it starts must get these signals.
        command.pre_exec(|| {
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    let cli = match command.spawn() {
        Ok(child) => child.id() as libc::pid_t,
        Err(e) => {
            if status {
                let errno = e.raw_os_error().unwrap_or(libc::ENOENT).to_ne_bytes();
                // SAFETY: a write of four bytes from a local.
                unsafe { libc::write(STATUS_FD, errno.as_ptr().cast(), errno.len()) };
            }
            exit(127);
        }
    };
    // SAFETY: plain syscalls on this process's own descriptors.
    unsafe {
        if status {
            libc::close(STATUS_FD);
        }
        // None of the CLI's pipes is held here: they close when the CLI
        // and what it started close them, as without the reaper.
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
            if null > 2 {
                libc::close(null);
            }
        }
    }

    let status = loop {
        // SAFETY: waits on the set blocked above.
        let sig = unsafe { libc::sigwaitinfo(&set, std::ptr::null_mut()) };
        if sig == libc::SIGCHLD {
            if let Some(status) = reap_cli(cli) {
                break status;
            }
        } else if sig > 0 {
            // SAFETY: a signal to the CLI this started.
            unsafe { libc::kill(cli, sig) };
        }
    };

    // What it left below: all of it is here, whatever session it is in.
    super::process::kill_below(std::process::id());
    reap_left(std::time::Duration::from_secs(2));

    // SAFETY: plain syscalls on this process.
    unsafe {
        if libc::WIFSIGNALED(status) {
            let sig = libc::WTERMSIG(status);
            // Died as the CLI did, without a core of unharness's own.
            let none = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &none);
            libc::signal(sig, libc::SIG_DFL);
            let mut only: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut only);
            libc::sigaddset(&mut only, sig);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &only, std::ptr::null_mut());
            libc::raise(sig);
            exit(128 + sig);
        }
        exit(libc::WEXITSTATUS(status));
    }
}

/// What the reaper passes on to the CLI.
#[cfg(target_os = "linux")]
const FORWARDED: [libc::c_int; 7] = [
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGWINCH,
];

/// Reap every child that has exited; the CLI's status if it was one.
#[cfg(target_os = "linux")]
fn reap_cli(cli: libc::pid_t) -> Option<libc::c_int> {
    let mut found = None;
    loop {
        let mut status = 0;
        // SAFETY: waits for this process's own children.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        let interrupted =
            pid == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
        if pid == cli {
            found = Some(status);
        } else if !(pid > 0 || interrupted) {
            return found;
        }
    }
}

/// Reap what was killed, for at most `within`: one that cannot die (stuck
/// in the kernel) does not hold back the CLI's exit.
#[cfg(target_os = "linux")]
fn reap_left(within: std::time::Duration) {
    let deadline = std::time::Instant::now() + within;
    loop {
        // SAFETY: waits for this process's own children.
        let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if pid > 0 {
            continue;
        }
        let interrupted =
            pid == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR);
        // None left (ECHILD), or the time is up.
        if (pid == -1 && !interrupted) || std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
