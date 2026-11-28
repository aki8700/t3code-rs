mod handoff;
mod orchestrator;
mod worktree;

#[cfg(test)]
mod tests;

pub(crate) use handoff::redact_credentials;

use crate::{
    coordinator::{Message, RunCoordinator, RunTicket, compact, text_value},
    database::with_db,
};
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::json;
use t3_core::*;
use t3_git::GitManager;
use t3_mcp::ForkOptions;
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
        self.new_thread(input, None).await
    }

    pub async fn patch_thread(&self, id: Uuid, patch: PatchThread) -> CoreResult<Thread> {
        let _guard = self.lock_thread(id).await;
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
        let _project = self.0.project_locks.get(id).write_owned().await;
        if self
            .0
            .active
            .lock()
            .await
            .values()
            .any(|run| run.project_id == id)
        {
            return Err(CoreError::InvalidArgument("project has active runs".into()));
        }
        self.0.store.delete_project(id).await
    }

    pub(crate) async fn new_thread(
        &self,
        input: CreateThread,
        parent: Option<(&Thread, &str)>,
    ) -> CoreResult<Thread> {
        let guard = self.lock_thread(Uuid::new_v4()).await;
        self.new_thread_locked(&guard, input, parent).await
    }

    pub(crate) async fn new_thread_locked(
        &self,
        guard: &crate::coordinator::ThreadGuard,
        input: CreateThread,
        parent: Option<(&Thread, &str)>,
    ) -> CoreResult<Thread> {
        let _project = self
            .0
            .project_locks
            .get(input.project_id)
            .read_owned()
            .await;
        validate_title(&input.title)?;
        let store = &self.0.store;
        let project = store.get_project(input.project_id).await?;
        let model_selection = input
            .model_selection
            .or(project.default_model)
            .unwrap_or_else(|| self.0.settings.get().default_model);
        self.driver(&model_selection)?;
        let shared = parent.and_then(|(parent, _)| parent.worktree_path.clone());
        let workspace = shared.as_deref().unwrap_or(&project.workspace_root);
        let (worktree_path, branch) = match input.isolation {
            IsolationMode::Worktree => {
                let (path, branch) = self
                    .0
                    .git
                    .create_worktree(workspace, &input.title, input.base_ref.as_deref())
                    .await
                    .map_err(|error| {
                        CoreError::InvalidArgument(format!(
                            "could not create worktree from cwd {}: {error}",
                            workspace.display()
                        ))
                    })?;
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
                id: guard.id,
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
        let _parent = self.lock_thread(spec.parent_thread_id).await;
        let child = self.lock_thread(Uuid::new_v4()).await;
        self.ensure_accepting_runs()?;
        let parent = self.0.store.get_thread(spec.parent_thread_id).await?;
        spec.validate_permissions(&parent)?;
        let thread = self
            .new_thread_locked(
                &child,
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
                &child,
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
        let status = self.0.store.get_thread(thread.id).await?.status;
        self.emit(WsServerMessage::SubagentUpdated {
            parent_thread_id: parent.id,
            child_thread_id: thread.id,
            status,
        });
        Ok((Thread { status, ..thread }, ticket))
    }

    pub async fn fork_thread(&self, id: Uuid, options: ForkOptions) -> CoreResult<Thread> {
        let _guard = self.lock_thread(id).await;
        let store = &self.0.store;
        let source = store.get_thread(id).await?;
        let summary = match options
            .handoff_summary
            .filter(|summary| !summary.trim().is_empty())
        {
            Some(summary) => compact(&summary, 4000),
            None => self.handoff_summary(&source).await?,
        };
        let child = self.lock_thread(Uuid::new_v4()).await;
        let target = self
            .new_thread_locked(
                &child,
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

    /// Compacts a Codex thread's own session instead of forking it. Other providers either
    /// compact natively through `/compact` (Claude, Antigravity) or fall back to `fork_thread`.
    pub async fn compact_thread(&self, id: Uuid) -> CoreResult<()> {
        let _guard = self.lock_thread(id).await;
        let thread = self.0.store.get_thread(id).await?;
        if thread.model_selection.provider != ProviderId::Codex {
            return Err(CoreError::InvalidArgument(
                "in-place compaction is only available for Codex threads".into(),
            ));
        }
        let session_id = thread.provider_session_id.clone().ok_or_else(|| {
            CoreError::InvalidArgument("thread has no Codex session to compact yet".into())
        })?;
        if self.0.active.lock().await.contains_key(&id) {
            return Err(CoreError::InvalidArgument(
                "thread has a running turn; wait for it to finish".into(),
            ));
        }
        let cwd = self.workspace(&thread).await?;
        crate::compact::compact_codex_session(&self.0.cfg.providers.codex_bin, &session_id, &cwd)
            .await
    }

    pub(crate) async fn discard_thread(&self, thread: &Thread) {
        match self.0.store.list_queued_runs(thread.id).await {
            Ok(runs) => {
                for run in runs {
                    self.clear_user_run(run.id);
                    self.complete_queued_ticket(run.id);
                }
            }
            Err(error) => {
                tracing::error!(%error, thread_id = %thread.id, "Could not clear queued thread bookkeeping");
            }
        }
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

    pub(super) async fn metadata_run(&self, thread: &Thread) -> CoreResult<Uuid> {
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
                cache_read_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
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
        let parent_run = {
            // Sibling completions can arrive together on a parent with no
            // runs yet. Choose or create the parent's metadata run only once.
            let _parent = self.lock_thread(parent_id).await;
            let store = &self.0.store;
            match store.list_runs(parent_id).await?.last() {
                Some(run) => run.id,
                None => {
                    self.metadata_run(&store.get_thread(parent_id).await?)
                        .await?
                }
            }
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

    pub(super) async fn latest_assistant(&self, id: Uuid) -> CoreResult<Option<String>> {
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
}

pub(crate) fn validate_title(title: &str) -> CoreResult<()> {
    if title.trim().is_empty() {
        Err(CoreError::InvalidArgument("title is required".into()))
    } else {
        Ok(())
    }
}
