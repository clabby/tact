//! The web interface: a second front-end onto the sessions this process runs.
//!
//! The server holds projections and a command port and never mutates session state; every change
//! is made by the terminal event loop and reaches the server through [`bridge`]. See
//! `docs/web.md` for the architecture and the wire protocol.

mod api;
mod assets;
pub(crate) mod bridge;
mod diff;
mod hub;
mod registry;
mod review;
#[cfg(test)]
mod testing;
mod token;
mod wire;

use crate::app::config::Config;
use api::AppState;
use assets::{AssetStore, WebAssets};
use hub::Hub;
use registry::{InstanceRecord, Registration, RegistryError};
use review::{ReviewState, bridge_agent};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use token::{MachineToken, TokenError};
use tokio::{net::TcpListener, sync::mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

/// Ports tried after the configured one before falling back to an ephemeral port.
const PORT_SCAN_SPAN: u16 = 20;

#[derive(Debug, Error)]
enum StartError {
    #[error("the web interface is disabled")]
    Disabled,
    #[error(transparent)]
    Token(#[from] TokenError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("could not listen on {bind}: {source}")]
    Bind {
        bind: IpAddr,
        #[source]
        source: io::Error,
    },
    #[error("could not build the HTTP client: {0}")]
    Client(#[source] reqwest::Error),
}

/// Starts the web server for this process and reports its readiness through `end.status`.
///
/// Startup failure only sets [`bridge::WebStatus::Unavailable`]; it never reaches the terminal.
/// The server runs until `shutdown` is cancelled.
pub(crate) fn spawn(
    config: &Config,
    workspace: &Path,
    end: bridge::WebEnd,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let settings = Settings::new(config, workspace);
    tokio::spawn(async move {
        let bridge::WebEnd {
            publications,
            requests,
            queries,
            auxiliary,
            status,
        } = end;
        let hub = Hub::spawn(publications, shutdown.clone());
        match Server::start(
            settings,
            hub,
            Channels {
                requests,
                queries,
                auxiliary,
            },
            shutdown.clone(),
        )
        .await
        {
            Ok(server) => {
                status.send_replace(bridge::WebStatus::Ready {
                    url: server.login_url.clone(),
                });
                server.run().await;
            }
            Err(error) => {
                status.send_replace(bridge::WebStatus::Unavailable {
                    reason: error.to_string(),
                });
                shutdown.cancelled().await;
            }
        }
    })
}

struct Settings {
    enabled: bool,
    bind: IpAddr,
    port: u16,
    public_url: Option<String>,
    /// The Tact home directory; the web state lives in its `web` subdirectory.
    home: PathBuf,
    workspace: PathBuf,
}

impl Settings {
    fn new(config: &Config, workspace: &Path) -> Self {
        let web = config.web();
        Self {
            enabled: web.enabled(),
            bind: web.bind(),
            port: web.port(),
            public_url: web
                .public_url()
                .map(|url| url.trim_end_matches('/').to_owned()),
            home: config.path().parent().unwrap_or(Path::new(".")).to_owned(),
            workspace: workspace.to_owned(),
        }
    }
}

/// The command, query, and auxiliary-work senders of the bridge.
struct Channels {
    requests: UnboundedSender<bridge::Request>,
    queries: UnboundedSender<bridge::QueryRequest>,
    auxiliary: UnboundedSender<bridge::AuxiliaryRequest>,
}

struct Server {
    listener: TcpListener,
    app: axum::Router,
    login_url: String,
    shutdown: CancellationToken,
    review: Arc<ReviewState>,
    _registration: Registration,
}

impl Server {
    async fn start(
        settings: Settings,
        hub: Hub,
        channels: Channels,
        shutdown: CancellationToken,
    ) -> Result<Self, StartError> {
        if !settings.enabled {
            return Err(StartError::Disabled);
        }
        crate::install_tls_provider();
        let web_directory = settings.home.join("web");
        let token = MachineToken::load_or_create(&web_directory)?;
        let listener =
            bind(settings.bind, settings.port)
                .await
                .map_err(|source| StartError::Bind {
                    bind: settings.bind,
                    source,
                })?;
        let port = listener
            .local_addr()
            .map_err(|source| StartError::Bind {
                bind: settings.bind,
                source,
            })?
            .port();
        let origin = settings.public_url.unwrap_or_else(|| {
            format!(
                "http://{}",
                SocketAddr::new(reachable_host(settings.bind), port)
            )
        });
        let login_url = format!("{origin}/#k={}", token.expose());

        let registry_directory = web_directory.join("instances");
        let registration = Registration::create(
            &registry_directory,
            &InstanceRecord {
                pid: std::process::id(),
                port,
                workspace: settings.workspace.clone(),
                started_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |elapsed| {
                        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                    }),
            },
        )?;

        WebAssets::install_if_missing(settings.home.clone());
        let review = ReviewState::new(
            settings.workspace.clone(),
            hub.clone(),
            bridge_agent(channels.auxiliary),
            shutdown.clone(),
        );
        let state = Arc::new(AppState {
            token,
            hub,
            requests: channels.requests,
            queries: channels.queries,
            workspace: settings.workspace,
            port,
            registry_directory,
            assets: AssetStore::new(settings.home),
            client: reqwest::Client::builder()
                .build()
                .map_err(StartError::Client)?,
            shutdown: shutdown.clone(),
        });
        let app = api::router(state, review::router(Arc::clone(&review)));
        Ok(Self {
            listener,
            app,
            login_url,
            shutdown,
            review,
            _registration: registration,
        })
    }

    async fn run(self) {
        tokio::spawn(review::watch_workspace(self.review, self.shutdown.clone()));
        let shutdown = self.shutdown.clone();
        let served = axum::serve(self.listener, self.app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
        if served.is_err() {
            // The listener failed; the registry entry is removed on drop either way.
            self.shutdown.cancel();
        }
    }
}

/// Binds the first free port in `[first, first + PORT_SCAN_SPAN]`, then any free port.
async fn bind(address: IpAddr, first: u16) -> io::Result<TcpListener> {
    let scan = (first != 0)
        .then(|| first..=first.saturating_add(PORT_SCAN_SPAN))
        .into_iter()
        .flatten();
    let mut last_error = None;
    for port in scan.chain([0]) {
        match TcpListener::bind((address, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.expect("the ephemeral port is always attempted"))
}

/// The address a local browser should use for a listener bound to `bind`.
fn reachable_host(bind: IpAddr) -> IpAddr {
    match bind {
        IpAddr::V4(address) if address.is_unspecified() => {
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
        IpAddr::V6(address) if address.is_unspecified() => {
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
        address => address,
    }
}

#[cfg(test)]
mod tests {
    use super::{Channels, Server, Settings, bind, bridge, hub::Hub, registry};
    use std::{
        fs,
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };
    use tokio_util::sync::CancellationToken;

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    #[tokio::test]
    async fn binding_skips_occupied_ports_and_falls_back_to_any_free_port() {
        let occupied = bind(LOOPBACK, 0).await.unwrap();
        let first = occupied.local_addr().unwrap().port();

        let next = bind(LOOPBACK, first).await.unwrap();

        assert_ne!(next.local_addr().unwrap().port(), first);
        let ephemeral = bind(LOOPBACK, 0).await.unwrap();
        assert_ne!(ephemeral.local_addr().unwrap().port(), 0);
    }

    #[tokio::test]
    async fn the_server_registers_serves_and_unregisters() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (_terminal, end) = bridge::bridge();
        let shutdown = CancellationToken::new();
        let hub = Hub::spawn(end.publications, shutdown.clone());
        let server = Server::start(
            Settings {
                enabled: true,
                bind: LOOPBACK,
                port: 0,
                public_url: None,
                home: home.path().to_owned(),
                workspace: workspace.path().to_owned(),
            },
            hub,
            Channels {
                requests: end.requests,
                queries: end.queries,
                auxiliary: end.auxiliary,
            },
            shutdown.clone(),
        )
        .await
        .unwrap();
        let (origin, token) = server.login_url.split_once("/#k=").unwrap();
        let (origin, token) = (origin.to_owned(), token.to_owned());
        let instances = home.path().join("web/instances");
        assert_eq!(registry::read_all(&instances).len(), 1);
        let task = tokio::spawn(server.run());

        let client = reqwest::Client::new();
        let unauthenticated = client
            .get(format!("{origin}/api/instance"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
        let mut stream = client
            .get(format!("{origin}/api/stream"))
            .header("cookie", format!("tact={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(stream.status(), reqwest::StatusCode::OK);
        assert_eq!(stream.headers()["content-type"], "text/event-stream");
        let first = stream.chunk().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).starts_with("event: hello\n"));
        let live = stream.chunk().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&live).starts_with("event: live\n"));

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(registry::read_all(&instances).is_empty());
        assert!(fs::metadata(home.path().join("web/token")).is_ok());
    }

    #[tokio::test]
    async fn a_public_url_replaces_the_local_origin_in_the_login_link() {
        let home = tempfile::tempdir().unwrap();
        let (_terminal, end) = bridge::bridge();
        let shutdown = CancellationToken::new();
        let hub = Hub::spawn(end.publications, shutdown.clone());

        let server = Server::start(
            Settings {
                enabled: true,
                bind: LOOPBACK,
                port: 0,
                public_url: Some("https://tact.example.net".to_owned()),
                home: home.path().to_owned(),
                workspace: home.path().to_owned(),
            },
            hub,
            Channels {
                requests: end.requests,
                queries: end.queries,
                auxiliary: end.auxiliary,
            },
            shutdown,
        )
        .await
        .unwrap();

        assert!(server.login_url.starts_with("https://tact.example.net/#k="));
    }

    #[tokio::test]
    async fn a_disabled_server_does_not_start() {
        let (_terminal, end) = bridge::bridge();
        let shutdown = CancellationToken::new();
        let hub = Hub::spawn(end.publications, shutdown.clone());

        let result = Server::start(
            Settings {
                enabled: false,
                bind: LOOPBACK,
                port: 0,
                public_url: None,
                home: ".".into(),
                workspace: ".".into(),
            },
            hub,
            Channels {
                requests: end.requests,
                queries: end.queries,
                auxiliary: end.auxiliary,
            },
            shutdown,
        )
        .await;

        assert!(result.is_err());
    }
}
