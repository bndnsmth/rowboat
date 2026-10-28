//! Offline evidence packaging and explicit delegation; never opens a database.
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand, ValueEnum};
use rowboat_services::{
    Store,
    ai::{self, AgentControl, AgentProvider, AiPreferences},
};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, ValueEnum)]
pub enum Provider {
    Codex,
    Claude,
    Opencode,
}
impl From<Provider> for AgentProvider {
    fn from(value: Provider) -> Self {
        match value {
            Provider::Codex => Self::Codex,
            Provider::Claude => Self::Claude,
            Provider::Opencode => Self::Opencode,
        }
    }
}
#[derive(Args)]
pub struct AgentArgs {
    /// Override the saved agent; also explicitly enables AI for this invocation.
    #[arg(long, value_enum)]
    agent: Option<Provider>,
    /// Absolute executable path, not a shell command.
    #[arg(long)]
    executable: Option<String>,
    /// Agent-specific model override.
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u64).range(15..=600))]
    deadline: Option<u64>,
}
#[derive(Subcommand)]
pub enum AiCommand {
    /// Check the installed CLI's automation controls; no model request.
    Check {
        #[command(flatten)]
        agent: AgentArgs,
    },
    /// Package existing files for review; no SQL is executed and nothing is sent.
    Context {
        #[arg(long, value_enum)]
        engine: crate::Dialect,
        #[arg(long)]
        sql: PathBuf,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        schema: Option<PathBuf>,
        #[arg(long)]
        metrics: Option<PathBuf>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Send a reviewed file to your installed agent. Advice only; never applies SQL.
    Analyze {
        #[arg(long)]
        context: PathBuf,
        /// Confirm you reviewed the file and consent to the agent/provider receiving it.
        #[arg(long)]
        confirm_share: bool,
        #[command(flatten)]
        agent: AgentArgs,
        /// New private file; existing destinations are preserved. Default: stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}
fn read_context(path: &Path) -> Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .context("Open AI context source")?;
    ensure!(
        file.metadata()?.is_file(),
        "AI context source must be a regular file"
    );
    let mut content = String::new();
    file.take((ai::CONTEXT_LIMIT + 1) as u64)
        .read_to_string(&mut content)?;
    ensure!(
        content.len() <= ai::CONTEXT_LIMIT,
        "AI context source exceeds 192 KiB"
    );
    Ok(content)
}
fn settings(args: &AgentArgs, connection: Option<&str>) -> Result<AiPreferences> {
    let state = Store::open()?.load()?;
    let mut settings = state.preferences.ai;
    if let Some(connection) = connection {
        let profile = crate::profiles::select(&state.connections, connection)?;
        ensure!(
            !settings.disabled_connections.contains(&profile.id),
            "AI is disabled for this saved connection"
        );
    }
    if let Some(agent) = args.agent {
        settings.provider = agent.into();
        // A different provider must not silently reuse the saved provider's path/model.
        settings.executable.clear();
        settings.model.clear();
        settings.enabled = true;
    }
    if let Some(executable) = &args.executable {
        settings.executable = executable.clone();
    }
    if let Some(model) = &args.model {
        settings.model = model.clone();
    }
    if let Some(deadline) = args.deadline {
        settings.timeout_secs = deadline;
    }
    settings.validate()?;
    Ok(settings)
}
pub async fn run(command: &AiCommand, connection: Option<&str>) -> Result<()> {
    if let AiCommand::Context {
        engine,
        sql,
        plan,
        schema,
        metrics,
        output,
    } = command
    {
        let context = ai::performance_context(
            (*engine).into(),
            &read_context(sql)?,
            &read_context(plan)?,
            schema.as_deref().map(read_context).transpose()?.as_deref(),
            metrics.as_deref().map(read_context).transpose()?.as_deref(),
        )?;
        return crate::output(&format!("{context}\n"), output.clone());
    }
    let args = match command {
        AiCommand::Check { agent } | AiCommand::Analyze { agent, .. } => agent,
        _ => unreachable!(),
    };
    if let AiCommand::Analyze { confirm_share, .. } = command {
        ensure!(
            *confirm_share,
            "Review --context first, then supply --confirm-share. Your agent may send this content to a cloud provider and retain logs."
        );
    }
    if let AiCommand::Analyze {
        output: Some(path), ..
    } = command
    {
        match std::fs::symlink_metadata(path) {
            Ok(_) => anyhow::bail!("Advice destination already exists; no context was shared"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Inspect advice destination before sharing"),
        }
    }
    let settings = settings(args, connection)?;
    let control = AgentControl::default();
    let cancellation = control.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancellation.cancel();
        }
    });
    let result = match command {
        AiCommand::Check { .. } => match ai::check_agent(&settings, &control).await {
            Ok(message) => crate::output(&format!("{message}\n"), None),
            Err(error) => Err(error),
        },
        AiCommand::Analyze {
            context, output, ..
        } => match read_context(context) {
            Ok(context) => match ai::analyze(&settings, &context, &control, |_| {}).await {
                Ok(advice) => crate::output(&format!("{advice}\n"), output.clone()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        },
        _ => unreachable!(),
    };
    signal.abort();
    result
}
