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

/// Error message of a run that a server restart cancelled.
pub const INTERRUPTED_BY_RESTART: &str = "Interrupted by server restart";

/// The runs `SqliteStore::recover_interrupted_runs` found in flight.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveredRuns {
    /// Runs put back in the queue to resume their provider session.
    pub resumed: Vec<Uuid>,
    pub cancelled: Vec<Uuid>,
    /// Provider process groups still requiring restart cleanup.
    pub orphan_processes: Vec<OrphanProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanProcess {
    pub run_id: Uuid,
    pub process_group_id: i32,
    pub start_time: String,
}

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
];

const PREVIEW_BYTES: usize = 64 * 1024;
const READER_COUNT: usize = 4;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);
const ORPHAN_BLOBS: &str = "NOT EXISTS (
    SELECT 1 FROM turn_items WHERE blob_hash = blob_store.hash
)";

mod queue;
mod swarm;
pub use queue::{QueuedRun, QueuedRunInput, VerifyDeadEnd};
mod history;
mod prune;

pub use history::ImportHistoryReport;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GcReport {
    pub applied: bool,
    pub orphan_blobs_count: u64,
    pub orphan_blobs_bytes: u64,
    pub orphan_attachments_count: u64,
    pub orphan_attachments_bytes: u64,
    pub db_bytes_before: u64,
    pub db_bytes_after: u64,
}

/// An exclusive lease on the daemon's data directory.
#[derive(Debug)]
pub struct DaemonLock(File);

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // Closing alone leaves the lock held while a child process that is being
        // spawned still shares this descriptor (between fork and exec).
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "failed to release the daemon lock");
        }
    }
}

