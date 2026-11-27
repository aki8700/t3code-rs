use crate::{ServerState, VERSION, coordinator::RunCoordinator};
use axum::{
    Extension,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashSet, VecDeque};
use t3_core::{AuthIdentity, TELEMETRY_ENABLED, WsClientMessage, WsServerMessage};
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

pub(crate) const CONTROL_LANE_CAP: usize = 256;
pub(crate) const DELTA_LANE_CAP: usize = 128;

pub(crate) fn serialize_with_seq(
    message: &WsServerMessage,
    seq: u64,
) -> serde_json::Result<String> {
    let mut value = serde_json::to_value(message)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("seq".to_owned(), serde_json::Value::from(seq));
    }
    serde_json::to_string(&value)
}

/// Three-lane bounded per-client outbound queue:
/// 1. Control lane (never dropped by delta bursts; drained first).
/// 2. Latest-state lane (`RunUsageUpdated` coalesced per thread_id).
/// 3. Droppable delta lane (`Delta`, `ShellOutput`, `TurnItemAdded`; drops oldest & schedules `Resync`).
#[derive(Debug, Default)]
pub(crate) struct ClientLaneQueue {
    control: VecDeque<WsServerMessage>,
    latest: Vec<(Uuid, WsServerMessage)>,
    delta: VecDeque<WsServerMessage>,
    events_lost: bool,
    pending_resync: bool,
}

impl ClientLaneQueue {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn events_lost(&self) -> bool {
        self.events_lost
    }

    pub(crate) fn push(&mut self, msg: WsServerMessage) {
        match &msg {
            WsServerMessage::RunUsageUpdated { thread_id, .. } => {
                let thread_id = *thread_id;
                if let Some((_, slot)) = self
                    .latest
                    .iter_mut()
                    .find(|(existing_id, _)| *existing_id == thread_id)
                {
                    *slot = msg;
                } else {
                    self.latest.push((thread_id, msg));
                }
            }
            WsServerMessage::Delta { .. }
            | WsServerMessage::ShellOutput { .. }
            | WsServerMessage::TurnItemAdded { .. } => {
                if self.delta.len() >= DELTA_LANE_CAP {
                    self.delta.pop_front();
                    self.events_lost = true;
                    self.pending_resync = true;
                }
                self.delta.push_back(msg);
            }
            WsServerMessage::Hello { .. }
            | WsServerMessage::PendingRequest { .. }
            | WsServerMessage::PendingResolved { .. }
            | WsServerMessage::RunStarted { .. }
            | WsServerMessage::RunStatusChanged { .. }
            | WsServerMessage::ThreadStatusChanged { .. }
            | WsServerMessage::Resync { .. }
            | WsServerMessage::Error { .. }
            | WsServerMessage::SubagentUpdated { .. }
            | WsServerMessage::SwarmUpdated { .. }
            | WsServerMessage::ShellFinished { .. } => {
                if self.control.len() >= CONTROL_LANE_CAP {
                    self.events_lost = true;
                    if !self
                        .control
                        .iter()
                        .any(|m| matches!(m, WsServerMessage::Resync { thread_id: None }))
                    {
                        self.control.pop_front();
                        self.control
                            .push_back(WsServerMessage::Resync { thread_id: None });
                    }
                    return;
                }
                self.control.push_back(msg);
            }
        }
    }

    pub(crate) fn pop(&mut self) -> Option<WsServerMessage> {
        if let Some(msg) = self.control.pop_front() {
            return Some(msg);
        }
        if self.pending_resync {
            self.pending_resync = false;
            return Some(WsServerMessage::Resync { thread_id: None });
        }
        if !self.latest.is_empty() {
            let (_, msg) = self.latest.remove(0);
            return Some(msg);
        }
        self.delta.pop_front()
    }
}

pub(crate) async fn upgrade(
    State(state): State<ServerState>,
    Extension(identity): Extension<AuthIdentity>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| run(socket, state.coordinator, identity))
}

async fn run(mut socket: WebSocket, coordinator: RunCoordinator, identity: AuthIdentity) {
    let mut events = BroadcastStream::new(coordinator.subscribe());
    let mut subscriptions = HashSet::new();
    let mut queue = ClientLaneQueue::new();
    let mut seq: u64 = 0;
    if !send(
        &mut socket,
        &WsServerMessage::Hello {
            server_version: VERSION.into(),
            telemetry_enabled: TELEMETRY_ENABLED,
            auth_method: identity.method,
        },
        &mut seq,
    )
    .await
    {
        return;
    }
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str(&text) {
                            Ok(WsClientMessage::Subscribe { thread_id }) if subscriptions.insert(thread_id) => {
                                for request in coordinator.pending(thread_id).await {
                                    queue.push(WsServerMessage::PendingRequest { request });
                                }
                            }
                            Ok(WsClientMessage::Unsubscribe { thread_id }) => {
                                subscriptions.remove(&thread_id);
                            }
                            Ok(WsClientMessage::Ping) => {
                                if socket.send(Message::Text(r#"{"type":"pong"}"#.into())).await.is_err() { return; }
                            }
                            Ok(_) => {}
                            Err(_) => {
                                queue.push(WsServerMessage::Error { message: "invalid WebSocket message".into() });
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    _ => {}
                }
            }
            event = events.next() => {
                match event {
                    Some(Ok(event)) => {
                        if is_visible(&event, &subscriptions) {
                            queue.push(event);
                        }
                    }
                    Some(Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(
                        skipped,
                    ))) => {
                        tracing::debug!(skipped, "WebSocket client lagged behind broadcast channel");
                        queue.push(WsServerMessage::Resync { thread_id: None });
                    }
                    None => {
                        let _ = socket.close().await;
                        return;
                    }
                }
            }
        }
        while let Some(msg) = queue.pop() {
            if !send(&mut socket, &msg, &mut seq).await {
                return;
            }
        }
    }
}

