use crate::{
    coordinator::{Message, RunCoordinator, RunTicket, compact, text_value},
    database::with_db,
};
use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{borrow::Cow, path::PathBuf};
use t3_core::*;
use t3_git::GitManager;
use t3_mcp::{
    Capabilities, DelegateMode, DelegatedTask, ForkOptions, OrchestratorBackend, ThreadRead,
};
use t3_providers::ProviderRegistry;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct CreateThread {
    pub project_id: Uuid,
    pub title: String,
    pub model_selection: Option<ModelSelection>,
    #[serde(default)]
    pub runtime_mode: RuntimeMode,
    #[serde(default)]
    pub interaction_mode: InteractionMode,
    #[serde(default)]
    pub isolation: IsolationMode,
    pub base_ref: Option<String>,
}

#[derive(Deserialize)]
pub struct PatchThread {
    pub title: Option<String>,
    pub model_selection: Option<ModelSelection>,
    #[serde(default)]
    pub runtime_mode: Option<RuntimeMode>,
    #[serde(default)]
    pub interaction_mode: Option<InteractionMode>,
    pub archived: Option<bool>,
    pub pinned: Option<bool>,
}

impl RunCoordinator {
    pub async fn create_thread(&self, input: CreateThread) -> CoreResult<Thread> {
        let _guard = self.0.active.lock().await;
        self.new_thread(input, None).await
    }

    pub async fn patch_thread(&self, id: Uuid, patch: PatchThread) -> CoreResult<Thread> {
        let _guard = self.0.active.lock().await;
        let store = &self.0.store;
        let thread = store.get_thread(id).await?;
        if let Some(title) = &patch.title {
            validate_title(title)?;
        }
        if let Some(selection) = &patch.model_selection {
            self.driver(selection)?;
        }
        if let Some(title) = patch.title {
            store.update_thread_title(id, title).await?;
        }
        if let Some(selection) = patch.model_selection {
            self.select_model(&thread, &selection).await?;
        }
        if patch.runtime_mode.is_some() || patch.interaction_mode.is_some() {
            store
                .update_thread_modes(
                    id,
                    patch.runtime_mode.unwrap_or(thread.runtime_mode),
                    patch.interaction_mode.unwrap_or(thread.interaction_mode),
                )
                .await?;
        }
        if let Some(archived) = patch.archived {
            store.archive_thread(id, archived).await?;
        }
        if let Some(pinned) = patch.pinned {
            self.sql(move |db| {
                db.execute(
                    "UPDATE threads SET pinned = ?2, updated_at = ?3 WHERE id = ?1",
                    params![
                        id.to_string(),
                        pinned,
                        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                    ],
                )
            })
            .await?;
        }
        store.get_thread(id).await
    }