pub fn acquire_daemon_lock(data_dir: &Path) -> CoreResult<DaemonLock> {
    let mut options = File::options();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let path = data_dir.join("t3d.lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(storage_error("daemon lock must be a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(storage_error(error)),
    }
    let file = options.open(path).map_err(storage_error)?;
    let metadata = file.metadata().map_err(storage_error)?;
    if !metadata.is_file() {
        return Err(storage_error("daemon lock must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(storage_error("daemon lock must be private to its owner"));
        }
    }
    // File::try_lock uses flock(LOCK_EX | LOCK_NB) on Unix.
    match file.try_lock() {
        Ok(()) => Ok(DaemonLock(file)),
        Err(fs::TryLockError::WouldBlock) => {
            Err(storage_error("another daemon is using this data directory"))
        }
        Err(fs::TryLockError::Error(error)) => Err(storage_error(error)),
    }
}

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
    /// Saves the process identity as soon as a provider CLI starts.
    pub async fn record_orphan_process(
        &self,
        run_id: Uuid,
        process_group_id: i32,
        start_time: String,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "INSERT INTO run_orphan_processes (run_id, process_group_id, start_time)
                 SELECT ?1, ?2, ?3 WHERE EXISTS (
                    SELECT 1 FROM runs WHERE id = ?1 AND status IN (?4, ?5)
                 ) ON CONFLICT(run_id, process_group_id, start_time) DO NOTHING",
                    params![
                        run_id.to_string(),
                        process_group_id,
                        start_time,
                        encode(&RunStatus::Running)?,
                        encode(&RunStatus::RateLimited)?
                    ],
                )
                .map_err(storage_error)?;
            Ok(())
        })
        .await
    }

    /// Clears a cleanup request only after the recorded process group is confirmed gone.
    pub async fn clear_orphan_cleanup(&self, process: OrphanProcess) -> CoreResult<()> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "DELETE FROM run_orphan_processes
                     WHERE run_id = ?1 AND process_group_id = ?2 AND start_time = ?3
                        AND cleanup_pending = 1",
                    params![
                        process.run_id.to_string(),
                        process.process_group_id,
                        process.start_time
                    ],
                )
                .map_err(storage_error)?;
            Ok(())
        })
        .await
    }

    /// Removes process identities after a provider run has completed and cleaned up its groups.
    pub async fn clear_run_orphan_processes(&self, run_id: Uuid) -> CoreResult<()> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "DELETE FROM run_orphan_processes WHERE run_id = ?1 AND cleanup_pending = 0",
                    [run_id.to_string()],
                )
                .map_err(storage_error)?;
            Ok(())
        })
        .await
    }

    /// Returns threads whose provider processes still need restart cleanup.
    pub async fn threads_with_pending_orphan_processes(&self) -> CoreResult<Vec<Uuid>> {
        self.with_reader(|connection| {
            query_all(
                connection,
                "SELECT DISTINCT runs.thread_id
                 FROM run_orphan_processes
                 JOIN runs ON runs.id = run_orphan_processes.run_id
                 WHERE run_orphan_processes.cleanup_pending = 1
                 ORDER BY runs.thread_id",
                [],
                |row| parse(row, 0),
            )
        })
        .await
    }

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

    /// Open existing storage for maintenance without schema initialization.
    pub fn open_for_gc(
        db_path: &Path,
        storage_cfg: StorageConfig,
        apply: bool,
    ) -> CoreResult<Self> {
        let db_path = std::path::absolute(db_path).map_err(storage_error)?;
        match fs::metadata(&db_path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && !apply => {
                return Self::open_in_memory(storage_cfg);
            }
            Err(error) => return Err(storage_error(error)),
        }
        #[cfg(unix)]
        let (idle, storage_lock) = {
            let lock = lock_directory(&db_path)?;
            match lock.try_lock() {
                Ok(()) => (true, Some(lock)),
                Err(fs::TryLockError::WouldBlock) if !apply => (false, None),
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(storage_error(
                        "stop the server using this data directory before applying GC",
                    ));
                }
                Err(fs::TryLockError::Error(error)) => return Err(storage_error(error)),
            }
        };
        #[cfg(not(unix))]
        let (idle, storage_lock) = (true, None);
        let connection = if apply {
            Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
                .map_err(storage_error)?
        } else {
            open_gc_read_only(&db_path, idle)?
        };
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        Ok(Self {
            connections: Arc::new(StoreConnections {
                writer: Mutex::new(connection),
                readers: Vec::new(),
                next_reader: AtomicUsize::new(0),
                _checkpoint: None,
                _storage_lock: storage_lock,
            }),
            storage_cfg,
        })
    }

    /// Inspect storage or compact it. Callers must exclude attachment uploads
    /// when applying GC. The maintenance opener takes an exclusive daemon lock.
    pub async fn gc_storage(&self, attachments_dir: &Path, apply: bool) -> CoreResult<GcReport> {
        let attachments_dir = attachments_dir.to_owned();
        self.with_writer(move |connection| {
            let db_bytes_before = database_file_bytes(connection)?;
            let behavior = if apply {
                TransactionBehavior::Immediate
            } else {
                TransactionBehavior::Deferred
            };
            let transaction = connection
                .transaction_with_behavior(behavior)
                .map_err(storage_error)?;
            let (orphan_blobs_count, orphan_blobs_bytes) = transaction
                .query_row(
                    &format!(
                        "SELECT count(*), coalesce(sum(compressed_bytes), 0)
                         FROM blob_store WHERE {ORPHAN_BLOBS}"
                    ),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(storage_error)?;
            let attachments = orphan_attachment_files(&transaction, &attachments_dir)?;
            let orphan_attachments_count = attachments.len() as u64;
            let orphan_attachments_bytes = attachments.iter().map(|(_, bytes)| bytes).sum();
            let db_bytes_after = if apply {
                transaction
                    .execute(&format!("DELETE FROM blob_store WHERE {ORPHAN_BLOBS}"), [])
                    .map_err(storage_error)?;
                transaction.commit().map_err(storage_error)?;
                for (path, _) in attachments {
                    match fs::symlink_metadata(&path) {
                        Ok(metadata) if metadata.is_file() => {
                            fs::remove_file(&path).map_err(storage_error)?;
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(storage_error(error)),
                    }
                }
                checkpoint_truncate(connection)?;
                connection.execute_batch("VACUUM").map_err(storage_error)?;
                checkpoint_truncate(connection)?;
                database_file_bytes(connection)?
            } else {
                db_bytes_before
            };
            Ok(GcReport {
                applied: apply,
                orphan_blobs_count,
                orphan_blobs_bytes,
                orphan_attachments_count,
                orphan_attachments_bytes,
                db_bytes_before,
                db_bytes_after,
            })
        })
        .await
    }

    /// Reconcile runs left in flight when the previous server stopped.
    ///
    /// An in-flight run goes back to the queue, at most once, when its thread still holds the
    /// provider session of that run and its queued inputs survive; the coordinator then resumes
    /// that session. Every other in-flight run is cancelled. Runs that already ended, including
    /// those that ended rate limited, keep their status.
    pub fn recover_interrupted_runs(&self) -> CoreResult<RecoveredRuns> {
        let mut connection = lock_connection(&self.connections.writer)?;
        let transaction = connection.transaction().map_err(storage_error)?;
        let now = timestamp(Utc::now());
        let in_flight = query_all(
            &transaction,
            "SELECT runs.id, runs.thread_id, runs.provider, runs.model, runs.restart_resumes,
                EXISTS (SELECT 1 FROM queued_run_inputs WHERE run_id = runs.id),
                threads.provider_session_id, threads.model_selection, threads.archived
             FROM runs JOIN threads ON threads.id = runs.thread_id
             WHERE runs.status IN (?1, ?2) AND runs.completed_at IS NULL",
            params![
                encode(&RunStatus::Running)?,
                encode(&RunStatus::RateLimited)?
            ],
            |row| {
                let provider: ProviderId = decode(row, 2)?;
                let model: String = row.get(3)?;
                let selection: ModelSelection = decode(row, 7)?;
                let session: Option<String> = row.get(6)?;
                let resumable = row.get::<_, i64>(4)? == 0
                    && row.get::<_, bool>(5)?
                    && !row.get::<_, bool>(8)?
                    && selection.provider == provider
                    && selection.model == model;
                Ok((
                    parse::<Uuid>(row, 0)?,
                    parse::<Uuid>(row, 1)?,
                    session.filter(|_| resumable),
                ))
            },
        )?;
        let mut recovered = RecoveredRuns::default();
        for (run_id, thread_id, session) in in_flight {
            transaction
                .execute(
                    "UPDATE run_orphan_processes SET cleanup_pending = 1 WHERE run_id = ?1",
                    [run_id.to_string()],
                )
                .map_err(storage_error)?;
            if let Some(session) = session {
                transaction
                    .execute(
                        "UPDATE runs SET status = ?2, provider_session_id = ?3,
                            restart_resumes = restart_resumes + 1, completed_at = NULL,
                            error_message = NULL WHERE id = ?1",
                        params![run_id.to_string(), encode(&RunStatus::Queued)?, session],
                    )
                    .map_err(storage_error)?;
                recovered.resumed.push(run_id);
            } else {
                transaction
                    .execute(
                        "UPDATE runs SET status = ?2, completed_at = ?3, error_message = ?4
                         WHERE id = ?1",
                        params![
                            run_id.to_string(),
                            encode(&RunStatus::Cancelled)?,
                            now,
                            INTERRUPTED_BY_RESTART
                        ],
                    )
                    .map_err(storage_error)?;
                transaction
                    .execute(
                        "DELETE FROM queued_run_inputs WHERE run_id = ?1",
                        [run_id.to_string()],
                    )
                    .map_err(storage_error)?;
                recovered.cancelled.push(run_id);
            }
            transaction
                .execute(
                    "UPDATE threads SET status = ?2, updated_at = ?3 WHERE id = ?1",
                    params![
                        thread_id.to_string(),
                        encode(&ThreadStatus::Interrupted)?,
                        now
                    ],
                )
                .map_err(storage_error)?;
        }
        transaction
            .execute(
                "UPDATE threads SET status = ?1, updated_at = ?2 WHERE status = ?3",
                params![
                    encode(&ThreadStatus::Interrupted)?,
                    now,
                    encode(&ThreadStatus::Running)?,
                ],
            )
            .map_err(storage_error)?;
        transaction.execute(
            "UPDATE swarm_members SET state = 'waiting' WHERE role = 'worker' AND state = 'working'",
            [],
        ).map_err(storage_error)?;
        recovered.orphan_processes = query_all(
            &transaction,
            "SELECT run_id, process_group_id, start_time FROM run_orphan_processes
             WHERE cleanup_pending = 1 ORDER BY run_id, process_group_id",
            [],
            |row| {
                Ok(OrphanProcess {
                    run_id: parse(row, 0)?,
                    process_group_id: row.get(1)?,
                    start_time: row.get(2)?,
                })
            },
        )?;
        transaction.commit().map_err(storage_error)?;
        if !recovered.resumed.is_empty() || !recovered.cancelled.is_empty() {
            tracing::info!(
                resumed = recovered.resumed.len(),
                cancelled = recovered.cancelled.len(),
                "Recovered runs interrupted by server restart"
            );
        }
        Ok(recovered)
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

    async fn update_thread(
        &self,
        id: Uuid,
        query: &'static str,
        value: impl rusqlite::ToSql + Send + 'static,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            require_change(
                connection.execute(query, params![id.to_string(), value, timestamp(Utc::now())]),
                "thread",
                id,
            )
        })
        .await
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn create_project(&self, title: String, workspace_root: PathBuf) -> CoreResult<Project> {
        self.with_writer(move |connection| {
            let now = Utc::now();
            let project = Project {
                id: Uuid::new_v4(),
                title,
                workspace_root,
                default_model: None,
                instructions: None,
                verify_command: None,
                created_at: now,
                updated_at: now,
            };
            connection
                .execute(
                    "INSERT INTO projects (id, title, workspace_root, default_model, instructions,
                        verify_command, created_at, updated_at)
                 VALUES (?1, ?2, ?3, NULL, NULL, NULL, ?4, ?4)",
                    params![
                        project.id.to_string(),
                        project.title,
                        path_text(&project.workspace_root)?,
                        timestamp(now)
                    ],
                )
                .map_err(storage_error)?;
            Ok(project)
        })
        .await
    }

    async fn list_projects(&self) -> CoreResult<Vec<Project>> {
        self.with_reader(|connection| {
            query_all(
                connection,
                "SELECT id, title, workspace_root, default_model, instructions, verify_command,
                        created_at, updated_at
                 FROM projects ORDER BY updated_at DESC, id",
                [],
                project_row,
            )
        })
        .await
    }

    async fn get_project(&self, id: Uuid) -> CoreResult<Project> {
        self.with_reader(move |connection| {
            query_required(
                connection,
                "SELECT id, title, workspace_root, default_model, instructions, verify_command,
                        created_at, updated_at
                 FROM projects WHERE id = ?1",
                "project",
                id,
                project_row,
            )
        })
        .await
    }

    async fn delete_project(&self, id: Uuid) -> CoreResult<()> {
        self.with_writer(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            require_change(
                transaction.execute("DELETE FROM projects WHERE id = ?1", [id.to_string()]),
                "project",
                id,
            )?;
            transaction
                .execute(&format!("DELETE FROM blob_store WHERE {ORPHAN_BLOBS}"), [])
                .map_err(storage_error)?;
            transaction.commit().map_err(storage_error)
        })
        .await
    }

    async fn create_thread(&self, thread: Thread) -> CoreResult<Thread> {
        self.with_writer(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            insert_thread(&transaction, &thread)?;
            transaction.commit().map_err(storage_error)?;
            Ok(thread)
        })
        .await
    }

    async fn list_threads(
        &self,
        project_id: Option<Uuid>,
        include_archived: bool,
        limit: usize,
    ) -> CoreResult<Vec<Thread>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                &format!(
                    "{THREAD_SELECT} WHERE (?1 IS NULL OR project_id = ?1) AND (?2 OR NOT archived)
                     ORDER BY pinned DESC, updated_at DESC, id LIMIT ?3"
                ),
                params![
                    project_id.map(|id| id.to_string()),
                    include_archived,
                    sql_limit(limit)
                ],
                thread_row,
            )
        })
        .await
    }

    async fn list_child_threads(&self, parent_thread_id: Uuid) -> CoreResult<Vec<Thread>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                &format!("{THREAD_SELECT} WHERE parent_thread_id = ?1 ORDER BY created_at, id"),
                [parent_thread_id.to_string()],
                thread_row,
            )
        })
        .await
    }

    async fn get_thread(&self, id: Uuid) -> CoreResult<Thread> {
        self.with_reader(move |connection| {
            query_required(
                connection,
                &format!("{THREAD_SELECT} WHERE id = ?1"),
                "thread",
                id,
                thread_row,
            )
        })
        .await
    }

    async fn update_thread_status(&self, id: Uuid, status: ThreadStatus) -> CoreResult<()> {
        self.update_thread(
            id,
            "UPDATE threads SET status = ?2, updated_at = ?3 WHERE id = ?1",
            encode(&status)?,
        )
        .await
    }

    async fn update_thread_session_id(&self, id: Uuid, session_id: String) -> CoreResult<()> {
        self.update_thread(
            id,
            "UPDATE threads SET provider_session_id = ?2, updated_at = ?3 WHERE id = ?1",
            session_id,
        )
        .await
    }

    async fn update_thread_model(&self, id: Uuid, selection: ModelSelection) -> CoreResult<()> {
        self.update_thread(
            id,
            "UPDATE threads SET model_selection = ?2, updated_at = ?3 WHERE id = ?1",
            encode(&selection)?,
        )
        .await
    }

    async fn update_thread_modes(
        &self,
        id: Uuid,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            require_change(
                connection.execute(
                    "UPDATE threads SET runtime_mode = ?2, interaction_mode = ?3, updated_at = ?4
                     WHERE id = ?1",
                    params![
                        id.to_string(),
                        encode(&runtime_mode)?,
                        encode(&interaction_mode)?,
                        timestamp(Utc::now())
                    ],
                ),
                "thread",
                id,
            )
        })
        .await
    }

    async fn update_thread_title(&self, id: Uuid, title: String) -> CoreResult<()> {
        self.with_writer(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            let key = id.to_string();
            require_change(
                transaction.execute(
                    "UPDATE threads SET title = ?2, updated_at = ?3 WHERE id = ?1",
                    params![key, title, timestamp(Utc::now())],
                ),
                "thread",
                id,
            )?;
            transaction
                .execute(
                    "UPDATE fts_entries SET title = ?2 WHERE thread_id = ?1 AND item_id IS NULL",
                    params![key, title],
                )
                .map_err(storage_error)?;
            transaction.commit().map_err(storage_error)
        })
        .await
    }

    async fn archive_thread(&self, id: Uuid, archived: bool) -> CoreResult<()> {
        self.update_thread(
            id,
            "UPDATE threads SET archived = ?2, updated_at = ?3 WHERE id = ?1",
            archived,
        )
        .await
    }

    async fn create_run(&self, run: Run) -> CoreResult<Run> {
        self.with_writer(move |connection| {
            insert_run(connection, &run)?;
            Ok(run)
        })
        .await
    }

    async fn list_runs(&self, thread_id: Uuid) -> CoreResult<Vec<Run>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT id, thread_id, ordinal, provider, model, effort, status, prompt, provider_session_id,
                    input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, reasoning_tokens,
                    duration_ms, error_message, started_at, completed_at
                 FROM runs WHERE thread_id = ?1 ORDER BY ordinal, started_at, id",
                [thread_id.to_string()],
                run_row,
            )
        })
        .await
    }

    async fn finish_run(
        &self,
        run_id: Uuid,
        status: RunStatus,
        provider_session_id: Option<String>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        duration_ms: u64,
        error_message: Option<String>,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            let transaction = connection.transaction().map_err(storage_error)?;
            require_change(
                transaction.execute(
                    "UPDATE runs SET status = ?2, provider_session_id = COALESCE(?3, provider_session_id),
                        input_tokens = ?4, output_tokens = ?5, cache_read_tokens = ?6,
                        cache_creation_tokens = ?7, reasoning_tokens = ?8, duration_ms = ?9,
                        error_message = ?10, completed_at = ?11 WHERE id = ?1",
                    params![
                        run_id.to_string(), encode(&status)?, provider_session_id,
                        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                        reasoning_tokens, duration_ms, error_message, timestamp(Utc::now()),
                    ],
                ),
                "run",
                run_id,
            )?;
            transaction.execute("DELETE FROM queued_run_inputs WHERE run_id = ?1", [run_id.to_string()])
                .map_err(storage_error)?;
            transaction.commit().map_err(storage_error)
        })
        .await
    }

    async fn append_turn_item(&self, item: NewTurnItem) -> CoreResult<TurnItem> {
        let cfg = self.storage_cfg.clone();
        self.with_writer(move |connection| append_item(connection, item, &cfg))
            .await
    }

    async fn list_turn_items(
        &self,
        thread_id: Uuid,
        limit: usize,
        offset: usize,
    ) -> CoreResult<Vec<TurnItem>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT item.id, item.thread_id, item.run_id, item.ordinal, item.kind, item.summary,
                    item.raw_bytes, item.compressed_bytes, item.created_at, item.payload_text,
                    blob.raw_bytes, blob.zstd_data
                 FROM turn_items AS item LEFT JOIN blob_store AS blob ON blob.hash = item.blob_hash
                 WHERE item.thread_id = ?1 ORDER BY item.ordinal LIMIT ?2 OFFSET ?3",
                params![thread_id.to_string(), sql_limit(limit), sql_limit(offset)],
                item_row,
            )
        })
        .await
    }

    async fn list_turn_items_page(
        &self,
        thread_id: Uuid,
        limit: usize,
        before_ordinal: Option<i64>,
    ) -> CoreResult<(Vec<TurnItem>, bool, usize)> {
        self.with_reader(move |connection| {
            let snapshot = connection.unchecked_transaction().map_err(storage_error)?;
            let thread_id = thread_id.to_string();
            let total = snapshot
                .query_row(
                    "SELECT count(*) FROM turn_items WHERE thread_id = ?1",
                    [&thread_id],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            let last = before_ordinal.map_or(Some(i64::MAX), |ordinal| ordinal.checked_sub(1));
            let mut items = query_all(
                &snapshot,
                "SELECT item.id, item.thread_id, item.run_id, item.ordinal, item.kind, item.summary,
                    item.raw_bytes, item.compressed_bytes, item.created_at, item.payload_text,
                    blob.raw_bytes, blob.zstd_data
                 FROM turn_items AS item LEFT JOIN blob_store AS blob ON blob.hash = item.blob_hash
                 WHERE item.thread_id = ?1 AND item.ordinal <= ?2
                 ORDER BY item.ordinal DESC LIMIT ?3",
                params![thread_id, last, sql_limit(limit)],
                item_row,
            )?;
            let has_more = if items.len() == limit {
                let older_than_page = items
                    .last()
                    .map_or(last, |item| item.ordinal.checked_sub(1));
                snapshot
                    .query_row(
                        "SELECT EXISTS (
                            SELECT 1 FROM turn_items WHERE thread_id = ?1 AND ordinal <= ?2
                        )",
                        params![thread_id, older_than_page],
                        |row| row.get(0),
                    )
                    .map_err(storage_error)?
            } else {
                false
            };
            items.reverse();
            snapshot.commit().map_err(storage_error)?;
            Ok((items, has_more, total))
        })
        .await
    }

    async fn record_handoff(&self, handoff: ContextHandoff) -> CoreResult<ContextHandoff> {
        self.with_writer(move |connection| {
            connection.execute(
                "INSERT INTO context_handoffs (id, source_thread_id, target_thread_id, from_model,
                    to_model, summary, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    handoff.id.to_string(), handoff.source_thread_id.to_string(),
                    handoff.target_thread_id.to_string(), encode(&handoff.from_model)?,
                    encode(&handoff.to_model)?, handoff.summary, timestamp(handoff.created_at),
                ],
            ).map_err(storage_error)?;
            Ok(handoff)
        })
        .await
    }

    async fn list_handoffs(&self, thread_id: Uuid) -> CoreResult<Vec<ContextHandoff>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT id, source_thread_id, target_thread_id, from_model, to_model, summary, created_at
                 FROM context_handoffs WHERE source_thread_id = ?1 OR target_thread_id = ?1
                 ORDER BY created_at, id",
                [thread_id.to_string()],
                handoff_row,
            )
        })
        .await
    }

    async fn save_attachment(&self, attachment: Attachment) -> CoreResult<Attachment> {
        self.with_writer(move |connection| {
            connection.execute(
                "INSERT INTO attachments (id, thread_id, filename, mime_type, size_bytes, blake3_hash,
                    storage_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    attachment.id.to_string(), attachment.thread_id.map(|id| id.to_string()),
                    attachment.filename, attachment.mime_type, attachment.size_bytes, attachment.blake3_hash,
                    path_text(&attachment.storage_path)?, timestamp(attachment.created_at),
                ],
            ).map_err(storage_error)?;
            Ok(attachment)
        })
        .await
    }

    async fn get_attachment(&self, id: Uuid) -> CoreResult<Attachment> {
        self.with_reader(move |connection| {
            query_required(
                connection,
                &format!("{ATTACHMENT_SELECT} WHERE id = ?1"),
                "attachment",
                id,
                attachment_row,
            )
        })
        .await
    }

    async fn list_attachments(&self, thread_id: Uuid) -> CoreResult<Vec<Attachment>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                &format!("{ATTACHMENT_SELECT} WHERE thread_id = ?1 ORDER BY created_at, id"),
                [thread_id.to_string()],
                attachment_row,
            )
        })
        .await
    }

    async fn search_fts(&self, query: &str, limit: usize) -> CoreResult<Vec<SearchHit>> {
        let query = query.trim().to_owned();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.with_reader(move |connection| {
            let limit = sql_limit(limit);
            if query.chars().count() >= 3 {
                query_all(
                    connection,
                    "SELECT threads.id, threads.title, item_id,
                        snippet(fts_entries, -1, '', '', '…', 24), fts_entries.rank, threads.updated_at
                     FROM fts_entries JOIN threads ON threads.id = fts_entries.thread_id
                     WHERE fts_entries MATCH ?1
                     ORDER BY fts_entries.rank, threads.updated_at DESC, fts_entries.rowid LIMIT ?2",
                    params![format!("\"{}\"", query.replace('"', "\"\"")), limit],
                    search_row,
                )
            } else {
                let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                query_all(
                    connection,
                    "SELECT threads.id, threads.title, item_id,
                        CASE WHEN item_id IS NULL THEN fts_entries.title
                            ELSE substr(body, max(1, instr(lower(body), lower(?3)) - 40), 120) END,
                        0.0, threads.updated_at
                     FROM fts_entries JOIN threads ON threads.id = fts_entries.thread_id
                     WHERE fts_entries.title LIKE '%' || ?1 || '%' ESCAPE '\\'
                        OR body LIKE '%' || ?1 || '%' ESCAPE '\\'
                     ORDER BY threads.updated_at DESC, fts_entries.rowid LIMIT ?2",
                    params![escaped, limit, query],
                    search_row,
                )
            }
        })
        .await
    }

    async fn db_stats(&self) -> CoreResult<DbStats> {
        self.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT (SELECT page_count FROM pragma_page_count) *
                        (SELECT page_size FROM pragma_page_size),
                    (SELECT count(*) FROM threads), (SELECT count(*) FROM runs),
                    (SELECT count(*) FROM turn_items),
                    (SELECT coalesce(sum(raw_bytes), 0) FROM turn_items),
                    (SELECT coalesce(sum(length(CAST(payload_text AS BLOB))), 0) FROM turn_items)
                        + (SELECT coalesce(sum(compressed_bytes), 0) FROM blob_store),
                    (SELECT count(*) FROM blob_store), (SELECT count(*) FROM attachments)",
                    [],
                    |row| {
                        let raw_payload_bytes: u64 = row.get(4)?;
                        let stored_payload_bytes: u64 = row.get(5)?;
                        Ok(DbStats {
                            db_size_bytes: row.get(0)?,
                            threads_count: row.get(1)?,
                            runs_count: row.get(2)?,
                            turn_items_count: row.get(3)?,
                            raw_payload_bytes,
                            stored_payload_bytes,
                            compression_ratio: if raw_payload_bytes == 0 {
                                1.0
                            } else {
                                stored_payload_bytes as f64 / raw_payload_bytes as f64
                            },
                            dedup_blobs_count: row.get(6)?,
                            attachments_count: row.get(7)?,
                        })
                    },
                )
                .map_err(storage_error)
        })
        .await
    }
}

