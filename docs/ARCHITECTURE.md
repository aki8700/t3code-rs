# t3code-rs Architecture

Personal, zero-telemetry, high-performance Rust rewrite of T3 Code (`pingdotgg/t3code`), optimized from real multi-provider AI usage data.

## Core Design Principles
1. **Zero Telemetry & Zero Credential Reading**: Never reads `~/.codex/auth.json`, `~/.claude.json`, or `~/.t3/userdata/clerk`. Zero PostHog/OTLP/Clerk/T3 Connect calls. The ONLY permitted outbound HTTP request in the entire binary is the explicitly configured Cloudflare Access JWKS endpoint (`https://<team>/cdn-cgi/access/certs`) when `--auth cloudflare-access` is active.
2. **4 Active Providers Only**: `Claude`, `Codex`, `Antigravity` (standalone `agy`), `OpenCode` (free cloud + local LLMs), plus `Mock` for tests.
3. **Storage Efficiency**: SQLite WAL + zstd compression for `TurnItem` payloads > 4 KiB + blake3 content-addressed blob deduplication + SQLite FTS5 `tokenize='trigram'` for instant Japanese full-text search.
4. **Native Multi-Provider Orchestration**: Built-in MCP server (`t3-mcp`) injected into spawned turns, supporting `delegate_task` (`shared` / `worktree` isolation), reactive parent wakeups, context handoff/forking, rate-limit auto-resume, and built-in `orch_stats`.
5. **Single Binary Deployment**: Axum HTTP/WebSocket API + embedded Japanese-first Web/Mobile PWA (`rust-embed` from `web/`).

## Crate Boundaries
- `crates/core` (`t3-core`): Domain types, config validation, privacy invariants, `ProviderDriver` and `Store` traits, WS/Stream events.
- `crates/store` (`t3-store`): `SqliteStore` implementing `t3_core::Store`, zstd+blake3 blob dedup table, FTS5 trigram search, `orch_stats`, `db_stats`, optional read-only T3 project importer (reads ONLY `projection_projects`).
- `crates/git` (`t3-git`): `GitManager` for `worktree` add/remove/list, branch info, and whitespace-ignoring stacked diffs (`DiffSummary`).
- `crates/providers` (`t3-providers`): `ProviderRegistry`, `ClaudeDriver`, `CodexDriver`, `AntigravityDriver`, `OpenCodeDriver`, `MockDriver`, process-group teardown (`setpgid` / `killpg`), rate-limit auto-resume coordinator, steer/queue manager.
- `crates/mcp` (`t3-mcp`): JSON-RPC 2.0 MCP server (stdio + HTTP handler) exposing `orchestrator_capabilities`, `delegate_task`, `task_status`, `task_cancel`, `thread_list`, `thread_read`, `thread_send`, `thread_fork`, `orch_stats`.
- `crates/server` (`t3-server`): Axum HTTP/WS server, Cloudflare Access RS256 JWKS JWT auth (header `Cf-Access-Jwt-Assertion` + cookie `CF_Authorization`), loopback-only Tailscale header auth (`Tailscale-User-Login`), Bearer token, LocalOnly, attachment upload/download, embedded `web/` assets.
- `crates/cli` (`t3d`): Binary entrypoint (`serve`, `mcp`, `status`, `stats`, `import-projects`, `audit-privacy`, `cf-tunnel`, `launchd-plist`).
- `web/`: Self-contained Japanese-first PWA (`index.html`, `styles.css`, `app.js`, `manifest.json`), zero npm runtime dependency required to serve.

## REST & WebSocket API Contract (`/api/...` and `/ws`)
All `/api/*` and `/ws` endpoints pass through the configured `AuthLayer` (except `GET /healthz`).
- `GET /healthz` -> `{ ok, version, telemetry_enabled: false }`
- `GET /api/meta` -> `{ version, auth_identity, telemetry_enabled: false, privacy: PrivacyAuditReport, follow_up_behavior, default_model }`
- `GET /api/providers` -> `Vec<ProviderHealth>`
- `GET /api/projects` / `POST /api/projects` (`{ title, workspace_root }`) / `DELETE /api/projects/:id`
- `GET /api/threads?project_id=&include_archived=&limit=`
- `POST /api/threads` (`{ project_id, title, model_selection, runtime_mode, interaction_mode, isolation, base_ref }`)
- `GET /api/threads/:id` -> `{ thread, runs, items, children, handoffs, attachments }`
- `PATCH /api/threads/:id` (`{ title?, model_selection?, archived?, pinned? }`)
- `POST /api/threads/:id/messages` (`{ prompt, behavior?: "steer"|"queue", attachment_ids?: [Uuid], model_selection?: ModelSelection }`)
- `POST /api/threads/:id/interrupt`
- `POST /api/threads/:id/fork` (`{ title?, target_model?: ModelSelection, handoff_summary?: String }`)
- `POST /api/threads/:id/compact` -> `204` (Codex only: compacts the thread's own CLI session in place through `codex app-server` `thread/compact/start`; the thread id and session id do not change)
- `POST /api/threads/:id/respond` (`PendingResponse`)
- `GET /api/threads/:id/diff?ignore_whitespace=true` -> `DiffSummary`
- `POST /api/threads/:id/delegate` (`DelegationSpec`) -> `{ thread, run_id }`
- `POST /api/attachments` (multipart file upload, optional `thread_id`) -> `Attachment`
- `GET /api/attachments/:id` -> raw file bytes with `Content-Type`
- `GET /api/search?q=<query>&limit=30` -> `Vec<SearchHit>` (SQLite FTS5 trigram)
- `GET /api/stats/orch` -> `Vec<OrchModelStat>`
- `GET /api/stats/db` -> `DbStats`
- `POST /mcp` -> Streamable HTTP JSON-RPC 2.0 MCP endpoint (requires valid auth or internal ephemeral run token)
- `GET /ws` -> WebSocket streaming `WsServerMessage` and accepting `WsClientMessage`
