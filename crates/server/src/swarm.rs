use crate::{
    coordinator::{ActiveRun, Message, RunCoordinator},
    threads::CreateThread,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::{collections::HashMap, fmt::Write, path::Path};
use t3_core::*;
use t3_mcp::{SwarmPostResult, SwarmReadMessage};
use t3_providers::{ProviderRegistry, format_delegated_error};
use tokio::sync::watch;
use uuid::Uuid;

const MAX_WAKE_DEPTH: u8 = 8;
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_ACTIVE_WORKERS: usize = 8;
const WAKE_BATCH: usize = 16;
const ENDED_ERROR_CHARS: usize = 200;
const READ_MESSAGE_MAX_CHARS: usize = 2_000;

/// Board posts `(from, to]` handed to a run that has not finished yet, per member thread.
/// The member's `last_read_seq` advances only when that run completes, so a failed or
/// cancelled run leaves its posts unread for the next wake (at-least-once delivery).
/// Lost on restart, which can only cause a duplicate delivery.
pub(crate) type WakeDeliveries = std::sync::Mutex<HashMap<Uuid, Vec<WakeDelivery>>>;

pub(crate) struct WakeDelivery {
    run_id: Uuid,
    from: i64,
    to: i64,
}

pub(crate) fn is_swarm_command(prompt: &str) -> bool {
    prompt.split_whitespace().next() == Some("/swarm")
}

#[derive(Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub(crate) enum Action {
    #[serde(rename = "swarm_add_worker")]
    Add(SwarmWorkerSpec),
    #[serde(rename = "swarm_post_message")]
    Post { text: String },
    #[serde(rename = "swarm_kill_worker")]
    Kill { name: String },
}

pub(crate) struct SwarmTurn {
    marker: Option<String>,
}

impl SwarmTurn {
    pub(crate) fn prepare(
        board: &SwarmBoardSnapshot,
        thread_id: Uuid,
        provider: ProviderId,
        prompt: &mut String,
    ) -> CoreResult<Self> {
        let member = live_member(board, thread_id)?;
        let coordinator = member.role == "coordinator";
        let report = format!(
            "Work on the assigned brief. When finished or blocked, call swarm_post_message once with text starting with @coordinator [done] followed by result:, verified: (commands run and their outcome, or not verified), blockers: and files:. Send one post within the limit of {MAX_MESSAGE_BYTES} bytes (about 1300 Japanese characters); write longer details to a file and list its path. End your turn after posting. Reply to subsequent updates only when you have new information."
        );
        let lead = format!(
            "Coordinate work using swarm_status, swarm_add_worker, swarm_post_message, swarm_read_board and swarm_kill_worker. Run independent slices in parallel with separate worker contexts. Each brief states the objective, the output expected, boundaries (files or scope the worker owns), an effort budget and a stop condition. When a worker ends without reporting or repeats a result, change the brief or model, or stop it, instead of resending the same request. Keep passed work frozen, and escalate no-undo merges (migrations/secrets/deploy) to the human. Mentions wake the named member. After assigning work, end your turn and wait for results addressed to @coordinator. Each brief and post has a limit of {MAX_MESSAGE_BYTES} bytes (about 1300 Japanese characters); put a longer brief in a file and send its path."
        );
        let instructions = if coordinator { lead } else { report };
        let marker =
            (provider != ProviderId::Mock).then(|| format!("T3_SWARM_ACTION {}", board.board_id));
        let mut instructions = format!(
            "Swarm board {}. You are {}.\n{instructions}\n",
            board.board_id, member.name
        );
        if let Some(marker) = &marker {
            let examples = if coordinator {
                r#"{"tool":"swarm_add_worker","name":"reviewer","brief":"Review the changes"}
{"tool":"swarm_post_message","text":"@reviewer Focus on error handling"}
{"tool":"swarm_kill_worker","name":"reviewer"}"#
            } else {
                r#"{"tool":"swarm_post_message","text":"@coordinator [done] result: ... verified: ... blockers: none files: ..."}"#
            };
            instructions.push_str("When swarm MCP is unavailable or requires approval, return plain action lines at the end of your response. The daemon runs those actions after the turn. Use only the lines needed, with your own arguments, and only one method per action.\n");
            for example in examples.lines() {
                writeln!(instructions, "{marker} {example}").expect("write to String");
            }
            instructions.push_str("Members: ");
            for member in board
                .members
                .iter()
                .filter(|member| member.state != "killed")
            {
                write!(instructions, "{} ({}), ", member.name, member.state)
                    .expect("write to String");
            }
            instructions.truncate(instructions.len() - 2);
            instructions.push('\n');
        }
        *prompt = format!("{prompt}\n\n{instructions}");
        Ok(Self { marker })
    }