    pub(crate) async fn select_model(
        &self,
        thread: &Thread,
        model: &ModelSelection,
    ) -> CoreResult<()> {
        if thread.model_selection.provider == model.provider
            && thread.model_selection.model == model.model
        {
            return self
                .0
                .store
                .update_thread_model(thread.id, model.clone())
                .await;
        }
        let id = thread.id.to_string();
        let selection =
            serde_json::to_string(model).map_err(|error| CoreError::Internal(error.to_string()))?;
        self.sql(move |db| {
            db.execute(
                "UPDATE threads SET model_selection = ?2, provider_session_id = NULL, updated_at = ?3 WHERE id = ?1",
                params![id, selection, Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)],
            )
        }).await?;
        self.clear_live_context(thread.id).await;
        Ok(())
    }

    pub async fn delete_project(&self, id: Uuid) -> CoreResult<()> {
        let guard = self.0.active.lock().await;
        if guard.values().any(|run| run.project_id == id) {
            return Err(CoreError::InvalidArgument("project has active runs".into()));
        }
        self.0.store.delete_project(id).await
    }

    pub async fn workspace(&self, thread: &Thread) -> CoreResult<PathBuf> {
        match &thread.worktree_path {
            Some(path) => Ok(path.clone()),
            None => Ok(self
                .0
                .store
                .get_project(thread.project_id)
                .await?
                .workspace_root),
        }
    }

    pub async fn diff(&self, id: Uuid, ignore_whitespace: bool) -> CoreResult<DiffSummary> {
        let thread = self.0.store.get_thread(id).await?;
        GitManager::diff_summary(&self.workspace(&thread).await?, ignore_whitespace).await
    }

    pub(crate) async fn new_thread(
        &self,
        input: CreateThread,
        parent: Option<(&Thread, &str)>,
    ) -> CoreResult<Thread> {
        validate_title(&input.title)?;
        let store = &self.0.store;
        let project = store.get_project(input.project_id).await?;
        let model_selection = input
            .model_selection
            .or(project.default_model)
            .unwrap_or_else(|| self.0.cfg.default_model.clone());
        self.driver(&model_selection)?;
        let shared = parent.and_then(|(parent, _)| parent.worktree_path.clone());
        let workspace = shared.as_deref().unwrap_or(&project.workspace_root);
        let (worktree_path, branch) = match input.isolation {
            IsolationMode::Worktree => {
                let (path, branch) = self
                    .0
                    .git
                    .create_worktree(workspace, &input.title, input.base_ref.as_deref())
                    .await?;
                (Some(path), Some(branch))
            }
            IsolationMode::Shared => (
                shared.clone(),
                GitManager::current_branch(workspace).await.unwrap_or(None),
            ),
        };
        let now = Utc::now();
        let result = store
            .create_thread(Thread {
                id: Uuid::new_v4(),
                project_id: project.id,
                parent_thread_id: parent.map(|(parent, _)| parent.id),
                relationship: parent.map(|(_, relationship)| relationship.to_owned()),
                title: input.title,
                model_selection,
                runtime_mode: input.runtime_mode,
                interaction_mode: input.interaction_mode,
                status: ThreadStatus::Idle,
                worktree_path: worktree_path.clone(),
                branch,
                provider_session_id: None,
                archived: false,
                pinned: false,
                created_at: now,
                updated_at: now,
            })
            .await;
        if result.is_err()
            && input.isolation == IsolationMode::Worktree
            && let Some(path) = worktree_path
        {
            let _ = self.0.git.remove_worktree(workspace, &path).await;
        }
        result
    }

    pub(crate) async fn launch_child(
        &self,
        spec: DelegationSpec,
    ) -> CoreResult<(Thread, RunTicket)> {
        if spec.task.trim().is_empty() {
            return Err(CoreError::InvalidArgument("task is required".into()));
        }
        let mut active = self.0.active.lock().await;
        self.ensure_accepting_runs()?;
        let parent = self.0.store.get_thread(spec.parent_thread_id).await?;
        spec.validate_permissions(&parent)?;
        let thread = self
            .new_thread(
                CreateThread {
                    project_id: parent.project_id,
                    title: spec.title,
                    model_selection: Some(spec.model_selection),
                    runtime_mode: spec.runtime_mode,
                    interaction_mode: spec.interaction_mode,
                    isolation: spec.isolation,
                    base_ref: spec.base_ref,
                },
                Some((&parent, "delegated")),
            )
            .await?;
        let ticket = self
            .start_delegated(
                &mut active,
                thread.clone(),
                Message {
                    prompt: spec.task,
                    ..Default::default()
                },
            )
            .await;
        let ticket = match ticket {
            Ok(ticket) => ticket,
            Err(error) => {
                self.discard_thread(&thread).await;
                if spec.isolation == IsolationMode::Worktree
                    && let Some(path) = &thread.worktree_path
                    && let Ok(workspace) = self.workspace(&parent).await
                {
                    let _ = self.0.git.remove_worktree(&workspace, path).await;
                }
                return Err(error);
            }
        };
        self.emit(WsServerMessage::SubagentUpdated {
            parent_thread_id: parent.id,
            child_thread_id: thread.id,
            status: ThreadStatus::Running,
        });
        Ok((
            Thread {
                status: ThreadStatus::Running,
                ..thread
            },
            ticket,
        ))
    }

    pub async fn fork_thread(&self, id: Uuid, options: ForkOptions) -> CoreResult<Thread> {
        let _guard = self.0.active.lock().await;
        let store = &self.0.store;
        let source = store.get_thread(id).await?;
        let summary = match options
            .handoff_summary
            .filter(|summary| !summary.trim().is_empty())
        {
            Some(summary) => compact(&summary, 4000),
            None => self.handoff_summary(&source).await?,
        };
        let target = self
            .new_thread(
                CreateThread {
                    project_id: source.project_id,
                    title: options
                        .title
                        .unwrap_or_else(|| format!("{} (fork)", source.title)),
                    model_selection: Some(
                        options
                            .target_model
                            .unwrap_or_else(|| source.model_selection.clone()),
                    ),
                    runtime_mode: source.runtime_mode,
                    interaction_mode: source.interaction_mode,
                    isolation: IsolationMode::Shared,
                    base_ref: None,
                },
                Some((&source, "fork")),
            )
            .await?;
        let handoff = ContextHandoff {
            id: Uuid::new_v4(),
            source_thread_id: id,
            target_thread_id: target.id,
            from_model: source.model_selection,
            to_model: target.model_selection.clone(),
            summary,
            created_at: Utc::now(),
        };
        let persist = async {
            store.record_handoff(handoff.clone()).await?;
            let run_id = self.metadata_run(&target).await?;
            self.append(NewTurnItem {
                thread_id: target.id,
                run_id,
                kind: TurnItemKind::Handoff,
                summary: Some(compact(&handoff.summary, 240)),
                payload: json!(handoff),
                searchable_text: Some(handoff.summary),
            })
            .await?;
            Ok(())
        }
        .await;
        if let Err(error) = persist {
            self.discard_thread(&target).await;
            return Err(error);
        }
        Ok(target)
    }

    pub(crate) async fn handoff_summary(&self, thread: &Thread) -> CoreResult<String> {
        let items = self.recent_items(thread.id, 100).await?;
        let runs = self.0.store.list_runs(thread.id).await?;
        Ok(format_handoff(&thread.title, &items, &runs))
    }

    pub(crate) async fn discard_thread(&self, thread: &Thread) {
        let id = thread.id.to_string();
        if let Err(error) = self
            .sql(move |db| {
                db.pragma_update(None, "foreign_keys", true)?;
                db.execute("DELETE FROM threads WHERE id = ?1", [id])
            })
            .await
        {
            tracing::error!(%error, thread_id = %thread.id, "Could not discard incomplete thread");
        }
    }

    async fn metadata_run(&self, thread: &Thread) -> CoreResult<Uuid> {
        let now = Utc::now();
        let run = self
            .0
            .store
            .create_run(Run {
                id: thread.id,
                thread_id: thread.id,
                ordinal: 0,
                provider: thread.model_selection.provider,
                model: thread.model_selection.model.clone(),
                effort: thread.model_selection.effort.clone(),
                status: RunStatus::Completed,
                prompt: String::new(),
                provider_session_id: None,
                input_tokens: None,
                output_tokens: None,
                duration_ms: None,
                error_message: None,
                started_at: now,
                completed_at: Some(now),
            })
            .await?;
        Ok(run.id)
    }

    pub(crate) async fn record_child_completion(
        &self,
        child: &Thread,
        status: ThreadStatus,
        error: Option<&str>,
    ) -> CoreResult<()> {
        let Some(parent_id) = child.parent_thread_id else {
            return Ok(());
        };
        let store = &self.0.store;
        let parent = store.get_thread(parent_id).await?;
        let parent_run = match store.list_runs(parent_id).await?.last() {
            Some(run) => run.id,
            None => self.metadata_run(&parent).await?,
        };
        let summary = match error {
            Some(error) => Some(compact(error, 1000)),
            None => self.latest_assistant(child.id).await?,
        };
        self.append(NewTurnItem {
            thread_id: parent_id,
            run_id: parent_run,
            kind: TurnItemKind::Subagent,
            summary: summary.clone(),
            payload: json!({
                "child_thread_id": child.id, "title": child.title, "status": status,
                "provider": child.model_selection.provider, "model": child.model_selection.model,
                "summary": summary,
            }),
            searchable_text: summary,
        })
        .await?;
        Ok(())
    }

    pub(crate) async fn recent_items(&self, id: Uuid, limit: usize) -> CoreResult<Vec<TurnItem>> {
        let count = self
            .sql(move |db| {
                db.query_row(
                    "SELECT count(*) FROM turn_items WHERE thread_id = ?1",
                    [id.to_string()],
                    |row| row.get::<_, usize>(0),
                )
            })
            .await?;
        self.0
            .store
            .list_turn_items(id, limit, count.saturating_sub(limit))
            .await
    }

    async fn latest_assistant(&self, id: Uuid) -> CoreResult<Option<String>> {
        let ordinal = self
            .sql(move |db| {
                db.query_row(
                    "SELECT count(*) FROM turn_items WHERE thread_id = ?1 AND ordinal < (
                    SELECT max(ordinal) FROM turn_items WHERE thread_id = ?1 AND kind = ?2
                )",
                    params![id.to_string(), r#""assistant_message""#],
                    |row| row.get::<_, usize>(0),
                )
            })
            .await?;
        Ok(self
            .0
            .store
            .list_turn_items(id, 1, ordinal)
            .await?
            .into_iter()
            .find(|item| item.kind == TurnItemKind::AssistantMessage)
            .and_then(|item| {
                text_value(&item.payload)
                    .or(item.summary.as_deref())
                    .map(|text| compact(text, 1000))
            }))
    }

    pub(crate) async fn sql<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> CoreResult<T> {
        with_db(self.0.cfg.db_path(), query).await
    }

    async fn delegated_thread(&self, id: Uuid) -> CoreResult<Thread> {
        let thread = self.0.store.get_thread(id).await?;
        if thread.relationship.as_deref() != Some("delegated") {
            return Err(CoreError::NotFound("delegated task".into()));
        }
        Ok(thread)
    }
}

