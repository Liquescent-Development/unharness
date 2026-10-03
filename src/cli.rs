use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "unharness",
    author = "Richard Kiene",
    version = "0.1.0",
    about = "Vendor-neutral CLI orchestrator and skill synchronizer for AI coding agents (agy, claude, codex)",
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

    /// Reasoning effort (low, medium, high, xhigh, max)
    #[arg(short = 'e', long)]
    pub effort: Option<String>,

    /// Output format for print mode (text, json, stream-json)
    #[arg(long)]
    pub format: Option<String>,

    /// Skip unharness TUI and run directly in the underlying harness CLI
    #[arg(long, alias = "raw")]
    pub no_tui: bool,

    /// Skip pre-flight sync of skills and rules
    #[arg(long)]
    pub no_sync: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize unharness in current repository (AGENTS.md, .agents/skills, symlinks)
    Init,

    /// Synchronize skills and rules across harnesses (.claude, .agents, .gemini)
    Sync {
        /// Sync workspace skills and rules only
        #[arg(short = 'w', long)]
        workspace: bool,

        /// Sync global user skills only
        #[arg(short = 'g', long)]
        global: bool,
    },

    /// Health check: detect installed harnesses, auth status, active skills, symlinks
    Doctor,

    /// Manage, list, and validate skills
    Skills {
        #[command(subcommand)]
        cmd: SkillsSubcommand,
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

#[derive(Subcommand, Debug)]
pub enum SkillsSubcommand {
    /// List all available skills (workspace and global)
    List,

    /// Create a new portable skill template with valid frontmatter
    Create {
        /// Name of the new skill (kebab-case)
        name: String,

        /// Create in user global skills (~/.agents/skills) instead of workspace
        #[arg(short, long)]
        global: bool,
    },

    /// Validate all SKILL.md files for valid frontmatter and metadata
    Validate,

    /// Synchronize skills across harness directories
    Sync,
}
