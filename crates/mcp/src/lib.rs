//! Transport-independent MCP orchestration tools.

mod code_tools;

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;
use t3_core::*;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub providers: Vec<ProviderHealth>,
    pub model_tiers: Vec<ModelSpec>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelegateMode {
    #[default]
    Async,
    Wait,
}

#[derive(Debug, Serialize)]
pub struct DelegatedTask {
    pub task_id: Uuid,
    pub child_thread_id: Uuid,
    pub run_id: Uuid,
    pub status: ThreadStatus,
    pub worktree_path: Option<std::path::PathBuf>,
    pub summary: Option<String>,
    pub worktree: Option<WorktreeChange>,
    pub overlaps: Vec<WorktreeOverlap>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeChange {
    pub thread_id: Uuid,
    pub worker: String,
    pub changed_file_count: usize,
    /// The first changed files only; see `changed_file_count` for the total.
    pub changed_files: Vec<String>,
    pub behind: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeOverlap {
    pub file: String,
    pub worker_ids: Vec<Uuid>,
    pub workers: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ThreadRead {
    pub thread: Thread,
    pub items: Vec<TurnItem>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ForkOptions {
    pub title: Option<String>,
    pub target_model: Option<ModelSelection>,
    pub handoff_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMergeResult {
    pub merged: bool,
    pub child_thread_id: Uuid,
    pub branch: String,
    pub summary: String,
    pub conflict_files: Vec<String>,
    pub cleaned_up: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwarmMemberResult {
    pub board_id: Uuid,
    pub name: String,
    pub thread_id: Uuid,
    pub provider: ProviderId,
    pub model: String,
    pub effort: Option<String>,
    pub state: String,
    pub role: String,
    pub last_read_seq: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<SwarmMember> for SwarmMemberResult {
    fn from(member: SwarmMember) -> Self {
        Self {
            board_id: member.board_id,
            name: member.name,
            thread_id: member.thread_id,
            provider: member.provider,
            model: member.model,
            effort: member.effort,
            state: member.state,
            role: member.role,
            last_read_seq: member.last_read_seq,
            created_at: member.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SwarmWorkerResult {
    #[serde(flatten)]
    pub member: SwarmMemberResult,
    pub brief_chars: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwarmMemberSummary {
    pub name: String,
    pub state: String,
    pub provider: ProviderId,
    pub model: String,
    pub last_read_seq: i64,
}

impl From<SwarmMember> for SwarmMemberSummary {
    fn from(member: SwarmMember) -> Self {
        Self {
            name: member.name,
            state: member.state,
            provider: member.provider,
            model: member.model,
            last_read_seq: member.last_read_seq,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SwarmStatusResult {
    pub board_id: Uuid,
    /// Members that are not killed. Killed members appear only in `killed_count`.
    pub members: Vec<SwarmMemberSummary>,
    pub killed_count: usize,
    pub head_seq: i64,
    pub message_count: usize,
    pub worktrees: Vec<WorktreeChange>,
    pub overlaps: Vec<WorktreeOverlap>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwarmPostResult {
    pub seq: i64,
    pub board_id: Uuid,
    pub woken: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SwarmReadMessage {
    pub seq: i64,
    pub board_id: Uuid,
    pub sender: String,
    pub text: String,
    pub mentions: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
struct PauseAllInput {
    paused: bool,
}

#[async_trait]
pub trait OrchestratorBackend: Send + Sync {
    async fn get_thread(&self, id: Uuid) -> CoreResult<Thread>;
    /// Resolve the workspace for a scoped code tool. Backends with shared threads
    /// should also resolve the caller's project root when there is no worktree.
    async fn workspace_root(&self, thread: &Thread) -> CoreResult<PathBuf> {
        thread
            .worktree_path
            .clone()
            .ok_or_else(|| CoreError::NotFound("caller workspace".into()))
    }
    async fn capabilities(&self) -> CoreResult<Capabilities>;
    async fn delegate(&self, spec: DelegationSpec, mode: DelegateMode)
    -> CoreResult<DelegatedTask>;
    async fn task_status(&self, task_id: Uuid) -> CoreResult<DelegatedTask>;
    async fn task_cancel(&self, task_id: Uuid) -> CoreResult<DelegatedTask>;
    async fn task_diff(&self, _caller_id: Uuid, _child_thread_id: Uuid) -> CoreResult<DiffSummary> {
        Err(CoreError::InvalidArgument(
            "task_diff is unsupported by this backend".into(),
        ))
    }
    async fn task_merge(
        &self,
        _caller_id: Uuid,
        _child_thread_id: Uuid,
        _commit_message: Option<String>,
        _cleanup: bool,
    ) -> CoreResult<TaskMergeResult> {
        Err(CoreError::InvalidArgument(
            "task_merge is unsupported by this backend".into(),
        ))
    }
    async fn thread_list(
        &self,
        project_id: Option<Uuid>,
        include_archived: bool,
        limit: usize,
    ) -> CoreResult<Vec<Thread>>;
    async fn thread_read(&self, thread_id: Uuid, limit: usize) -> CoreResult<ThreadRead>;
    async fn thread_send(
        &self,
        thread_id: Uuid,
        prompt: String,
        behavior: Option<FollowUpBehavior>,
    ) -> CoreResult<Uuid>;
    async fn thread_fork(&self, thread_id: Uuid, options: ForkOptions) -> CoreResult<Thread>;
    async fn orch_stats(&self) -> CoreResult<Vec<OrchModelStat>>;
    async fn pause_all(&self, _paused: bool) -> CoreResult<bool> {
        Err(CoreError::InvalidArgument(
            "pause-all is unsupported by this backend".into(),
        ))
    }
    async fn swarm_status(&self, _thread_id: Uuid) -> CoreResult<Option<SwarmStatusResult>> {
        swarm_unsupported()
    }
    async fn swarm_add_worker(
        &self,
        _thread_id: Uuid,
        _spec: SwarmWorkerSpec,
    ) -> CoreResult<SwarmWorkerResult> {
        swarm_unsupported()
    }
    async fn swarm_post_message(
        &self,
        _thread_id: Uuid,
        _text: String,
    ) -> CoreResult<SwarmPostResult> {
        swarm_unsupported()
    }
    async fn swarm_read_board(
        &self,
        _thread_id: Uuid,
        _since_seq: Option<i64>,
        _limit: usize,
        _seq: Option<i64>,
    ) -> CoreResult<Vec<SwarmReadMessage>> {
        swarm_unsupported()
    }
    async fn swarm_kill_worker(
        &self,
        _thread_id: Uuid,
        _name: &str,
    ) -> CoreResult<SwarmMemberResult> {
        swarm_unsupported()
    }
}

fn swarm_unsupported<T>() -> CoreResult<T> {
    Err(CoreError::InvalidArgument(
        "swarm tools are unsupported by this backend".into(),
    ))
}

pub struct McpServer<B> {
    backend: B,
}

#[derive(Clone, Copy)]
struct CallContext<'a> {
    thread_id: Option<Uuid>,
    caller: Option<&'a Thread>,
}

impl<B: OrchestratorBackend> McpServer<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub async fn handle_bytes(&self, request: &[u8], thread_id: Option<Uuid>) -> Option<Value> {
        self.parse(
            request,
            CallContext {
                thread_id,
                caller: None,
            },
        )
        .await
    }

    pub async fn handle_scoped_bytes(&self, request: &[u8], caller: &Thread) -> Option<Value> {
        self.parse(
            request,
            CallContext {
                thread_id: Some(caller.id),
                caller: Some(caller),
            },
        )
        .await
    }

    async fn parse(&self, request: &[u8], context: CallContext<'_>) -> Option<Value> {
        match serde_json::from_slice(request) {
            Ok(value) => self.dispatch(value, context).await,
            Err(_) => Some(rpc_error(Value::Null, -32700, "Parse error")),
        }
    }

    pub async fn handle(&self, request: Value, thread_id: Option<Uuid>) -> Option<Value> {
        self.dispatch(
            request,
            CallContext {
                thread_id,
                caller: None,
            },
        )
        .await
    }

    async fn dispatch(&self, request: Value, context: CallContext<'_>) -> Option<Value> {
        if let Value::Array(requests) = request {
            if requests.is_empty() {
                return Some(rpc_error(Value::Null, -32600, "Invalid request"));
            }
            let mut responses = Vec::new();
            for request in requests {
                responses.extend(self.handle_one(request, context).await);
            }
            return (!responses.is_empty()).then_some(Value::Array(responses));
        }
        self.handle_one(request, context).await
    }

    async fn handle_one(&self, request: Value, context: CallContext<'_>) -> Option<Value> {
        let id = request.get("id");
        if request["jsonrpc"] != "2.0"
            || id.is_some_and(|id| !(id.is_null() || id.is_string() || id.is_number()))
        {
            return Some(rpc_error(Value::Null, -32600, "Invalid request"));
        }
        let Some(method) = request["method"].as_str() else {
            return Some(rpc_error(Value::Null, -32600, "Invalid request"));
        };
        let id = id?.clone();
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "t3code-rs", "version": env!("CARGO_PKG_VERSION")}
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => {
                let Ok(call) = serde_json::from_value::<ToolCall>(request["params"].clone()) else {
                    return Some(rpc_error(id, -32602, "Invalid tool call parameters"));
                };
                match self.call(&call.name, call.arguments, context).await {
                    Ok(data) => json!({"content": [{"type": "text", "text": data.to_string()}]}),
                    Err(error) => json!({
                        "isError": true, "content": [{"type": "text", "text": error.to_string()}]
                    }),
                }
            }
            _ => return Some(rpc_error(id, -32601, "Method not found")),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    async fn call(&self, name: &str, args: Value, context: CallContext<'_>) -> CoreResult<Value> {
        validate_argument_keys(name, &args)?;
        let backend = &self.backend;
        let CallContext { thread_id, caller } = context;
        match name {
            "orchestrator_capabilities" => encode(backend.capabilities().await?),
            "pause_all" => {
                let args: PauseAllInput = decode(args)?;
                encode(backend.pause_all(args.paused).await?)
            }
            "swarm_status" => {
                let args: SwarmStatus = decode(args)?;
                encode(
                    backend
                        .swarm_status(swarm_thread(args.thread_id, context)?)
                        .await?,
                )
            }
            "swarm_add_worker" => {
                let args: SwarmArgs<SwarmWorkerSpec> = decode(args)?;
                encode(
                    backend
                        .swarm_add_worker(swarm_thread(args.thread_id, context)?, args.input)
                        .await?,
                )
            }
            "swarm_post_message" => {
                let args: SwarmArgs<SwarmPost> = decode(args)?;
                encode(
                    backend
                        .swarm_post_message(swarm_thread(args.thread_id, context)?, args.input.text)
                        .await?,
                )
            }
            "swarm_read_board" => {
                let args: SwarmArgs<SwarmRead> = decode(args)?;
                encode(
                    backend
                        .swarm_read_board(
                            swarm_thread(args.thread_id, context)?,
                            args.input.since_seq,
                            args.input.limit,
                            args.input.seq,
                        )
                        .await?,
                )
            }
            "swarm_kill_worker" => {
                let args: SwarmArgs<SwarmKill> = decode(args)?;
                encode(
                    backend
                        .swarm_kill_worker(swarm_thread(args.thread_id, context)?, &args.input.name)
                        .await?,
                )
            }
            "delegate_task" => {
                let args: DelegateArgs = decode(args)?;
                let parent_thread_id = resolve_thread(args.parent_thread_id, thread_id)?;
                let parent = self.scoped_thread(parent_thread_id, caller).await?;
                if args.isolation == IsolationMode::Worktree {
                    let parent = match parent {
                        Some(parent) => parent,
                        None => backend.get_thread(parent_thread_id).await?,
                    };
                    if parent.interaction_mode == InteractionMode::Plan
                        || caller
                            .is_some_and(|caller| caller.interaction_mode == InteractionMode::Plan)
                    {
                        return Err(CoreError::InvalidArgument(
                            "a plan thread cannot delegate to a worktree".into(),
                        ));
                    }
                }
                let spec = DelegationSpec {
                    parent_thread_id,
                    title: args.title,
                    task: args.task,
                    model_selection: ModelSelection {
                        provider: args.provider,
                        model: args.model,
                        effort: args.effort,
                        agent: None,
                    },
                    isolation: args.isolation,
                    base_ref: args.base_ref,
                    runtime_mode: args.runtime_mode,
                    interaction_mode: args.interaction_mode,
                };
                if let Some(caller) = caller {
                    spec.validate_permissions(caller)?;
                }
                encode(backend.delegate(spec, args.mode).await?)
            }
            "task_status" | "task_cancel" => {
                let id = decode::<TaskArgs>(args)?.task_id;
                self.scoped_thread(id, caller).await?;
                if name == "task_status" {
                    encode(backend.task_status(id).await?)
                } else {
                    encode(backend.task_cancel(id).await?)
                }
            }
            "task_diff" => {
                let args: TaskDiffArgs = decode(args)?;
                let caller_id = resolve_caller_thread(args.parent_thread_id, context)?;
                self.scoped_thread(args.child_thread_id, caller).await?;
                encode(backend.task_diff(caller_id, args.child_thread_id).await?)
            }
            "task_merge" => {
                let args: TaskMergeArgs = decode(args)?;
                let caller_id = resolve_caller_thread(args.parent_thread_id, context)?;
                let is_plan = if let Some(caller) = caller {
                    caller.interaction_mode == InteractionMode::Plan
                } else {
                    backend.get_thread(caller_id).await?.interaction_mode == InteractionMode::Plan
                };
                if is_plan {
                    return Err(CoreError::InvalidArgument(
                        "a plan thread cannot merge a task worktree".into(),
                    ));
                }
                self.check_target_permissions(args.child_thread_id, caller)
                    .await?;
                encode(
                    backend
                        .task_merge(
                            caller_id,
                            args.child_thread_id,
                            args.commit_message,
                            args.cleanup,
                        )
                        .await?,
                )
            }
            "thread_list" => {
                let args: ListArgs = decode(args)?;
                let project_id = if let Some(caller) = caller {
                    if args.project_id.is_some_and(|id| id != caller.project_id) {
                        return Err(CoreError::NotFound("project".into()));
                    }
                    Some(caller.project_id)
                } else {
                    args.project_id
                };
                encode(
                    backend
                        .thread_list(project_id, args.include_archived, args.limit)
                        .await?,
                )
            }
            "thread_read" => {
                let args: ReadArgs = decode(args)?;
                let id = resolve_thread(args.thread_id, thread_id)?;
                self.scoped_thread(id, caller).await?;
                encode(backend.thread_read(id, args.limit).await?)
            }
            "thread_send" => {
                let args: SendArgs = decode(args)?;
                let id = resolve_thread(args.thread_id, thread_id)?;
                self.check_target_permissions(id, caller).await?;
                let run_id = backend.thread_send(id, args.prompt, args.behavior).await?;
                Ok(json!({"thread_id": id, "run_id": run_id}))
            }
            "thread_fork" => {
                let args: ForkArgs = decode(args)?;
                let id = resolve_thread(args.thread_id, thread_id)?;
                self.check_target_permissions(id, caller).await?;
                let mut options = args.options;
                if let Some(provider) = args.provider {
                    let model = args.model.ok_or_else(|| {
                        CoreError::InvalidArgument("model is required with provider".into())
                    })?;
                    options.target_model = Some(ModelSelection {
                        provider,
                        model,
                        effort: args.effort,
                        agent: None,
                    });
                } else if args.model.is_some() || args.effort.is_some() {
                    return Err(CoreError::InvalidArgument(
                        "provider is required with model or effort".into(),
                    ));
                }
                encode(backend.thread_fork(id, options).await?)
            }
            "orch_stats" => encode(backend.orch_stats().await?),
            "compaction_econ" => encode(t3_maki::compaction_econ(decode(args)?)),
            "code_index" | "code_slice" | "code_grep" | "code_diff" => {
                let thread = match (caller, thread_id) {
                    (Some(caller), _) => Some(caller.clone()),
                    (None, Some(id)) => Some(backend.get_thread(id).await?),
                    (None, None) => None,
                };
                let mut project = None;
                let workspace = if let Some(mut thread) = thread {
                    let workspace = backend.workspace_root(&thread).await?;
                    if name == "code_diff" && thread.worktree_path.take().is_some() {
                        // A linked checkout can use the registered project's Git metadata.
                        match backend.workspace_root(&thread).await {
                            Ok(root) => project = Some(root),
                            Err(CoreError::NotFound(_)) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    Some(workspace)
                } else {
                    None
                };
                code_tools::call(name.to_owned(), args, workspace, project).await
            }
            _ => Err(CoreError::InvalidArgument(format!("unknown tool: {name}"))),
        }
    }

    async fn scoped_thread(&self, id: Uuid, caller: Option<&Thread>) -> CoreResult<Option<Thread>> {
        let Some(caller) = caller else {
            return Ok(None);
        };
        let thread = self.backend.get_thread(id).await?;
        if thread.project_id != caller.project_id {
            return Err(CoreError::NotFound("thread".into()));
        }
        Ok(Some(thread))
    }

    async fn check_target_permissions(&self, id: Uuid, caller: Option<&Thread>) -> CoreResult<()> {
        if let (Some(caller), Some(target)) = (caller, self.scoped_thread(id, caller).await?)
            && (target.runtime_mode > caller.runtime_mode
                || (caller.interaction_mode == InteractionMode::Plan
                    && target.interaction_mode == InteractionMode::Default))
        {
            return Err(CoreError::InvalidArgument(
                "target thread exceeds the caller thread's permissions".into(),
            ));
        }
        Ok(())
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn resolve_thread(arg: Option<Uuid>, context: Option<Uuid>) -> CoreResult<Uuid> {
    arg.or(context)
        .ok_or_else(|| CoreError::InvalidArgument("thread ID is required".into()))
}

fn swarm_thread(arg: Option<Uuid>, context: CallContext<'_>) -> CoreResult<Uuid> {
    let id = resolve_thread(arg, context.thread_id)?;
    if context.caller.is_some_and(|caller| caller.id != id) {
        return Err(CoreError::InvalidArgument(
            "swarm tools must use the caller's thread".into(),
        ));
    }
    Ok(id)
}

fn resolve_caller_thread(arg: Option<Uuid>, context: CallContext<'_>) -> CoreResult<Uuid> {
    let id = resolve_thread(arg, context.thread_id)?;
    if context.caller.is_some_and(|caller| caller.id != id) {
        return Err(CoreError::InvalidArgument("caller thread mismatch".into()));
    }
    Ok(id)
}

fn decode<T: DeserializeOwned>(args: Value) -> CoreResult<T> {
    serde_json::from_value(args).map_err(|error| CoreError::InvalidArgument(error.to_string()))
}

fn encode<T: Serialize>(data: T) -> CoreResult<Value> {
    serde_json::to_value(data).map_err(|error| CoreError::Internal(error.to_string()))
}

#[derive(Deserialize)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
}

fn empty_object() -> Value {
    json!({})
}

fn validate_argument_keys(name: &str, args: &Value) -> CoreResult<()> {
    let Some(schema) = tools()
        .iter()
        .find(|tool| tool["name"] == name)
        .map(|tool| &tool["inputSchema"])
    else {
        return Ok(());
    };
    let aliases: &[&str] = match name {
        "task_diff" | "task_merge" => &["task_id"],
        _ => &[],
    };
    validate_schema_argument_keys(schema, args, name, aliases)
}

fn validate_schema_argument_keys(
    schema: &Value,
    args: &Value,
    path: &str,
    aliases: &[&str],
) -> CoreResult<()> {
    let Some(properties) = schema["properties"].as_object() else {
        return Ok(());
    };
    let Some(arguments) = args.as_object() else {
        return Ok(());
    };

    let mut valid_keys = properties.keys().cloned().collect::<Vec<_>>();
    valid_keys.extend(aliases.iter().map(|alias| (*alias).to_owned()));
    valid_keys.sort();
    valid_keys.dedup();

    let unknown = arguments
        .keys()
        .filter(|key| valid_keys.binary_search(key).is_err())
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        let valid = if valid_keys.is_empty() {
            "(none)".to_owned()
        } else {
            valid_keys.join(", ")
        };
        return Err(CoreError::InvalidArgument(format!(
            "unknown argument key(s) for {path}: {}; valid keys: {valid}",
            unknown.join(", ")
        )));
    }

    for (key, value) in arguments {
        if let Some(property_schema) = properties.get(key) {
            validate_schema_argument_keys(property_schema, value, &format!("{path}.{key}"), &[])?;
        }
    }
    Ok(())
}

fn default_limit() -> usize {
    50
}
fn default_swarm_limit() -> usize {
    20
}

#[derive(Deserialize)]
struct SwarmStatus {
    thread_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct SwarmArgs<T> {
    thread_id: Option<Uuid>,
    #[serde(flatten)]
    input: T,
}

#[derive(Deserialize)]
struct SwarmPost {
    text: String,
}

#[derive(Deserialize)]
struct SwarmRead {
    #[serde(default)]
    since_seq: Option<i64>,
    #[serde(default = "default_swarm_limit")]
    limit: usize,
    #[serde(default)]
    seq: Option<i64>,
}

#[derive(Deserialize)]
struct SwarmKill {
    name: String,
}

#[derive(Deserialize)]
struct DelegateArgs {
    parent_thread_id: Option<Uuid>,
    title: String,
    task: String,
    #[serde(deserialize_with = "provider_id")]
    provider: ProviderId,
    model: String,
    effort: Option<String>,
    #[serde(default)]
    isolation: IsolationMode,
    base_ref: Option<String>,
    #[serde(default)]
    mode: DelegateMode,
    #[serde(default)]
    runtime_mode: RuntimeMode,
    #[serde(default)]
    interaction_mode: InteractionMode,
}

#[derive(Deserialize)]
struct TaskArgs {
    task_id: Uuid,
}

#[derive(Deserialize)]
struct TaskDiffArgs {
    #[serde(alias = "task_id")]
    child_thread_id: Uuid,
    parent_thread_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct TaskMergeArgs {
    #[serde(alias = "task_id")]
    child_thread_id: Uuid,
    parent_thread_id: Option<Uuid>,
    commit_message: Option<String>,
    #[serde(default)]
    cleanup: bool,
}

#[derive(Deserialize)]
struct ListArgs {
    project_id: Option<Uuid>,
    #[serde(default)]
    include_archived: bool,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct ReadArgs {
    thread_id: Option<Uuid>,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct SendArgs {
    thread_id: Option<Uuid>,
    prompt: String,
    behavior: Option<FollowUpBehavior>,
}

#[derive(Deserialize)]
struct ForkArgs {
    thread_id: Option<Uuid>,
    #[serde(flatten)]
    options: ForkOptions,
    #[serde(default, deserialize_with = "optional_provider_id")]
    provider: Option<ProviderId>,
    model: Option<String>,
    effort: Option<String>,
}

fn optional_provider_id<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<ProviderId>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|id| parse_provider(&id))
        .transpose()
}

fn provider_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ProviderId, D::Error> {
    parse_provider(&String::deserialize(d)?)
}

fn parse_provider<E: serde::de::Error>(id: &str) -> Result<ProviderId, E> {
    if id == "opencode" {
        Ok(ProviderId::OpenCode)
    } else {
        serde_json::from_value(json!(id)).map_err(E::custom)
    }
}

fn tools() -> &'static [Value] {
    static TOOLS: OnceLock<Vec<Value>> = OnceLock::new();
    TOOLS.get_or_init(build_tools).as_slice()
}

fn build_tools() -> Vec<Value> {
    let string = json!({"type": "string"});
    let uuid = json!({"type": "string", "format": "uuid"});
    let limit = json!({"type": "integer", "minimum": 0});
    let swarm_limit = json!({"type": "integer", "minimum": 0, "default": 20});
    let provider =
        json!({"enum": ["claude", "codex", "antigravity", "open-code", "opencode", "mock"]});
    let make = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({"name": name, "description": description, "inputSchema": {
            "type": "object", "properties": properties, "required": required,
            "additionalProperties": false
        }})
    };
    vec![
        make(
            "orchestrator_capabilities",
            "List provider health and curated models by tier.",
            json!({}),
            &[],
        ),
        make(
            "pause_all",
            "Pause or resume unattended run dispatch for this server. Running turns finish and direct user messages can still start.",
            json!({"paused": {"type": "boolean"}}),
            &["paused"],
        ),
        make(
            "swarm_status",
            "Get compact swarm status with live member records (killed members only as killed_count), worktree changed files, commits behind each base branch, and files shared by multiple workers.",
            json!({"thread_id": uuid}),
            &[],
        ),
        make(
            "swarm_add_worker",
            "Start a named worker and return its member record with brief_chars (not the brief text). The brief states the objective, expected output, boundaries, effort budget and stop condition.",
            json!({"thread_id": uuid, "name": string, "brief": string, "provider": provider,
                "model": string, "effort": string, "isolation": {"enum": ["shared", "worktree"]}}),
            &["name", "brief"],
        ),
        make(
            "swarm_post_message",
            "Post to this swarm and return seq, board_id, and names of members woken. @name, @all, and @coordinator wake addressed members, up to a wake depth limit. At most 4096 bytes: report once and pass longer details as a file path.",
            json!({"thread_id": uuid, "text": string}),
            &["text"],
        ),
        make(
            "swarm_read_board",
            "Read messages after since_seq (defaults to the caller's last_read_seq), up to 20 by default. Text is limited to 2,000 characters; provide seq to fetch one message with full text.",
            json!({"thread_id": uuid, "since_seq": {"type": "integer", "minimum": 0}, "limit": swarm_limit,
                "seq": {"type": "integer", "minimum": 1}}),
            &[],
        ),
        make(
            "swarm_kill_worker",
            "Stop a worker and cancel its queued follow-ups. Return the affected member record.",
            json!({"thread_id": uuid, "name": string}),
            &["name"],
        ),
        make(
            "delegate_task",
            "Start a child task, optionally in a separate Git worktree. The parent defaults to the current thread.",
            json!({
                "parent_thread_id": uuid, "title": string, "task": string, "provider": provider,
                "model": string, "effort": string, "isolation": {"enum": ["shared", "worktree"]},
                "base_ref": string, "mode": {"enum": ["async", "wait"]},
                "runtime_mode": {"enum": ["approval-required", "auto-accept-edits", "full-access"]},
                "interaction_mode": {"enum": ["default", "plan"]}
            }),
            &["title", "task", "provider", "model"],
        ),
        make(
            "task_status",
            "Read a delegated task's status, summary, worktree changed files, commits behind its base, and overlapping workers.",
            json!({"task_id": uuid}),
            &["task_id"],
        ),
        make(
            "task_cancel",
            "Interrupt a delegated task.",
            json!({"task_id": uuid}),
            &["task_id"],
        ),
        make(
            "task_diff",
            "Inspect the Git diff of a delegated child worktree branch relative to its parent.",
            json!({"child_thread_id": uuid, "parent_thread_id": uuid}),
            &["child_thread_id"],
        ),
        make(
            "task_merge",
            "Merge a delegated child worktree branch into its parent workspace, optionally cleaning up the worktree and t3rs/* branch.",
            json!({
                "child_thread_id": uuid,
                "parent_thread_id": uuid,
                "commit_message": string,
                "cleanup": {"type": "boolean"}
            }),
            &["child_thread_id"],
        ),
        make(
            "thread_list",
            "List recent threads.",
            json!({"project_id": uuid, "include_archived": {"type": "boolean"}, "limit": limit}),
            &[],
        ),
        make(
            "thread_read",
            "Read thread metadata and recent turn items.",
            json!({"thread_id": uuid, "limit": limit}),
            &[],
        ),
        make(
            "thread_send",
            "Send a follow-up prompt to a thread.",
            json!({"thread_id": uuid, "prompt": string, "behavior": {"enum": ["steer", "queue"]}}),
            &["prompt"],
        ),
        make(
            "thread_fork",
            "Fork a thread with a compact context handoff.",
            json!({
                "thread_id": uuid, "title": string, "provider": provider, "model": string,
                "effort": string, "handoff_summary": string, "target_model": {
                    "type": "object", "properties": {"provider": provider, "model": string, "effort": string, "agent": string},
                    "required": ["provider", "model"], "additionalProperties": false
                }
            }),
            &[],
        ),
        make(
            "orch_stats",
            "Read run and token statistics per provider and model.",
            json!({}),
            &[],
        ),
        make(
            "code_index",
            "List source declarations and their line ranges for a file or directory.",
            json!({"path": string, "max_depth": limit, "max_lines": limit}),
            &["path"],
        ),
        make(
            "code_slice",
            "Read a named symbol or a 1-based inclusive line range.",
            json!({
                "path": string, "symbol": string, "max_bytes": limit,
                "start": {"type": "integer", "minimum": 1}, "end": {"type": "integer", "minimum": 1}
            }),
            &["path"],
        ),
        make(
            "code_grep",
            "Search source lines with a regex and show their enclosing symbols. A glob without a slash matches file names at any depth.",
            json!({"path": string, "pattern": string, "glob": string, "max_hits": limit, "max_bytes": limit}),
            &["pattern", "path"],
        ),
        make(
            "code_diff",
            "Show compact tracked Git changes. The optional path is literal and relative to cwd. Pass a revision such as HEAD to compare against it.",
            json!({"cwd": string, "rev": string, "path": string, "max_bytes": limit}),
            &["cwd"],
        ),
        make(
            "compaction_econ",
            "Estimate prompt-cache compaction cost, per-turn savings, and break-even turns using prices in a common unit.",
            json!({
                "prefix": limit, "replaced": limit, "summary": limit, "tail": limit,
                "instruction": limit, "summarizer_reuses_prefix": {"type": "boolean"},
                "prices": {
                    "type": "object", "properties": {
                        "input": {"type": "number"}, "output": {"type": "number"},
                        "cache_read": {"type": "number"}, "cache_write": {"type": "number"}
                    },
                    "required": ["input", "output", "cache_read", "cache_write"],
                    "additionalProperties": false
                },
                "remaining_turns": {"type": "number", "minimum": 0}
            }),
            &[
                "prefix",
                "replaced",
                "summary",
                "tail",
                "instruction",
                "summarizer_reuses_prefix",
                "prices",
            ],
        ),
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use t3_maki::{CompactionEconInput, Prices};

    #[derive(Debug, PartialEq, Eq)]
    struct RecordedMerge {
        caller_id: Uuid,
        child_thread_id: Uuid,
        commit_message: Option<String>,
        cleanup: bool,
    }

    struct StubBackend {
        threads: Vec<Thread>,
        support_merge: bool,
        last_merge: Arc<Mutex<Option<RecordedMerge>>>,
    }

    impl StubBackend {
        fn new(threads: Vec<Thread>, support_merge: bool) -> Self {
            Self {
                threads,
                support_merge,
                last_merge: Arc::new(Mutex::new(None)),
            }
        }
    }

    #[async_trait]
    impl OrchestratorBackend for StubBackend {
        async fn get_thread(&self, id: Uuid) -> CoreResult<Thread> {
            self.threads
                .iter()
                .find(|t| t.id == id)
                .cloned()
                .ok_or_else(|| CoreError::NotFound("thread".into()))
        }

        async fn capabilities(&self) -> CoreResult<Capabilities> {
            Ok(Capabilities {
                providers: vec![],
                model_tiers: vec![],
            })
        }

        async fn delegate(
            &self,
            _spec: DelegationSpec,
            _mode: DelegateMode,
        ) -> CoreResult<DelegatedTask> {
            Err(CoreError::InvalidArgument("unused".into()))
        }

        async fn task_status(&self, _task_id: Uuid) -> CoreResult<DelegatedTask> {
            Err(CoreError::InvalidArgument("unused".into()))
        }

        async fn task_cancel(&self, _task_id: Uuid) -> CoreResult<DelegatedTask> {
            Err(CoreError::InvalidArgument("unused".into()))
        }

        async fn task_diff(
            &self,
            caller_id: Uuid,
            child_thread_id: Uuid,
        ) -> CoreResult<DiffSummary> {
            if !self.support_merge {
                return Err(CoreError::InvalidArgument(
                    "task_diff is unsupported by this backend".into(),
                ));
            }
            Ok(DiffSummary {
                workspace_path: PathBuf::from(format!("/tmp/{caller_id}")),
                branch: Some(format!("t3rs/{child_thread_id}")),
                clean: false,
                total_additions: 3,
                total_deletions: 1,
                files: vec![DiffFile {
                    path: "src/lib.rs".into(),
                    status: "M".into(),
                    additions: 3,
                    deletions: 1,
                    patch: "+fn hi() {}\n".into(),
                }],
                risk: None,
            })
        }

        async fn task_merge(
            &self,
            caller_id: Uuid,
            child_thread_id: Uuid,
            commit_message: Option<String>,
            cleanup: bool,
        ) -> CoreResult<TaskMergeResult> {
            if !self.support_merge {
                return Err(CoreError::InvalidArgument(
                    "task_merge is unsupported by this backend".into(),
                ));
            }
            *self.last_merge.lock().unwrap() = Some(RecordedMerge {
                caller_id,
                child_thread_id,
                commit_message,
                cleanup,
            });
            Ok(TaskMergeResult {
                merged: true,
                child_thread_id,
                branch: "t3rs/child".into(),
                summary: "merged".into(),
                conflict_files: vec![],
                cleaned_up: cleanup,
            })
        }

        async fn thread_list(
            &self,
            _project_id: Option<Uuid>,
            _include_archived: bool,
            _limit: usize,
        ) -> CoreResult<Vec<Thread>> {
            Ok(vec![])
        }

        async fn thread_read(&self, _thread_id: Uuid, _limit: usize) -> CoreResult<ThreadRead> {
            Err(CoreError::NotFound("thread".into()))
        }

        async fn thread_send(
            &self,
            _thread_id: Uuid,
            _prompt: String,
            _behavior: Option<FollowUpBehavior>,
        ) -> CoreResult<Uuid> {
            Ok(Uuid::nil())
        }

        async fn thread_fork(&self, _thread_id: Uuid, _options: ForkOptions) -> CoreResult<Thread> {
            Err(CoreError::NotFound("thread".into()))
        }

        async fn orch_stats(&self) -> CoreResult<Vec<OrchModelStat>> {
            Ok(vec![])
        }
    }

    struct DefaultUnsupportedBackend(Thread);

    #[async_trait]
    impl OrchestratorBackend for DefaultUnsupportedBackend {
        async fn get_thread(&self, id: Uuid) -> CoreResult<Thread> {
            (self.0.id == id)
                .then(|| self.0.clone())
                .ok_or_else(|| CoreError::NotFound("thread".into()))
        }
        async fn capabilities(&self) -> CoreResult<Capabilities> {
            Ok(Capabilities {
                providers: vec![],
                model_tiers: vec![],
            })
        }
        async fn delegate(
            &self,
            _spec: DelegationSpec,
            _mode: DelegateMode,
        ) -> CoreResult<DelegatedTask> {
            Err(CoreError::InvalidArgument("unused".into()))
        }
        async fn task_status(&self, _task_id: Uuid) -> CoreResult<DelegatedTask> {
            Err(CoreError::InvalidArgument("unused".into()))
        }
        async fn task_cancel(&self, _task_id: Uuid) -> CoreResult<DelegatedTask> {
            Err(CoreError::InvalidArgument("unused".into()))
        }
        async fn thread_list(
            &self,
            _project_id: Option<Uuid>,
            _include_archived: bool,
            _limit: usize,
        ) -> CoreResult<Vec<Thread>> {
            Ok(vec![])
        }
        async fn thread_read(&self, _thread_id: Uuid, _limit: usize) -> CoreResult<ThreadRead> {
            Err(CoreError::NotFound("thread".into()))
        }
        async fn thread_send(
            &self,
            _thread_id: Uuid,
            _prompt: String,
            _behavior: Option<FollowUpBehavior>,
        ) -> CoreResult<Uuid> {
            Ok(Uuid::nil())
        }
        async fn thread_fork(&self, _thread_id: Uuid, _options: ForkOptions) -> CoreResult<Thread> {
            Err(CoreError::NotFound("thread".into()))
        }
        async fn orch_stats(&self) -> CoreResult<Vec<OrchModelStat>> {
            Ok(vec![])
        }
    }

    fn make_thread(
        project_id: Uuid,
        runtime_mode: RuntimeMode,
        interaction_mode: InteractionMode,
    ) -> Thread {
        let now = "2026-11-25T00:00:00Z".parse().unwrap();
        Thread {
            id: Uuid::new_v4(),
            project_id,
            parent_thread_id: None,
            relationship: None,
            title: "test".into(),
            model_selection: ModelSelection::default(),
            runtime_mode,
            interaction_mode,
            status: ThreadStatus::Idle,
            worktree_path: None,
            branch: None,
            provider_session_id: None,
            archived: false,
            pinned: false,
            created_at: now,
            updated_at: now,
        }
    }

    fn rpc_call(name: &str, arguments: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        })
    }

    /// Field names of an argument struct. The pattern has no `..`, so adding a
    /// field without listing it here (and in the schema) fails to compile.
    macro_rules! fields {
        ($ty:ident { $($field:ident),+ }) => {{
            let _ = |$ty { $($field: _),+ }: $ty| {};
            vec![$(stringify!($field)),+]
        }};
    }
    pub(crate) use fields;

    fn assert_schema_keys(schema: &Value, mut fields: Vec<&str>, path: &str) {
        let mut keys = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        fields.sort_unstable();
        assert_eq!(keys, fields, "{path}");
    }

    #[test]
    fn tool_argument_fields_match_the_declared_schemas() {
        // `SwarmArgs` flattens `input` and `ForkArgs` flattens `options`.
        let _ = |SwarmArgs::<()> {
                     thread_id: _,
                     input: _,
                 }| {};
        let swarm = |input: Vec<&'static str>| [vec!["thread_id"], input].concat();
        let mut fork = fields!(ForkArgs {
            thread_id,
            options,
            provider,
            model,
            effort
        });
        fork.retain(|field| *field != "options");
        fork.extend(fields!(ForkOptions {
            title,
            target_model,
            handoff_summary
        }));
        let mut expected = vec![
            ("orchestrator_capabilities", vec![]),
            ("pause_all", fields!(PauseAllInput { paused })),
            ("swarm_status", fields!(SwarmStatus { thread_id })),
            (
                "swarm_add_worker",
                swarm(fields!(SwarmWorkerSpec {
                    name,
                    brief,
                    provider,
                    model,
                    effort,
                    isolation
                })),
            ),
            ("swarm_post_message", swarm(fields!(SwarmPost { text }))),
            (
                "swarm_read_board",
                swarm(fields!(SwarmRead {
                    since_seq,
                    limit,
                    seq
                })),
            ),
            ("swarm_kill_worker", swarm(fields!(SwarmKill { name }))),
            (
                "delegate_task",
                fields!(DelegateArgs {
                    parent_thread_id,
                    title,
                    task,
                    provider,
                    model,
                    effort,
                    isolation,
                    base_ref,
                    mode,
                    runtime_mode,
                    interaction_mode
                }),
            ),
            ("task_status", fields!(TaskArgs { task_id })),
            ("task_cancel", fields!(TaskArgs { task_id })),
            (
                "task_diff",
                fields!(TaskDiffArgs {
                    child_thread_id,
                    parent_thread_id
                }),
            ),
            (
                "task_merge",
                fields!(TaskMergeArgs {
                    child_thread_id,
                    parent_thread_id,
                    commit_message,
                    cleanup
                }),
            ),
            (
                "thread_list",
                fields!(ListArgs {
                    project_id,
                    include_archived,
                    limit
                }),
            ),
            ("thread_read", fields!(ReadArgs { thread_id, limit })),
            (
                "thread_send",
                fields!(SendArgs {
                    thread_id,
                    prompt,
                    behavior
                }),
            ),
            ("thread_fork", fork),
            ("orch_stats", vec![]),
            (
                "compaction_econ",
                fields!(CompactionEconInput {
                    prefix,
                    replaced,
                    summary,
                    tail,
                    instruction,
                    summarizer_reuses_prefix,
                    prices,
                    remaining_turns
                }),
            ),
        ];
        expected.extend(code_tools::argument_fields());

        let listed = tools();
        assert_eq!(listed.len(), expected.len());
        for (name, fields) in expected {
            assert_schema_keys(listed_schema(listed, name), fields, name);
        }
        assert_schema_keys(
            &listed_schema(listed, "thread_fork")["properties"]["target_model"],
            fields!(ModelSelection {
                provider,
                model,
                effort,
                agent
            }),
            "thread_fork.target_model",
        );
        assert_schema_keys(
            &listed_schema(listed, "compaction_econ")["properties"]["prices"],
            fields!(Prices {
                input,
                output,
                cache_read,
                cache_write
            }),
            "compaction_econ.prices",
        );
    }

    #[test]
    fn every_tool_schema_closes_object_properties() {
        fn check(schema: &Value, path: &str) {
            if schema["type"] == "object" {
                assert_eq!(schema["additionalProperties"], false, "{path}");
                if let Some(properties) = schema["properties"].as_object() {
                    for (key, value) in properties {
                        check(value, &format!("{path}.{key}"));
                    }
                }
            }
        }

        for tool in tools() {
            check(&tool["inputSchema"], tool["name"].as_str().unwrap());
        }
    }

    #[tokio::test]
    async fn unknown_tool_argument_is_rejected_with_valid_keys() {
        let parent = make_thread(
            Uuid::new_v4(),
            RuntimeMode::FullAccess,
            InteractionMode::Default,
        );
        let server = McpServer::new(DefaultUnsupportedBackend(parent.clone()));
        let response = server
            .handle(
                rpc_call("task_status", json!({"taskid": Uuid::new_v4()})),
                None,
            )
            .await
            .unwrap();
        assert_eq!(response["result"]["isError"], true);
        let error = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("taskid"), "{error}");
        assert!(error.contains("task_id"), "{error}");
        assert!(error.contains("valid keys"), "{error}");

        let mistyped_target = server
            .handle(
                rpc_call(
                    "thread_send",
                    json!({"prompt": "send this", "threadid": Uuid::new_v4()}),
                ),
                Some(parent.id),
            )
            .await
            .unwrap();
        assert_eq!(mistyped_target["result"]["isError"], true);
        let error = mistyped_target["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(error.contains("threadid"), "{error}");
        assert!(error.contains("thread_id"), "{error}");
        assert!(error.contains("valid keys"), "{error}");

        let nested = server
            .handle(
                rpc_call(
                    "compaction_econ",
                    json!({"prices": {
                        "input": 0.1,
                        "output": 0.1,
                        "cache_read": 0.1,
                        "cache_wirte": 0.1
                    }}),
                ),
                None,
            )
            .await
            .unwrap();
        let error = nested["result"]["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("compaction_econ.prices"), "{error}");
        assert!(error.contains("cache_write"), "{error}");
        assert!(error.contains("valid keys"), "{error}");
    }

    #[tokio::test]
    async fn declared_serde_argument_aliases_remain_valid() {
        let parent = make_thread(
            Uuid::new_v4(),
            RuntimeMode::FullAccess,
            InteractionMode::Default,
        );
        let server = McpServer::new(DefaultUnsupportedBackend(parent.clone()));
        for name in ["task_diff", "task_merge"] {
            let response = server
                .handle(
                    rpc_call(name, json!({"task_id": Uuid::new_v4()})),
                    Some(parent.id),
                )
                .await
                .unwrap();
            let error = response["result"]["content"][0]["text"].as_str().unwrap();
            assert!(error.contains("unsupported"), "{error}");
            assert!(!error.contains("valid keys"), "{error}");
        }
    }

    fn listed_schema<'a>(tools: &'a [Value], name: &str) -> &'a Value {
        &tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing tool schema for {name}"))["inputSchema"]
    }

    #[tokio::test]
    async fn tools_list_registers_task_diff_and_task_merge() {
        let parent = make_thread(
            Uuid::new_v4(),
            RuntimeMode::FullAccess,
            InteractionMode::Default,
        );
        let server = McpServer::new(DefaultUnsupportedBackend(parent));
        let list = server
            .handle(
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
                None,
            )
            .await
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "task_diff"));
        assert!(tools.iter().any(|t| t["name"] == "task_merge"));
    }

    #[tokio::test]
    async fn default_trait_methods_return_unsupported() {
        let parent = make_thread(
            Uuid::new_v4(),
            RuntimeMode::FullAccess,
            InteractionMode::Default,
        );
        let parent_id = parent.id;
        let server = McpServer::new(DefaultUnsupportedBackend(parent));
        let diff_res = server
            .handle(
                rpc_call("task_diff", json!({"child_thread_id": Uuid::new_v4()})),
                Some(parent_id),
            )
            .await
            .unwrap();
        assert_eq!(diff_res["result"]["isError"], true);
        assert!(
            diff_res["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unsupported")
        );
        let merge_res = server
            .handle(
                rpc_call("task_merge", json!({"child_thread_id": Uuid::new_v4()})),
                Some(parent_id),
            )
            .await
            .unwrap();
        assert_eq!(merge_res["result"]["isError"], true);
        assert!(
            merge_res["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("unsupported")
        );
    }

    #[tokio::test]
    async fn task_diff_and_task_merge_enforce_permissions_and_dispatch() {
        let project = Uuid::new_v4();
        let other_project = Uuid::new_v4();
        let parent = make_thread(
            project,
            RuntimeMode::AutoAcceptEdits,
            InteractionMode::Default,
        );
        let mut child = make_thread(
            project,
            RuntimeMode::AutoAcceptEdits,
            InteractionMode::Default,
        );
        child.parent_thread_id = Some(parent.id);
        let privileged_child =
            make_thread(project, RuntimeMode::FullAccess, InteractionMode::Default);
        let foreign_child = make_thread(
            other_project,
            RuntimeMode::ApprovalRequired,
            InteractionMode::Default,
        );
        let plan_parent = make_thread(project, RuntimeMode::FullAccess, InteractionMode::Plan);

        let backend = StubBackend::new(
            vec![
                parent.clone(),
                child.clone(),
                privileged_child.clone(),
                foreign_child.clone(),
                plan_parent.clone(),
            ],
            true,
        );
        let recorded = backend.last_merge.clone();
        let server = McpServer::new(backend);

        // Happy path: scoped task_diff and task_merge.
        let diff_req = rpc_call("task_diff", json!({"child_thread_id": child.id}));
        let diff_res = server
            .handle_scoped_bytes(diff_req.to_string().as_bytes(), &parent)
            .await
            .unwrap();
        assert_ne!(diff_res["result"]["isError"], true);
        let diff_payload: Value =
            serde_json::from_str(diff_res["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(diff_payload["total_additions"], 3);

        let merge_req = rpc_call(
            "task_merge",
            json!({"child_thread_id": child.id, "commit_message": "merge child", "cleanup": true}),
        );
        let merge_res = server
            .handle_scoped_bytes(merge_req.to_string().as_bytes(), &parent)
            .await
            .unwrap();
        assert_ne!(merge_res["result"]["isError"], true);
        let merge_payload: TaskMergeResult =
            serde_json::from_str(merge_res["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert!(merge_payload.merged);
        assert!(merge_payload.cleaned_up);
        assert_eq!(
            *recorded.lock().unwrap(),
            Some(RecordedMerge {
                caller_id: parent.id,
                child_thread_id: child.id,
                commit_message: Some("merge child".into()),
                cleanup: true,
            })
        );

        // Plan caller cannot call task_merge (both scoped and unscoped).
        let plan_req = rpc_call("task_merge", json!({"child_thread_id": child.id}));
        let plan_scoped = server
            .handle_scoped_bytes(plan_req.to_string().as_bytes(), &plan_parent)
            .await
            .unwrap();
        assert_eq!(plan_scoped["result"]["isError"], true);
        assert!(
            plan_scoped["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("plan")
        );
        let plan_unscoped = server.handle(plan_req, Some(plan_parent.id)).await.unwrap();
        assert_eq!(plan_unscoped["result"]["isError"], true);

        // Caller cannot merge a higher-privilege target thread.
        let esc_req = rpc_call(
            "task_merge",
            json!({"child_thread_id": privileged_child.id}),
        );
        let esc_res = server
            .handle_scoped_bytes(esc_req.to_string().as_bytes(), &parent)
            .await
            .unwrap();
        assert_eq!(esc_res["result"]["isError"], true);
        assert!(
            esc_res["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("permissions")
        );

        // Caller cannot inspect or merge a thread in another project.
        let foreign_req = rpc_call("task_diff", json!({"child_thread_id": foreign_child.id}));
        let foreign_res = server
            .handle_scoped_bytes(foreign_req.to_string().as_bytes(), &parent)
            .await
            .unwrap();
        assert_eq!(foreign_res["result"]["isError"], true);

        // Scoped caller cannot spoof another parent_thread_id.
        let spoof_req = rpc_call(
            "task_merge",
            json!({"child_thread_id": child.id, "parent_thread_id": plan_parent.id}),
        );
        let spoof_res = server
            .handle_scoped_bytes(spoof_req.to_string().as_bytes(), &parent)
            .await
            .unwrap();
        assert_eq!(spoof_res["result"]["isError"], true);
    }
}
