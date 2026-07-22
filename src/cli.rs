//! Command-line parsing and dispatch.

use crate::{
    config::{AuthMode, Config, ConfigOverrides, ReasoningEffort},
    core::ConfiguredAgent,
    error::{AuthResult, Result, RuntimeError},
    shutdown, tui,
};
use clap::{ArgAction, Parser, Subcommand, builder::NonEmptyStringValueParser};
use crossterm::style::{Color, Stylize};
use std::{env, env::VarError, path::PathBuf};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

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
    "\nbuild timestamp (Unix): ",
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

    /// Replace Nanocodex's standard instructions.
    #[arg(
        long,
        global = true,
        env = "TACT_INSTRUCTIONS",
        value_parser = NonEmptyStringValueParser::new()
    )]
    instructions: Option<String>,

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
        /// Prompt submitted to the agent.
        #[arg(env = "TACT_PROMPT", value_parser = NonEmptyStringValueParser::new())]
        prompt: String,
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

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Add a local stdio MCP server.
    Add {
        /// Name for the MCP server configuration.
        #[arg(value_parser = NonEmptyStringValueParser::new())]
        name: String,

        /// Environment variable copied into the server configuration.
        #[arg(long, value_name = "NAME", value_parser = NonEmptyStringValueParser::new())]
        env: Vec<String>,

        /// Working directory for the server process.
        #[arg(long, value_name = "PATH")]
        cwd: Option<PathBuf>,

        /// Command used to launch the server.
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },
}

impl Cli {
    pub(crate) async fn run(self) -> Result<()> {
        if self.resume.is_some() && self.command.is_some() {
            return Err(RuntimeError::ResumeWithCommand.into());
        }
        if self.command.is_none() {
            tui::ensure_interactive()?;
        }

        let overrides = ConfigOverrides {
            path: self.config,
            auth_mode: self.auth,
            auth_file: self.auth_file,
            workspace: self.workspace,
            thinking: self.thinking,
            instructions: self.instructions,
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

        match self.command {
            Some(command) => command.run(&config).await,
            None => Self::run_tui(config, self.resume).await,
        }
    }

    async fn run_tui(config: Config, resume: Option<String>) -> Result<()> {
        let shutdown = CancellationToken::new();
        let run = tui::run(config, resume, shutdown.clone());
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
    async fn run(self, config: &Config) -> Result<()> {
        match self {
            Self::Auth { command } => command.run(config).await.map_err(Into::into),
            Self::Config { command } => command.run(config),
            Self::Mcp { command } => command.run(config),
            Self::Run { prompt } => Self::run_agent(config, prompt).await,
        }
    }

    async fn run_agent(config: &Config, prompt: String) -> Result<()> {
        let shutdown = CancellationToken::new();
        let run = ConfiguredAgent::run_from_config(config, prompt, shutdown.clone());
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

impl AuthCommand {
    async fn run(self, config: &Config) -> AuthResult<()> {
        match self {
            Self::Login => config.auth().login().await,
            Self::Status => config.auth().status(),
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
        match self {
            Self::Add {
                name,
                env,
                cwd,
                mut command,
            } => {
                let arguments = command.split_off(1);
                let program = command.pop().expect("clap requires a command");
                let environment = env
                    .into_iter()
                    .map(|name| read_mcp_environment(name, |name| env::var(name)))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                config.add_mcp_server(
                    &name,
                    &program,
                    &arguments,
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

fn read_mcp_environment(
    name: String,
    read: impl FnOnce(&str) -> std::result::Result<String, VarError>,
) -> std::result::Result<(String, Zeroizing<String>), crate::error::ConfigError> {
    match read(&name) {
        Ok(value) => Ok((name, Zeroizing::new(value))),
        Err(VarError::NotPresent) => {
            Err(crate::error::ConfigError::McpEnvironmentNotPresent { name })
        }
        // VarError owns and renders the non-Unicode value, so discard it before constructing the
        // diagnostic. The process environment retains the original outside tact's ownership.
        Err(VarError::NotUnicode(_)) => {
            Err(crate::error::ConfigError::McpEnvironmentNotUnicode { name })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, McpCommand, read_mcp_environment, resume_command};
    use crate::{cli::Command, config::AuthMode};
    use clap::{CommandFactory, Parser, error::ErrorKind};
    use std::{env::VarError, ffi::OsString, path::PathBuf};

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
    fn bare_invocation_selects_the_tui() {
        let cli = Cli::try_parse_from(["tact"]).unwrap();

        assert!(cli.command.is_none());
    }

    #[test]
    fn resume_selects_a_persisted_tui_session() {
        let cli = Cli::try_parse_from(["tact", "--resume", "session one"]).unwrap();

        assert_eq!(cli.resume.as_deref(), Some("session one"));
        assert_eq!(resume_command("session one"), "tact --resume 'session one'");
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
        ])
        .unwrap();

        assert_eq!(cli.config.unwrap(), PathBuf::from("tact.toml"));
        assert_eq!(cli.auth, Some(AuthMode::ChatGpt));
        assert_eq!(cli.auth_file.unwrap(), PathBuf::from("auth.json"));
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
                },
        }) = cli.command
        else {
            panic!("expected mcp add command");
        };
        assert_eq!(name, "filesystem");
        assert_eq!(env, ["TOKEN"]);
        assert_eq!(cwd.unwrap(), PathBuf::from("servers/filesystem"));
        assert_eq!(
            command,
            ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."]
        );
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
            Some(Command::Run { prompt }) if prompt == "inspect the workspace"
        ));
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
            ("instructions", "TACT_INSTRUCTIONS"),
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
        assert_eq!(arguments.len(), 1);
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
