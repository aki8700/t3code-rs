use crate::{ServerState, VERSION, coordinator::RunCoordinator};
use axum::{
    Extension,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::Response,
};
use futures_util::{Sink, SinkExt, StreamExt};
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

    /// When the control lane is full, send its oldest message before accepting more input.
    #[must_use = "send the returned control message before accepting more input"]
    pub(crate) fn push(&mut self, msg: WsServerMessage) -> Option<WsServerMessage> {
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
                let ready = if self.control.len() == CONTROL_LANE_CAP {
                    self.control.pop_front()
                } else {
                    None
                };
                self.control.push_back(msg);
                return ready;
            }
        }
        None
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

    async fn send_batch(
        &mut self,
        socket: &mut (impl Sink<Message> + Unpin),
        messages: impl IntoIterator<Item = WsServerMessage>,
        seq: &mut u64,
    ) -> bool {
        for msg in messages {
            if let Some(ready) = self.push(msg)
                && !send(socket, &ready, seq).await
            {
                return false;
            }
        }
        while let Some(msg) = self.pop() {
            if !send(socket, &msg, seq).await {
                return false;
            }
        }
        true
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
        let mut pending = Vec::new();
        let message = tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str(&text) {
                            Ok(WsClientMessage::Subscribe { thread_id }) if subscriptions.insert(thread_id) => {
                                pending = coordinator.pending(thread_id).await;
                                None
                            }
                            Ok(WsClientMessage::Unsubscribe { thread_id }) => {
                                subscriptions.remove(&thread_id);
                                None
                            }
                            Ok(WsClientMessage::Ping) => {
                                if socket.send(Message::Text(r#"{"type":"pong"}"#.into())).await.is_err() { return; }
                                None
                            }
                            Ok(_) => None,
                            Err(_) => Some(WsServerMessage::Error { message: "invalid WebSocket message".into() }),
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                    _ => None,
                }
            }
            event = events.next() => {
                match event {
                    Some(Ok(event)) => is_visible(&event, &subscriptions).then_some(event),
                    Some(Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(
                        skipped,
                    ))) => {
                        tracing::debug!(skipped, "WebSocket client lagged behind broadcast channel");
                        Some(WsServerMessage::Resync { thread_id: None })
                    }
                    None => {
                        let _ = socket.close().await;
                        return;
                    }
                }
            }
        };
        let messages = pending
            .into_iter()
            .map(|request| WsServerMessage::PendingRequest { request })
            .chain(message);
        if !queue.send_batch(&mut socket, messages, &mut seq).await {
            return;
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

async fn send(
    socket: &mut (impl Sink<Message> + Unpin),
    message: &WsServerMessage,
    seq: &mut u64,
) -> bool {
    *seq = seq.saturating_add(1);
    match serialize_with_seq(message, *seq) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod lane_queue_tests {
    use super::*;

    #[tokio::test]
    async fn pending_controls_survive_a_burst_larger_than_the_control_lane() {
        let mut queue = ClientLaneQueue::new();
        let mut wire = Vec::<Message>::new();
        let mut seq = 0;
        let thread_id = Uuid::new_v4();
        let controls = (1..=CONTROL_LANE_CAP + 2).map(|n| WsServerMessage::PendingResolved {
            thread_id,
            request_id: Uuid::from_u128(n as u128),
        });
        assert!(queue.send_batch(&mut wire, controls, &mut seq).await);
        assert!(!queue.events_lost());
        assert_eq!(wire.len(), CONTROL_LANE_CAP + 2);
        for (index, frame) in wire.iter().enumerate() {
            let data: serde_json::Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            let n = index + 1;
            assert_eq!(data["type"], "pending_resolved");
            assert_eq!(data["request_id"], Uuid::from_u128(n as u128).to_string());
            assert_eq!(data["seq"], n);
        }
    }

    #[tokio::test]
    async fn mixed_bursts_resync_once_each_and_sequence_only_the_delivered_frames() {
        use t3_core::{TurnItem, TurnItemKind};
        let mut queue = ClientLaneQueue::new();
        let thread_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let deltas: Vec<_> = (0..DELTA_LANE_CAP + 3)
            .map(|i| match i % 3 {
                0 => WsServerMessage::Delta {
                    thread_id,
                    run_id,
                    channel: "assistant".into(),
                    text: i.to_string(),
                },
                1 => WsServerMessage::ShellOutput {
                    exec_id: run_id,
                    thread_id: Some(thread_id),
                    stream: "stdout".into(),
                    chunk: i.to_string(),
                },
                _ => WsServerMessage::TurnItemAdded {
                    thread_id,
                    item: TurnItem {
                        id: Uuid::new_v4(),
                        thread_id,
                        run_id,
                        ordinal: i as i64,
                        kind: TurnItemKind::AssistantMessage,
                        summary: None,
                        payload: serde_json::Value::Null,
                        raw_bytes: 0,
                        compressed_bytes: None,
                        created_at: chrono::Utc::now(),
                    },
                },
            })
            .collect();
        let control = WsServerMessage::PendingResolved {
            thread_id,
            request_id: Uuid::new_v4(),
        };
        let usage = |input_tokens| WsServerMessage::RunUsageUpdated {
            thread_id,
            run_id,
            input_tokens,
            context_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let mut wire = Vec::<Message>::new();
        let mut seq = 0;
        for _ in 0..2 {
            let batch = [usage(10)]
                .into_iter()
                .chain(deltas.iter().cloned())
                .chain([control.clone(), usage(20)]);
            assert!(queue.send_batch(&mut wire, batch, &mut seq).await);
            assert!(queue.events_lost());
        }

        // The first three events span all droppable variants. The rest retain FIFO order.
        let expected = [
            control,
            WsServerMessage::Resync { thread_id: None },
            usage(20),
        ];
        let one_burst = expected.iter().chain(&deltas[3..]);
        assert_eq!(wire.len(), 2 * (3 + DELTA_LANE_CAP));
        for (index, (frame, msg)) in wire.iter().zip(one_burst.cycle()).enumerate() {
            let mut data: serde_json::Value =
                serde_json::from_str(frame.to_text().unwrap()).unwrap();
            assert_eq!(
                data.as_object_mut().unwrap().remove("seq"),
                Some((index + 1).into())
            );
            assert_eq!(data, serde_json::to_value(msg).unwrap());
        }
    }

    #[test]
    fn filling_the_delta_lane_exactly_preserves_every_event_without_a_resync() {
        let mut queue = ClientLaneQueue::new();
        for i in 0..DELTA_LANE_CAP {
            assert!(
                queue
                    .push(WsServerMessage::Delta {
                        thread_id: Uuid::nil(),
                        run_id: Uuid::nil(),
                        channel: "assistant".into(),
                        text: i.to_string(),
                    })
                    .is_none()
            );
        }
        assert!(!queue.events_lost());
        for i in 0..DELTA_LANE_CAP {
            assert!(
                matches!(queue.pop(), Some(WsServerMessage::Delta { text, .. }) if text == i.to_string())
            );
        }
        assert!(queue.pop().is_none());
    }

    #[test]
    fn run_usage_updated_coalesces_to_latest_per_thread() {
        let mut queue = ClientLaneQueue::new();
        let thread_a = Uuid::new_v4();
        let thread_b = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        assert!(
            queue
                .push(WsServerMessage::RunUsageUpdated {
                    thread_id: thread_a,
                    run_id,
                    context_tokens: 100,
                    input_tokens: 100,
                    output_tokens: 10,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                })
                .is_none()
        );
        assert!(
            queue
                .push(WsServerMessage::RunUsageUpdated {
                    thread_id: thread_b,
                    run_id,
                    context_tokens: 200,
                    input_tokens: 200,
                    output_tokens: 20,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                })
                .is_none()
        );
        assert!(
            queue
                .push(WsServerMessage::RunUsageUpdated {
                    thread_id: thread_a,
                    run_id,
                    context_tokens: 999,
                    input_tokens: 999,
                    output_tokens: 88,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                })
                .is_none()
        );

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
