use std::{
    collections::HashSet,
    error::Error,
    fmt::{Display, Write},
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Params, Row, TransactionBehavior, params, types::Type,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use t3_core::{
    Attachment, ContextHandoff, CoreError, CoreResult, DbStats, InteractionMode, ModelSelection,
    NewTurnItem, Project, ProviderId, Run, RunStatus, RuntimeMode, SearchHit, StorageConfig, Store,
    Thread, ThreadStatus, TurnItem,
};
use uuid::Uuid;

const MIGRATIONS: &[&str] = &[
    "ALTER TABLE projects ADD COLUMN instructions TEXT",
    "ALTER TABLE projects ADD COLUMN verify_command TEXT",
    "ALTER TABLE runs ADD COLUMN cache_read_tokens INTEGER",
    "ALTER TABLE runs ADD COLUMN cache_creation_tokens INTEGER",
    "ALTER TABLE runs ADD COLUMN reasoning_tokens INTEGER",
    "ALTER TABLE runs ADD COLUMN restart_resumes INTEGER NOT NULL DEFAULT 0",
    "CREATE TABLE IF NOT EXISTS run_orphan_processes (
        run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
        process_group_id INTEGER NOT NULL,
        start_time TEXT NOT NULL,
        cleanup_pending INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (run_id, process_group_id, start_time)
    )",
    "INSERT OR IGNORE INTO fts_item_rows (item_id, thread_id, fts_rowid)
        SELECT item_id, thread_id, rowid FROM fts_entries WHERE item_id IS NOT NULL",
    "ALTER TABLE swarm_messages ADD COLUMN sender_run_id TEXT",
    "CREATE INDEX IF NOT EXISTS swarm_messages_board_sender_run
        ON swarm_messages(board_id, sender, sender_run_id)",
    "CREATE TABLE IF NOT EXISTS swarm_wake_deliveries (
        thread_id TEXT NOT NULL,
        run_id TEXT NOT NULL,
        from_seq INTEGER NOT NULL,
        to_seq INTEGER NOT NULL,
        redeliveries INTEGER NOT NULL DEFAULT 0,
        wake_depth INTEGER NOT NULL DEFAULT 0,
        mode_ceiling_json TEXT,
        created_at TEXT NOT NULL,
        PRIMARY KEY (thread_id, run_id, from_seq, to_seq)
    )",
    "CREATE INDEX IF NOT EXISTS swarm_wake_deliveries_thread
        ON swarm_wake_deliveries(thread_id)",
];

const PREVIEW_BYTES: usize = 64 * 1024;
const READER_COUNT: usize = 4;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);
const ORPHAN_BLOBS: &str = "NOT EXISTS (
    SELECT 1 FROM turn_items WHERE blob_hash = blob_store.hash
)";

mod gc;
mod history;
mod items;
mod prune;
mod queue;
mod recovery;
mod rows;
mod store;
mod swarm;
#[cfg(test)]
mod tests;

pub use gc::{DaemonLock, GcReport, acquire_daemon_lock};
pub use history::ImportHistoryReport;
pub use queue::{QueuedRun, QueuedRunInput, VerifyDeadEnd};
pub use recovery::{INTERRUPTED_BY_RESTART, OrphanProcess, RecoveredRuns};
pub use swarm::SwarmWakeDeliveryRow;

#[cfg(unix)]
use gc::lock_directory;
use gc::open_gc_read_only;
use items::*;
use rows::*;

#[derive(Clone)]
pub struct SqliteStore {
    connections: Arc<StoreConnections>,
    storage_cfg: StorageConfig,
}

struct StoreConnections {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
    _checkpoint: Option<WalCheckpoint>,
    _storage_lock: Option<File>,
}

impl StoreConnections {
    fn reader(&self) -> &Mutex<Connection> {
        if self.readers.is_empty() {
            return &self.writer;
        }
        let index = self.next_reader.fetch_add(1, Ordering::Relaxed) % self.readers.len();
        &self.readers[index]
    }
}