#[cfg(unix)]
fn lock_directory(db_path: &Path) -> CoreResult<File> {
    let directory = db_path
        .parent()
        .ok_or_else(|| storage_error("database needs a parent directory"))?;
    File::open(directory).map_err(storage_error)
}

fn open_gc_read_only(path: &Path, idle: bool) -> CoreResult<Connection> {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let wal_bytes = match fs::metadata(wal) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(storage_error(error)),
    };
    let mut uri = String::from("file:");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            write!(uri, "%{byte:02X}").expect("format into string");
        }
    }
    // Immutable opening avoids creating sidecars on a clean, idle WAL database.
    // An active or uncheckpointed database needs its WAL and existing read-only index.
    uri.push_str(if idle && wal_bytes == 0 {
        "?immutable=1"
    } else {
        "?readonly_shm=1"
    });
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(storage_error)
}

fn database_file_bytes(connection: &Connection) -> CoreResult<u64> {
    match connection.path().filter(|path| !path.is_empty()) {
        Some(path) => fs::metadata(path)
            .map(|metadata| metadata.len())
            .map_err(storage_error),
        None => Ok(0),
    }
}

fn orphan_attachment_files(connection: &Connection, dir: &Path) -> CoreResult<Vec<(PathBuf, u64)>> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(storage_error(error)),
    }
    let references: HashSet<String> = query_all(
        connection,
        "SELECT DISTINCT blake3_hash FROM attachments",
        [],
        |row| row.get(0),
    )?
    .into_iter()
    .collect();
    let mut orphans = Vec::new();
    for entry in fs::read_dir(dir).map_err(storage_error)? {
        let entry = entry.map_err(storage_error)?;
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        let is_hash = bytes.len() == 64
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
        if !(is_hash || bytes.starts_with(b".tmp-"))
            || name.to_str().is_some_and(|name| references.contains(name))
        {
            continue;
        }
        match entry.metadata() {
            Ok(metadata) if metadata.is_file() => orphans.push((entry.path(), metadata.len())),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(storage_error(error)),
        }
    }
    Ok(orphans)
}