    pub(crate) fn actions(&self, output: &str) -> Vec<Action> {
        let Some(marker) = &self.marker else {
            return Vec::new();
        };
        let mut actions = output
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .map_while(|line| line.strip_prefix(marker)?.strip_prefix(' '))
            .take(8)
            .filter_map(|action| serde_json::from_str(action).ok())
            .collect::<Vec<_>>();
        actions.reverse();
        actions
    }
}

impl RunCoordinator {
    pub(crate) async fn get_board(
        &self,
        thread_id: Uuid,
    ) -> CoreResult<Option<SwarmBoardSnapshot>> {
        self.0.store.get_swarm_board_by_thread(thread_id).await
    }

    async fn require_board(&self, thread_id: Uuid) -> CoreResult<SwarmBoardSnapshot> {
        self.get_board(thread_id)
            .await?
            .ok_or_else(|| CoreError::NotFound("swarm board".into()))
    }

    pub(crate) async fn ensure_board(&self, thread: &Thread) -> CoreResult<()> {
        if self.get_board(thread.id).await?.is_none() {
            let board = self.0.store.ensure_swarm_board(thread.clone()).await?;
            self.emit_board(&board);
        }
        Ok(())
    }

    fn emit_board(&self, board: &SwarmBoardSnapshot) {
        self.emit(WsServerMessage::SwarmUpdated {
            thread_id: board.coordinator_thread_id,
            board: board.clone(),
        });
    }

    pub(crate) async fn swarm_state(&self, thread_id: Uuid, state: &str) -> CoreResult<()> {
        if self
            .0
            .store
            .update_swarm_member_state(thread_id, state)
            .await?
        {
            self.emit_board(&self.require_board(thread_id).await?);
        }
        Ok(())
    }

    async fn effective_thread(
        &self,
        active: &HashMap<Uuid, ActiveRun>,
        id: Uuid,
    ) -> CoreResult<Thread> {
        let mut thread = self.0.store.get_thread(id).await?;
        if let Some(run) = active.get(&id) {
            thread.runtime_mode = thread.runtime_mode.min(run.runtime_mode);
            if run.interaction_mode == InteractionMode::Plan {
                thread.interaction_mode = InteractionMode::Plan;
            }
        }
        Ok(thread)
    }

    pub(crate) async fn add_worker(
        &self,
        caller: Uuid,
        spec: SwarmWorkerSpec,
    ) -> CoreResult<SwarmMember> {
        let mut active = self.0.active.lock().await;
        self.add_worker_locked(&mut active, caller, spec).await
    }

    async fn add_worker_locked(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        caller_id: Uuid,
        spec: SwarmWorkerSpec,
    ) -> CoreResult<SwarmMember> {
        self.ensure_accepting_runs()?;
        if !valid_name(&spec.name) || matches!(spec.name.as_str(), "all" | "coordinator" | "user") {
            return Err(CoreError::InvalidArgument(
                "invalid swarm worker name".into(),
            ));
        }
        if spec.brief.trim().is_empty() {
            return Err(CoreError::InvalidArgument("swarm brief is required".into()));
        }
        let text = format!("@{} {}", spec.name, spec.brief);
        validate_text(&text)?;
        let store = &self.0.store;
        let board = self.require_board(caller_id).await?;
        let actor = live_member(&board, caller_id)?;
        if board.members.iter().any(|member| member.name == spec.name) {
            return Err(CoreError::InvalidArgument(
                "swarm name is already in use".into(),
            ));
        }
        if live_worker_count(&board) >= MAX_ACTIVE_WORKERS {
            return Err(CoreError::InvalidArgument(format!(
                "swarm allows at most {MAX_ACTIVE_WORKERS} active workers"
            )));
        }
        let depth = wake_depth(active, caller_id);
        if depth >= MAX_WAKE_DEPTH {
            return Err(CoreError::InvalidArgument(
                "maximum swarm wake depth reached".into(),
            ));
        }
        let caller = self.effective_thread(active, caller_id).await?;
        let parent = store.get_thread(board.coordinator_thread_id).await?;
        if !can_dispatch(&caller, &parent) {
            return Err(CoreError::InvalidArgument(
                "swarm coordinator exceeds caller permissions or is archived".into(),
            ));
        }
        if parent.interaction_mode == InteractionMode::Plan
            && spec.isolation == IsolationMode::Worktree
        {
            return Err(CoreError::InvalidArgument(
                "a plan thread cannot delegate to a worktree".into(),
            ));
        }
        let model_selection = worker_model(&parent.model_selection, &spec);
        let worker = self
            .new_thread(
                CreateThread {
                    project_id: parent.project_id,
                    title: spec.name.clone(),
                    model_selection: Some(model_selection.clone()),
                    runtime_mode: parent.runtime_mode,
                    interaction_mode: parent.interaction_mode,
                    isolation: spec.isolation,
                    base_ref: None,
                },
                Some((&parent, "swarm-worker")),
            )
            .await?;
        let member = SwarmMember {
            board_id: board.board_id,
            name: spec.name,
            thread_id: worker.id,
            provider: model_selection.provider,
            model: model_selection.model,
            effort: model_selection.effort,
            state: "working".into(),
            role: "worker".into(),
            last_read_seq: board.head_seq,
            created_at: Utc::now(),
        };
        let result = async {
            store.upsert_swarm_member(member.clone()).await?;
            let post = store
                .append_swarm_message(
                    board.board_id,
                    actor.name.clone(),
                    text,
                    vec![member.name.clone()],
                )
                .await?;
            let prompt = format!("Your swarm brief from {}:\n{}", actor.name, spec.brief);
            let run_id = self
                .send_locked(
                    active,
                    worker.clone(),
                    queued_message(prompt),
                    depth + 1,
                    false,
                )
                .await?;
            self.record_delivery(worker.id, run_id, board.head_seq, post.seq);
            CoreResult::Ok(())
        }
        .await;
        if let Err(error) = result {
            if !active.contains_key(&worker.id) {
                self.discard_thread(&worker).await;
                if spec.isolation == IsolationMode::Worktree
                    && let Some(path) = &worker.worktree_path
                    && let Ok(workspace) = self.workspace(&parent).await
                {
                    let _ = self.0.git.remove_worktree(&workspace, path).await;
                }
            }
            return Err(error);
        }
        self.emit_board(&self.require_board(worker.id).await?);
        Ok(member)
    }

