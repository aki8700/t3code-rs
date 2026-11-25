//! HTTP, WebSocket, and MCP services for t3code-rs.

mod assets;
mod attachments;
pub mod auth;
pub mod coordinator;
mod database;
pub mod privacy;
pub mod routes;
mod threads;
mod usage;
mod ws;

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_core::{CoreError, CoreResult, McpInjectionSpec, ServerConfig, create_private_dir};
use t3_mcp::McpServer;
use t3_store::SqliteStore;
use tokio::{net::TcpListener, sync::watch};
use uuid::Uuid;

pub use auth::AuthLayer;
pub use coordinator::RunCoordinator;
pub use database::orchestration_stats;
pub use privacy::audit_privacy;
pub use routes::router;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct ServerState {
    pub coordinator: RunCoordinator,
    pub store: SqliteStore,
    pub config: Arc<ServerConfig>,
    auth: AuthLayer,
    usage_home: Option<PathBuf>,
    usage_cache: Arc<Mutex<usage::UsageScanCache>>,
    mcp: Arc<McpServer<RunCoordinator>>,
}

impl ServerState {
    pub fn new(config: ServerConfig, server_url: String) -> CoreResult<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| CoreError::Internal(error.to_string()))?;
        let token = blake3::Hash::from_bytes(bytes).to_hex().to_string();
        let auth = AuthLayer::new(&config, Arc::from(token.as_str()))?;
        create_private_dir(&config.data_dir).map_err(storage_io)?;
        let store = SqliteStore::open(config.db_path(), config.storage.clone())?;
        store.recover_interrupted_runs()?;
        let injection = McpInjectionSpec {
            binary_path: std::env::current_exe().map_err(internal_io)?,
            server_url,
            internal_token: token,
            thread_id: Uuid::nil(),
        };
        let coordinator = RunCoordinator::new(config.clone(), store.clone(), injection);
        Ok(Self {
            mcp: Arc::new(McpServer::new(coordinator.clone())),
            coordinator,
            store,
            auth,
            config: Arc::new(config),
            usage_home: std::env::home_dir(),
            usage_cache: Arc::new(Mutex::new(usage::UsageScanCache::default())),
        })
    }
}

pub async fn serve(config: ServerConfig) -> CoreResult<()> {
    config
        .validate()
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(internal_io)?;
    let signal = async {
        #[cfg(unix)]
        {
            tokio::select! {
                result = tokio::signal::ctrl_c() => result,
                _ = terminate.recv() => Ok(()),
            }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await
    };
    let listener = TcpListener::bind((config.host.as_str(), config.port))
        .await
        .map_err(internal_io)?;
    let public_address = listener.local_addr().map_err(internal_io)?;
    let mcp_listener = if public_address.ip().is_loopback() {
        None
    } else {
        let address = match public_address {
            SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        Some(TcpListener::bind((address, 0)).await.map_err(internal_io)?)
    };
    let mcp_address = mcp_listener
        .as_ref()
        .map(TcpListener::local_addr)
        .transpose()
        .map_err(internal_io)?
        .unwrap_or(public_address);
    let state = ServerState::new(config, format!("http://{mcp_address}"))?;
    let coordinator = state.coordinator.clone();
    let (shutdown, stop) = watch::channel(false);
    let public = axum::serve(
        listener,
        router(state.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(wait_for_shutdown(stop.clone()));
    let internal = async move {
        if let Some(listener) = mcp_listener {
            axum::serve(
                listener,
                routes::mcp_router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(wait_for_shutdown(stop))
            .await
        } else {
            wait_for_shutdown(stop).await;
            Ok(())
        }
    };
    let servers = async { tokio::try_join!(async { public.await }, internal).map(|_| ()) };
    tokio::pin!(servers);
    tracing::info!(address = %public_address, "t3d listening");
    tokio::select! {
        result = &mut servers => {
            coordinator.interrupt_all().await;
            result
        },
        result = signal => {
            shutdown.send_replace(true);
            // Drive both listeners while waiting for providers and in-flight requests.
            let drain = async {
                tokio::time::timeout(Duration::from_secs(5), servers).await.unwrap_or(Ok(()))
            };
            let ((), drained) = tokio::join!(coordinator.interrupt_all(), drain);
            result.and(drained)
        }
    }
    .map_err(internal_io)
}

async fn wait_for_shutdown(mut stop: watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

fn storage_io(error: io::Error) -> CoreError {
    CoreError::Storage(error.to_string())
}

fn internal_io(error: io::Error) -> CoreError {
    CoreError::Internal(error.to_string())
}
