//! Built-in, compile-time agent guidance. Dispatch before resolving any connection
//! or opening the profile store; these commands only write embedded text to stdout.
use anyhow::{Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::io::{self, Write};

pub const HELP: &str = "Agent guide (offline): rowboatctl skills get core --full";

const QUICKSTART: &str = include_str!("../skills/core.md");
const FULL: &str = include_str!("../skills/core-full.md");
const DESCRIPTION: &str =
    "Explore databases and execute bounded SQL with reliable machine-readable results";

#[derive(Subcommand)]
pub enum SkillCommand {
    /// List embedded skill IDs; no connection, credentials or profile store needed.
    List {
        /// Emit a JSON array of skill metadata instead of a concise text list.
        #[arg(long)]
        json: bool,
    },
    /// Print a skill's Markdown quickstart; --full prints the complete agent guide.
    Get {
        /// Built-in skill ID (see `rowboatctl skills list`).
        id: String,
        /// Include connection, result, timeout, session and write semantics.
        #[arg(long)]
        full: bool,
    },
}

/// Parse skill commands without connection environment defaults. Dispatch alone
/// is too late for a non-UTF-8 ROWBOAT_URL: clap's String parser would reject it.
/// All other invocations retain the normal CLI parser and environment behavior.
pub(super) fn parse_cli() -> super::Cli {
    let offline = super::Cli::command()
        .mut_arg("url", |arg| arg.env(None::<&str>))
        .mut_arg("ssh_known_hosts", |arg| arg.env(None::<&str>))
        .try_get_matches_from(std::env::args_os());
    match offline {
        Ok(matches) if matches.subcommand_name() == Some("skills") => {
            super::Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
        }
        // Clap handles explicit help/version before parsing environment values.
        // Render them through the original tree, retaining its env-name hints,
        // hidden values, subcommand routing and short/long help formatting.
        _ => super::Cli::parse(),
    }
}

pub fn run(command: &SkillCommand) -> Result<()> {
    let text = match command {
        SkillCommand::List { json: true } => {
            let metadata = serde_json::json!([{
                "id": "core",
                "description": DESCRIPTION,
                "command": "rowboatctl skills get core --full"
            }]);
            format!("{}\n", serde_json::to_string_pretty(&metadata)?)
        }
        SkillCommand::List { json: false } => {
            format!("core  {DESCRIPTION}\n\nRead: rowboatctl skills get core --full\n")
        }
        SkillCommand::Get { id, full } => {
            if id != "core" {
                bail!("Unknown skill {id:?}. Available IDs: core. Run `rowboatctl skills list`.");
            }
            if *full { FULL } else { QUICKSTART }.to_owned()
        }
    };
    io::stdout().lock().write_all(text.as_bytes())?;
    Ok(())
}
