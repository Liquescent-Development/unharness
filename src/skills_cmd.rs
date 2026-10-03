//! `unharness skills ...` delegates to the Vercel `skills` CLI
//! (<https://github.com/vercel-labs/skills>), which installs skills into
//! `.agents/skills` and projects them into every supported agent's directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

use crate::harness::which;

/// How the `skills` CLI will be invoked on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsCli {
    /// A `skills` binary is on PATH.
    Installed(PathBuf),
    /// No binary, but `npx` can fetch and run it.
    ViaNpx(PathBuf),
    /// Neither `skills` nor `npx` is available.
    Missing,
}

impl SkillsCli {
    pub fn detect() -> Self {
        if let Some(p) = which("skills") {
            return SkillsCli::Installed(p);
        }
        if let Some(p) = which("npx") {
            return SkillsCli::ViaNpx(p);
        }
        SkillsCli::Missing
    }

    pub fn describe(&self) -> String {
        match self {
            SkillsCli::Installed(p) => format!("skills CLI at {}", p.display()),
            SkillsCli::ViaNpx(p) => format!("via npx ({}); downloads on first use", p.display()),
            SkillsCli::Missing => {
                "not available (install Node.js or `npm i -g skills`)".to_string()
            }
        }
    }

    pub fn command(&self) -> Result<Command> {
        match self {
            SkillsCli::Installed(p) => Ok(Command::new(p)),
            SkillsCli::ViaNpx(p) => {
                let mut c = Command::new(p);
                c.arg("--yes").arg("skills");
                Ok(c)
            }
            SkillsCli::Missing => bail!(
                "The `skills` CLI is not available. Install Node.js (for `npx skills`) or `npm install -g skills`."
            ),
        }
    }
}

/// Replace the current process with `skills <args...>` (falls back to a child
/// process on non-unix platforms).
pub fn run_skills(cwd: &Path, args: &[String]) -> Result<()> {
    let mut cmd = SkillsCli::detect().command()?;
    cmd.args(args).current_dir(cwd);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        bail!("Failed to run skills CLI: {}", err);
    }

    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}