fn checkpoint_truncate(connection: &Connection) -> CoreResult<()> {
    let busy = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, i32>(0)
        })
        .map_err(storage_error)?;
    if busy != 0 {
        return Err(storage_error("database is busy during WAL checkpoint"));
    }
    Ok(())
}

fn insert_thread(transaction: &Connection, thread: &Thread) -> CoreResult<()> {
    let id = thread.id.to_string();
    transaction
        .prepare_cached(
            "INSERT INTO threads (
            id, project_id, parent_thread_id, relationship, title, model_selection,
            runtime_mode, interaction_mode, status, worktree_path, branch, provider_session_id,
            archived, pinned, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .map_err(storage_error)?
        .execute(params![
            id,
            thread.project_id.to_string(),
            thread.parent_thread_id.map(|id| id.to_string()),
            thread.relationship,
            thread.title,
            encode(&thread.model_selection)?,
            encode(&thread.runtime_mode)?,
            encode(&thread.interaction_mode)?,
            encode(&thread.status)?,
            thread.worktree_path.as_deref().map(path_text).transpose()?,
            thread.branch,
            thread.provider_session_id,
            thread.archived,
            thread.pinned,
            timestamp(thread.created_at),
            timestamp(thread.updated_at),
        ])
        .map_err(storage_error)?;
    transaction
        .prepare_cached("INSERT INTO fts_entries (thread_id, title) VALUES (?1, ?2)")
        .map_err(storage_error)?
        .execute(params![id, thread.title])
        .map_err(storage_error)?;
    Ok(())
}

fn insert_run(connection: &Connection, run: &Run) -> CoreResult<()> {
    connection
        .prepare_cached(
            "INSERT INTO runs (
            id, thread_id, ordinal, provider, model, effort, status, prompt, provider_session_id,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, reasoning_tokens,
            duration_ms, error_message, started_at, completed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )
        .map_err(storage_error)?
        .execute(params![
            run.id.to_string(),
            run.thread_id.to_string(),
            run.ordinal,
            encode(&run.provider)?,
            run.model,
            run.effort,
            encode(&run.status)?,
            run.prompt,
            run.provider_session_id,
            run.input_tokens,
            run.output_tokens,
            run.cache_read_tokens,
            run.cache_creation_tokens,
            run.reasoning_tokens,
            run.duration_ms,
            run.error_message,
            timestamp(run.started_at),
            run.completed_at.map(timestamp),
        ])
        .map_err(storage_error)?;
    Ok(())
}

fn append_item(
    connection: &mut Connection,
    item: NewTurnItem,
    cfg: &StorageConfig,
) -> CoreResult<TurnItem> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let saved = insert_item(
        &transaction,
        item,
        cfg,
        ItemPosition {
            id: Uuid::new_v4(),
            ordinal: None,
            created_at: Utc::now(),
        },
        true,
    )?;
    transaction.commit().map_err(storage_error)?;
    Ok(saved)
}

