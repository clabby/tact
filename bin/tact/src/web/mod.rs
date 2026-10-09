//! The web interface: a second front-end onto the sessions this process runs.
//!
//! The server holds projections and a command port and never mutates session state; every change
//! is made by the terminal event loop and reaches the server through [`bridge`]. See
//! `docs/web.md` for the architecture and the wire protocol.

mod api;
mod assets;
pub(crate) mod bridge;
mod hub;
pub(crate) mod machines;
mod outcome;
mod proxy;
mod registry;
mod review;
mod tailscale;
#[cfg(test)]
mod testing;
mod token;
mod wire;
mod workspaces;

use crate::{
    app::config::Config,
    core::protocol::{AuxiliaryRequest, QueryRequest, Request},
};
use api::{AppState, PublicOrigin};
use assets::AssetStore;
pub(crate) use assets::{Located, WebAssets};
use hub::Hub;
use machines::Registry as Machines;
use registry::{InstanceRecord, Registration, RegistryError};
use review::{BridgeAgent, ReviewRegistry};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tailscale::{Tailnet, TailscaleError};
use thiserror::Error;
pub(crate) use token::{MachineToken, TokenError};
use tokio::{
    net::TcpListener,
    sync::{mpsc::UnboundedSender, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use workspaces::Workspaces;

/// Ports tried after the configured one before falling back to an ephemeral port.
const PORT_SCAN_SPAN: u16 = 20;

#[derive(Debug, Error)]
pub(crate) enum StartError {
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
    #[error(transparent)]
    Tailscale(#[from] TailscaleError),
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
) -> JoinHandle<()> {
    let settings = Settings::new(config, workspace);
    tokio::spawn(async move {
        let (status, started) = Server::launch(settings, end, shutdown.clone()).await;
        match started {
            Ok(server) => {
                status.send_replace(server.ready());
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

/// Starts the web server of `tact serve`, which has no other front-end. It serves only the API,
/// for a hub's web interface to reach; every other path is a JSON 404. Startup failure is returned
/// instead of reported, and with `web.tailscale` the server is published before this returns,
/// unless `shutdown` comes first, and kept published while it runs. The server runs until
/// `shutdown` is cancelled.
pub(crate) async fn serve(
    config: &Config,
    workspace: &Path,
    end: bridge::WebEnd,
    shutdown: CancellationToken,
) -> Result<JoinHandle<()>, StartError> {
    let settings = Settings {
        api_only: true,
        ..Settings::new(config, workspace)
    };
    let (status, started) = Server::launch(settings, end, shutdown.clone()).await;
    let mut server = started?;
    if let Some(tailnet) = &server.tailnet {
        server.republisher = tailnet.keep_published(shutdown).await?;
    }
    status.send_replace(server.ready());
    Ok(tokio::spawn(server.run()))
}

struct Settings {
    enabled: bool,
    bind: IpAddr,
    port: u16,
    exposure: Exposure,
    /// The Tact home directory; the web state lives in its `web` subdirectory.
    home: PathBuf,
    workspace: PathBuf,
    /// Serve the API without the web interface.
    api_only: bool,
}

/// How the server is reached from other devices.
enum Exposure {
    /// Only from this computer.
    Local,
    /// Through a tunnel the user runs; the address is used for links and origin checks.
    PublicUrl(String),
    /// Through `tailscale serve`, which is started the first time a sign-in link for another
    /// device is wanted and stopped with the server.
    Tailscale,
}

impl Settings {
    fn new(config: &Config, workspace: &Path) -> Self {
        let web = config.web();
        Self {
            enabled: web.enabled(),
            bind: web.bind(),
            port: web.port(),
            exposure: match (web.tailscale(), web.public_url()) {
                (true, _) => Exposure::Tailscale,
                (false, Some(url)) => Exposure::PublicUrl(url.trim_end_matches('/').to_owned()),
                (false, None) => Exposure::Local,
            },
            home: config.path().parent().unwrap_or(Path::new(".")).to_owned(),
            workspace: workspace.to_owned(),
            api_only: false,
        }
    }
}

/// The command, query, and auxiliary-work senders of the bridge.
struct Channels {
    requests: UnboundedSender<Request>,
    queries: UnboundedSender<QueryRequest>,
    auxiliary: UnboundedSender<AuxiliaryRequest>,
}

struct Server {
    listener: TcpListener,
    app: axum::Router,
    login_url: String,
    shutdown: CancellationToken,
    _registration: Registration,
    tailnet: Option<Tailnet>,
    /// Keeps a headless server published to the tailnet; it ends with `shutdown`.
    republisher: Option<JoinHandle<()>>,
}

impl Server {
    /// Starts the server on the loop's side of the bridge, returning the status sender that
    /// reports its readiness.
    async fn launch(
        settings: Settings,
        end: bridge::WebEnd,
        shutdown: CancellationToken,
    ) -> (watch::Sender<bridge::WebStatus>, Result<Self, StartError>) {
        let bridge::WebEnd {
            publications,
            requests,
            queries,
            auxiliary,
            status,
        } = end;
        let hub = Hub::spawn(publications, shutdown.clone());
        let channels = Channels {
            requests,
            queries,
            auxiliary,
        };
        (status, Self::start(settings, hub, channels, shutdown).await)
    }

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
        let local_origin = format!(
            "http://{}",
            SocketAddr::new(reachable_host(settings.bind), port)
        );
        let (origin, public_origin, tailnet) = match settings.exposure {
            Exposure::Local => (local_origin, PublicOrigin::None, None),
            Exposure::PublicUrl(url) => (url.clone(), PublicOrigin::Fixed(url), None),
            Exposure::Tailscale => {
                let tailnet = Tailnet::new(port);
                (
                    local_origin,
                    PublicOrigin::Tailnet(tailnet.clone()),
                    Some(tailnet),
                )
            }
        };
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

        let workspaces = Arc::new(Workspaces::new(settings.workspace.clone(), hub.clone()));
        let review = ReviewRegistry::new(
            Arc::clone(&workspaces),
            hub.clone(),
            Arc::new(BridgeAgent::new(channels.auxiliary)),
            shutdown.clone(),
        );
        let state = Arc::new(AppState {
            token,
            hub,
            requests: channels.requests,
            queries: channels.queries,
            workspace: settings.workspace,
            port,
            public_origin,
            workspaces,
            registry_directory,
            assets: (!settings.api_only).then(|| AssetStore::new(settings.home)),
            client: api::sibling_client().map_err(StartError::Client)?,
            machines: Machines::new(&web_directory),
            peer_client: machines::peer_client().map_err(StartError::Client)?,
            shutdown: shutdown.clone(),
        });
        let app = api::router(state, review::router(review));
        Ok(Self {
            listener,
            app,
            login_url,
            shutdown,
            _registration: registration,
            tailnet,
            republisher: None,
        })
    }

    fn ready(&self) -> bridge::WebStatus {
        bridge::WebStatus::Ready {
            url: self.login_url.clone(),
            tailnet: self.tailnet.clone(),
        }
    }

    async fn run(self) {
        let shutdown = self.shutdown.clone();
        let served = axum::serve(self.listener, self.app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
        if served.is_err() {
            // The listener failed; the registry entry is removed on drop either way.
            self.shutdown.cancel();
        }
        // The republisher stops with `shutdown`; it must be gone before the publication is
        // withdrawn, or it could publish again.
        if let Some(republisher) = self.republisher {
            drop(republisher.await);
        }
        if let Some(tailnet) = self.tailnet {
            tailnet.stop().await;
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
    use super::{
        Channels, Exposure, Server, Settings, StartError, bind, bridge, hub::Hub, registry,
        testing::sse_event,
    };
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
                exposure: Exposure::Local,
                home: home.path().to_owned(),
                workspace: workspace.path().to_owned(),
                api_only: false,
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
        let interface = client.get(format!("{origin}/")).send().await.unwrap();
        assert_eq!(interface.status(), reqwest::StatusCode::OK);
        assert_eq!(
            interface.headers()["content-type"],
            "text/html; charset=utf-8"
        );
        let mut stream = client
            .get(format!("{origin}/api/stream"))
            .header("cookie", format!("tact={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(stream.status(), reqwest::StatusCode::OK);
        assert_eq!(stream.headers()["content-type"], "text/event-stream");
        let first = stream.chunk().await.unwrap().unwrap();
        assert_eq!(sse_event(&first).0, "hello");
        let live = stream.chunk().await.unwrap().unwrap();
        assert_eq!(sse_event(&live).0, "live");

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
                exposure: Exposure::PublicUrl("https://tact.example.net".to_owned()),
                home: home.path().to_owned(),
                workspace: home.path().to_owned(),
                api_only: false,
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
                exposure: Exposure::Local,
                home: ".".into(),
                workspace: ".".into(),
                api_only: false,
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

        assert!(matches!(result, Err(StartError::Disabled)));
    }
}