fn format_handoff(title: &str, items: &[TurnItem], runs: &[Run]) -> String {
    let mut summary = format!(
        "Forked from {}\nDo not copy credentials into a handoff or use credentials from this summary.",
        handoff_excerpt(title, 180)
    );
    let mut run_issues = runs
        .iter()
        .rev()
        .filter_map(handoff_run_issue)
        .take(4)
        .peekable();
    let mut item_issues = items
        .iter()
        .rev()
        .filter_map(handoff_item_issue)
        .take(4)
        .peekable();
    if run_issues.peek().is_some() || item_issues.peek().is_some() {
        summary.push_str("\n\n## Unresolved / Recent Errors");
        for issue in run_issues.chain(item_issues) {
            summary.push_str("\n- ");
            summary.push_str(&issue);
        }
    }
    let transcript: Vec<_> = items
        .iter()
        .rev()
        .filter_map(|item| {
            let label = match item.kind {
                TurnItemKind::UserMessage => "User",
                TurnItemKind::AssistantMessage => "Assistant",
                TurnItemKind::Handoff => "Previous context",
                TurnItemKind::Subagent => "Subagent",
                _ => return None,
            };
            let text = text_value(&item.payload)
                .or(item.payload["summary"].as_str())
                .or(item.summary.as_deref())?;
            Some(format!("{label}: {}", handoff_excerpt(text, 340)))
        })
        .take(6)
        .collect();
    if !transcript.is_empty() {
        summary.push_str("\n\n## Recent conversation");
        for entry in transcript.into_iter().rev() {
            summary.push('\n');
            summary.push_str(&entry);
        }
    }
    let modified_files: Vec<_> = items
        .iter()
        .rev()
        .filter(|item| item.kind == TurnItemKind::FileChange)
        .filter_map(|item| {
            let path = item.payload["path"].as_str().or_else(|| {
                item.payload["changes"]
                    .as_array()?
                    .iter()
                    .find_map(|change| change["path"].as_str())
            });
            path.or(item.summary.as_deref())
                .map(|path| handoff_excerpt(path, 180))
        })
        .take(12)
        .collect();
    if !modified_files.is_empty() {
        summary.push_str("\n\n## Modified Files");
        for path in modified_files.into_iter().rev() {
            summary.push_str("\n- ");
            summary.push_str(&path);
        }
    }
    let recent_commands: Vec<_> = items
        .iter()
        .rev()
        .filter(|item| item.kind == TurnItemKind::CommandExecution)
        .filter_map(|item| {
            let command = item.payload["command"]
                .as_str()
                .or(item.summary.as_deref())?;
            let exit_code = item.payload["exit_code"].as_i64()?;
            Some(format!(
                "{} (exit code {exit_code})",
                handoff_excerpt(command, 180)
            ))
        })
        .take(8)
        .collect();
    if !recent_commands.is_empty() {
        summary.push_str("\n\n## Recent Commands");
        for command in recent_commands.into_iter().rev() {
            summary.push_str("\n- ");
            summary.push_str(&command);
        }
    }
    summary
}

