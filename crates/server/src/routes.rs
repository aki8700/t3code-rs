use crate::{
    ServerState, VERSION, assets, attachments, audit_privacy,
    auth::authenticate,
    coordinator::Message,
    storage_io,
    threads::{CreateThread, PatchThread, validate_title},
    ws,
};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_core::*;
use t3_mcp::{ForkOptions, OrchestratorBackend};
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

pub fn router(state: ServerState) -> Router {
    let api = Router::new()
        .route("/api/meta", get(meta))
        .route("/api/providers", get(providers))
        .route("/api/projects", get(projects).post(create_project))
        .route("/api/projects/{id}", axum::routing::delete(delete_project))
        .route("/api/threads", get(threads).post(create_thread))
        .route("/api/threads/{id}", get(thread_detail).patch(patch_thread))
        .route("/api/threads/{id}/messages", post(send_message))
        .route("/api/threads/{id}/interrupt", post(interrupt))
        .route("/api/threads/{id}/fork", post(fork_thread))
        .route("/api/threads/{id}/respond", post(respond))
        .route("/api/threads/{id}/diff", get(diff))
        .route("/api/threads/{id}/delegate", post(delegate))
        .route("/api/threads/{id}/swarm", get(swarm_board))
        .route("/api/threads/{id}/swarm/messages", post(swarm_message))
        .route(
            "/api/threads/{id}/swarm/workers/{name}/kill",
            post(swarm_kill),
        )
        .route(
            "/api/attachments",
            post(attachments::upload)
                .layer(DefaultBodyLimit::max(attachments::MAX_BYTES + 64 * 1024)),
        )
        .route("/api/attachments/{id}", get(attachments::download))
        .route("/api/search", get(search))
        .route("/api/stats/orch", get(orch_stats))
        .route("/api/stats/db", get(db_stats))
        .route("/api/usage", get(crate::usage::get))
        .route("/api/usage/limits", get(crate::limits::get))
        .route(
            "/api/usage/limits/reset-credit",
            post(crate::limits::consume_reset_credit),
        )
        .route("/ws", get(ws::upgrade))
        .route_layer(middleware::from_fn_with_state(
            state.auth.clone(),
            authenticate,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state.clone());
    Router::new()
        .merge(api)
        .merge(mcp_router(state))
        .route("/healthz", get(health))
        .route("/", get(assets::index))
        .route("/js/{name}", get(assets::js_module))
        .route("/{asset}", get(assets::named))
        .layer(middleware::from_fn(assets::security_headers))
}

pub(crate) fn mcp_router(state: ServerState) -> Router {
    Router::new()
        .route("/mcp", post(mcp))
        .route_layer(middleware::from_fn_with_state(
            state.auth.clone(),
            authenticate,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state)
}

pub(crate) struct ApiError(CoreError);
pub(crate) type ApiResult<T> = Result<T, ApiError>;

impl From<CoreError> for ApiError {
    fn from(error: CoreError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            CoreError::NotFound(_) => StatusCode::NOT_FOUND,
            CoreError::InvalidArgument(_) | CoreError::Git(_) => StatusCode::BAD_REQUEST,
            CoreError::Auth(_) => StatusCode::UNAUTHORIZED,
            CoreError::Provider(_) => StatusCode::CONFLICT,
            CoreError::Storage(_) | CoreError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = if status.is_server_error() {
            tracing::error!(error = %self.0, "API request failed");
            "Internal server error".to_owned()
        } else {
            self.0.to_string()
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

async fn health() -> Json<Value> {
    Json(json!({"ok": true, "version": VERSION, "telemetry_enabled": TELEMETRY_ENABLED}))
}

async fn meta(
    State(state): State<ServerState>,
    Extension(identity): Extension<AuthIdentity>,
) -> Json<Value> {
    Json(json!({
        "version": VERSION, "auth_identity": identity, "telemetry_enabled": TELEMETRY_ENABLED,
        "privacy": audit_privacy(&state.config), "follow_up_behavior": state.config.follow_up_behavior,
        "default_model": state.config.default_model,
    }))
}

async fn providers(State(state): State<ServerState>) -> Json<Vec<ProviderHealth>> {
    Json(state.coordinator.0.providers.health_all().await)
}

async fn projects(State(state): State<ServerState>) -> ApiResult<Json<Vec<Project>>> {
    Ok(Json(state.store.list_projects().await?))
}

#[derive(Deserialize)]
struct CreateProject {
    title: String,
    workspace_root: PathBuf,
}

async fn create_project(
    State(state): State<ServerState>,
    Json(input): Json<CreateProject>,
) -> ApiResult<Json<Project>> {
    validate_title(&input.title)?;
    let workspace = tokio::fs::canonicalize(&input.workspace_root)
        .await
        .map_err(|_| {
            CoreError::InvalidArgument("workspace_root must be an existing directory".into())
        })?;
    if !tokio::fs::metadata(&workspace)
        .await
        .map_err(storage_io)?
        .is_dir()
    {
        return Err(CoreError::InvalidArgument("workspace_root must be a directory".into()).into());
    }
    Ok(Json(
        state.store.create_project(input.title, workspace).await?,
    ))
}

async fn delete_project(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.coordinator.delete_project(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ThreadList {
    project_id: Option<Uuid>,
    #[serde(default)]
    include_archived: bool,
    limit: Option<usize>,
}

async fn threads(
    State(state): State<ServerState>,
    Query(query): Query<ThreadList>,
) -> ApiResult<Json<Vec<Thread>>> {
    Ok(Json(
        state
            .coordinator
            .thread_list(
                query.project_id,
                query.include_archived,
                query.limit.unwrap_or(500),
            )
            .await?,
    ))
}

async fn create_thread(
    State(state): State<ServerState>,
    Json(input): Json<CreateThread>,
) -> ApiResult<Json<Thread>> {
    Ok(Json(state.coordinator.create_thread(input).await?))
}

#[derive(Deserialize)]
struct ThreadPage {
    limit: Option<usize>,
    before_ordinal: Option<i64>,
}

async fn thread_detail(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    Query(page): Query<ThreadPage>,
) -> ApiResult<Json<Value>> {
    let store = &state.store;
    let thread = store.get_thread(id).await?;
    let (runs, (items, has_more, total_items), children, handoffs, mut attachments, breakdown) =
        tokio::try_join!(
            store.list_runs(id),
            store.list_turn_items_page(
                id,
                page.limit.unwrap_or(200).clamp(1, 1000),
                page.before_ordinal
            ),
            store.list_child_threads(id),
            store.list_handoffs(id),
            store.list_attachments(id),
            state.coordinator.items_breakdown(id),
        )?;
    let mut known: std::collections::HashSet<_> =
        attachments.iter().map(|attachment| attachment.id).collect();
    // Uploads made before a thread exists are referenced by its message items.
    for item in items
        .iter()
        .filter(|item| item.kind == TurnItemKind::UserMessage)
    {
        if let Some(ids) = item.payload["attachment_ids"].as_array() {
            for id in ids
                .iter()
                .filter_map(|id| id.as_str()?.parse::<Uuid>().ok())
            {
                if known.insert(id)
                    && let Ok(attachment) = store.get_attachment(id).await
                {
                    attachments.push(attachment);
                }
            }
        }
    }
    let context = build_thread_context(&state, &thread, &runs, &breakdown).await;
    Ok(Json(json!({
        "thread": thread, "runs": runs, "items": items, "children": children,
        "has_more": has_more, "total_items": total_items,
        "handoffs": handoffs, "attachments": attachments,
        "context": context,
    })))
}

async fn build_thread_context(
    state: &ServerState,
    thread: &Thread,
    runs: &[Run],
    breakdown: &[crate::coordinator::ItemBreakdown],
) -> Value {
    let context_window =
        t3_providers::context_window_for(thread.model_selection.provider, &thread.model_selection.model);
    let total_input_tokens: u64 = runs.iter().filter_map(|r| r.input_tokens).sum();
    let total_output_tokens: u64 = runs.iter().filter_map(|r| r.output_tokens).sum();
    let total_duration_ms: u64 = runs.iter().filter_map(|r| r.duration_ms).sum();
    let total_raw_bytes: u64 = breakdown.iter().map(|b| b.raw_bytes).sum();

    let matches_thread = |snap: &crate::coordinator::LiveContextUsage| {
        snap.provider == thread.model_selection.provider
            && snap.model == thread.model_selection.model
            && (thread.provider_session_id.is_none()
                || snap.session_id.is_none()
                || snap.session_id == thread.provider_session_id)
    };
    let live = state
        .coordinator
        .live_context(thread.id)
        .await
        .filter(matches_thread);
    let stored = if live.is_none() {
        state
            .coordinator
            .stored_context(thread.id)
            .await
            .filter(matches_thread)
    } else {
        None
    };
    let session = if live.is_none()
        && stored.is_none()
        && matches!(
            thread.model_selection.provider,
            ProviderId::Claude | ProviderId::Codex
        )
        && let Some(session_id) = thread.provider_session_id.clone()
    {
        let home = state.usage_home.clone();
        let usage_cache = state.usage_cache.clone();
        let provider = thread.model_selection.provider;
        tokio::task::spawn_blocking(move || {
            let mut cache = usage_cache.lock().ok()?;
            cache.session_context_usage(home.as_deref(), provider, &session_id)
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };

    let (source, used_tokens, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens) =
        if let Some(snap) = live {
            (
                "live",
                snap.context_tokens,
                snap.input_tokens,
                snap.output_tokens,
                snap.cache_read_tokens,
                snap.cache_write_tokens,
            )
        } else if let Some(snap) = stored {
            (
                "stored",
                snap.context_tokens,
                snap.input_tokens,
                snap.output_tokens,
                snap.cache_read_tokens,
                snap.cache_write_tokens,
            )
        } else if let Some(sess) = session {
            (
                "session",
                sess.context_tokens,
                sess.input_tokens,
                sess.output_tokens,
                sess.cache_read_tokens,
                sess.cache_write_tokens,
            )
        } else if let Some(last_run) = runs
            .iter()
            .rev()
            .find(|r| r.input_tokens.is_some() || r.output_tokens.is_some())
        {
            let inp = last_run.input_tokens.unwrap_or(0);
            let out = last_run.output_tokens.unwrap_or(0);
            ("run", inp.saturating_add(out), inp, out, 0, 0)
        } else if total_raw_bytes > 0 {
            // Rough heuristic for mixed Japanese/code payloads (~3.5 bytes/token).
            let est = ((total_raw_bytes as f64) / 3.5).round() as u64;
            ("estimated", est, est, 0, 0, 0)
        } else {
            ("empty", 0, 0, 0, 0, 0)
        };

    json!({
        "context_window": context_window,
        "used_tokens": used_tokens,
        "source": source,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cache_read_tokens": cache_read_tokens,
        "cache_write_tokens": cache_write_tokens,
        "total_input_tokens": total_input_tokens,
        "total_output_tokens": total_output_tokens,
        "total_duration_ms": total_duration_ms,
        "total_raw_bytes": total_raw_bytes,
        "items_breakdown": breakdown,
    })
}

async fn swarm_board(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Option<SwarmBoardSnapshot>>> {
    Ok(Json(state.coordinator.get_board(id).await?))
}

#[derive(Deserialize)]
struct SwarmPost {
    text: String,
}

async fn swarm_message(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    Json(input): Json<SwarmPost>,
) -> ApiResult<Json<SwarmBoardSnapshot>> {
    Ok(Json(
        state
            .coordinator
            .post_message(id, Some("user"), input.text)
            .await?,
    ))
}

async fn swarm_kill(
    State(state): State<ServerState>,
    Path((id, name)): Path<(Uuid, String)>,
) -> ApiResult<Json<SwarmBoardSnapshot>> {
    Ok(Json(state.coordinator.kill_worker(id, &name, false).await?))
}

async fn patch_thread(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    Json(patch): Json<PatchThread>,
) -> ApiResult<Json<Thread>> {
    Ok(Json(state.coordinator.patch_thread(id, patch).await?))
}

async fn send_message(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    Json(message): Json<Message>,
) -> ApiResult<Json<Value>> {
    let run_id = state.coordinator.send(id, message).await?;
    Ok(Json(json!({"run_id": run_id})))
}

async fn interrupt(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.coordinator.interrupt(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn fork_thread(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    Json(options): Json<ForkOptions>,
) -> ApiResult<Json<Thread>> {
    Ok(Json(state.coordinator.fork_thread(id, options).await?))
}

async fn respond(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    Json(response): Json<PendingResponse>,
) -> ApiResult<StatusCode> {
    state.coordinator.respond_request(id, response).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct DiffQuery {
    ignore_whitespace: Option<bool>,
}

async fn diff(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    Query(query): Query<DiffQuery>,
) -> ApiResult<Json<DiffSummary>> {
    Ok(Json(
        state
            .coordinator
            .diff(id, query.ignore_whitespace.unwrap_or(true))
            .await?,
    ))
}

async fn delegate(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
    Json(spec): Json<DelegationSpec>,
) -> ApiResult<Json<Value>> {
    if spec.parent_thread_id != id {
        return Err(
            CoreError::InvalidArgument("parent_thread_id must match the route".into()).into(),
        );
    }
    let (thread, ticket) = state.coordinator.launch_child(spec).await?;
    Ok(Json(json!({"thread": thread, "run_id": ticket.id})))
}

#[derive(Deserialize)]
struct Search {
    q: String,
    limit: Option<usize>,
}

async fn search(
    State(state): State<ServerState>,
    Query(query): Query<Search>,
) -> ApiResult<Json<Vec<SearchHit>>> {
    Ok(Json(
        state
            .store
            .search_fts(&query.q, query.limit.unwrap_or(30).min(200))
            .await?,
    ))
}

async fn orch_stats(State(state): State<ServerState>) -> ApiResult<Json<Vec<OrchModelStat>>> {
    Ok(Json(state.coordinator.orch_stats().await?))
}

async fn db_stats(State(state): State<ServerState>) -> ApiResult<Json<DbStats>> {
    Ok(Json(state.store.db_stats().await?))
}

async fn mcp(
    State(state): State<ServerState>,
    Extension(identity): Extension<AuthIdentity>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let response = if identity.method == "internal-mcp" {
        let run = state
            .auth
            .active_run(&headers)
            .ok_or_else(|| CoreError::Auth("inactive MCP token".into()))?;
        let mut caller = state.store.get_thread(run.thread_id).await?;
        caller.runtime_mode = caller.runtime_mode.min(run.runtime_mode);
        if run.interaction_mode == InteractionMode::Plan {
            caller.interaction_mode = InteractionMode::Plan;
        }
        state.mcp.handle_scoped_bytes(&body, &caller).await
    } else {
        let thread_id = headers
            .get("x-t3-thread-id")
            .map(|header| {
                header
                    .to_str()
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| CoreError::InvalidArgument("invalid X-T3-Thread-Id".into()))
            })
            .transpose()?;
        state.mcp.handle_bytes(&body, thread_id).await
    };
    Ok(match response {
        Some(response) => Json(response).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    })
}