    pub(crate) async fn post_message(
        &self,
        caller_id: Uuid,
        sender_override: Option<&str>,
        text: String,
    ) -> CoreResult<SwarmBoardSnapshot> {
        self.post_message_outcome(caller_id, sender_override, text)
            .await
            .map(|(board, _)| board)
    }

    pub(crate) async fn post_message_result(
        &self,
        caller_id: Uuid,
        text: String,
    ) -> CoreResult<SwarmPostResult> {
        self.post_message_outcome(caller_id, None, text)
            .await
            .map(|(_, result)| result)
    }

    async fn post_message_outcome(
        &self,
        caller_id: Uuid,
        sender_override: Option<&str>,
        text: String,
    ) -> CoreResult<(SwarmBoardSnapshot, SwarmPostResult)> {
        let mut active = self.0.active.lock().await;
        let depth = wake_depth(&active, caller_id);
        self.post_locked(&mut active, caller_id, sender_override, text, depth)
            .await
    }

    /// `depth` is the wake depth of the run that caused the post. System notices pass the
    /// depth of the run they report on, so notices cannot wake members forever.
    async fn post_locked(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        caller_id: Uuid,
        sender_override: Option<&str>,
        text: String,
        depth: u8,
    ) -> CoreResult<(SwarmBoardSnapshot, SwarmPostResult)> {
        validate_text(&text)?;
        let board = self.require_board(caller_id).await?;
        let (sender, caller) = if let Some(sender) = sender_override {
            (sender.to_owned(), None)
        } else {
            let member = live_member(&board, caller_id)?;
            (
                member.name.clone(),
                Some(self.effective_thread(active, caller_id).await?),
            )
        };
        let mentions = mentions_in(&text);
        let post = self
            .0
            .store
            .append_swarm_message(board.board_id, sender.clone(), text, mentions)
            .await?;
        let board = self.require_board(caller_id).await?;
        self.emit_board(&board);
        let mut woken = Vec::new();
        let targets = mentioned_members(&board, &sender, &post.mentions);
        if depth >= MAX_WAKE_DEPTH {
            if !targets.is_empty() {
                // No mention in the notice: it must not wake anyone itself.
                self.0
                    .store
                    .append_swarm_message(
                        board.board_id,
                        "system".into(),
                        held_wake_text(post.seq, &targets),
                        Vec::new(),
                    )
                    .await?;
            }
        } else {
            for member in targets {
                match self
                    .wake_member(active, member, caller.as_ref(), depth + 1)
                    .await
                {
                    Ok(true) => woken.push(member.name.clone()),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, thread_id = %member.thread_id, "Could not deliver swarm mention");
                    }
                }
            }
        }
        let updated = self.require_board(caller_id).await?;
        self.emit_board(&updated);
        Ok((
            updated,
            SwarmPostResult {
                seq: post.seq,
                board_id: post.board_id,
                woken,
            },
        ))
    }

    async fn wake_member(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        member: &SwarmMember,
        caller: Option<&Thread>,
        depth: u8,
    ) -> CoreResult<bool> {
        let store = &self.0.store;
        let target = store.get_thread(member.thread_id).await?;
        if target.archived || caller.is_some_and(|caller| !can_dispatch(caller, &target)) {
            return Ok(false);
        }
        let mut cursor = self
            .in_flight_seq(active, target.id)
            .await?
            .max(member.last_read_seq);
        let mut woken = false;
        loop {
            let messages = store
                .list_swarm_messages(member.board_id, cursor, WAKE_BATCH)
                .await?;
            let Some(last) = messages.last() else {
                return Ok(woken);
            };
            let unread = messages
                .iter()
                .filter(|message| message.sender != member.name)
                .collect::<Vec<_>>();
            if !unread.is_empty() {
                let mut prompt = format!("Unread swarm board posts for @{}:\n", member.name);
                for message in unread {
                    writeln!(
                        prompt,
                        "#{} {}: {}",
                        message.seq, message.sender, message.text
                    )
                    .expect("write to String");
                }
                let run_id = self
                    .send_locked(active, target.clone(), queued_message(prompt), depth, false)
                    .await?;
                self.record_delivery(target.id, run_id, cursor, last.seq);
                woken = true;
            } else {
                // Only the member's own posts: nothing to deliver.
                store
                    .ack_swarm_member_read_seq(target.id, cursor, last.seq)
                    .await?;
            }
            cursor = last.seq;
            if messages.len() < WAKE_BATCH {
                return Ok(woken);
            }
        }
    }

    fn deliveries(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Vec<WakeDelivery>>> {
        self.0
            .wake_deliveries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn record_delivery(&self, thread_id: Uuid, run_id: Uuid, from: i64, to: i64) {
        self.deliveries()
            .entry(thread_id)
            .or_default()
            .push(WakeDelivery { run_id, from, to });
    }

    /// Highest post seq already handed to a queued or running run of the thread. Deliveries
    /// whose run was dropped from the queue without running are forgotten here.
    async fn in_flight_seq(
        &self,
        active: &HashMap<Uuid, ActiveRun>,
        thread_id: Uuid,
    ) -> CoreResult<i64> {
        let queued = self.0.store.list_queued_runs(thread_id).await?;
        let running = active.get(&thread_id).map(|run| run.id);
        let mut deliveries = self.deliveries();
        let Some(list) = deliveries.get_mut(&thread_id) else {
            return Ok(0);
        };
        list.retain(|delivery| {
            Some(delivery.run_id) == running || queued.iter().any(|run| run.id == delivery.run_id)
        });
        Ok(list.iter().map(|delivery| delivery.to).max().unwrap_or(0))
    }

    /// Called when a run ends. A completed run acknowledges the posts it delivered. Any
    /// other outcome drops every pending delivery of the thread, so the next wake starts
    /// again from the acknowledged cursor and includes the posts this run never finished.
    pub(crate) async fn settle_wake(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        status: RunStatus,
    ) -> CoreResult<()> {
        let delivered = {
            let mut deliveries = self.deliveries();
            let Some(list) = deliveries.get_mut(&thread_id) else {
                return Ok(());
            };
            let (delivered, rest) = std::mem::take(list)
                .into_iter()
                .partition::<Vec<_>, _>(|delivery| delivery.run_id == run_id);
            if status == RunStatus::Completed && !rest.is_empty() {
                *list = rest;
            } else {
                deliveries.remove(&thread_id);
            }
            delivered
        };
        if status == RunStatus::Completed {
            for delivery in delivered {
                self.0
                    .store
                    .ack_swarm_member_read_seq(thread_id, delivery.from, delivery.to)
                    .await?;
            }
        }
        Ok(())
    }

    /// Wakes the coordinator when a non-coordinator member's run ends without a board post
    /// since `run_started_at`, so a coordinator waiting on that worker cannot stall forever.
    /// The notice is a follow-up queued on the coordinator, so an in-progress coordinator turn
    /// finishes first and then picks it up.
    pub(crate) async fn notify_worker_ended_silently(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        thread_id: Uuid,
        run_started_at: DateTime<Utc>,
        status: RunStatus,
        error: Option<&str>,
    ) -> CoreResult<()> {
        let Some(board) = self.get_board(thread_id).await? else {
            return Ok(());
        };
        let Some(member) = board
            .members
            .iter()
            .find(|member| member.thread_id == thread_id)
        else {
            return Ok(());
        };
        if member.role == "coordinator"
            || member.state == "killed"
            || self
                .worker_posted_since(board.board_id, &member.name, run_started_at)
                .await?
        {
            return Ok(());
        }
        let text = silent_end_text(member, status, error);
        // The ended run is still registered, so a coordinator that keeps re-waking a
        // silent worker accumulates depth even though it is idle between its turns.
        let coordinator = board.coordinator_thread_id;
        let depth = wake_depth(active, thread_id).max(wake_depth(active, coordinator));
        self.post_locked(active, coordinator, Some("system"), text, depth)
            .await?;
        Ok(())
    }

    /// Pages through the whole board, since the snapshot only holds the most recent messages.
    async fn worker_posted_since(
        &self,
        board_id: Uuid,
        sender: &str,
        since: DateTime<Utc>,
    ) -> CoreResult<bool> {
        const PAGE: usize = 200;
        let mut cursor = 0;
        loop {
            let page = self
                .0
                .store
                .list_swarm_messages(board_id, cursor, PAGE)
                .await?;
            if posted_since(&page, sender, since) {
                return Ok(true);
            }
            let Some(last) = page.last() else {
                return Ok(false);
            };
            if page.len() < PAGE {
                return Ok(false);
            }
            cursor = last.seq;
        }
    }

    pub(crate) async fn read_board(
        &self,
        thread_id: Uuid,
        since_seq: Option<i64>,
        limit: usize,
        seq: Option<i64>,
    ) -> CoreResult<Vec<SwarmReadMessage>> {
        let board = self.require_board(thread_id).await?;
        let messages = if let Some(seq) = seq {
            if seq <= 0 {
                Vec::new()
            } else {
                self.0
                    .store
                    .list_swarm_messages(board.board_id, seq - 1, 1)
                    .await?
                    .into_iter()
                    .filter(|message| message.seq == seq)
                    .collect()
            }
        } else {
            let since_seq = since_seq.unwrap_or_else(|| {
                board
                    .members
                    .iter()
                    .find(|member| member.thread_id == thread_id)
                    .map_or(0, |member| member.last_read_seq)
            });
            self.0
                .store
                .list_swarm_messages(board.board_id, since_seq.max(0), limit.min(200))
                .await?
        };
        Ok(messages
            .into_iter()
            .map(|message| read_message(message, seq.is_none()))
            .collect())
    }

    pub(crate) async fn kill_worker(
        &self,
        caller_id: Uuid,
        name: &str,
        from_agent: bool,
    ) -> CoreResult<SwarmBoardSnapshot> {
        let done = {
            let mut active = self.0.active.lock().await;
            self.kill_worker_locked(&mut active, caller_id, name, from_agent)
                .await?
        };
        if let Some(mut done) = done {
            let _ = done.wait_for(|completed| *completed).await;
        }
        self.require_board(caller_id).await
    }

    async fn kill_worker_locked(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        caller_id: Uuid,
        name: &str,
        from_agent: bool,
    ) -> CoreResult<Option<tokio::sync::watch::Receiver<bool>>> {
        let board = self.require_board(caller_id).await?;
        let member = board
            .members
            .iter()
            .find(|member| member.name == name && member.role == "worker")
            .ok_or_else(|| CoreError::NotFound("swarm worker".into()))?;
        if from_agent {
            let caller = live_member(&board, caller_id)?;
            let actor = self.effective_thread(active, caller_id).await?;
            let target = self.0.store.get_thread(member.thread_id).await?;
            if caller.role != "coordinator" || !can_dispatch(&actor, &target) {
                return Err(CoreError::InvalidArgument(
                    "caller lacks permission to kill this worker".into(),
                ));
            }
        }
        let thread_id = member.thread_id;
        self.swarm_state(thread_id, "killed").await?;
        let done = self.interrupt_locked(active, thread_id).await?;
        self.cleanup_killed_worktree(&board, thread_id, done.clone())
            .await;
        Ok(done)
    }

    /// Best-effort removal of a killed worker's own worktree, deferred until its run settles.
    /// Shared-isolation workers reuse the coordinator's worktree path, so they are skipped.
    async fn cleanup_killed_worktree(
        &self,
        board: &SwarmBoardSnapshot,
        thread_id: Uuid,
        done: Option<watch::Receiver<bool>>,
    ) {
        let worker = match self.0.store.get_thread(thread_id).await {
            Ok(thread) => thread,
            Err(error) => {
                tracing::warn!(%error, %thread_id, "Could not load killed worker for worktree cleanup");
                return;
            }
        };
        let coordinator = match self.0.store.get_thread(board.coordinator_thread_id).await {
            Ok(thread) => thread,
            Err(error) => {
                tracing::warn!(%error, "Could not load coordinator for worktree cleanup");
                return;
            }
        };
        let Some(path) = worker
            .worktree_path
            .filter(|path| coordinator.worktree_path.as_ref() != Some(path))
        else {
            return;
        };
        let coordinator = self.clone();
        let project_id = worker.project_id;
        let board_id = board.board_id;
        tokio::spawn(async move {
            if let Some(mut done) = done {
                let _ = done.wait_for(|completed| *completed).await;
            }
            let note = coordinator.remove_worker_worktree(project_id, &path).await;
            if let Some(text) = note {
                let posted = coordinator
                    .0
                    .store
                    .append_swarm_message(board_id, "system".into(), text, Vec::new())
                    .await;
                if posted.is_ok()
                    && let Ok(board) = coordinator.require_board(thread_id).await
                {
                    coordinator.emit_board(&board);
                }
            }
        });
    }

    /// Returns a board note when the worktree was snapshotted or had to be kept.
    /// `remove_worktree` saves uncommitted work under `refs/t3/snapshots/` first.
    async fn remove_worker_worktree(&self, project_id: Uuid, path: &Path) -> Option<String> {
        let workspace = match self.0.store.get_project(project_id).await {
            Ok(project) => project.workspace_root,
            Err(error) => {
                tracing::warn!(%error, %project_id, "Could not load project for worktree cleanup");
                return None;
            }
        };
        match self.0.git.remove_worktree(&workspace, path).await {
            Ok(snapshot) => snapshot.map(|snapshot| {
                format!("Killed worker's uncommitted changes were saved as {snapshot}")
            }),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "Kept killed worker worktree");
                Some(format!(
                    "Kept killed worker worktree {}: {error}",
                    path.display()
                ))
            }
        }
    }

    pub(crate) async fn apply_swarm_actions(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        thread_id: Uuid,
        actions: Vec<Action>,
    ) -> CoreResult<()> {
        // These actions run after the coordinator's turn ended, so it cannot see
        // an error. Apply the rest and report failures on the board instead.
        let mut failures = String::new();
        for action in actions {
            let (label, result) = match action {
                Action::Add(spec) => (
                    format!("swarm_add_worker {}", spec.name),
                    self.add_worker_locked(active, thread_id, spec)
                        .await
                        .map(drop),
                ),
                Action::Post { text } => {
                    let depth = wake_depth(active, thread_id);
                    (
                        "swarm_post_message".to_owned(),
                        self.post_locked(active, thread_id, None, text, depth)
                            .await
                            .map(drop),
                    )
                }
                Action::Kill { name } => (
                    format!("swarm_kill_worker {name}"),
                    self.kill_worker_locked(active, thread_id, &name, true)
                        .await
                        .map(drop),
                ),
            };
            if let Err(error) = result {
                let error: String = error.to_string().chars().take(ENDED_ERROR_CHARS).collect();
                let _ = writeln!(failures, "- {label}: {error}");
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        let text = format!(
            "@coordinator Swarm action(s) from your last turn failed:\n{failures}Fix the action and emit it again."
        );
        let depth = wake_depth(active, thread_id);
        self.post_locked(active, thread_id, Some("system"), text, depth)
            .await
            .map(drop)
    }
}

fn queued_message(prompt: String) -> Message {
    Message {
        prompt,
        behavior: Some(FollowUpBehavior::Queue),
        ..Default::default()
    }
}

fn read_message(message: SwarmMessage, truncate: bool) -> SwarmReadMessage {
    let truncated = truncate && message.text.chars().count() > READ_MESSAGE_MAX_CHARS;
    let text = if truncated {
        message.text.chars().take(READ_MESSAGE_MAX_CHARS).collect()
    } else {
        message.text
    };
    SwarmReadMessage {
        seq: message.seq,
        board_id: message.board_id,
        sender: message.sender,
        text,
        mentions: message.mentions,
        created_at: message.created_at,
        truncated,
    }
}

fn live_member(board: &SwarmBoardSnapshot, thread_id: Uuid) -> CoreResult<&SwarmMember> {
    let member = board
        .members
        .iter()
        .find(|member| member.thread_id == thread_id)
        .ok_or_else(|| CoreError::NotFound("swarm member".into()))?;
    if member.state == "killed" {
        return Err(CoreError::InvalidArgument("swarm worker is killed".into()));
    }
    Ok(member)
}

fn mentioned_members<'a>(
    board: &'a SwarmBoardSnapshot,
    sender: &str,
    mentions: &[String],
) -> Vec<&'a SwarmMember> {
    board
        .members
        .iter()
        .filter(|member| {
            member.state != "killed"
                && member.name != sender
                && mentions
                    .iter()
                    .any(|name| name == "all" || name == &member.name)
        })
        .collect()
}