fn is_visible(event: &WsServerMessage, subscriptions: &HashSet<Uuid>) -> bool {
    match event {
        WsServerMessage::Delta { thread_id, .. }
        | WsServerMessage::TurnItemAdded { thread_id, .. }
        | WsServerMessage::RunUsageUpdated { thread_id, .. } => subscriptions.contains(thread_id),
        WsServerMessage::ShellOutput { thread_id, .. }
        | WsServerMessage::ShellFinished { thread_id, .. } => {
            thread_id.is_none_or(|tid| subscriptions.contains(&tid))
        }
        WsServerMessage::SwarmUpdated { board, .. } => board
            .members
            .iter()
            .any(|member| subscriptions.contains(&member.thread_id)),
        _ => true,
    }
}

async fn send(socket: &mut WebSocket, message: &WsServerMessage, seq: &mut u64) -> bool {
    *seq = seq.saturating_add(1);
    match serialize_with_seq(message, *seq) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod lane_queue_tests {
    use super::*;

    #[test]
    fn delta_overflow_drops_oldest_and_never_drops_or_delays_control() {
        let mut queue = ClientLaneQueue::new();
        let thread_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        for i in 0..200 {
            queue.push(WsServerMessage::Delta {
                thread_id,
                run_id,
                channel: "assistant".into(),
                text: format!("delta-{i}"),
            });
        }
        queue.push(WsServerMessage::Error {
            message: "ctrl-err".to_owned(),
        });
        assert!(queue.events_lost());

        let first = queue.pop().expect("control must drain before delta");
        assert!(matches!(first, WsServerMessage::Error { .. }));

        let second = queue.pop().expect("resync must follow control on overflow");
        assert!(matches!(
            second,
            WsServerMessage::Resync { thread_id: None }
        ));

        let mut remaining_deltas = Vec::new();
        while let Some(msg) = queue.pop() {
            if let WsServerMessage::Delta { text, .. } = msg {
                remaining_deltas.push(text);
            }
        }
        assert_eq!(remaining_deltas.len(), DELTA_LANE_CAP);
        assert_eq!(
            remaining_deltas.first().map(String::as_str),
            Some("delta-72")
        );
        assert_eq!(
            remaining_deltas.last().map(String::as_str),
            Some("delta-199")
        );
    }

    #[test]
    fn run_usage_updated_coalesces_to_latest_per_thread() {
        let mut queue = ClientLaneQueue::new();
        let thread_a = Uuid::new_v4();
        let thread_b = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        queue.push(WsServerMessage::RunUsageUpdated {
            thread_id: thread_a,
            run_id,
            context_tokens: 100,
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        queue.push(WsServerMessage::RunUsageUpdated {
            thread_id: thread_b,
            run_id,
            context_tokens: 200,
            input_tokens: 200,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        queue.push(WsServerMessage::RunUsageUpdated {
            thread_id: thread_a,
            run_id,
            context_tokens: 999,
            input_tokens: 999,
            output_tokens: 88,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });

        let msg1 = queue.pop().expect("coalesced thread_a usage");
        assert!(matches!(
            msg1,
            WsServerMessage::RunUsageUpdated {
                thread_id,
                input_tokens: 999,
                output_tokens: 88,
                ..
            } if thread_id == thread_a
        ));
        let msg2 = queue.pop().expect("coalesced thread_b usage");
        assert!(matches!(
            msg2,
            WsServerMessage::RunUsageUpdated {
                thread_id,
                input_tokens: 200,
                ..
            } if thread_id == thread_b
        ));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn serialize_with_seq_injects_seq_field_into_existing_json() {
        let msg = WsServerMessage::Resync { thread_id: None };
        let baseline: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        let with_seq: serde_json::Value =
            serde_json::from_str(&serialize_with_seq(&msg, 42).unwrap()).unwrap();
        assert_eq!(with_seq["type"], baseline["type"]);
        assert_eq!(with_seq["seq"], serde_json::json!(42));
    }
}
