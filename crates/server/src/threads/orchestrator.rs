use crate::coordinator::{Message, RunCoordinator};
use async_trait::async_trait;
use t3_core::{
    CoreError, CoreResult, DelegationSpec, DiffSummary, FollowUpBehavior, McpToolProfile,
    OrchModelStat, RunStatus, Store, SwarmWorkerSpec, Thread, ThreadStatus, UndoTier,
    classify_diff_risk,
};
use t3_mcp::{
    Capabilities, DelegateMode, DelegatedTask, ForkOptions, OrchestratorBackend, SwarmMemberResult,
    SwarmMemberSummary, SwarmPostResult, SwarmReadMessage, SwarmStatusResult, SwarmWorkerResult,
    TaskMergeResult, ThreadRead,
};
use t3_providers::{ProviderRegistry, format_delegated_error};
use uuid::Uuid;

impl RunCoordinator {
    async fn delegated_thread(&self, id: Uuid) -> CoreResult<Thread> {
        let thread = self.0.store.get_thread(id).await?;
        if thread.relationship.as_deref() != Some("delegated") {
            return Err(CoreError::NotFound("delegated task".into()));
        }
        Ok(thread)
    }
}

#[async_trait]
impl OrchestratorBackend for RunCoordinator {
    async fn tool_profile(&self, thread_id: Uuid) -> McpToolProfile {
        if let Ok(Some(board)) = self.get_board(thread_id).await
            && board
                .members
                .iter()
                .any(|member| member.thread_id == thread_id && member.role == "worker")
        {
            return McpToolProfile::Worker;
        }
        if self
            .0
            .store
            .get_thread(thread_id)
            .await
            .ok()
            .and_then(|thread| thread.relationship)
            .as_deref()
            == Some("swarm-worker")
        {
            McpToolProfile::Worker
        } else {
            McpToolProfile::Full
        }
    }

    async fn pause_all(&self, paused: bool) -> CoreResult<bool> {
        self.set_pause_all(paused).await
    }

    async fn swarm_status(&self, thread_id: Uuid) -> CoreResult<Option<SwarmStatusResult>> {
        let Some(board) = self.get_board(thread_id).await? else {
            return Ok(None);
        };
        let message_count = self.0.store.count_swarm_messages(board.board_id).await?;
        let (worktrees, overlaps) = self
            .worktree_changes(board.coordinator_thread_id, "swarm-worker")
            .await?;
        let (killed, live): (Vec<_>, Vec<_>) = board
            .members
            .into_iter()
            .partition(|member| member.state == "killed");
        Ok(Some(SwarmStatusResult {
            board_id: board.board_id,
            members: live.into_iter().map(SwarmMemberSummary::from).collect(),
            killed_count: killed.len(),
            head_seq: board.head_seq,
            message_count,
            worktrees,
            overlaps,
        }))
    }

    async fn swarm_add_worker(
        &self,
        thread_id: Uuid,
        spec: SwarmWorkerSpec,
    ) -> CoreResult<SwarmWorkerResult> {
        let brief_chars = spec.brief.chars().count();
        let member = self.add_worker(thread_id, spec).await?;
        Ok(SwarmWorkerResult {
            member: member.into(),
            brief_chars,
        })
    }

    async fn swarm_post_message(
        &self,
        thread_id: Uuid,
        text: String,
    ) -> CoreResult<SwarmPostResult> {
        self.post_message_result(thread_id, text).await
    }

    async fn swarm_read_board(
        &self,
        thread_id: Uuid,
        since_seq: Option<i64>,
        limit: usize,
        seq: Option<i64>,
    ) -> CoreResult<Vec<SwarmReadMessage>> {
        self.read_board(thread_id, since_seq, limit, seq).await
    }

    async fn swarm_kill_worker(
        &self,
        thread_id: Uuid,
        name: &str,
    ) -> CoreResult<SwarmMemberResult> {
        let board = self.kill_worker(thread_id, name, true).await?;
        let member = board
            .members
            .into_iter()
            .find(|member| member.name == name)
            .ok_or_else(|| CoreError::NotFound("swarm worker".into()))?;
        Ok(member.into())
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
            routing_hints: t3_mcp::default_routing_hints(),
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
            worktree: None,
            overlaps: Vec::new(),
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
            .rev()
            .find(|run| run.ordinal > 0)
            .ok_or_else(|| CoreError::NotFound("delegated run".into()))?;
        let status = match run.status {
            RunStatus::Queued | RunStatus::Running => ThreadStatus::Running,
            _ => thread.status,
        };
        let summary = match run.status {
            RunStatus::Failed | RunStatus::RateLimited => run
                .error_message
                .as_deref()
                .map(|error| {
                    format_delegated_error(
                        thread.model_selection.provider,
                        &thread.model_selection.model,
                        error,
                    )
                })
                .or(self.latest_assistant(thread.id).await?),
            _ => self
                .latest_assistant(thread.id)
                .await?
                .or(run.error_message),
        };
        let (worktree, overlaps) = if let Some(parent_id) = thread.parent_thread_id {
            let (worktrees, overlaps) = self.worktree_changes(parent_id, "delegated").await?;
            (
                worktrees
                    .iter()
                    .find(|change| change.thread_id == thread.id)
                    .cloned(),
                overlaps,
            )
        } else {
            (None, Vec::new())
        };
        Ok(DelegatedTask {
            task_id,
            child_thread_id: thread.id,
            run_id: run.id,
            status,
            worktree_path: thread.worktree_path,
            summary,
            worktree,
            overlaps,
        })
    }

    async fn task_cancel(&self, task_id: Uuid) -> CoreResult<DelegatedTask> {
        self.delegated_thread(task_id).await?;
        self.interrupt(task_id).await?;
        self.task_status(task_id).await
    }

    async fn task_diff(&self, caller_id: Uuid, child_thread_id: Uuid) -> CoreResult<DiffSummary> {
        self.child_worktree_diff(caller_id, child_thread_id, false)
            .await
    }

    async fn task_merge(
        &self,
        caller_id: Uuid,
        child_thread_id: Uuid,
        commit_message: Option<String>,
        cleanup: bool,
    ) -> CoreResult<TaskMergeResult> {
        // Undo-price gate for autonomous agent merges: `no-undo` changes (migrations, secrets,
        // deploy/infra, destructive SQL) require explicit human sign-off via the UI or `t3d merge`.
        let diff = self
            .child_worktree_diff(caller_id, child_thread_id, false)
            .await?;
        let risk = diff.risk.unwrap_or_else(|| classify_diff_risk(&diff.files));
        if risk.tier == UndoTier::NoUndo {
            let paths = if risk.flagged_paths.is_empty() {
                "sensitive changes".to_owned()
            } else {
                risk.flagged_paths.join(", ")
            };
            let reasons = risk.reasons.join("; ");
            return Err(CoreError::Auth(format!(
                "undo-gate (no-undo): autonomous agent merge is blocked for [{paths}] ({reasons}). Escalate to the human to review and merge via the UI or `t3d merge`."
            )));
        }
        let mut merged = self
            .merge_child_worktree(caller_id, child_thread_id, commit_message, cleanup)
            .await?;
        if merged.merged && risk.tier == UndoTier::Cleanup && !risk.flagged_paths.is_empty() {
            merged.summary.push_str(&format!(
                " [undo-gate: cleanup — second-opinion review recommended for: {}]",
                risk.flagged_paths.join(", ")
            ));
        }
        Ok(merged)
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
            false,
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
