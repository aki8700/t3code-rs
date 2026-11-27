CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    workspace_root TEXT NOT NULL,
    default_model TEXT,
    instructions TEXT,
    verify_command TEXT,
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
    cache_read_tokens INTEGER,
    cache_creation_tokens INTEGER,
    reasoning_tokens INTEGER,
    duration_ms INTEGER,
    error_message TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    restart_resumes INTEGER NOT NULL DEFAULT 0,
    UNIQUE (id, thread_id)
);
CREATE INDEX IF NOT EXISTS runs_thread_ordinal ON runs(thread_id, ordinal);

CREATE TABLE IF NOT EXISTS run_orphan_processes (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    process_group_id INTEGER NOT NULL,
    start_time TEXT NOT NULL,
    cleanup_pending INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, process_group_id, start_time)
);

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

CREATE TABLE IF NOT EXISTS queued_run_inputs (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    input_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS runs_status_thread_ordinal ON runs(status, thread_id, ordinal);

CREATE TABLE IF NOT EXISTS run_checkpoints (
    run_id TEXT PRIMARY KEY NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    pre_tree TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS run_checkpoints_thread ON run_checkpoints(thread_id);

CREATE TABLE IF NOT EXISTS swarm_boards (
    id TEXT PRIMARY KEY NOT NULL,
    coordinator_thread_id TEXT UNIQUE NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS swarm_members (
    board_id TEXT NOT NULL REFERENCES swarm_boards(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    thread_id TEXT UNIQUE NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    effort TEXT,
    state TEXT NOT NULL,
    role TEXT NOT NULL,
    last_read_seq INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    PRIMARY KEY (board_id, name)
);

CREATE TABLE IF NOT EXISTS swarm_messages (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    board_id TEXT NOT NULL REFERENCES swarm_boards(id) ON DELETE CASCADE,
    sender TEXT NOT NULL,
    text TEXT NOT NULL,
    mentions_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS swarm_messages_board_seq ON swarm_messages(board_id, seq);

CREATE TABLE IF NOT EXISTS thread_context (
    thread_id TEXT PRIMARY KEY NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    session_id TEXT,
    context_tokens INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
