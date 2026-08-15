use clap::{ArgAction, Parser};
mod memory_store;

use memory_store::InMemoryBackend;
use std::{env, env::VarError, net::SocketAddr, str::FromStr};
use tact_memory::server::{Credential, MemoryServer, protocol, protocol::RemoteRole};
use thiserror::Error;
use tracing::{info, warn};
use zeroize::Zeroize;

#[derive(Debug, Parser)]
#[command(about = "Process-lifetime in-memory server for shared Tact memory")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,

    #[arg(long, value_name = "NAMESPACE=ENV_VAR", action = ArgAction::Append)]
    writer: Vec<CredentialSpec>,

    #[arg(long, value_name = "NAMESPACE=ENV_VAR", action = ArgAction::Append)]
    reader: Vec<CredentialSpec>,
}

#[derive(Clone, Debug)]
struct CredentialSpec {
    namespace: String,
    environment_variable: String,
}

impl FromStr for CredentialSpec {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (namespace, environment_variable) =
            value.split_once('=').ok_or("expected NAMESPACE=ENV_VAR")?;
        if !protocol::is_valid_namespace(namespace) {
            return Err("invalid memory namespace");
        }
        if !valid_environment_variable(environment_variable) {
            return Err("invalid environment-variable name");
        }
        Ok(Self {
            namespace: namespace.to_owned(),
            environment_variable: environment_variable.to_owned(),
        })
    }
}

impl CredentialSpec {
    fn load(self, role: RemoteRole) -> Result<Credential, CliError> {
        let token = match env::var(&self.environment_variable) {
            Ok(token) => token,
            Err(VarError::NotUnicode(value)) => {
                let mut bytes = value.into_encoded_bytes();
                bytes.zeroize();
                return Err(CliError::CredentialEnvironment {
                    name: self.environment_variable,
                });
            }
            Err(VarError::NotPresent) => {
                return Err(CliError::CredentialEnvironment {
                    name: self.environment_variable,
                });
            }
        };
        Credential::new(self.namespace, role, token).map_err(|_| CliError::InvalidCredential {
            name: self.environment_variable,
        })
    }
}

fn valid_environment_variable(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Debug, Error)]
enum CliError {
    #[error("could not read bearer credential from environment variable {name}")]
    CredentialEnvironment { name: String },
    #[error("environment variable {name} does not contain a valid bearer credential")]
    InvalidCredential { name: String },
    #[error(transparent)]
    Server(#[from] tact_memory::server::ServerBuildError),
    #[error("could not bind the memory server")]
    Bind(#[source] std::io::Error),
    #[error("could not determine the bound memory server address")]
    LocalAddress(#[source] std::io::Error),
    #[error("memory server failed")]
    Serve(#[source] std::io::Error),
}

#[tokio::main]
async fn main() -> Result<(), CliError> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let credentials = cli
        .writer
        .into_iter()
        .map(|spec| spec.load(RemoteRole::Writer))
        .chain(
            cli.reader
                .into_iter()
                .map(|spec| spec.load(RemoteRole::Reader)),
        )
        .collect::<Result<Vec<_>, _>>()?;
    let backend = InMemoryBackend::default();
    let server = MemoryServer::new(move |namespace| backend.bind(namespace), credentials)?;
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .map_err(CliError::Bind)?;
    let address = listener.local_addr().map_err(CliError::LocalAddress)?;
    println!("{address}");
    info!(address = %address, "process-lifetime remote memory server listening");
    axum::serve(listener, server.router())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(CliError::Serve)
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            warn!(%error, "could not install interrupt signal handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                warn!(%error, "could not install termination signal handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(unix)]
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }

    #[cfg(not(unix))]
    interrupt.await;
}

#[cfg(test)]
mod tests {
    use super::{CredentialSpec, valid_environment_variable};

    #[test]
    fn credential_specs_validate_namespace_and_environment_variable() {
        let spec = "writer-one=TACT_MEMORY_TOKEN"
            .parse::<CredentialSpec>()
            .unwrap();
        assert_eq!(spec.namespace, "writer-one");
        assert_eq!(spec.environment_variable, "TACT_MEMORY_TOKEN");
        assert!("missing-separator".parse::<CredentialSpec>().is_err());
        assert!("bad namespace=TOKEN".parse::<CredentialSpec>().is_err());
        assert!("writer=bad-name".parse::<CredentialSpec>().is_err());
        assert!(valid_environment_variable("_TOKEN_2"));
        assert!(!valid_environment_variable("2_TOKEN"));
    }
}
