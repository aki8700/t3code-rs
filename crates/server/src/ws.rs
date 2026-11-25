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
use std::collections::HashSet;
use t3_core::{AuthIdentity, TELEMETRY_ENABLED, WsClientMessage, WsServerMessage};
use tokio_stream::wrappers::BroadcastStream;

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
    if !send(
        &mut socket,
        &WsServerMessage::Hello {
            server_version: VERSION.into(),
            telemetry_enabled: TELEMETRY_ENABLED,
            auth_method: identity.method,
        },
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
                                    if !send(&mut socket, &WsServerMessage::PendingRequest { request }).await { return; }
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
                                if !send(&mut socket, &WsServerMessage::Error { message: "invalid WebSocket message".into() }).await { return; }
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
                        let visible = match &event {
                            WsServerMessage::Delta { thread_id, .. }
                            | WsServerMessage::TurnItemAdded { thread_id, .. }
                            | WsServerMessage::RunUsageUpdated { thread_id, .. } => {
                                subscriptions.contains(thread_id)
                            }
                            WsServerMessage::SwarmUpdated { board, .. } => board.members.iter().any(|member| subscriptions.contains(&member.thread_id)),
                            _ => true,
                        };
                        if visible && !send(&mut socket, &event).await { return; }
                    }
                    Some(Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(
                        skipped,
                    ))) => {
                        tracing::debug!(skipped, "WebSocket client lagged behind broadcast channel");
                    }
                    None => {
                        let _ = socket.close().await;
                        return;
                    }
                }
            }
        }
    }
}

async fn send(socket: &mut WebSocket, message: &WsServerMessage) -> bool {
    match serde_json::to_string(message) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}