struct WalCheckpoint {
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl WalCheckpoint {
    fn start(path: &Path) -> CoreResult<Self> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(storage_error)?;
        connection
            .execute_batch("PRAGMA busy_timeout = 5000; PRAGMA synchronous = NORMAL;")
            .map_err(storage_error)?;
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let signal = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("t3-wal-checkpoint".into())
            .spawn(move || {
                let (lock, wake) = &*signal;
                loop {
                    let stopping = lock.lock().unwrap_or_else(|error| error.into_inner());
                    let (stopping, _) = wake
                        .wait_timeout_while(stopping, CHECKPOINT_INTERVAL, |stop| !*stop)
                        .unwrap_or_else(|error| error.into_inner());
                    let shutdown = *stopping;
                    drop(stopping);
                    if let Err(error) = connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE)") {
                        tracing::warn!(%error, "SQLite WAL checkpoint failed");
                    }
                    if shutdown {
                        break;
                    }
                }
            })
            .map_err(storage_error)?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for WalCheckpoint {
    fn drop(&mut self) {
        let (lock, wake) = &*self.stop;
        *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
        wake.notify_one();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("SQLite WAL checkpoint thread panicked");
        }
    }
}

impl SqliteStore {
    pub fn open(db_path: impl AsRef<Path>, storage_cfg: StorageConfig) -> CoreResult<Self> {
        let db_path = std::path::absolute(db_path.as_ref()).map_err(storage_error)?;
        #[cfg(unix)]
        let storage_lock = {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let lock = lock_directory(&db_path)?;
            // The daemon holds a shared lock across attachment file and row creation.
            lock.try_lock_shared()
                .map_err(io::Error::from)
                .map_err(storage_error)?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&db_path)
                .map_err(storage_error)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(storage_error)?;
            Some(lock)
        };
        #[cfg(not(unix))]
        let storage_lock = None;
        let connection = Connection::open(&db_path).map_err(storage_error)?;
        Self::initialize(connection, storage_cfg, Some(&db_path), storage_lock)
    }

    pub fn open_in_memory(storage_cfg: StorageConfig) -> CoreResult<Self> {
        let connection = Connection::open_in_memory().map_err(storage_error)?;
        Self::initialize(connection, storage_cfg, None, None)
    }

    fn initialize(
        mut connection: Connection,
        storage_cfg: StorageConfig,
        db_path: Option<&Path>,
        storage_lock: Option<File>,
    ) -> CoreResult<Self> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        if db_path.is_some() {
            let mode: String = connection
                .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
                .map_err(storage_error)?;
            if mode != "wal" {
                return Err(storage_error(format!(
                    "SQLite did not enable WAL (mode: {mode})"
                )));
            }
        }
        connection
            .execute_batch(
                "PRAGMA synchronous=NORMAL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA temp_store=MEMORY;",
            )
            .map_err(storage_error)?;
        let transaction = connection.transaction().map_err(storage_error)?;
        transaction
            .execute_batch(include_str!("schema.sql"))
            .map_err(storage_error)?;
        for migration in MIGRATIONS {
            if let Err(error) = transaction.execute_batch(migration)
                && !matches!(
                    &error,
                    rusqlite::Error::SqliteFailure(_, Some(message))
                        if message.to_ascii_lowercase().contains("duplicate column name")
                )
            {
                return Err(storage_error(error));
            }
        }
        transaction.commit().map_err(storage_error)?;
        let mut readers = Vec::new();
        let checkpoint = if let Some(path) = db_path {
            readers.reserve_exact(READER_COUNT);
            for _ in 0..READER_COUNT {
                let reader = Connection::open_with_flags(
                    path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(storage_error)?;
                reader
                    .execute_batch(
                        "PRAGMA busy_timeout = 5000;
                         PRAGMA query_only = ON;
                         PRAGMA cache_size = -8000;
                         PRAGMA temp_store = MEMORY;",
                    )
                    .map_err(storage_error)?;
                readers.push(Mutex::new(reader));
            }
            Some(WalCheckpoint::start(path)?)
        } else {
            None
        };
        Ok(Self {
            connections: Arc::new(StoreConnections {
                writer: Mutex::new(connection),
                readers,
                next_reader: AtomicUsize::new(0),
                _checkpoint: checkpoint,
                _storage_lock: storage_lock,
            }),
            storage_cfg,
        })
    }

    pub async fn gemini_app_session(
        &self,
        thread: Uuid,
    ) -> CoreResult<Option<(String, String, String)>> {
        self.with_reader(move |connection| {
            connection.query_row(
                "SELECT conversation_id, model, effort FROM gemini_app_sessions WHERE thread_id = ?1",
                [thread.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(storage_error)
        }).await
    }

    pub async fn bind_gemini_app_session(
        &self,
        thread: Uuid,
        conversation: String,
        model: String,
        effort: String,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            connection.execute(
                "INSERT INTO gemini_app_sessions (thread_id, conversation_id, model, effort) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(thread_id) DO UPDATE SET conversation_id = excluded.conversation_id,
                 model = excluded.model, effort = excluded.effort",
                rusqlite::params![thread.to_string(), conversation, model, effort],
            ).map_err(storage_error)?;
            Ok(())
        }).await
    }

    async fn with_writer<T, F>(&self, operation: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> CoreResult<T> + Send + 'static,
    {
        let connections = Arc::clone(&self.connections);
        tokio::task::spawn_blocking(move || {
            let mut connection = lock_connection(&connections.writer)?;
            operation(&mut connection)
        })
        .await
        .map_err(storage_error)?
    }

    async fn with_reader<T, F>(&self, operation: F) -> CoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> CoreResult<T> + Send + 'static,
    {
        let connections = Arc::clone(&self.connections);
        tokio::task::spawn_blocking(move || {
            let connection = lock_connection(connections.reader())?;
            operation(&connection)
        })
        .await
        .map_err(storage_error)?
    }

    /// Imports active project titles and workspace roots from a legacy database.
    pub async fn import_projects_from_t3_sqlite(&self, legacy_db_path: &Path) -> CoreResult<usize> {
        let legacy_path = legacy_db_path.to_owned();
        self.with_writer(move |connection| {
            let legacy = Connection::open_with_flags(legacy_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(storage_error)?;
            let projects = query_all(
                &legacy,
                "SELECT title, workspace_root FROM projection_projects WHERE deleted_at IS NULL",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?;
            let transaction = connection.transaction().map_err(storage_error)?;
            let mut inserted = 0;
            for (title, workspace_root) in projects {
                inserted += transaction
                    .execute(
                        "INSERT INTO projects (id, title, workspace_root, created_at, updated_at)
                         SELECT ?1, ?2, ?3, ?4, ?4
                         WHERE NOT EXISTS (SELECT 1 FROM projects WHERE workspace_root = ?3)",
                        params![
                            Uuid::new_v4().to_string(),
                            title,
                            workspace_root,
                            timestamp(Utc::now())
                        ],
                    )
                    .map_err(storage_error)?;
            }
            transaction.commit().map_err(storage_error)?;
            tracing::debug!(inserted, "Imported legacy project metadata");
            Ok(inserted)
        })
        .await
    }

    pub async fn update_project(
        &self,
        id: Uuid,
        title: Option<String>,
        default_model: Option<Option<ModelSelection>>,
        instructions: Option<Option<String>>,
        verify_command: Option<Option<String>>,
    ) -> CoreResult<Project> {
        self.with_writer(move |connection| {
            let current = query_required(
                connection,
                "SELECT id, title, workspace_root, default_model, instructions, verify_command,
                        created_at, updated_at FROM projects WHERE id = ?1",
                "project",
                id,
                project_row,
            )?;
            let title = title.unwrap_or(current.title);
            let default_model = default_model.unwrap_or(current.default_model);
            let instructions = instructions
                .map(trimmed_optional)
                .unwrap_or(current.instructions);
            let verify_command = verify_command
                .map(trimmed_optional)
                .unwrap_or(current.verify_command);
            let updated_at = Utc::now();
            connection
                .execute(
                    "UPDATE projects SET title = ?2, default_model = ?3, instructions = ?4,
                    verify_command = ?5, updated_at = ?6 WHERE id = ?1",
                    params![
                        id.to_string(),
                        title,
                        default_model.as_ref().map(encode).transpose()?,
                        instructions,
                        verify_command,
                        timestamp(updated_at),
                    ],
                )
                .map_err(storage_error)?;
            Ok(Project {
                title,
                default_model,
                instructions,
                verify_command,
                updated_at,
                ..current
            })
        })
        .await
    }
}