struct ItemPosition {
    id: Uuid,
    ordinal: Option<i64>,
    created_at: DateTime<Utc>,
}

fn insert_item(
    connection: &Connection,
    item: NewTurnItem,
    cfg: &StorageConfig,
    position: ItemPosition,
    index_payload: bool,
) -> CoreResult<TurnItem> {
    let ItemPosition {
        id,
        ordinal,
        created_at,
    } = position;
    let NewTurnItem {
        thread_id,
        run_id,
        kind,
        summary,
        mut payload,
        searchable_text,
    } = item;
    let mut text = encode(&payload)?;
    let raw_bytes = text.len();
    if raw_bytes > cfg.max_payload_bytes {
        (payload, text) = prune::prune_payload(payload, text, cfg.max_payload_bytes)?;
    }

    let body = searchable_text
        .as_deref()
        .filter(|text| !text.is_empty())
        .or_else(|| summary.as_deref().filter(|text| !text.is_empty()))
        .or_else(|| {
            if !index_payload {
                return None;
            }
            payload.as_str().or_else(|| {
                ["text", "content", "message", "summary", "output", "preview"]
                    .into_iter()
                    .find_map(|field| payload.get(field).and_then(Value::as_str))
            })
        })
        .map(|text| &text[..text.floor_char_boundary(PREVIEW_BYTES)]);
    let (blob_hash, compressed_bytes) = store_item_blob(connection, &text, raw_bytes, cfg)?;

    let thread_key = thread_id.to_string();
    let ordinal = connection
        .prepare_cached(
            "INSERT INTO turn_items (id, thread_id, run_id, ordinal, kind, summary, payload_text,
            blob_hash, raw_bytes, compressed_bytes, created_at)
         VALUES (?1, ?2, ?3,
            coalesce(?4, (SELECT coalesce(max(ordinal), 0) + 1 FROM turn_items WHERE thread_id = ?2)),
            ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         RETURNING ordinal",
        ).map_err(storage_error)?.query_row(
            params![
                id.to_string(),
                thread_key,
                run_id.to_string(),
                ordinal,
                encode(&kind)?,
                summary,
                blob_hash.is_none().then_some(&text),
                blob_hash,
                raw_bytes,
                compressed_bytes,
                timestamp(created_at),
            ],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    if let Some(body) = body.filter(|body| !body.is_empty()) {
        connection
            .prepare_cached(
                "INSERT INTO fts_entries (thread_id, item_id, body) VALUES (?1, ?2, ?3)",
            )
            .map_err(storage_error)?
            .execute(params![thread_key, id.to_string(), body])
            .map_err(storage_error)?;
    }
    Ok(TurnItem {
        id,
        thread_id,
        run_id,
        ordinal,
        kind,
        summary,
        payload,
        raw_bytes,
        compressed_bytes,
        created_at,
    })
}

fn store_item_blob(
    connection: &Connection,
    text: &str,
    raw_bytes: usize,
    cfg: &StorageConfig,
) -> CoreResult<(Option<String>, Option<usize>)> {
    if raw_bytes > cfg.zstd_threshold_bytes {
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        // A matching blob may have been compressed with a previous configuration.
        let stored_bytes = connection
            .prepare_cached("SELECT compressed_bytes FROM blob_store WHERE hash = ?1")
            .map_err(storage_error)?
            .query_row([&hash], |row| row.get::<_, usize>(0))
            .optional()
            .map_err(storage_error)?;
        let compressed_bytes = if let Some(stored_bytes) = stored_bytes {
            stored_bytes
        } else {
            let data =
                zstd::bulk::compress(text.as_bytes(), cfg.zstd_level).map_err(storage_error)?;
            connection
                .prepare_cached(
                    "INSERT INTO blob_store (hash, raw_bytes, compressed_bytes, zstd_data)
                 VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(storage_error)?
                .execute(params![hash, text.len(), data.len(), data])
                .map_err(storage_error)?;
            data.len()
        };
        Ok((Some(hash), Some(compressed_bytes)))
    } else {
        Ok((None, None))
    }
}

const THREAD_SELECT: &str =
    "SELECT id, project_id, parent_thread_id, relationship, title, model_selection, runtime_mode,
        interaction_mode, status, worktree_path, branch, provider_session_id, archived, pinned,
        created_at, updated_at FROM threads";

const ATTACHMENT_SELECT: &str =
    "SELECT id, thread_id, filename, mime_type, size_bytes, blake3_hash, storage_path, created_at
     FROM attachments";

fn project_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: parse(row, 0)?,
        title: row.get(1)?,
        workspace_root: PathBuf::from(row.get::<_, String>(2)?),
        default_model: row
            .get::<_, Option<String>>(3)?
            .map(|text| serde_json::from_str(&text).map_err(|e| conversion_error(3, Type::Text, e)))
            .transpose()?,
        instructions: row.get(4)?,
        verify_command: row.get(5)?,
        created_at: parse(row, 6)?,
        updated_at: parse(row, 7)?,
    })
}

