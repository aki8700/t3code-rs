use crate::coordinator::RunCoordinator;
use chrono::Utc;
use rusqlite::params;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex, OnceLock},
};
use t3_core::{CoreError, CoreResult, DiffSummary, InteractionMode, Store, Thread};
use t3_git::{GitManager, MergeOutcome};
use t3_mcp::{TaskMergeResult, WorktreeChange, WorktreeOverlap};
use uuid::Uuid;

/// Changed files listed per worker in status calls; `changed_file_count` has the total.
const MAX_LISTED_CHANGED_FILES: usize = 20;

struct ChildWorktreeContext {
    parent: Thread,
    child: Thread,
    parent_workspace: PathBuf,
    canonical_parent: PathBuf,
    child_worktree: PathBuf,
    child_branch: String,
}

async fn canonicalize_or_self(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf())
}

pub(super) fn workspace_merge_lock(canonical_workspace: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static MERGE_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let locks = MERGE_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut guard = locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        guard
            .entry(canonical_workspace.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

impl RunCoordinator {
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

    pub async fn child_worktree_diff(
        &self,
        parent_id: Uuid,
        child_id: Uuid,
        ignore_whitespace: bool,
    ) -> CoreResult<DiffSummary> {
        let ctx = self.resolve_child_worktree(parent_id, child_id).await?;
        GitManager::branch_diff_summary(
            &ctx.parent_workspace,
            "HEAD",
            &ctx.child_branch,
            ignore_whitespace,
        )
        .await
    }

    pub async fn merge_child_worktree(
        &self,
        parent_id: Uuid,
        child_id: Uuid,
        commit_message: Option<String>,
        cleanup: bool,
    ) -> CoreResult<TaskMergeResult> {
        let ctx = self.resolve_child_worktree(parent_id, child_id).await?;
        let (active_plan, child_active) = {
            let active = self.0.active.lock().await;
            (
                active
                    .get(&ctx.parent.id)
                    .is_some_and(|run| run.interaction_mode == InteractionMode::Plan),
                active.contains_key(&ctx.child.id),
            )
        };
        if ctx.parent.interaction_mode == InteractionMode::Plan || active_plan {
            return Err(CoreError::InvalidArgument(
                "cannot merge a child worktree in plan mode".into(),
            ));
        }
        if child_active {
            return Err(CoreError::InvalidArgument(
                "cannot merge while child thread has an active run".into(),
            ));
        }

        let merge_lock = workspace_merge_lock(&ctx.canonical_parent);
        let _merge_guard = merge_lock.lock().await;

        if self.0.active.lock().await.contains_key(&ctx.child.id) {
            return Err(CoreError::InvalidArgument(
                "cannot merge while child thread has an active run".into(),
            ));
        }

        if GitManager::has_merge_in_progress(&ctx.parent_workspace).await? {
            return Err(CoreError::InvalidArgument(
                "parent workspace already has a merge in progress".into(),
            ));
        }
        if GitManager::has_uncommitted_changes(&ctx.child_worktree).await? {
            return Err(CoreError::InvalidArgument(
                "child worktree has uncommitted changes; commit or discard them before merging"
                    .into(),
            ));
        }

        match GitManager::merge_branch_checked(
            &ctx.parent_workspace,
            &ctx.child_branch,
            commit_message.as_deref(),
        )
        .await?
        {
            MergeOutcome::Conflict {
                conflict_files,
                message,
            } => Ok(TaskMergeResult {
                merged: false,
                child_thread_id: ctx.child.id,
                branch: ctx.child_branch,
                summary: message,
                conflict_files,
                cleaned_up: false,
            }),
            MergeOutcome::Merged { sha, summary } => {
                let mut result_summary = if summary.is_empty() {
                    format!("Merged {} at {sha}", ctx.child_branch)
                } else {
                    format!("{summary} ({sha})")
                };
                let mut cleaned_up = false;
                if cleanup {
                    let active = self.0.active.lock().await;
                    if active.contains_key(&ctx.child.id) {
                        result_summary
                            .push_str(" (cleanup skipped: child thread has an active run)");
                    } else {
                        let remove_res = self
                            .0
                            .git
                            .remove_worktree(&ctx.parent_workspace, &ctx.child_worktree)
                            .await;
                        // A dirty child is snapshotted on top of its merged head before removal.
                        let remove_res = remove_res.map(drop);
                        let branch_res =
                            if remove_res.is_ok() && ctx.child_branch.starts_with("t3rs/") {
                                GitManager::delete_branch(&ctx.parent_workspace, &ctx.child_branch)
                                    .await
                            } else {
                                Ok(())
                            };
                        let prune_res = self.0.git.prune_worktrees(&ctx.parent_workspace).await;
                        match (remove_res, branch_res, prune_res) {
                            (Ok(()), Ok(()), Ok(())) => {
                                cleaned_up = true;
                                let cid = ctx.child.id.to_string();
                                let now =
                                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
                                if let Err(error) = self
                                    .sql(move |db| {
                                        db.execute(
                                            "UPDATE threads SET worktree_path = NULL, branch = NULL, updated_at = ?2 WHERE id = ?1",
                                            params![cid, now],
                                        )
                                    })
                                    .await
                                {
                                    tracing::warn!(%error, child_id = %ctx.child.id, "Could not clear merged child worktree metadata");
                                }
                            }
                            (r, b, p) => {
                                let mut errs = Vec::new();
                                if let Err(error) = r {
                                    tracing::warn!(%error, child_id = %ctx.child.id, "Could not remove merged child worktree");
                                    errs.push(error.to_string());
                                }
                                if let Err(error) = b {
                                    tracing::warn!(%error, branch = %ctx.child_branch, "Could not delete merged child branch");
                                    errs.push(error.to_string());
                                }
                                if let Err(error) = p {
                                    tracing::warn!(%error, "Could not prune worktrees after merge");
                                    errs.push(error.to_string());
                                }
                                result_summary.push_str(&format!(
                                    " (cleanup incomplete: {})",
                                    errs.join("; ")
                                ));
                            }
                        }
                    }
                }
                Ok(TaskMergeResult {
                    merged: true,
                    child_thread_id: ctx.child.id,
                    branch: ctx.child_branch,
                    summary: result_summary,
                    conflict_files: Vec::new(),
                    cleaned_up,
                })
            }
        }
    }

    async fn resolve_child_worktree(
        &self,
        parent_id: Uuid,
        child_id: Uuid,
    ) -> CoreResult<ChildWorktreeContext> {
        let parent = self.0.store.get_thread(parent_id).await?;
        let child = self.0.store.get_thread(child_id).await?;
        if child.parent_thread_id != Some(parent.id) || child.project_id != parent.project_id {
            return Err(CoreError::InvalidArgument(
                "child thread does not belong to parent thread".into(),
            ));
        }
        let parent_workspace = self.workspace(&parent).await?;
        let child_worktree = child.worktree_path.clone().ok_or_else(|| {
            CoreError::InvalidArgument("child thread does not have a dedicated worktree".into())
        })?;
        let canonical_parent = canonicalize_or_self(&parent_workspace).await;
        let canonical_child = canonicalize_or_self(&child_worktree).await;
        if child_worktree == parent_workspace || canonical_child == canonical_parent {
            return Err(CoreError::InvalidArgument(
                "child thread shares the parent workspace".into(),
            ));
        }
        let child_branch = child
            .branch
            .clone()
            .filter(|branch| !branch.trim().is_empty())
            .ok_or_else(|| {
                CoreError::InvalidArgument("child thread does not have a dedicated branch".into())
            })?;
        let live_parent_branch = GitManager::current_branch(&parent_workspace)
            .await
            .ok()
            .flatten();
        if parent.branch.as_ref() == Some(&child_branch)
            || live_parent_branch.as_ref() == Some(&child_branch)
        {
            return Err(CoreError::InvalidArgument(
                "child thread shares the parent branch".into(),
            ));
        }
        Ok(ChildWorktreeContext {
            parent,
            child,
            parent_workspace,
            canonical_parent,
            child_worktree,
            child_branch,
        })
    }

    pub(super) async fn worktree_changes(
        &self,
        parent_id: Uuid,
        relationship: &str,
    ) -> CoreResult<(Vec<WorktreeChange>, Vec<WorktreeOverlap>)> {
        let parent = self.0.store.get_thread(parent_id).await?;
        let parent_workspace = self.workspace(&parent).await?;
        let base_ref = GitManager::current_branch(&parent_workspace)
            .await
            .ok()
            .flatten()
            .or_else(|| parent.branch.clone());
        let live_swarm_workers = if relationship == "swarm-worker" {
            self.get_board(parent_id).await?.map(|board| {
                board
                    .members
                    .into_iter()
                    .filter(|member| member.state != "killed")
                    .map(|member| member.thread_id)
                    .collect::<std::collections::HashSet<_>>()
            })
        } else {
            None
        };
        let children = self
            .0
            .store
            .list_child_threads(parent_id)
            .await?
            .into_iter()
            .filter(|child| child.relationship.as_deref() == Some(relationship))
            .filter(|child| {
                live_swarm_workers
                    .as_ref()
                    .is_none_or(|live| live.contains(&child.id))
            });
        let mut by_file = BTreeMap::<String, Vec<(Uuid, String)>>::new();
        let mut worktrees = Vec::new();
        for child in children {
            let Some(path) = child.worktree_path.as_deref() else {
                continue;
            };
            if path == parent_workspace.as_path() {
                continue;
            }
            let status = match base_ref.as_deref() {
                Some(base_ref) => GitManager::worktree_branch_status(path, base_ref)
                    .await
                    .ok(),
                None => None,
            };
            let mut report = WorktreeChange {
                thread_id: child.id,
                worker: child.title,
                changed_file_count: 0,
                changed_files: status
                    .as_ref()
                    .map_or_else(Vec::new, |status| status.changed_files.clone()),
                behind: status.map(|status| status.behind),
            };
            for file in &report.changed_files {
                by_file
                    .entry(file.clone())
                    .or_default()
                    .push((report.thread_id, report.worker.clone()));
            }
            // Overlaps carry the signal; per-worker lists stay short so status calls stay small.
            report.changed_file_count = report.changed_files.len();
            report.changed_files.truncate(MAX_LISTED_CHANGED_FILES);
            worktrees.push(report);
        }
        let overlaps = by_file
            .into_iter()
            .filter(|(_, workers)| workers.len() > 1)
            .map(|(file, workers)| WorktreeOverlap {
                file,
                worker_ids: workers.iter().map(|(id, _)| *id).collect(),
                workers: workers.into_iter().map(|(_, name)| name).collect(),
            })
            .collect();
        Ok((worktrees, overlaps))
    }
}
