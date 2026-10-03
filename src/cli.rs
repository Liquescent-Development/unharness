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

    /// Select AI harness (agy, claude, codex)
    #[arg(short = 'H', long, env = "UNHARNESS_HARNESS")]
    pub harness: Option<String>,

    /// Run single prompt non-interactively and print response
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Auto-approve permissions / dangerously skip permissions
    #[arg(short = 'y', long, alias = "dangerously-skip-permissions")]
    pub auto: bool,

    /// Force interactive mode
    #[arg(short = 'i', long)]
    pub interactive: bool,

    /// Override model for the session
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Reasoning effort (harness-specific, e.g. low, medium, high, xhigh, max)
    #[arg(short = 'e', long)]
    pub effort: Option<String>,

    /// Output format for print mode (text, json, stream-json)
    #[arg(long)]
    pub format: Option<String>,

    /// Skip unharness TUI and run directly in the underlying harness CLI
    #[arg(long, alias = "raw")]
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

    /// Manage skills via the `skills` CLI (e.g. `unharness skills add owner/repo`)
    Skills {
        /// Arguments passed through to `npx skills`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Switch default harness in config (agy, claude, codex)
    Switch {
        /// Target harness name
        harness: String,

        /// Apply change globally to ~/.config/unharness/config.toml
        #[arg(short, long)]
        global: bool,
    },

    /// Explicitly run a prompt through a harness
    Run(CommonRunArgs),
}