fn thread_row(row: &Row<'_>) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: parse(row, 0)?,
        project_id: parse(row, 1)?,
        parent_thread_id: parse_optional(row, 2)?,
        relationship: row.get(3)?,
        title: row.get(4)?,
        model_selection: decode(row, 5)?,
        runtime_mode: decode(row, 6)?,
        interaction_mode: decode(row, 7)?,
        status: decode(row, 8)?,
        worktree_path: row.get::<_, Option<String>>(9)?.map(PathBuf::from),
        branch: row.get(10)?,
        provider_session_id: row.get(11)?,
        archived: row.get(12)?,
        pinned: row.get(13)?,
        created_at: parse(row, 14)?,
        updated_at: parse(row, 15)?,
    })
}

fn run_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: parse(row, 0)?,
        thread_id: parse(row, 1)?,
        ordinal: row.get(2)?,
        provider: decode(row, 3)?,
        model: row.get(4)?,
        effort: row.get(5)?,
        status: decode(row, 6)?,
        prompt: row.get(7)?,
        provider_session_id: row.get(8)?,
        input_tokens: row.get(9)?,
        output_tokens: row.get(10)?,
        cache_read_tokens: row.get(11)?,
        cache_creation_tokens: row.get(12)?,
        reasoning_tokens: row.get(13)?,
        duration_ms: row.get(14)?,
        error_message: row.get(15)?,
        started_at: parse(row, 16)?,
        completed_at: parse_optional(row, 17)?,
    })
}