fn held_wake_text(seq: i64, targets: &[&SwarmMember]) -> String {
    let names = targets
        .iter()
        .map(|member| member.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[system] wake depth limit {MAX_WAKE_DEPTH} reached at post #{seq}; not woken: {names}. A user message resumes the swarm."
    )
}

fn wake_depth(active: &HashMap<Uuid, ActiveRun>, id: Uuid) -> u8 {
    active.get(&id).map_or(0, |run| run.wake_depth)
}

fn live_worker_count(board: &SwarmBoardSnapshot) -> usize {
    board
        .members
        .iter()
        .filter(|member| member.role != "coordinator" && member.state != "killed")
        .count()
}

fn posted_since(messages: &[SwarmMessage], sender: &str, since: DateTime<Utc>) -> bool {
    messages
        .iter()
        .any(|message| message.sender == sender && message.created_at >= since)
}

/// The notice names the worker without an `@` so that posting it does not wake the ended worker.
fn silent_end_text(member: &SwarmMember, status: RunStatus, error: Option<&str>) -> String {
    let name = &member.name;
    let error = error.map(str::trim).filter(|error| !error.is_empty());
    if matches!(status, RunStatus::Failed | RunStatus::RateLimited)
        && let Some(error) = error
    {
        let delegated = format_delegated_error(member.provider, &member.model, error);
        let delegated: String = delegated.chars().take(ENDED_ERROR_CHARS).collect();
        format!(
            "{}\n@coordinator [system] worker {name} ended ({status:?}) without posting",
            delegated.replace('@', "(at)")
        )
    } else {
        format!("@coordinator [system] worker {name} ended ({status:?}) without posting")
    }
}

