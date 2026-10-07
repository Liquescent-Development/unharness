//! A harness CLI's own reaper (Linux). unharness's binary, started in the
//! CLI's place, makes itself the subreaper of everything below it, runs the
//! CLI on the same pipes, and once the CLI exits kills what is left under
//! it. Without it, what the CLI put in a session of its own, or forked
//! away twice, is adopted by init when the CLI exits by itself, out of
//! every tree unharness can find, and outlives the session.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The first argument that makes unharness a reaper.
pub const ARG: &str = "__unharness-reap";

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

/// `cmd`, run through the reaper once that is enabled. A CLI that cannot
/// be found fails here, as it would when spawned bare.
pub fn wrap(cmd: std::process::Command) -> std::io::Result<std::process::Command> {
    let Some(exe) = EXE.get() else {
        return Ok(cmd);
    };
    let program = cmd.get_program();
    let path = cmd
        .get_envs()
        .find(|(k, _)| *k == "PATH")
        .and_then(|(_, v)| v.map(OsStr::to_os_string));
    if !found(program, path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory",
        ));
    }
    let mut outer = std::process::Command::new(exe);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // As `ps` shows it.
        outer.arg0("unharness");
    }
    outer.arg(ARG).arg("--").arg(program).args(cmd.get_args());
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => outer.env(key, value),
            None => outer.env_remove(key),
        };
    }
    if let Some(dir) = cmd.get_current_dir() {
        outer.current_dir(dir);
    }
    Ok(outer)
}

/// Whether `program` names an executable file, the way `execvp` looks.
/// One given relative to a directory is left to the reaper to find.
fn found(program: &OsStr, path: Option<OsString>) -> bool {
    let given = Path::new(program);
    if given.is_absolute() {
        return executable(given);
    }
    if given.components().count() > 1 {
        return true;
    }
    let path = path
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    std::env::split_paths(&path).any(|dir| executable(&dir.join(given)))
}

fn executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

/// Run as the reaper: `<exe> __unharness-reap -- <program> <args>…`. Called
/// before anything else in `main`, while this is the only thread, and
/// never returns: it exits as the CLI did.
#[cfg(target_os = "linux")]
pub fn main() -> ! {
    use std::process::exit;

    let mut args = std::env::args_os().skip(2);
    if args.next().as_deref() != Some(OsStr::new("--")) {
        exit(2);
    }
    let Some(program) = args.next() else { exit(2) };
    let rest: Vec<OsString> = args.collect();

    // SAFETY: plain syscalls on this process, before any other thread.
    let set = unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in FORWARDED.iter().chain(&[libc::SIGCHLD]) {
            libc::sigaddset(&mut set, *sig);
        }
        // Taken one at a time below. The CLI starts with none blocked:
        // `Command` clears the mask in the child.
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    let cli = match std::process::Command::new(&program).args(&rest).spawn() {
        Ok(child) => child.id() as libc::pid_t,
        Err(e) => {
            eprintln!("failed to spawn {program:?}: {e}");
            exit(127);
        }
    };
    // SAFETY: plain syscalls on this process's own descriptors.
    unsafe {
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
            if let Some(status) = reap(Some(cli)) {
                break status;
            }
        } else if sig > 0 {
            // SAFETY: a signal to the CLI this started.
            unsafe { libc::kill(cli, sig) };
        }
    };

    // What it left below: all of it is here, whatever session it is in.
    super::process::kill_below(std::process::id());
    while reap(None).is_some() {}

    // SAFETY: plain syscalls on this process.
    unsafe {
        if libc::WIFSIGNALED(status) {
            let sig = libc::WTERMSIG(status);
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

/// Reap every child that has exited. With `cli`, the CLI's status if it
/// was among them; without, a blocking wait for the next one.
#[cfg(target_os = "linux")]
fn reap(cli: Option<libc::pid_t>) -> Option<libc::c_int> {
    let mut found = None;
    loop {
        let mut status = 0;
        let flags = if cli.is_some() { libc::WNOHANG } else { 0 };
        // SAFETY: waits for this process's own children.
        let pid = unsafe { libc::waitpid(-1, &mut status, flags) };
        if pid > 0 && Some(pid) == cli {
            found = Some(status);
        } else if pid > 0 && cli.is_none() {
            return Some(status);
        } else if pid == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        } else if pid <= 0 {
            return found;
        }
    }
}