fn item_row(row: &Row<'_>) -> rusqlite::Result<TurnItem> {
    let payload = match row.get::<_, Option<String>>(9)? {
        Some(text) => {
            serde_json::from_str(&text).map_err(|e| conversion_error(9, Type::Text, e))?
        }
        None => {
            let raw_bytes = row.get(10)?;
            let data: Vec<u8> = row.get(11)?;
            let data = zstd::bulk::decompress(&data, raw_bytes)
                .map_err(|e| conversion_error(11, Type::Blob, e))?;
            serde_json::from_slice(&data).map_err(|e| conversion_error(11, Type::Blob, e))?
        }
    };
    Ok(TurnItem {
        id: parse(row, 0)?,
        thread_id: parse(row, 1)?,
        run_id: parse(row, 2)?,
        ordinal: row.get(3)?,
        kind: decode(row, 4)?,
        summary: row.get(5)?,
        payload,
        raw_bytes: row.get(6)?,
        compressed_bytes: row.get(7)?,
        created_at: parse(row, 8)?,
    })
}

fn handoff_row(row: &Row<'_>) -> rusqlite::Result<ContextHandoff> {
    Ok(ContextHandoff {
        id: parse(row, 0)?,
        source_thread_id: parse(row, 1)?,
        target_thread_id: parse(row, 2)?,
        from_model: decode(row, 3)?,
        to_model: decode(row, 4)?,
        summary: row.get(5)?,
        created_at: parse(row, 6)?,
    })
}