fn can_dispatch(caller: &Thread, target: &Thread) -> bool {
    !caller.archived
        && !target.archived
        && target.runtime_mode <= caller.runtime_mode
        && (caller.interaction_mode != InteractionMode::Plan
            || target.interaction_mode == InteractionMode::Plan)
}

fn worker_model(default: &ModelSelection, spec: &SwarmWorkerSpec) -> ModelSelection {
    let provider = spec.provider.unwrap_or(default.provider);
    let model = spec.model.clone().unwrap_or_else(|| {
        if provider == default.provider {
            default.model.clone()
        } else {
            ProviderRegistry::default_models(provider).remove(0).id
        }
    });
    let inherit = provider == default.provider && model == default.model;
    ModelSelection {
        provider,
        model,
        effort: spec
            .effort
            .clone()
            .or_else(|| inherit.then(|| default.effort.clone()).flatten()),
        agent: inherit.then(|| default.agent.clone()).flatten(),
    }
}

fn validate_text(text: &str) -> CoreResult<()> {
    if text.trim().is_empty() || text.len() > MAX_MESSAGE_BYTES {
        return Err(CoreError::InvalidArgument(format!(
            "swarm message must contain 1 to {MAX_MESSAGE_BYTES} bytes; write the details to a file and post a summary with its path"
        )));
    }
    Ok(())
}

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 24
        && (name.as_bytes()[0].is_ascii_lowercase() || name.as_bytes()[0].is_ascii_digit())
        && name.bytes().all(name_byte)
}

