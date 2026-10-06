use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "unharness",
    author = "Richard Kiene",
    version,
    about = "Vendor-neutral TUI and CLI runner for AI coding agents (agy, claude, codex, pi)",
    long_about = None
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    #[command(flatten)]
    pub run_args: CommonRunArgs,
}

#[derive(Args, Debug, Clone, Default)]
pub struct CommonRunArgs {
    /// Initial prompt to pass to the harness
    #[arg(trailing_var_arg = true)]
    pub prompt: Vec<String>,

    /// Select AI harness (agy, claude, codex, pi)
    #[arg(short = 'H', long, env = "UNHARNESS_HARNESS")]
    pub harness: Option<String>,

    /// Run single prompt non-interactively and print response
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Permission policy: ask, accept-edits, auto, bypass
    #[arg(long, env = "UNHARNESS_POLICY")]
    pub policy: Option<String>,

    /// Sandbox around the harness: read-only, workspace-write, off
    #[arg(long, env = "UNHARNESS_SANDBOX")]
    pub sandbox: Option<String>,

    /// Shorthand for --policy bypass (dangerously skip all permissions)
    #[arg(short = 'y', long = "yes")]
    pub auto: bool,

    /// Provider within the harness (e.g. anthropic, openai, google; pi supports many)
    #[arg(long)]
    pub provider: Option<String>,

    /// Override model for the session
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Reasoning effort (harness-specific, e.g. low, medium, high, xhigh, max)
    #[arg(short = 'e', long)]
    pub effort: Option<String>,

    /// Resume a saved conversation by id (prefix ok), or the most recent one when no id is given
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub resume: Option<String>,

    /// Output format for print mode (text, json, stream-json)
    #[arg(long)]
    pub format: Option<String>,

    /// Skip unharness TUI and run directly in the underlying harness CLI
    #[arg(long)]
    pub no_tui: bool,

    /// Skip pre-flight sync of rules symlinks
    #[arg(long)]
    pub no_sync: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize unharness in current repository (AGENTS.md, .agents/skills, symlinks)
    Init,

    /// Synchronize rules symlinks (CLAUDE.md, GEMINI.md -> AGENTS.md)
    Sync,

    /// Health check: detect installed harnesses, auth status, skills CLI, symlinks
    Doctor,

    /// Manage skills via the `skills` CLI (add, list, update, remove, find, init),
    /// or `import` ones the installed harnesses already have
    /// (`unharness skills import [--from claude,codex,pi] [-g] [--all]`)
    #[command(disable_help_flag = true)]
    Skills {
        /// Arguments passed through to `npx skills`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// List providers and models per harness
    Models {
        /// Only this harness
        #[arg(short = 'H', long)]
        harness: Option<String>,

        /// Only this provider
        #[arg(long)]
        provider: Option<String>,
    },

    /// List or clear saved conversations for this workspace
    #[command(alias = "conversations")]
    Sessions {
        /// Remove all saved conversations
        #[arg(long)]
        clear: bool,
    },

    /// Switch default harness in config (agy, claude, codex, pi)
    Switch {
        /// Target harness name
        harness: String,

        /// Apply change globally to ~/.config/unharness/config.toml
        #[arg(short, long)]
        global: bool,
    },

    /// Explicitly run a prompt through a harness
    Run(CommonRunArgs),

    /// Update unharness itself (a shell-installer install; others are told
    /// what to run)
    Update {
        /// Install this version instead of the latest (e.g. 0.3.0)
        #[arg(conflicts_with_all = ["check", "prerelease"])]
        version: Option<String>,

        /// Only report whether a newer release exists
        #[arg(long)]
        check: bool,

        /// Include prereleases
        #[arg(long)]
        prerelease: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_flag_forms() {
        let c = Cli::parse_from(["unharness", "--resume"]);
        assert_eq!(c.run_args.resume.as_deref(), Some(""));
        let c = Cli::parse_from(["unharness", "--resume", "abc"]);
        assert_eq!(c.run_args.resume.as_deref(), Some("abc"));
        let c = Cli::parse_from(["unharness", "-y", "--policy", "ask", "do", "it"]);
        assert!(c.run_args.auto);
        assert_eq!(c.run_args.policy.as_deref(), Some("ask"));
        assert_eq!(c.run_args.prompt, vec!["do", "it"]);
        let c = Cli::parse_from(["unharness", "skills", "add", "--global", "x/y"]);
        match c.command {
            Some(Commands::Skills { args }) => assert_eq!(args, vec!["add", "--global", "x/y"]),
            other => panic!("{other:?}"),
        }
        // --help belongs to the skills CLI, not to us.
        let c = Cli::parse_from(["unharness", "skills", "--help"]);
        match c.command {
            Some(Commands::Skills { args }) => assert_eq!(args, vec!["--help"]),
            other => panic!("{other:?}"),
        }
    }
}