fn handoff_run_issue(run: &Run) -> Option<String> {
    let error = run
        .error_message
        .as_deref()
        .filter(|text| !text.trim().is_empty());
    let status = match run.status {
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "interrupted",
        RunStatus::RateLimited => "rate limited",
        RunStatus::Queued => "queued",
        RunStatus::Running => "unfinished",
        RunStatus::Completed if error.is_some() => "reported an error",
        RunStatus::Completed => return None,
    };
    let mut issue = format!("Run {} ({status})", run.ordinal);
    if let Some(error) = error {
        issue.push_str(": ");
        issue.push_str(&handoff_excerpt(error, 220));
    }
    if !run.prompt.trim().is_empty() {
        issue.push_str(" — Request: ");
        issue.push_str(&handoff_excerpt(&run.prompt, 100));
    }
    Some(issue)
}

fn handoff_item_issue(item: &TurnItem) -> Option<String> {
    let payload = &item.payload;
    let state = &payload["state"];
    for key in ["error", "error_message", "errors"] {
        if let Some(error) = [payload, state]
            .into_iter()
            .filter_map(|data| data.get(key))
            .find(|error| match error {
                Value::Null | Value::Bool(_) => false,
                Value::String(s) => !s.trim().is_empty(),
                Value::Array(a) => !a.is_empty(),
                Value::Object(o) => !o.is_empty(),
                _ => true,
            })
        {
            let detail = text_value(error).map_or_else(|| error.to_string(), str::to_owned);
            return Some(format!(
                "Item {}: {}",
                item.ordinal,
                handoff_excerpt(&detail, 180)
            ));
        }
    }
    let status = match payload["status"].as_str().or(state["status"].as_str()) {
        Some("failed" | "error") => "failed",
        Some("interrupted" | "cancelled" | "canceled") => "interrupted",
        Some("rate-limited" | "rate_limited") => "rate limited",
        Some("queued" | "pending" | "running" | "in_progress" | "in-progress") => "unfinished",
        _ if !payload["retry_after_secs"].is_null() => "rate limited",
        _ if payload["is_error"] == true || payload["error"] == true => "failed",
        _ if payload["exit_code"].as_i64().is_some_and(|code| code != 0) => "failed",
        _ => return None,
    };
    let detail = text_value(payload)
        .or_else(|| payload["content"].as_array()?.iter().find_map(text_value))
        .or_else(|| text_value(state))
        .or(payload["aggregated_output"].as_str())
        .or(payload["summary"].as_str())
        .or(item.summary.as_deref())
        .map(|detail| format!(": {}", handoff_excerpt(detail, 180)))
        .unwrap_or_default();
    Some(format!("Item {} ({status}){detail}", item.ordinal))
}