fn mentions_in(text: &str) -> Vec<String> {
    let mut mentions = Vec::new();
    for (at, _) in text.match_indices('@') {
        if text[..at]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '@'))
        {
            continue;
        }
        let tail = &text[at + 1..];
        let end = tail.bytes().take_while(|&byte| name_byte(byte)).count();
        let name = &tail[..end];
        if valid_name(name)
            && !tail[end..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
            && !mentions.iter().any(|seen| seen == name)
        {
            mentions.push(name.to_owned());
        }
    }
    mentions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_user_prompt_prefix_and_uses_stable_board_marker() {
        let board_id = Uuid::new_v4();
        let thread_id = Uuid::new_v4();
        let board = SwarmBoardSnapshot {
            board_id,
            coordinator_thread_id: thread_id,
            title: "test".into(),
            members: vec![SwarmMember {
                board_id,
                name: "coordinator".into(),
                thread_id,
                provider: ProviderId::Codex,
                model: "test".into(),
                effort: None,
                state: "working".into(),
                role: "coordinator".into(),
                last_read_seq: 0,
                created_at: Utc::now(),
            }],
            messages: Vec::new(),
            head_seq: 0,
        };
        let mut prompt = "cached user prompt".to_owned();
        let turn = SwarmTurn::prepare(&board, thread_id, ProviderId::Codex, &mut prompt).unwrap();
        let marker = format!("T3_SWARM_ACTION {board_id}");
        assert!(prompt.starts_with("cached user prompt\n\nSwarm board"));
        assert!(prompt.contains(&marker));
        assert_eq!(
            turn.actions(&format!(
                "{marker} {{\"tool\":\"swarm_post_message\",\"text\":\"done\"}}"
            ))
            .len(),
            1
        );
        let mut next_prompt = "another prompt".to_owned();
        let next_turn =
            SwarmTurn::prepare(&board, thread_id, ProviderId::Codex, &mut next_prompt).unwrap();
        assert_eq!(turn.marker, next_turn.marker);
    }

    #[test]
    fn prompts_state_the_brief_and_report_contract() {
        let coordinator = member("coordinator", "coordinator", "working");
        let worker = member("coder", "worker", "working");
        let board = SwarmBoardSnapshot {
            board_id: Uuid::nil(),
            coordinator_thread_id: coordinator.thread_id,
            title: "test".into(),
            members: vec![coordinator.clone(), worker.clone()],
            messages: Vec::new(),
            head_seq: 0,
        };
        let prompt_for = |thread_id| {
            let mut prompt = String::new();
            SwarmTurn::prepare(&board, thread_id, ProviderId::Codex, &mut prompt).unwrap();
            prompt
        };
        let lead = prompt_for(coordinator.thread_id);
        for field in [
            "objective",
            "output",
            "boundaries",
            "effort budget",
            "stop condition",
        ] {
            assert!(lead.contains(field), "missing {field}: {lead}");
        }
        let report = prompt_for(worker.thread_id);
        for field in ["result:", "verified:", "blockers:", "files:"] {
            assert!(report.contains(field), "missing {field}: {report}");
        }
        assert!(report.contains("one post"), "{report}");
        assert!(report.contains(&MAX_MESSAGE_BYTES.to_string()), "{report}");
        // Briefs and posts share the limit, and Japanese text reaches it at a third of the count.
        for prompt in [&lead, &report] {
            assert!(prompt.contains("4096 bytes"), "{prompt}");
            assert!(prompt.contains("1300 Japanese characters"), "{prompt}");
        }
    }

    #[test]
    fn killed_history_does_not_change_live_only_prompt() {
        let board_id = Uuid::new_v4();
        let coordinator_id = Uuid::new_v4();
        let mut live = member("coordinator", "coordinator", "working");
        live.thread_id = coordinator_id;
        let reviewer = member("reviewer", "worker", "working");
        let old_history = (0..64)
            .map(|i| member(&format!("old{i}"), "worker", "killed"))
            .collect::<Vec<_>>();
        let short_board = SwarmBoardSnapshot {
            board_id,
            coordinator_thread_id: coordinator_id,
            title: "long lived board".into(),
            members: vec![live.clone(), reviewer.clone()],
            messages: vec![message(1, "old0", Utc::now())],
            head_seq: 1,
        };
        let mut long_board = short_board.clone();
        long_board.members.extend(old_history);
        long_board
            .messages
            .extend((2..102).map(|seq| message(seq, "old0", Utc::now())));
        long_board.head_seq = 101;

        let mut short_prompt = "same user request".to_owned();
        let mut long_prompt = short_prompt.clone();
        SwarmTurn::prepare(
            &short_board,
            coordinator_id,
            ProviderId::Codex,
            &mut short_prompt,
        )
        .unwrap();
        SwarmTurn::prepare(
            &long_board,
            coordinator_id,
            ProviderId::Codex,
            &mut long_prompt,
        )
        .unwrap();

        assert_eq!(short_prompt.as_bytes(), long_prompt.as_bytes());
        assert!(long_prompt.contains("coordinator (working), reviewer (working)"));
        assert!(!long_prompt.contains("old0 (killed)"));
        // The explicit status board remains complete even though prompt roster context is live-only.
        assert_eq!(long_board.members.len(), 66);
        assert_eq!(long_board.messages.len(), 101);
        assert_eq!(long_board.head_seq, 101);
    }

    fn member(name: &str, role: &str, state: &str) -> SwarmMember {
        SwarmMember {
            board_id: Uuid::nil(),
            name: name.into(),
            thread_id: Uuid::new_v4(),
            provider: ProviderId::Codex,
            model: "test".into(),
            effort: None,
            state: state.into(),
            role: role.into(),
            last_read_seq: 0,
            created_at: Utc::now(),
        }
    }

    fn message(seq: i64, sender: &str, created_at: DateTime<Utc>) -> SwarmMessage {
        SwarmMessage {
            seq,
            board_id: Uuid::nil(),
            sender: sender.into(),
            text: "post".into(),
            mentions: Vec::new(),
            created_at,
        }
    }

    #[test]
    fn active_worker_limit_ignores_coordinator_and_killed_members() {
        let mut members = vec![member("coordinator", "coordinator", "working")];
        members
            .extend((0..MAX_ACTIVE_WORKERS).map(|i| member(&format!("w{i}"), "worker", "working")));
        members.push(member("old", "worker", "killed"));
        let board = SwarmBoardSnapshot {
            board_id: Uuid::nil(),
            coordinator_thread_id: Uuid::nil(),
            title: "test".into(),
            members,
            messages: Vec::new(),
            head_seq: 0,
        };
        assert_eq!(live_worker_count(&board), MAX_ACTIVE_WORKERS);
    }

    #[test]
    fn worker_post_must_match_sender_and_run_start() {
        let start = Utc::now();
        let before = start - chrono::Duration::seconds(5);
        let after = start + chrono::Duration::seconds(5);
        let messages = [message(1, "reviewer", after), message(2, "coder", before)];
        assert!(!posted_since(&messages, "coder", start));
        assert!(posted_since(&messages, "reviewer", start));
        assert!(posted_since(&messages, "reviewer", after));
        assert!(!posted_since(
            &messages,
            "reviewer",
            after + chrono::Duration::seconds(1)
        ));
    }

    #[test]
    fn silent_end_notice_does_not_mention_the_ended_worker() {
        let member = member("coder", "worker", "working");
        let text = silent_end_text(&member, RunStatus::Failed, Some("boom @coder"));
        assert_eq!(
            text,
            "provider_error: provider=codex model=test reset_at=unknown; re-run on another provider. boom (at)coder\n@coordinator [system] worker coder ended (Failed) without posting"
        );
        assert_eq!(mentions_in(&text), vec!["coordinator".to_owned()]);
        assert_eq!(
            silent_end_text(&member, RunStatus::Completed, None),
            "@coordinator [system] worker coder ended (Completed) without posting"
        );
        assert!(
            validate_text(&silent_end_text(
                &member,
                RunStatus::Failed,
                Some(&"x".repeat(10_000))
            ))
            .is_ok()
        );
    }
}