fn attachment_row(row: &Row<'_>) -> rusqlite::Result<Attachment> {
    Ok(Attachment {
        id: parse(row, 0)?,
        thread_id: parse_optional(row, 1)?,
        filename: row.get(2)?,
        mime_type: row.get(3)?,
        size_bytes: row.get(4)?,
        blake3_hash: row.get(5)?,
        storage_path: PathBuf::from(row.get::<_, String>(6)?),
        created_at: parse(row, 7)?,
    })
}

fn search_row(row: &Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        thread_id: parse(row, 0)?,
        thread_title: row.get(1)?,
        item_id: parse_optional(row, 2)?,
        snippet: row.get(3)?,
        rank: row.get(4)?,
        updated_at: parse(row, 5)?,
    })
}

fn query_all<T>(
    connection: &Connection,
    sql: &str,
    params: impl Params,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> CoreResult<Vec<T>> {
    let mut stmt = connection.prepare_cached(sql).map_err(storage_error)?;
    let rows = stmt.query_map(params, map).map_err(storage_error)?;
    rows.collect::<rusqlite::Result<_>>().map_err(storage_error)
}

fn query_required<T>(
    connection: &Connection,
    sql: &str,
    entity: &str,
    id: Uuid,
    map: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
) -> CoreResult<T> {
    connection
        .query_row(sql, [id.to_string()], map)
        .optional()
        .map_err(storage_error)?
        .ok_or_else(|| not_found(entity, id))
}

fn require_change(changed: rusqlite::Result<usize>, entity: &str, id: Uuid) -> CoreResult<()> {
    if changed.map_err(storage_error)? == 0 {
        return Err(not_found(entity, id));
    }
    Ok(())
}

fn storage_error(error: impl Display) -> CoreError {
    CoreError::Storage(error.to_string())
}

fn lock_connection(connection: &Mutex<Connection>) -> CoreResult<MutexGuard<'_, Connection>> {
    connection
        .lock()
        .map_err(|_| storage_error("SQLite connection lock poisoned"))
}

fn not_found(entity: &str, id: Uuid) -> CoreError {
    CoreError::NotFound(format!("{entity} {id}"))
}

fn encode(value: &impl Serialize) -> CoreResult<String> {
    serde_json::to_string(value).map_err(storage_error)
}

fn decode<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|error| conversion_error(index, Type::Text, error))
}

fn parse<T: FromStr>(row: &Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T::Err: Error + Send + Sync + 'static,
{
    let text: String = row.get(index)?;
    text.parse()
        .map_err(|error| conversion_error(index, Type::Text, error))
}

fn parse_optional<T: FromStr>(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<T>>
where
    T::Err: Error + Send + Sync + 'static,
{
    row.get::<_, Option<String>>(index)?
        .map(|text| {
            text.parse()
                .map_err(|error| conversion_error(index, Type::Text, error))
        })
        .transpose()
}

fn conversion_error(
    index: usize,
    kind: Type,
    error: impl Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, kind, Box::new(error))
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn sql_limit(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn path_text(path: &Path) -> CoreResult<&str> {
    path.to_str()
        .ok_or_else(|| CoreError::InvalidArgument("path is not valid UTF-8".into()))
}

fn trimmed_optional(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

#[cfg(test)]
mod tests;
