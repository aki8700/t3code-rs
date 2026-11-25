use crate::{
    coordinator::{ActiveRun, Message, RunCoordinator},
    threads::CreateThread,
};
use chrono::Utc;
use serde::Deserialize;
use std::{collections::HashMap, fmt::Write};
use t3_core::*;
use t3_providers::ProviderRegistry;
use uuid::Uuid;

const MAX_WAKE_DEPTH: u8 = 8;
const MAX_MESSAGE_BYTES: usize = 4096;
const WAKE_BATCH: usize = 16;

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
        let instructions = if coordinator {
            "Coordinate work using swarm_status, swarm_add_worker, swarm_post_message, swarm_read_board and swarm_kill_worker. Mentions wake the named member. After assigning work, end your turn and wait for results addressed to @coordinator."
        } else {
            "Work on the assigned brief. When finished, call swarm_post_message with text starting with @coordinator [done] and include your result and any blocker. End your turn after posting. Reply to subsequent updates only when you have new information."
        };
        let marker =
            (provider != ProviderId::Mock).then(|| format!("T3_SWARM_ACTION {}", Uuid::new_v4()));
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
                r#"{"tool":"swarm_post_message","text":"@coordinator [done] Result and any blocker"}"#
            };
            instructions.push_str("When swarm MCP is unavailable or requires approval, return plain action lines at the end of your response. The daemon runs those actions after the turn. Use only the lines needed, with your own arguments, and only one method per action.\n");
            for example in examples.lines() {
                writeln!(instructions, "{marker} {example}").expect("write to String");
            }
            instructions.push_str("Members: ");
            for member in &board.members {
                write!(instructions, "{} ({}), ", member.name, member.state)
                    .expect("write to String");
            }
            instructions.truncate(instructions.len() - 2);
            instructions.push('\n');
        }
        *prompt = format!("{instructions}\n{prompt}");
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
        let mut member = SwarmMember {
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
            self.send_locked(active, worker.clone(), queued_message(prompt), depth + 1)
                .await?;
            member.last_read_seq = post.seq;
            store
                .update_swarm_member_read_seq(worker.id, post.seq)
                .await
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
        let mut active = self.0.active.lock().await;
        self.post_locked(&mut active, caller_id, sender_override, text)
            .await
    }

    async fn post_locked(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        caller_id: Uuid,
        sender_override: Option<&str>,
        text: String,
    ) -> CoreResult<SwarmBoardSnapshot> {
        validate_text(&text)?;
        let board = self.require_board(caller_id).await?;
        let (sender, caller, depth) = if let Some(sender) = sender_override {
            (sender.to_owned(), None, 0)
        } else {
            let member = live_member(&board, caller_id)?;
            (
                member.name.clone(),
                Some(self.effective_thread(active, caller_id).await?),
                wake_depth(active, caller_id),
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
        if depth >= MAX_WAKE_DEPTH || post.mentions.is_empty() {
            return Ok(board);
        }
        for member in &board.members {
            if member.state == "killed"
                || member.name == sender
                || !post
                    .mentions
                    .iter()
                    .any(|name| name == "all" || name == &member.name)
            {
                continue;
            }
            if let Err(error) = self
                .wake_member(active, member, caller.as_ref(), depth + 1)
                .await
            {
                tracing::warn!(%error, thread_id = %member.thread_id, "Could not deliver swarm mention");
            }
        }
        let updated = self.require_board(caller_id).await?;
        self.emit_board(&updated);
        Ok(updated)
    }

    async fn wake_member(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        member: &SwarmMember,
        caller: Option<&Thread>,
        depth: u8,
    ) -> CoreResult<()> {
        let store = &self.0.store;
        let target = store.get_thread(member.thread_id).await?;
        if target.archived || caller.is_some_and(|caller| !can_dispatch(caller, &target)) {
            return Ok(());
        }
        let mut cursor = member.last_read_seq;
        loop {
            let messages = store
                .list_swarm_messages(member.board_id, cursor, WAKE_BATCH)
                .await?;
            let Some(last) = messages.last() else {
                return Ok(());
            };
            let mut prompt = format!("Unread swarm board posts for @{}:\n", member.name);
            for message in &messages {
                writeln!(
                    prompt,
                    "#{} {}: {}",
                    message.seq, message.sender, message.text
                )
                .expect("write to String");
            }
            self.send_locked(active, target.clone(), queued_message(prompt), depth)
                .await?;
            cursor = last.seq;
            store
                .update_swarm_member_read_seq(target.id, cursor)
                .await?;
            if messages.len() < WAKE_BATCH {
                return Ok(());
            }
        }
    }

    pub(crate) async fn read_board(
        &self,
        thread_id: Uuid,
        since: i64,
        limit: usize,
    ) -> CoreResult<Vec<SwarmMessage>> {
        let board = self.require_board(thread_id).await?;
        self.0
            .store
            .list_swarm_messages(board.board_id, since.max(0), limit.min(200))
            .await
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
        self.swarm_state(member.thread_id, "killed").await?;
        self.interrupt_locked(active, member.thread_id).await
    }

    pub(crate) async fn apply_swarm_actions(
        &self,
        active: &mut HashMap<Uuid, ActiveRun>,
        thread_id: Uuid,
        actions: Vec<Action>,
    ) -> CoreResult<()> {
        for action in actions {
            match action {
                Action::Add(spec) => {
                    self.add_worker_locked(active, thread_id, spec).await?;
                }
                Action::Post { text } => {
                    self.post_locked(active, thread_id, None, text).await?;
                }
                Action::Kill { name } => {
                    self.kill_worker_locked(active, thread_id, &name, true)
                        .await?;
                }
            }
        }
        Ok(())
    }
}

fn queued_message(prompt: String) -> Message {
    Message {
        prompt,
        behavior: Some(FollowUpBehavior::Queue),
        ..Default::default()
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

fn wake_depth(active: &HashMap<Uuid, ActiveRun>, id: Uuid) -> u8 {
    active.get(&id).map_or(0, |run| run.wake_depth)
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
            "swarm message must contain 1 to {MAX_MESSAGE_BYTES} bytes"
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
