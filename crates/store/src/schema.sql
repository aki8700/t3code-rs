CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    workspace_root TEXT NOT NULL,
    default_model TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS projects_workspace_root ON projects(workspace_root);

CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    parent_thread_id TEXT REFERENCES threads(id) ON DELETE SET NULL,
    relationship TEXT,
    title TEXT NOT NULL,
    model_selection TEXT NOT NULL,
    runtime_mode TEXT NOT NULL,
    interaction_mode TEXT NOT NULL,
    status TEXT NOT NULL,
    worktree_path TEXT,
    branch TEXT,
    provider_session_id TEXT,
    archived INTEGER NOT NULL,
    pinned INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS threads_project ON threads(project_id);
CREATE INDEX IF NOT EXISTS threads_parent ON threads(parent_thread_id);

CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    effort TEXT,
    status TEXT NOT NULL,
    prompt TEXT NOT NULL,
    provider_session_id TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    duration_ms INTEGER,
    error_message TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    UNIQUE (id, thread_id)
);
CREATE INDEX IF NOT EXISTS runs_thread_ordinal ON runs(thread_id, ordinal);

CREATE TABLE IF NOT EXISTS blob_store (
    hash TEXT PRIMARY KEY NOT NULL,
    raw_bytes INTEGER NOT NULL,
    compressed_bytes INTEGER NOT NULL,
    zstd_data BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS turn_items (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL,
    summary TEXT,
    payload_text TEXT,
    blob_hash TEXT REFERENCES blob_store(hash),
    raw_bytes INTEGER NOT NULL,
    compressed_bytes INTEGER,
    created_at TEXT NOT NULL,
    FOREIGN KEY (run_id, thread_id) REFERENCES runs(id, thread_id) ON DELETE CASCADE,
    CHECK ((payload_text IS NULL) <> (blob_hash IS NULL)),
    CHECK ((compressed_bytes IS NULL) = (blob_hash IS NULL)),
    UNIQUE (thread_id, ordinal)
);
CREATE INDEX IF NOT EXISTS turn_items_run ON turn_items(run_id, thread_id);
CREATE INDEX IF NOT EXISTS turn_items_blob ON turn_items(blob_hash) WHERE blob_hash IS NOT NULL;

CREATE TABLE IF NOT EXISTS context_handoffs (
    id TEXT PRIMARY KEY NOT NULL,
    source_thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    target_thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    from_model TEXT NOT NULL,
    to_model TEXT NOT NULL,
    summary TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS handoffs_source ON context_handoffs(source_thread_id);
CREATE INDEX IF NOT EXISTS handoffs_target ON context_handoffs(target_thread_id);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT REFERENCES threads(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    blake3_hash TEXT NOT NULL,
    storage_path TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS attachments_thread ON attachments(thread_id);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_entries USING fts5(
    thread_id UNINDEXED, item_id UNINDEXED, title, body, tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS threads_delete_fts AFTER DELETE ON threads BEGIN
    DELETE FROM fts_entries WHERE thread_id = old.id;
END;
