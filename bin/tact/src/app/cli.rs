//! Command-line parsing and dispatch.

use crate::{
    app::{
        config::{AuthMode, Config, ConfigOverrides, ReasoningEffort, ReasoningMode},
        error::{AuthResult, Error, Result, RuntimeError},
        shutdown, update,
    },
    core::ConfiguredAgent,
    tui,
};
use clap::{ArgAction, Parser, Subcommand, builder::NonEmptyStringValueParser};
use crossterm::style::{Color, Stylize};
use nanocodex::Model;
use std::{env, env::VarError, fmt, path::PathBuf};
use tokio_util::sync::CancellationToken;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const BUILD_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit: ",
    env!("TACT_GIT_SHA"),
    " (",
    env!("TACT_GIT_BRANCH"),
    ", ",
    env!("TACT_GIT_DIRTY"),
    ")\ncommit timestamp: ",
    env!("TACT_GIT_COMMIT_TIMESTAMP"),
    "\nbuild timestamp: ",
    env!("TACT_BUILD_TIMESTAMP"),
    "\ntarget: ",
    env!("TACT_BUILD_TARGET"),
    "\nprofile: ",
    env!("TACT_BUILD_PROFILE"),
    "\nrustc: ",
    env!("TACT_RUSTC_VERSION"),
);

/// Command-line interface for `tact`.
#[derive(Debug, Parser)]
#[command(
    version,
    long_version = BUILD_VERSION,
    about = "A terminal interface for Nanocodex",
    subcommand_negates_reqs = true
)]
pub(crate) struct Cli {
    /// Load configuration from this file.
    #[arg(long, global = true, env = "TACT_CONFIG", value_name = "PATH")]
    config: Option<PathBuf>,

    /// Select the authentication method.
    #[arg(
        long,
        global = true,
        env = "TACT_AUTH",
        value_enum,
        value_name = "MODE"
    )]
    auth: Option<AuthMode>,

    /// Use this Codex-compatible credential file.
    #[arg(long, global = true, env = "TACT_AUTH_FILE", value_name = "PATH")]
    auth_file: Option<PathBuf>,

    /// Working directory exposed to the agent.
    #[arg(long, global = true, env = "TACT_WORKSPACE", value_name = "PATH")]
    workspace: Option<PathBuf>,

    /// Reasoning effort used by the model.
    #[arg(
        long,
        global = true,
        env = "TACT_THINKING",
        value_enum,
        value_name = "LEVEL"
    )]
    thinking: Option<ReasoningEffort>,

    /// Reasoning execution mode used for new sessions.
    #[arg(
        long,
        global = true,
        env = "TACT_REASONING_MODE",
        value_enum,
        value_name = "MODE"
    )]
    reasoning_mode: Option<ReasoningMode>,

    /// Model used when starting a new agent.
    #[arg(long, global = true, env = "TACT_MODEL", value_name = "MODEL")]
    model: Option<Model>,

    /// Maximum number of sub-agents that may run concurrently.
    #[arg(long, global = true, env = "TACT_MAX_SUBAGENTS", value_name = "COUNT")]
    max_subagents: Option<usize>,

    /// Replace Nanocodex's standard instructions, before Tact's built-in appendix.
    #[arg(
        long,
        global = true,
        env = "TACT_INSTRUCTIONS",
        value_parser = NonEmptyStringValueParser::new()
    )]
    instructions: Option<String>,

    /// Append after Tact's built-in instructions.
    #[arg(
        long,
        global = true,
        env = "TACT_APPEND_INSTRUCTIONS",
        value_parser = NonEmptyStringValueParser::new()
    )]
    append_instructions: Option<String>,

    /// Expose standalone web search to the model.
    #[arg(long, global = true, env = "TACT_WEB_SEARCH", action = ArgAction::Set)]
    web_search: Option<bool>,

    /// Expose image generation to the model.
    #[arg(
        long,
        global = true,
        env = "TACT_IMAGE_GENERATION",
        action = ArgAction::Set
    )]
    image_generation: Option<bool>,

    /// Override the Responses API WebSocket endpoint.
    #[arg(
        long,
        global = true,
        env = "TACT_WEBSOCKET_URL",
        value_parser = NonEmptyStringValueParser::new()
    )]
    websocket_url: Option<String>,

    /// Override the OpenAI HTTP API base URL.
    #[arg(
        long,
        global = true,
        env = "TACT_API_BASE_URL",
        value_parser = NonEmptyStringValueParser::new()
    )]
    api_base_url: Option<String>,

    /// Resume a persisted interactive session.
    #[arg(long, global = true, env = "TACT_RESUME", value_name = "SESSION_ID")]
    resume: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage authentication.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Inspect the effective configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage MCP servers.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Run one prompt and stream Nanocodex events as JSONL.
    Run {
        #[cfg(feature = "harbor-evals")]
        /// Write child-agent events and the final cleanup state as JSONL.
        #[arg(long, env = "TACT_ORCHESTRATION_LOG", value_name = "PATH")]
        orchestration_log: Option<PathBuf>,

        /// Prompt submitted to the agent.
        #[arg(env = "TACT_PROMPT", value_parser = NonEmptyStringValueParser::new())]
        prompt: String,
    },
    /// Open the interactive session picker.
    Resume,
    /// Transfer memories between the global local store and the remote service.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Download and install the latest signed tact release.
    Update,
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Replace the remote personal namespace with the complete local corpus.
    Push {
        /// Report the local corpus that would be pushed without contacting the remote.
        #[arg(long)]
        dry_run: bool,
    },
    /// Merge remote memories into the global local store.
    #[command(group(
        clap::ArgGroup::new("selection")
            .required(true)
            .multiple(false)
            .args(["all", "namespace"])
    ))]
    Pull {
        /// Pull every namespace visible to the configured token.
        #[arg(long)]
        all: bool,
        /// Pull one namespace; repeat to select more than one.
        #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new())]
        namespace: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Sign in with a ChatGPT subscription.
    Login,
    /// Show the effective authentication source.
    Status,
    /// Remove the shared ChatGPT credentials.
    Logout,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the selected configuration file path.
    Path,
    /// Print the complete effective configuration.
    Show,
}

