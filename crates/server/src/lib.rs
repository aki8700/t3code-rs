//! HTTP, WebSocket, and MCP services for t3code-rs.

mod assets;
mod attachments;
pub mod auth;
mod compact;
pub mod coordinator;
mod database;
mod export;
mod limits;
pub mod privacy;
mod provider_hold;
mod resume;
pub mod routes;
mod settings;
mod shell;
mod sleep_guard;
mod swarm;
mod threads;
mod usage;
mod verify;
mod workspace_meta;
mod ws;

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_core::{CoreError, CoreResult, McpInjectionSpec, ServerConfig, Store, create_private_dir};
use t3_git::GitManager;
use t3_mcp::McpServer;
use t3_store::{DaemonLock, SqliteStore, acquire_daemon_lock};
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
    settings: settings::SettingsStore,
    auth: AuthLayer,
    usage_home: Option<PathBuf>,
    usage_cache: Arc<Mutex<usage::UsageScanCache>>,
    limits_cache: Arc<limits::UsageLimitsCache>,
    shells: shell::ActiveShells,
    mcp: Arc<McpServer<RunCoordinator>>,
    daemon_lock: Arc<DaemonLock>,
    startup_orphaned_threads: Vec<Uuid>,
}

impl ServerState {
    pub fn new(config: ServerConfig, server_url: String) -> CoreResult<Self> {
        let tokens = auth::McpTokens::default();
        let auth = AuthLayer::new(&config, tokens.clone())?;
        create_private_dir(&config.data_dir).map_err(storage_io)?;
        let daemon_lock = Arc::new(acquire_daemon_lock(&config.data_dir)?);
        let store = SqliteStore::open(config.db_path(), config.storage.clone())?;
        let recovered = store.recover_interrupted_runs()?;
        let settings = settings::SettingsStore::load_or_init(&config);
        let injection = McpInjectionSpec {
            binary_path: std::env::current_exe().map_err(internal_io)?,
            server_url,
            internal_token: String::new(),
            thread_id: Uuid::nil(),
        };
        let limits_cache = Arc::new(limits::UsageLimitsCache::default());
        let coordinator = RunCoordinator::with_tokens_and_limits(
            config.clone(),
            store.clone(),
            injection,
            tokens,
            limits_cache.clone(),
            settings.clone(),
        );
        Ok(Self {
            mcp: Arc::new(McpServer::new(coordinator.clone())),
            coordinator,
            daemon_lock,
            store,
            auth,
            config: Arc::new(config),
            settings,
            usage_home: std::env::home_dir(),
            usage_cache: Arc::new(Mutex::new(usage::UsageScanCache::default())),
            limits_cache,
            shells: shell::ActiveShells::default(),
            startup_orphaned_threads: recovered.thread_ids,
        })
    }

    async fn stop_startup_orphaned_processes(&mut self) {
        for thread_id in self.startup_orphaned_threads.drain(..) {
            resume::stop_orphaned_processes(thread_id).await;
        }
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
    let mut state = ServerState::new(config, format!("http://{mcp_address}"))?;
    state.stop_startup_orphaned_processes().await;
    prune_known_project_snapshots(&state.store).await;
    let coordinator = state.coordinator.clone();
    let _daemon_lock = state.daemon_lock.clone();
    coordinator.resume_queued_runs().await?;
    let _sleep_guard = sleep_guard::spawn(coordinator.clone());
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

async fn prune_known_project_snapshots(store: &SqliteStore) {
    let projects = match store.list_projects().await {
        Ok(projects) => projects,
        Err(error) => {
            tracing::warn!(%error, "Failed to load projects for snapshot retention");
            return;
        }
    };
    for project in projects {
        if GitManager::is_git_repo(&project.workspace_root).await
            && let Err(error) = GitManager::prune_snapshots(&project.workspace_root).await
        {
            tracing::warn!(project = %project.id, path = %project.workspace_root.display(), %error, "Failed to prune expired worktree snapshots");
        }
    }
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

#[cfg(all(test, unix))]
mod startup_tests {
    use super::*;
    use chrono::Utc;
    use std::{os::unix::process::CommandExt, path::PathBuf};
    use t3_core::{
        InteractionMode, ModelSelection, NewTurnItem, ProviderId, Run, RunStatus, RuntimeMode,
        Store, Thread, ThreadStatus, TurnItemKind,
    };
    use t3_store::QueuedRunInput;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("t3-startup-test-{}", Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn second_restart_at_resume_limit_stops_leftover_provider_process() {
        let dir = TempDir::new();
        let config = ServerConfig {
            data_dir: dir.0.join("server"),
            enabled_providers: vec![ProviderId::Mock],
            ..ServerConfig::default()
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let store = SqliteStore::open(config.db_path(), config.storage.clone()).unwrap();
        let project = store
            .create_project("Restart test".into(), dir.0.clone())
            .await
            .unwrap();
        let selection = ModelSelection {
            provider: ProviderId::Mock,
            model: "mock".into(),
            ..ModelSelection::default()
        };
        let now = Utc::now();
        let thread = store
            .create_thread(Thread {
                id: Uuid::new_v4(),
                project_id: project.id,
                parent_thread_id: None,
                relationship: None,
                title: "Restart test".into(),
                model_selection: selection.clone(),
                runtime_mode: RuntimeMode::AutoAcceptEdits,
                interaction_mode: InteractionMode::Plan,
                status: ThreadStatus::Idle,
                worktree_path: None,
                branch: None,
                provider_session_id: None,
                archived: false,
                pinned: false,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        let run = Run {
            id: Uuid::new_v4(),
            thread_id: thread.id,
            ordinal: 1,
            provider: ProviderId::Mock,
            model: "mock".into(),
            effort: None,
            status: RunStatus::Queued,
            prompt: "queued".into(),
            provider_session_id: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            duration_ms: None,
            error_message: None,
            started_at: now,
            completed_at: None,
        };
        let run_id = run.id;
        let _ = store
            .enqueue_run(
                run,
                NewTurnItem {
                    thread_id: thread.id,
                    run_id,
                    kind: TurnItemKind::UserMessage,
                    summary: None,
                    payload: serde_json::json!({"text": "queued"}),
                    searchable_text: None,
                },
                QueuedRunInput {
                    model_selection: selection.clone(),
                    attachment_ids: Vec::new(),
                    notify_parent: false,
                    wake_depth: 0,
                    mode_ceiling: None,
                    verify_fix_depth: 0,
                    last_verify_signature: None,
                    verify_dead_ends: Vec::new(),
                    fallback_attempts: 0,
                },
            )
            .await
            .unwrap();
        store
            .start_queued_run(run_id, selection.clone(), None)
            .await
            .unwrap();
        store
            .update_thread_session_id(thread.id, "session-1".into())
            .await
            .unwrap();
        assert_eq!(
            store.recover_interrupted_runs().unwrap().resumed,
            vec![run_id]
        );
        store
            .start_queued_run(run_id, selection, Some("session-1".into()))
            .await
            .unwrap();
        drop(store);

        let thread_id = thread.id.to_string();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30; :", "sh", "--thread-id", thread_id.as_str()])
            .process_group(0)
            .spawn()
            .unwrap();
        let mut state = ServerState::new(config, "http://127.0.0.1:3774".into()).unwrap();
        state.stop_startup_orphaned_processes().await;

        let status = child.try_wait().unwrap();
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(
            status.is_some(),
            "leftover provider process was not stopped"
        );
        assert!(!status.unwrap().success());
        assert_eq!(
            state.store.list_runs(thread.id).await.unwrap()[0].status,
            RunStatus::Cancelled
        );
    }
}
