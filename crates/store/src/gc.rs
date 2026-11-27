use super::*;

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
pub struct DaemonLock(pub(super) File);

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

impl SqliteStore {
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
}

#[cfg(unix)]
pub(super) fn lock_directory(db_path: &Path) -> CoreResult<File> {
    let directory = db_path
        .parent()
        .ok_or_else(|| storage_error("database needs a parent directory"))?;
    File::open(directory).map_err(storage_error)
}

pub(super) fn open_gc_read_only(path: &Path, idle: bool) -> CoreResult<Connection> {
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