#[derive(Subcommand)]
enum McpCommand {
    /// Add a local stdio or remote Streamable HTTP MCP server.
    #[command(group(
        clap::ArgGroup::new("transport")
            .required(true)
            .args(["url", "command"])
    ), override_usage = "tact mcp add [OPTIONS] <NAME> (--url <URL> | -- <COMMAND>...)")]
    Add {
        /// Name for the MCP server configuration.
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        name: String,

        /// Environment variable copied into the server configuration.
        #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new(), conflicts_with = "url")]
        env: Vec<String>,

        /// Working directory for the server process.
        #[arg(long, value_name = "PATH", conflicts_with = "url")]
        cwd: Option<PathBuf>,

        /// URL for a remote Streamable HTTP server.
        #[arg(
            long,
            value_name = "URL",
            conflicts_with = "command",
            value_parser = NonEmptyStringValueParser::new()
        )]
        url: Option<String>,

        /// Environment variable containing the remote server's bearer token.
        #[arg(long, value_name = "NAME", requires = "url", conflicts_with = "command", value_parser = NonEmptyStringValueParser::new())]
        bearer_token_env_var: Option<String>,

        /// Resolve an HTTP header value from an environment variable (`HEADER=ENV_VAR`).
        #[arg(long, value_name = "HEADER=ENV_VAR", requires = "url", conflicts_with = "command", value_parser = parse_header_env)]
        header_env: Vec<(String, String)>,

        /// Command used to launch the server.
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },
}

impl fmt::Debug for McpCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add {
                name,
                env,
                cwd,
                url,
                bearer_token_env_var,
                header_env,
                command,
            } => formatter
                .debug_struct("Add")
                .field("name", name)
                .field("env", env)
                .field("cwd", cwd)
                .field("url", &url.as_ref().map(|_| "[REDACTED URL]"))
                .field("bearer_token_env_var", bearer_token_env_var)
                .field("header_env", header_env)
                .field("command", command)
                .finish(),
        }
    }
}

impl Zeroize for McpCommand {
    fn zeroize(&mut self) {
        let Self::Add { url, .. } = self;
        if let Some(url) = url {
            url.zeroize();
        }
    }
}

impl Drop for McpCommand {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for McpCommand {}

impl Cli {
    pub(crate) async fn run(self) -> Result<()> {
        if self.resume.is_some() && self.command.is_some() {
            return Err(RuntimeError::ResumeWithCommand.into());
        }
        if matches!(&self.command, None | Some(Command::Resume)) {
            tui::ensure_interactive()?;
        }
        if self
            .command
            .as_ref()
            .is_some_and(|command| !command.requires_config())
        {
            return self
                .command
                .expect("a config-independent command was checked above")
                .run_without_config()
                .await;
        }

        let overrides = ConfigOverrides {
            path: self.config,
            auth_mode: self.auth,
            auth_file: self.auth_file,
            workspace: self.workspace,
            thinking: self.thinking,
            reasoning_mode: self.reasoning_mode,
            max_subagents: self.max_subagents,
            instructions: self.instructions,
            append_instructions: self.append_instructions,
            web_search: self.web_search,
            image_generation: self.image_generation,
            websocket_url: self.websocket_url,
            api_base_url: self.api_base_url,
        };
        let config = if matches!(&self.command, Some(Command::Mcp { .. })) {
            Config::load_for_update(overrides)?
        } else {
            Config::load(overrides)?
        };
        let model = self.model.unwrap_or_default();

        match self.command {
            Some(Command::Resume) => {
                Self::run_tui(config, tui::StartupMode::ResumeSelector(model)).await
            }
            Some(command) => command.run_with_config(&config, model).await,
            None => {
                let startup = self
                    .resume
                    .map_or(tui::StartupMode::NewSession(model), |session_id| {
                        tui::StartupMode::ResumeSession(session_id)
                    });
                Self::run_tui(config, startup).await
            }
        }
    }

