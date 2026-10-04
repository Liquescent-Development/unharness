//! Putting selected text on the system clipboard.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use base64::Engine;

/// How the text got to the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copied {
    /// A clipboard tool took it and reported success.
    Tool,
    /// Sent to the terminal as OSC 52. Whether the terminal honoured it
    /// cannot be known.
    Terminal,
}

/// Terminals cap an OSC 52 payload; stay under the common limit.
const OSC52_MAX: usize = 100_000;

/// What this process can see of the desktop it runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Desktop {
    pub macos: bool,
    pub wayland: bool,
    pub x11: bool,
    /// Over ssh the local tools would fill the remote machine's clipboard.
    pub remote: bool,
}

impl Desktop {
    pub fn detect() -> Self {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        Desktop {
            macos: cfg!(target_os = "macos"),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            remote: set("SSH_CONNECTION") || set("SSH_TTY"),
        }
    }
}

/// The clipboard tools worth trying, best first.
pub fn tools(desktop: Desktop) -> Vec<(&'static str, &'static [&'static str])> {
    let mut out: Vec<(&'static str, &'static [&'static str])> = Vec::new();
    if desktop.remote {
        return out;
    }
    if desktop.macos {
        out.push(("pbcopy", &[]));
    }
    if desktop.wayland {
        out.push(("wl-copy", &[]));
    }
    if desktop.x11 {
        out.push(("xclip", &["-selection", "clipboard"]));
        out.push(("xsel", &["--clipboard", "--input"]));
    }
    out
}

/// The OSC 52 sequence that asks the terminal to set the clipboard.
pub fn osc52(text: &str) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    (encoded.len() <= OSC52_MAX).then(|| format!("\x1b]52;c;{encoded}\x07"))
}

fn run(tool: &str, args: &[&str], text: &str) -> bool {
    let Ok(mut child) = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|status| status.success()) && wrote
}

/// Copy `text`: with a clipboard tool where one works, else through the
/// terminal, written to `terminal`.
pub fn copy(text: &str, desktop: Desktop, terminal: &mut impl Write) -> Result<Copied> {
    for (tool, args) in tools(desktop) {
        if run(tool, args, text) {
            return Ok(Copied::Tool);
        }
    }
    let Some(sequence) = osc52(text) else {
        bail!("selection is too large to copy through the terminal");
    };
    terminal.write_all(sequence.as_bytes())?;
    terminal.flush()?;
    Ok(Copied::Terminal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_follow_the_desktop_and_never_run_over_ssh() {
        let names = |d: Desktop| tools(d).iter().map(|t| t.0).collect::<Vec<_>>();
        assert!(names(Desktop::default()).is_empty());
        assert_eq!(
            names(Desktop {
                wayland: true,
                x11: true,
                ..Default::default()
            }),
            ["wl-copy", "xclip", "xsel"]
        );
        assert_eq!(
            names(Desktop {
                macos: true,
                ..Default::default()
            }),
            ["pbcopy"]
        );
        assert!(
            names(Desktop {
                wayland: true,
                remote: true,
                ..Default::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn falls_back_to_osc52_and_refuses_what_will_not_fit() {
        let mut out = Vec::new();
        let how = copy("héllo\nworld", Desktop::default(), &mut out).unwrap();
        assert_eq!(how, Copied::Terminal);
        assert_eq!(out, b"\x1b]52;c;aMOpbGxvCndvcmxk\x07");

        let mut out = Vec::new();
        assert!(copy(&"x".repeat(OSC52_MAX), Desktop::default(), &mut out).is_err());
        assert!(out.is_empty());
    }
}