fn handoff_excerpt(text: &str, limit: usize) -> String {
    compact(&redact_credentials(text), limit)
}

fn redact_credentials(text: &str) -> Cow<'_, str> {
    let mut redacted = String::new();
    let mut cursor = 0;
    for (start, _) in text.match_indices(['s', 'g', 'x', 'B', 't', 'e']) {
        if start >= cursor
            && let Some(len) = credential_len(&text.as_bytes()[start..])
        {
            redacted.push_str(&text[cursor..start]);
            redacted.push_str("[REDACTED]");
            cursor = start + len;
        }
    }
    if cursor == 0 {
        Cow::Borrowed(text)
    } else {
        redacted.push_str(&text[cursor..]);
        Cow::Owned(redacted)
    }
}

fn credential_len(bytes: &[u8]) -> Option<usize> {
    let url_safe = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_');
    let (rest, min, allowed): (&[u8], usize, fn(u8) -> bool) = match bytes {
        [b's', b'k', b'-', rest @ ..] => (rest, 16, url_safe),
        [b'g', b'h', b'p' | b'o' | b'u', b'_', rest @ ..] => {
            (rest, 16, |b| b.is_ascii_alphanumeric())
        }
        [
            b'x',
            b'o',
            b'x',
            b'b' | b'a' | b'p' | b'r' | b's',
            b'-',
            rest @ ..,
        ] => (rest, 12, |b| b.is_ascii_alphanumeric() || b == b'-'),
        [b'B', b'e', b'a', b'r', b'e', b'r', b' ', rest @ ..] => (rest, 16, |b| {
            b.is_ascii_alphanumeric() || b"._~+/-".contains(&b)
        }),
        [b't', b'3', b'm', b'c', b'p', b'_', rest @ ..] => {
            (rest, 16, |b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }
        [b'e', b'y', b'J', first @ ..] => {
            let mut rest = first;
            for segment in 0..3 {
                let len = rest.iter().take_while(|&&b| url_safe(b)).count();
                if len < 10 {
                    return None;
                }
                rest = &rest[len..];
                if segment < 2 {
                    rest = rest.strip_prefix(b".")?;
                }
            }
            return Some(bytes.len() - rest.len());
        }
        _ => return None,
    };
    let len = rest.iter().take_while(|&&b| allowed(b)).count();
    (len >= min).then_some(bytes.len() - rest.len() + len)
}

pub(crate) fn validate_title(title: &str) -> CoreResult<()> {
    if title.trim().is_empty() {
        Err(CoreError::InvalidArgument("title is required".into()))
    } else {
        Ok(())
    }
}

#[async_trait]
impl OrchestratorBackend for RunCoordinator {
    async fn swarm_status(&self, thread_id: Uuid) -> CoreResult<Option<SwarmBoardSnapshot>> {
        self.get_board(thread_id).await
    }

    async fn swarm_add_worker(
        &self,
        thread_id: Uuid,
        spec: SwarmWorkerSpec,
    ) -> CoreResult<SwarmMember> {
        self.add_worker(thread_id, spec).await
    }

    async fn swarm_post_message(
        &self,
        thread_id: Uuid,
        text: String,
    ) -> CoreResult<SwarmBoardSnapshot> {
        self.post_message(thread_id, None, text).await
    }

    async fn swarm_read_board(
        &self,
        thread_id: Uuid,
        since: i64,
        limit: usize,
    ) -> CoreResult<Vec<SwarmMessage>> {
        self.read_board(thread_id, since, limit).await
    }

    async fn swarm_kill_worker(
        &self,
        thread_id: Uuid,
        name: &str,
    ) -> CoreResult<SwarmBoardSnapshot> {
        self.kill_worker(thread_id, name, true).await
    }

    async fn get_thread(&self, id: Uuid) -> CoreResult<Thread> {
        self.0.store.get_thread(id).await
    }

    async fn workspace_root(&self, thread: &Thread) -> CoreResult<std::path::PathBuf> {
        self.workspace(thread).await
    }

    async fn capabilities(&self) -> CoreResult<Capabilities> {
        let providers = self.0.providers.health_all().await;
        let model_tiers = self
            .0
            .cfg
            .enabled_providers
            .iter()
            .copied()
            .flat_map(ProviderRegistry::default_models)
            .collect();
        Ok(Capabilities {
            providers,
            model_tiers,
        })
    }

    async fn delegate(
        &self,
        spec: DelegationSpec,
        mode: DelegateMode,
    ) -> CoreResult<DelegatedTask> {
        let (thread, mut ticket) = self.launch_child(spec).await?;
        if matches!(mode, DelegateMode::Wait) {
            let _ = ticket.done.wait_for(|done| *done).await;
            return self.task_status(thread.id).await;
        }
        Ok(DelegatedTask {
            task_id: thread.id,
            child_thread_id: thread.id,
            run_id: ticket.id,
            status: thread.status,
            worktree_path: thread.worktree_path,
            summary: None,
        })
    }

    async fn task_status(&self, task_id: Uuid) -> CoreResult<DelegatedTask> {
        let thread = self.delegated_thread(task_id).await?;
        let run = self
            .0
            .store
            .list_runs(thread.id)
            .await?
            .into_iter()
            .find(|run| run.ordinal > 0)
            .ok_or_else(|| CoreError::NotFound("delegated run".into()))?;
        let summary = self
            .latest_assistant(thread.id)
            .await?
            .or(run.error_message);
        Ok(DelegatedTask {
            task_id,
            child_thread_id: thread.id,
            run_id: run.id,
            status: thread.status,
            worktree_path: thread.worktree_path,
            summary,
        })
    }

    async fn task_cancel(&self, task_id: Uuid) -> CoreResult<DelegatedTask> {
        self.delegated_thread(task_id).await?;
        self.interrupt(task_id).await?;
        self.task_status(task_id).await
    }

    async fn thread_list(
        &self,
        project_id: Option<Uuid>,
        include_archived: bool,
        limit: usize,
    ) -> CoreResult<Vec<Thread>> {
        self.0
            .store
            .list_threads(project_id, include_archived, limit.min(1000))
            .await
    }

    async fn thread_read(&self, thread_id: Uuid, limit: usize) -> CoreResult<ThreadRead> {
        let thread = self.0.store.get_thread(thread_id).await?;
        let items = self.recent_items(thread_id, limit.min(1000)).await?;
        Ok(ThreadRead { thread, items })
    }

    async fn thread_send(
        &self,
        thread_id: Uuid,
        prompt: String,
        behavior: Option<FollowUpBehavior>,
    ) -> CoreResult<Uuid> {
        self.send(
            thread_id,
            Message {
                prompt,
                behavior,
                ..Default::default()
            },
        )
        .await
    }

    async fn thread_fork(&self, thread_id: Uuid, options: ForkOptions) -> CoreResult<Thread> {
        self.fork_thread(thread_id, options).await
    }

    async fn orch_stats(&self) -> CoreResult<Vec<OrchModelStat>> {
        crate::orchestration_stats(&self.0.cfg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_supported_credentials_before_truncating() {
        let credentials = [
            "sk-1234567890abcdef_-",
            "ghp_1234567890abcdef",
            "gho_1234567890abcdef",
            "ghu_1234567890abcdef",
            "xoxb-12345-fixture",
            "Bearer abcdefgh012345._~+/-",
            "t3mcp_0123456789abcdef",
            "eyJ1234567890.ABCDEFGHIJ.KLMNOPQRST",
        ];
        let text = format!("日本語 ({}) sk-short", credentials.join(", "));
        let expected = format!("日本語 ({}) sk-short", ["[REDACTED]"; 8].join(", "));
        assert_eq!(redact_credentials(&text), expected);
        assert_eq!(
            handoff_excerpt(&format!("error {}", credentials[0]), 10),
            "error [RED…"
        );
    }

    #[test]
    fn preserves_an_unfinished_item_with_its_summary() {
        let item = TurnItem {
            id: Uuid::nil(),
            thread_id: Uuid::nil(),
            run_id: Uuid::nil(),
            ordinal: 7,
            kind: TurnItemKind::Subagent,
            summary: Some("Apply the outstanding migration".into()),
            payload: json!({"status": "running"}),
            raw_bytes: 0,
            compressed_bytes: None,
            created_at: Utc::now(),
        };
        let summary = format_handoff("Source", &[item], &[]);
        let (_, errors) = summary.split_once("## Unresolved / Recent Errors").unwrap();
        assert!(errors.contains("- Item 7 (unfinished): Apply the outstanding migration"));
    }

    #[test]
    fn handoff_includes_modified_files_and_recent_commands() {
        let item = |ordinal, kind, summary: Option<&str>, payload| TurnItem {
            id: Uuid::new_v4(),
            thread_id: Uuid::nil(),
            run_id: Uuid::nil(),
            ordinal,
            kind,
            summary: summary.map(str::to_owned),
            payload,
            raw_bytes: 0,
            compressed_bytes: None,
            created_at: Utc::now(),
        };
        let summary = format_handoff(
            "Source",
            &[
                item(
                    1,
                    TurnItemKind::FileChange,
                    None,
                    json!({"changes":[{"path":"src/main.rs","kind":"update"}]}),
                ),
                item(
                    2,
                    TurnItemKind::CommandExecution,
                    None,
                    json!({"command":"cargo check", "exit_code":0}),
                ),
            ],
            &[],
        );
        assert!(summary.contains("## Modified Files\n- src/main.rs"));
        assert!(summary.contains("## Recent Commands\n- cargo check (exit code 0)"));
    }

    #[test]
    fn preserves_provider_tool_errors_from_nested_payloads() {
        for payload in [
            json!({"is_error": true, "content": [{"type": "text", "text": "File missing"}]}),
            json!({"state": {"status": "error", "error": "File missing"}}),
        ] {
            let item = TurnItem {
                id: Uuid::nil(),
                thread_id: Uuid::nil(),
                run_id: Uuid::nil(),
                ordinal: 1,
                kind: TurnItemKind::ToolResult,
                summary: None,
                payload,
                raw_bytes: 0,
                compressed_bytes: None,
                created_at: Utc::now(),
            };
            assert!(handoff_item_issue(&item).unwrap().ends_with("File missing"));
        }
    }
}