    async fn run_tui(config: Config, startup: tui::StartupMode) -> Result<()> {
        let shutdown = CancellationToken::new();
        let run = tui::run(config, startup, shutdown.clone());
        tokio::pin!(run);

        let result = tokio::select! {
            result = &mut run => result,
            signal = shutdown::signal() => {
                shutdown.cancel();
                let result = run.await;
                signal.map_err(RuntimeError::ShutdownSignal)?;
                result
            }
        };
        if let Some(session_id) = result? {
            print_resume_hint(&session_id);
        }
        Ok(())
    }
}

fn print_resume_hint(session_id: &str) {
    let art = r"  _             _
 | |_ __ _  ___| |_
 | __/ _` |/ __| __|
 | || (_| | (__| |_
  \__\__,_|\___|\__|";
    println!("\n{}", art.with(Color::Cyan));
    println!(
        "{} {}",
        "Resume this session:".with(Color::DarkGrey),
        resume_command(session_id).with(Color::Green)
    );
}

fn resume_command(session_id: &str) -> String {
    let session_id = shlex::try_quote(session_id)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| format!("'{session_id}'"));
    format!("tact --resume {session_id}")
}

impl Command {
    const fn requires_config(&self) -> bool {
        !matches!(self, Self::Update)
    }

    async fn run_without_config(self) -> Result<()> {
        let Self::Update = self else {
            unreachable!("only update is config-independent");
        };
        match update::install_latest().await.map_err(Error::update)? {
            update::UpdateStatus::UpToDate { version } => {
                println!("tact v{version} is already up to date.");
            }
            update::UpdateStatus::Updated { from, to } => {
                println!("Updated tact from v{from} to v{to}.");
            }
            update::UpdateStatus::UseCargo { command } => {
                println!("This tact binary is managed by Cargo. Update it with `{command}`.");
            }
            update::UpdateStatus::UsePackageManager { manager } => {
                println!(
                    "This tact binary is managed by {manager}. Update it with your package manager."
                );
            }
        }
        Ok(())
    }

    async fn run_with_config(self, config: &Config, model: Model) -> Result<()> {
        match self {
            Self::Auth { command } => command.run(config).await.map_err(Into::into),
            Self::Config { command } => command.run(config),
            Self::Mcp { command } => command.run(config),
            Self::Run {
                prompt,
                #[cfg(feature = "harbor-evals")]
                orchestration_log,
            } => {
                Self::run_agent(
                    config,
                    model,
                    prompt,
                    #[cfg(feature = "harbor-evals")]
                    orchestration_log,
                )
                .await
            }
            Self::Resume => unreachable!("resume is dispatched to the TUI"),
            Self::Memory { command } => command.run(config).await,
            Self::Update => unreachable!("update is dispatched before configuration is loaded"),
        }
    }

    async fn run_agent(
        config: &Config,
        model: Model,
        prompt: String,
        #[cfg(feature = "harbor-evals")] orchestration_log: Option<PathBuf>,
    ) -> Result<()> {
        let shutdown = CancellationToken::new();
        let run = ConfiguredAgent::run_from_config(
            config,
            model,
            prompt,
            shutdown.clone(),
            #[cfg(feature = "harbor-evals")]
            orchestration_log,
        );
        tokio::pin!(run);

        tokio::select! {
            result = &mut run => result,
            signal = shutdown::signal() => {
                shutdown.cancel();
                let result = run.await;
                signal.map_err(RuntimeError::ShutdownSignal)?;
                result
            }
        }
    }
}

impl MemoryCommand {
    async fn run(self, config: &Config) -> Result<()> {
        match self {
            Self::Push { dry_run } => push_memories(config, dry_run).await,
            Self::Pull { all, namespace } => pull_memories(config, all, namespace).await,
        }
    }
}

async fn push_memories(config: &Config, dry_run: bool) -> Result<()> {
    use crate::app::error::MemoryTransferError;
    use tact_memory::{
        MemoryStore, RemoteMemoryClient, RemoteRole, RemoteToken, SelectedMemoryStore,
    };

    let remote = config
        .memory()
        .remote()
        .ok_or(MemoryTransferError::RemoteNotConfigured)?;

    let store = SelectedMemoryStore::local(config.memory_path());
    let mut memories = export_memories(store.clone()).await?;
    let content_bytes = memories
        .iter()
        .map(|memory| memory.content.len())
        .sum::<usize>();
    if dry_run {
        println!(
            "Would push {} memories ({} content bytes) as the complete snapshot for namespace `{}`; remote-only rows may be deleted.",
            memories.len(),
            content_bytes,
            remote.namespace()
        );
        return Ok(());
    }

    let token =
        RemoteToken::new(remote.bearer_token().to_owned()).map_err(MemoryTransferError::Push)?;
    let client = RemoteMemoryClient::new(remote.endpoint(), remote.namespace().to_owned(), token)
        .map_err(MemoryTransferError::Push)?;
    if client.session().await.map_err(MemoryTransferError::Push)? != RemoteRole::Writer {
        return Err(MemoryTransferError::Push(tact_memory::RemoteClientError::ReadOnly).into());
    }
    let remote_store = SelectedMemoryStore::remote(client);
    for _ in 0..3 {
        let report = remote_store
            .sync(&memories)
            .await
            .map_err(MemoryTransferError::PushStore)?;

        let current = export_memories(store.clone()).await?;
        if same_replication_snapshot(&memories, &current) {
            println!(
                "Pushed {} memories to namespace `{}`: {} inserted, {} replaced, {} unchanged, {} deleted.",
                memories.len(),
                remote.namespace(),
                report.inserted,
                report.replaced,
                report.unchanged,
                report.deleted
            );
            return Ok(());
        }
        memories = current;
    }
    Err(MemoryTransferError::LocalChanged.into())
}

async fn pull_memories(config: &Config, all: bool, namespaces: Vec<String>) -> Result<()> {
    use crate::app::error::MemoryTransferError;
    use tact_memory::{
        LocalMemoryStore, MemoryStore, RemoteMemoryClient, RemoteToken, SelectedMemoryStore,
    };

    let remote = config
        .memory()
        .remote()
        .ok_or(MemoryTransferError::RemoteNotConfigured)?;
    let token =
        RemoteToken::new(remote.bearer_token().to_owned()).map_err(MemoryTransferError::Pull)?;
    let client = RemoteMemoryClient::new(remote.endpoint(), remote.namespace().to_owned(), token)
        .map_err(MemoryTransferError::Pull)?;
    client.session().await.map_err(MemoryTransferError::Pull)?;
    let selection = (!all).then_some(namespaces.as_slice());
    let memories = SelectedMemoryStore::remote(client)
        .export_all(selection)
        .await
        .map_err(MemoryTransferError::PullStore)?;
    let fetched = memories.len();
    let report = LocalMemoryStore::new(config.memory_path())
        .merge_remote_export(memories)
        .await
        .map_err(MemoryTransferError::Merge)?;
    let selected = if all {
        "all namespaces".to_owned()
    } else {
        format!("namespaces `{}`", namespaces.join("`, `"))
    };
    println!(
        "Pulled {selected}: {fetched} fetched, {} inserted, {} skipped.",
        report.inserted, report.skipped
    );
    Ok(())
}

async fn export_memories(
    store: tact_memory::SelectedMemoryStore,
) -> std::result::Result<Vec<tact_memory::MemoryRecord>, crate::app::error::MemoryTransferError> {
    use crate::app::error::MemoryTransferError;
    use tact_memory::MemoryStore;

    let mut memories = store
        .export_all(None)
        .await
        .map_err(MemoryTransferError::Local)?;
    memories.sort_unstable_by_key(|memory| memory.key.id);
    Ok(memories)
}

fn same_replication_snapshot(
    left: &[tact_memory::MemoryRecord],
    right: &[tact_memory::MemoryRecord],
) -> bool {
    left == right
}

impl AuthCommand {
    async fn run(self, config: &Config) -> AuthResult<()> {
        match self {
            Self::Login => config.auth().login().await,
            Self::Status => config.auth().status().await,
            Self::Logout => config.auth().logout(),
        }
    }
}

impl ConfigCommand {
    fn run(self, config: &Config) -> Result<()> {
        match self {
            Self::Path => println!("{}", config.path().display()),
            Self::Show => print!("{}", config.to_toml()?),
        }

        Ok(())
    }
}

impl McpCommand {
    fn run(self, config: &Config) -> Result<()> {
        match &self {
            Self::Add {
                name,
                env,
                cwd,
                url,
                bearer_token_env_var,
                header_env,
                command,
            } => {
                if let Some(url) = url {
                    config.add_http_mcp_server(
                        name,
                        url,
                        bearer_token_env_var.as_deref(),
                        header_env
                            .iter()
                            .map(|(header, variable)| (header.as_str(), variable.as_str())),
                    )?;
                    println!("Added MCP server `{name}`.");
                    return Ok(());
                }

                let (program, arguments) = command.split_first().expect("clap requires a command");
                let environment = env
                    .iter()
                    .map(|name| read_mcp_environment(name.clone(), |name| env::var(name)))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                config.add_mcp_server(
                    name,
                    program,
                    arguments,
                    environment
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.as_str())),
                    cwd.as_deref(),
                )?;
                println!("Added MCP server `{name}`.");
            }
        }

        Ok(())
    }
}

fn parse_header_env(value: &str) -> std::result::Result<(String, String), String> {
    let Some((header, variable)) = value.split_once('=') else {
        return Err("expected HEADER=ENV_VAR".into());
    };
    if header.is_empty() || variable.is_empty() {
        return Err("header and environment variable names must not be empty".into());
    }
    Ok((header.into(), variable.into()))
}

fn read_mcp_environment(
    name: String,
    read: impl FnOnce(&str) -> std::result::Result<String, VarError>,
) -> std::result::Result<(String, Zeroizing<String>), crate::app::error::ConfigError> {
    match read(&name) {
        Ok(value) => Ok((name, Zeroizing::new(value))),
        Err(VarError::NotPresent) => {
            Err(crate::app::error::ConfigError::McpEnvironmentNotPresent { name })
        }
        // VarError owns and renders the non-Unicode value, so discard it before constructing the
        // diagnostic. The process environment retains the original outside tact's ownership.
        Err(VarError::NotUnicode(_)) => {
            Err(crate::app::error::ConfigError::McpEnvironmentNotUnicode { name })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, McpCommand, MemoryCommand, push_memories, read_mcp_environment, resume_command,
        same_replication_snapshot,
    };
    use crate::app::{
        cli::Command,
        config::{AuthMode, Config, ConfigOverrides},
        error::{ConfigError, Error},
    };
    use clap::{CommandFactory, Parser, error::ErrorKind};
    use nanocodex::Model;
    use std::{
        env::VarError,
        ffi::OsString,
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tempfile::tempdir;

    #[test]
    fn replication_snapshot_includes_memory_telemetry() {
        let original = tact_memory::MemoryRecord {
            key: tact_memory::MemoryKey::local(1, 1),
            content: "telemetry".to_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
            last_scanned_at_ms: None,
            scan_count: 0,
            last_used_at_ms: None,
            use_count: 0,
            probation_until_ms: Some(10),
        };
        let mut current = original.clone();
        current.last_scanned_at_ms = Some(2);
        current.scan_count = 1;
        current.last_used_at_ms = Some(3);
        current.use_count = 1;
        current.probation_until_ms = None;

        assert!(!same_replication_snapshot(&[original], &[current]));
    }

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn version_includes_build_metadata() {
        let error = Cli::try_parse_from(["tact", "--version"]).unwrap_err();
        let output = error.to_string();

        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert!(output.contains(env!("TACT_GIT_SHA")));
        assert!(output.contains(env!("TACT_BUILD_TIMESTAMP")));
        assert!(output.contains(env!("TACT_BUILD_TARGET")));
        assert!(output.contains(env!("TACT_RUSTC_VERSION")));
    }

    #[test]
    fn version_displays_human_readable_timestamps() {
        let error = Cli::try_parse_from(["tact", "--version"]).unwrap_err();
        let output = error.to_string();

        for label in ["commit timestamp: ", "build timestamp: "] {
            let timestamp = output
                .lines()
                .find_map(|line| line.strip_prefix(label))
                .unwrap_or_else(|| panic!("version should include {label}"));

            chrono::DateTime::parse_from_rfc3339(timestamp)
                .unwrap_or_else(|_| panic!("{label}should use RFC 3339 format"));
        }
    }

    #[test]
    fn bare_invocation_selects_the_tui() {
        let cli = Cli::try_parse_from(["tact"]).unwrap();

        assert!(cli.command.is_none());
    }

    #[test]
    fn model_selects_the_initial_agent() {
        let cli = Cli::try_parse_from(["tact", "--model", "terra"]).unwrap();

        assert_eq!(cli.model, Some(Model::Terra));
    }

    #[test]
    fn resume_selects_a_persisted_tui_session() {
        let cli = Cli::try_parse_from(["tact", "--resume", "session one"]).unwrap();

        assert_eq!(cli.resume.as_deref(), Some("session one"));
        assert_eq!(resume_command("session one"), "tact --resume 'session one'");
    }

    #[test]
    fn resume_subcommand_selects_the_session_picker() {
        let cli = Cli::try_parse_from(["tact", "resume"]).unwrap();

        assert!(matches!(cli.command, Some(Command::Resume)));
        assert!(cli.resume.is_none());
    }

    #[test]
    fn global_overrides_are_accepted_after_subcommands() {
        let cli = Cli::try_parse_from([
            "tact",
            "config",
            "show",
            "--config",
            "tact.toml",
            "--auth",
            "chatgpt",
            "--auth-file",
            "auth.json",
            "--model",
            "luna",
            "--max-subagents",
            "12",
        ])
        .unwrap();

        assert_eq!(cli.config.unwrap(), PathBuf::from("tact.toml"));
        assert_eq!(cli.auth, Some(AuthMode::ChatGpt));
        assert_eq!(cli.auth_file.unwrap(), PathBuf::from("auth.json"));
        assert_eq!(cli.model, Some(Model::Luna));
        assert_eq!(cli.max_subagents, Some(12));
        assert!(matches!(cli.command, Some(Command::Config { .. })));
    }

    #[test]
    fn api_key_mode_uses_kebab_case() {
        let cli = Cli::try_parse_from(["tact", "--auth", "api-key", "config", "show"]).unwrap();

        assert_eq!(cli.auth, Some(AuthMode::ApiKey));
    }

    #[test]
    fn authentication_commands_are_available() {
        for command in ["login", "status", "logout"] {
            let cli = Cli::try_parse_from(["tact", "auth", command]).unwrap();

            assert!(matches!(cli.command, Some(Command::Auth { .. })));
        }
    }

    #[test]
    fn mcp_add_accepts_a_stdio_command_and_options() {
        let cli = Cli::try_parse_from([
            "tact",
            "mcp",
            "add",
            "filesystem",
            "--env",
            "TOKEN",
            "--cwd",
            "servers/filesystem",
            "--",
            "npx",
            "-y",
            "@modelcontextprotocol/server-filesystem",
            ".",
        ])
        .unwrap();

        let Some(Command::Mcp {
            command:
                McpCommand::Add {
                    name,
                    env,
                    cwd,
                    command,
                    ..
                },
        }) = &cli.command
        else {
            panic!("expected mcp add command");
        };
        assert_eq!(name, "filesystem");
        assert_eq!(env.as_slice(), ["TOKEN"]);
        assert_eq!(
            cwd.as_deref(),
            Some(PathBuf::from("servers/filesystem").as_path())
        );
        assert_eq!(
            command.as_slice(),
            ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."]
        );
    }

    #[test]
    fn mcp_add_accepts_a_remote_server_with_environment_backed_auth() {
        let cli = Cli::try_parse_from([
            "tact",
            "mcp",
            "add",
            "docs",
            "--url",
            "https://example.com/mcp",
            "--bearer-token-env-var",
            "MCP_TOKEN",
            "--header-env",
            "X-Tenant=TENANT_ID",
        ])
        .unwrap();

        let Some(Command::Mcp {
            command:
                McpCommand::Add {
                    url,
                    bearer_token_env_var,
                    header_env,
                    command,
                    ..
                },
        }) = &cli.command
        else {
            panic!("expected mcp add command");
        };
        assert_eq!(url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(bearer_token_env_var.as_deref(), Some("MCP_TOKEN"));
        assert_eq!(
            header_env.as_slice(),
            [("X-Tenant".into(), "TENANT_ID".into())]
        );
        assert!(command.is_empty());
    }

    #[test]
    fn mcp_add_rejects_an_empty_remote_url_and_shows_transport_usage() {
        let error = Cli::try_parse_from(["tact", "mcp", "add", "docs", "--url", ""]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);

        let help = Cli::try_parse_from(["tact", "mcp", "add", "--help"])
            .unwrap_err()
            .to_string();
        assert!(
            help.contains("Usage: tact mcp add [OPTIONS] <NAME> (--url <URL> | -- <COMMAND>...)")
        );
    }

    #[test]
    fn mcp_add_rejects_whitespace_and_credential_bearing_urls_without_persisting_them() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "[auth]\nmode = \"api-key\"\nfile = \"auth.json\"\n";
        fs::write(&path, original).unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(path.clone()),
            ..ConfigOverrides::default()
        })
        .unwrap();

        for url in [" ", "https://user:not-a-real-secret@example.com/mcp"] {
            let cli = Cli::try_parse_from(["tact", "mcp", "add", "docs", "--url", url]).unwrap();
            assert!(!format!("{cli:?}").contains("not-a-real-secret"));
            let Some(Command::Mcp { command }) = cli.command else {
                panic!("expected mcp command");
            };
            let error = command.run(&config).unwrap_err();
            let rendered = format!("{error:?} {error}");
            assert!(matches!(
                error,
                Error::Config(ConfigError::McpUrl { name, .. }) if name == "docs"
            ));
            assert!(!rendered.contains("not-a-real-secret"));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn mcp_add_requires_exactly_one_transport_and_transport_specific_options() {
        for arguments in [
            vec!["tact", "mcp", "add", "missing"],
            vec![
                "tact",
                "mcp",
                "add",
                "mixed",
                "--url",
                "https://example.com/mcp",
                "--",
                "server",
            ],
            vec![
                "tact",
                "mcp",
                "add",
                "stdio",
                "--header-env",
                "X=Y",
                "--",
                "server",
            ],
            vec![
                "tact",
                "mcp",
                "add",
                "http",
                "--url",
                "https://example.com/mcp",
                "--cwd",
                ".",
            ],
        ] {
            assert!(
                Cli::try_parse_from(&arguments).is_err(),
                "accepted invalid arguments: {arguments:?}"
            );
        }
    }

    #[test]
    fn non_unicode_mcp_environment_errors_are_redacted() {
        let error = read_mcp_environment("TOKEN".into(), |_| {
            Err(VarError::NotUnicode(OsString::from("secret-sentinel")))
        })
        .unwrap_err();
        let debug = format!("{error:?}");
        let display = error.to_string();

        assert!(!debug.contains("secret-sentinel"));
        assert!(!display.contains("secret-sentinel"));
        assert!(display.contains("TOKEN"));
    }

    #[test]
    fn run_accepts_a_prompt() {
        let cli = Cli::try_parse_from(["tact", "run", "inspect the workspace"]).unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Run { prompt, .. }) if prompt == "inspect the workspace"
        ));
    }

    #[cfg(feature = "harbor-evals")]
    #[test]
    fn run_accepts_an_orchestration_log() {
        let cli = Cli::try_parse_from([
            "tact",
            "run",
            "--orchestration-log",
            "/logs/agent/orchestration.jsonl",
            "inspect the workspace",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Some(Command::Run {
                orchestration_log: Some(path),
                ..
            }) if path == std::path::Path::new("/logs/agent/orchestration.jsonl")
        ));
    }

    #[test]
    fn append_instructions_are_accepted() {
        let cli = Cli::try_parse_from([
            "tact",
            "--append-instructions",
            "Follow project conventions.",
            "run",
            "inspect the workspace",
        ])
        .unwrap();

        assert_eq!(
            cli.append_instructions.as_deref(),
            Some("Follow project conventions.")
        );
    }

    #[test]
    fn update_is_config_independent() {
        let cli = Cli::try_parse_from(["tact", "update"]).unwrap();
        let command = cli.command.expect("missing update command");

        assert!(matches!(&command, Command::Update));
        assert!(!command.requires_config());
    }

    #[test]
    fn memory_push_supports_a_dry_run_and_old_names_are_rejected() {
        let command = Cli::try_parse_from(["tact", "memory", "push", "--dry-run"])
            .unwrap()
            .command
            .unwrap();

        assert!(matches!(
            &command,
            Command::Memory {
                command: MemoryCommand::Push { dry_run: true }
            }
        ));
        assert!(command.requires_config());
        assert!(Cli::try_parse_from(["tact", "memory", "upload"]).is_err());
        assert!(Cli::try_parse_from(["tact", "sync-memories"]).is_err());
    }

    #[test]
    fn memory_pull_requires_exactly_one_selection_mode() {
        let all = Cli::try_parse_from(["tact", "memory", "pull", "--all"])
            .unwrap()
            .command
            .unwrap();
        assert!(matches!(
            all,
            Command::Memory {
                command: MemoryCommand::Pull { all: true, namespace }
            } if namespace.is_empty()
        ));

        let selected = Cli::try_parse_from([
            "tact",
            "memory",
            "pull",
            "--namespace",
            "one",
            "--namespace",
            "two",
        ])
        .unwrap()
        .command
        .unwrap();
        assert!(matches!(
            selected,
            Command::Memory {
                command: MemoryCommand::Pull { all: false, namespace }
            } if namespace == ["one", "two"]
        ));
        assert!(Cli::try_parse_from(["tact", "memory", "pull"]).is_err());
        assert!(
            Cli::try_parse_from(["tact", "memory", "pull", "--all", "--namespace", "one"]).is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn push_outside_workspace_roots_uses_the_async_remote_client() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let directory = tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[memory]\nenabled = true\n\
                 [memory.remote]\n\
                 endpoint = \"{endpoint}\"\n\
                 namespace = \"runtime-test\"\n\
                 bearer_token = \"runtime-test-token-000000000001\"\n\
                 workspace_roots = [\"{}\"]\n",
                directory.path().join("unrelated-root").display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();

        let result = push_memories(&config, false).await;

        assert!(matches!(result, Err(Error::MemoryTransfer(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn push_reconciles_a_local_write_that_races_the_first_snapshot() {
        use axum::{
            Json, Router,
            extract::State,
            routing::{get, post},
        };
        use tact_memory::{
            LocalMemoryStore, MemoryStore, VERSION,
            server::protocol::{
                RemoteRole, SESSION_PATH, SYNC_PATH, SessionResponse, SyncReport, SyncRequest,
            },
        };
        use tokio::sync::{Mutex, Notify};

        let _ = rustls::crypto::ring::default_provider().install_default();

        #[derive(Clone)]
        struct SyncState {
            attempts: Arc<AtomicUsize>,
            first_snapshot: Arc<Notify>,
            release_first: Arc<Notify>,
            snapshots: Arc<Mutex<Vec<SyncRequest>>>,
        }

        async fn session() -> Json<SessionResponse> {
            Json(SessionResponse {
                protocol_version: VERSION,
                namespace: "race-test".to_owned(),
                role: RemoteRole::Writer,
            })
        }

        async fn sync(
            State(state): State<SyncState>,
            Json(snapshot): Json<SyncRequest>,
        ) -> Json<SyncReport> {
            let inserted = snapshot.memories.len();
            state.snapshots.lock().await.push(snapshot);
            if state.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                state.first_snapshot.notify_one();
                state.release_first.notified().await;
            }
            Json(SyncReport {
                inserted,
                ..SyncReport::default()
            })
        }

        let state = SyncState {
            attempts: Arc::new(AtomicUsize::new(0)),
            first_snapshot: Arc::new(Notify::new()),
            release_first: Arc::new(Notify::new()),
            snapshots: Arc::new(Mutex::new(Vec::new())),
        };
        let router = Router::new()
            .route(&format!("/{SESSION_PATH}"), get(session))
            .route(&format!("/{SYNC_PATH}"), post(sync))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[memory]\nenabled = true\n\
                 [memory.remote]\n\
                 endpoint = \"{endpoint}\"\n\
                 namespace = \"race-test\"\n\
                 bearer_token = \"race-test-token-000000000000001\"\n\
                 workspace_roots = [\"{}\"]\n",
                directory.path().display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        LocalMemoryStore::new(config.memory_path())
            .put("before sync", None)
            .await
            .unwrap();

        let sync_config = config.clone();
        let sync_task = tokio::spawn(async move { push_memories(&sync_config, false).await });
        state.first_snapshot.notified().await;
        LocalMemoryStore::new(config.memory_path())
            .put("concurrent write", None)
            .await
            .unwrap();
        state.release_first.notify_one();

        sync_task.await.unwrap().unwrap();
        let snapshots = state.snapshots.lock().await;
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].memories.len(), 1);
        assert_eq!(snapshots[1].memories.len(), 2);
        assert!(
            snapshots[1]
                .memories
                .iter()
                .any(|memory| memory.content == "concurrent write")
        );

        server.abort();
        let _ = server.await;
    }

    #[test]
    fn every_cli_parameter_has_an_environment_variable() {
        let command = Cli::command();
        let expected = [
            ("config", "TACT_CONFIG"),
            ("auth", "TACT_AUTH"),
            ("auth_file", "TACT_AUTH_FILE"),
            ("workspace", "TACT_WORKSPACE"),
            ("thinking", "TACT_THINKING"),
            ("reasoning_mode", "TACT_REASONING_MODE"),
            ("model", "TACT_MODEL"),
            ("max_subagents", "TACT_MAX_SUBAGENTS"),
            ("instructions", "TACT_INSTRUCTIONS"),
            ("append_instructions", "TACT_APPEND_INSTRUCTIONS"),
            ("web_search", "TACT_WEB_SEARCH"),
            ("image_generation", "TACT_IMAGE_GENERATION"),
            ("websocket_url", "TACT_WEBSOCKET_URL"),
            ("api_base_url", "TACT_API_BASE_URL"),
            ("resume", "TACT_RESUME"),
        ];
        let arguments = command
            .get_arguments()
            .filter(|argument| !matches!(argument.get_id().as_str(), "help" | "version"))
            .collect::<Vec<_>>();
        assert_eq!(arguments.len(), expected.len());

        for (id, environment) in expected {
            let argument = arguments
                .iter()
                .copied()
                .find(|argument| argument.get_id() == id)
                .unwrap_or_else(|| panic!("missing {id} argument"));
            assert_eq!(
                argument.get_env().and_then(|value| value.to_str()),
                Some(environment)
            );
        }

        let run = command
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "run")
            .expect("missing run command");
        let arguments = run
            .get_arguments()
            .filter(|argument| {
                !argument.is_global_set()
                    && !matches!(argument.get_id().as_str(), "help" | "version")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            arguments.len(),
            if cfg!(feature = "harbor-evals") { 2 } else { 1 }
        );
        #[cfg(feature = "harbor-evals")]
        {
            let orchestration_log = arguments
                .iter()
                .copied()
                .find(|argument| argument.get_id() == "orchestration_log")
                .expect("missing orchestration log argument");
            assert_eq!(
                orchestration_log.get_env().and_then(|value| value.to_str()),
                Some("TACT_ORCHESTRATION_LOG")
            );
        }
        let prompt = arguments
            .into_iter()
            .find(|argument| argument.get_id() == "prompt")
            .expect("missing prompt argument");
        assert_eq!(
            prompt.get_env().and_then(|value| value.to_str()),
            Some("TACT_PROMPT")
        );
    }
}
