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

async fn thread_detail(
    State(state): State<ServerState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let store = &state.store;
    let thread = store.get_thread(id).await?;
    let (runs, items, children, handoffs, mut attachments) = tokio::try_join!(
        store.list_runs(id),
        store.list_turn_items(id, usize::MAX, 0),
        store.list_child_threads(id),
        store.list_handoffs(id),
        store.list_attachments(id),
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
    Ok(Json(json!({
        "thread": thread, "runs": runs, "items": items, "children": children,
        "handoffs": handoffs, "attachments": attachments,
    })))
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
        let id = identity
            .subject
            .parse()
            .map_err(|_| CoreError::Auth("invalid MCP thread".into()))?;
        let caller = state.store.get_thread(id).await?;
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
