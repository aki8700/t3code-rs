use crate::catalog;
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_core::{
    protocol::StreamEvent,
    traits::{CoreError, CoreResult, ProviderDriver, TurnRequest},
    types::{ProviderHealth, ProviderId, TurnItemKind},
};
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

#[derive(Default)]
struct MockRunControl {
    rate_limited: bool,
    active: Option<ActiveMockRun>,
}

struct ActiveMockRun {
    steer: mpsc::UnboundedSender<String>,
    interrupt: Arc<Notify>,
}

type MockRuns = Arc<Mutex<HashMap<Uuid, MockRunControl>>>;

/// An in-process provider with deterministic output and test scenarios.
#[derive(Default, Clone)]
pub struct MockDriver {
    runs: MockRuns,
}

impl MockDriver {
    pub fn new() -> Self {
        Self::default()
    }
}

struct MockRunGuard<'a> {
    runs: &'a MockRuns,
    id: Uuid,
}

impl Drop for MockRunGuard<'_> {
    fn drop(&mut self) {
        self.runs.lock().unwrap().remove(&self.id);
    }
}

#[async_trait]
impl ProviderDriver for MockDriver {
    fn provider_id(&self) -> ProviderId {
        ProviderId::Mock
    }

    fn supports_mcp(&self) -> bool {
        true
    }

    async fn health_check(&self) -> ProviderHealth {
        let provider = self.provider_id();
        ProviderHealth {
            provider,
            available: true,
            binary_path: None,
            version: Some("mock".into()),
            models: catalog::default_models(provider),
            error: None,
        }
    }

    async fn run_turn(
        &self,
        req: TurnRequest,
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> CoreResult<()> {
        let (steer, mut steering) = mpsc::unbounded_channel();
        let interrupt = Arc::new(Notify::new());
        let limited = {
            let mut runs = self.runs.lock().unwrap();
            let run = runs.entry(req.run_id).or_default();
            if run.active.is_some() {
                return Err(CoreError::Provider(format!(
                    "run {} is already active",
                    req.run_id
                )));
            }
            let limited = req.prompt.contains("[rate-limit-once]") && !run.rate_limited;
            if limited {
                run.rate_limited = true;
            } else {
                run.active = Some(ActiveMockRun {
                    steer,
                    interrupt: Arc::clone(&interrupt),
                });
            }
            limited
        };

        let emit = |event| {
            let _ = tx.send(event);
        };
        emit(StreamEvent::SessionStarted {
            provider_session_id: req
                .resume_session_id
                .unwrap_or_else(|| req.run_id.to_string()),
        });
        if limited {
            emit(StreamEvent::RateLimited {
                retry_after_secs: 0,
                message: "mock limit".into(),
            });
            return Ok(());
        }
        let _guard = MockRunGuard {
            runs: &self.runs,
            id: req.run_id,
        };

        emit(StreamEvent::ReasoningDelta {
            text: "mock reasoning".into(),
        });
        emit(StreamEvent::Item {
            kind: TurnItemKind::ToolCall,
            summary: Some("mock tool".into()),
            payload: json!({"name": "mock"}),
        });
        emit(StreamEvent::Item {
            kind: TurnItemKind::ToolResult,
            summary: None,
            payload: json!({"output": if req.prompt.contains("[large-output]") {
                "x".repeat(100 * 1024)
            } else {
                "ok".into()
            }}),
        });

        if req.prompt.contains("[slow]") {
            let mut ticks = tokio::time::interval_at(
                tokio::time::Instant::now() + Duration::from_millis(50),
                Duration::from_millis(50),
            );
            for chunk in ["mock", " ", "slow", " ", req.prompt.as_str()] {
                loop {
                    tokio::select! {
                        biased;
                        _ = interrupt.notified() => {
                            emit(StreamEvent::Failed { message: "interrupted".into() });
                            return Ok(());
                        }
                        _ = tx.closed() => return Ok(()),
                        Some(text) = steering.recv() => emit(StreamEvent::AssistantDelta { text }),
                        _ = ticks.tick() => {
                            emit(StreamEvent::AssistantDelta { text: chunk.into() });
                            break;
                        }
                    }
                }
            }
        } else {
            emit(StreamEvent::AssistantDelta { text: req.prompt });
        }

        emit(StreamEvent::Usage {
            input_tokens: 1,
            output_tokens: 1,
        });
        emit(StreamEvent::Completed {
            final_text: Some("done".into()),
        });
        Ok(())
    }

    async fn send_steer(&self, run_id: Uuid, message: String) -> CoreResult<bool> {
        Ok(self
            .runs
            .lock()
            .unwrap()
            .get(&run_id)
            .and_then(|run| run.active.as_ref())
            .is_some_and(|run| run.steer.send(message).is_ok()))
    }

    async fn interrupt(&self, run_id: Uuid) -> CoreResult<()> {
        let mut runs = self.runs.lock().unwrap();
        if let Some(run) = runs.get_mut(&run_id) {
            if let Some(active) = &run.active {
                active.interrupt.notify_one();
            } else {
                runs.remove(&run_id);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{run_turn_with_retry, test_request};
    use tokio::time::timeout;

    async fn collect(prompt: &str) -> Vec<StreamEvent> {
        let mut req = test_request(ProviderId::Mock);
        req.prompt = prompt.into();
        let (tx, mut rx) = mpsc::unbounded_channel();
        MockDriver::new().run_turn(req, tx).await.unwrap();
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[tokio::test]
    async fn normal_mock_turn() {
        let events = collect("regular prompt").await;
        assert!(matches!(events[0], StreamEvent::SessionStarted { .. }));
        assert!(matches!(events[1], StreamEvent::ReasoningDelta { .. }));
        assert!(matches!(
            events[2],
            StreamEvent::Item {
                kind: TurnItemKind::ToolCall,
                ..
            }
        ));
        assert!(matches!(
            events[3],
            StreamEvent::Item {
                kind: TurnItemKind::ToolResult,
                ..
            }
        ));
        assert!(
            matches!(&events[4], StreamEvent::AssistantDelta { text } if text == "regular prompt")
        );
        assert!(matches!(
            events[5],
            StreamEvent::Usage {
                input_tokens: 1,
                output_tokens: 1
            }
        ));
        assert!(matches!(events[6], StreamEvent::Completed { .. }));
    }

    #[tokio::test]
    async fn large_output_has_100_kib_of_repetitive_tool_output() {
        let events = collect("[large-output]").await;
        let output = events
            .iter()
            .find_map(|event| match event {
                StreamEvent::Item {
                    kind: TurnItemKind::ToolResult,
                    payload,
                    ..
                } => payload["output"].as_str(),
                _ => None,
            })
            .unwrap();
        assert_eq!(output.len(), 102_400);
        assert!(output.bytes().all(|byte| byte == b'x'));
    }

    async fn start_slow(
        driver: &MockDriver,
    ) -> (
        Uuid,
        mpsc::UnboundedReceiver<StreamEvent>,
        tokio::task::JoinHandle<CoreResult<()>>,
    ) {
        let mut req = test_request(ProviderId::Mock);
        req.prompt = "[slow]".into();
        let id = req.run_id;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let running = driver.clone();
        let turn = tokio::spawn(async move { running.run_turn(req, tx).await });
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::SessionStarted { .. })
        ));
        (id, rx, turn)
    }

    #[tokio::test]
    async fn slow_mock_streams_steering_inline_and_cleans_up() {
        let driver = MockDriver::new();
        let (id, mut rx, turn) = start_slow(&driver).await;
        assert!(
            driver
                .send_steer(id, "new instruction".into())
                .await
                .unwrap()
        );
        let mut deltas = Vec::new();
        let mut completed = false;
        while let Some(event) = timeout(Duration::from_secs(2), rx.recv()).await.unwrap() {
            match event {
                StreamEvent::AssistantDelta { text } => deltas.push(text),
                StreamEvent::Completed { .. } => completed = true,
                _ => {}
            }
        }
        turn.await.unwrap().unwrap();
        assert!(completed);
        assert_eq!(
            deltas,
            ["new instruction", "mock", " ", "slow", " ", "[slow]"]
        );
        assert!(!driver.send_steer(id, "late".into()).await.unwrap());
    }

    #[tokio::test]
    async fn slow_mock_interrupt_emits_failure_without_completion() {
        let driver = MockDriver::new();
        let (id, mut rx, turn) = start_slow(&driver).await;
        driver.interrupt(id).await.unwrap();
        timeout(Duration::from_secs(2), turn)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            matches!(events.last(), Some(StreamEvent::Failed { message }) if message == "interrupted")
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, StreamEvent::Completed { .. }))
        );
        assert!(!driver.send_steer(id, "late".into()).await.unwrap());
    }

    #[tokio::test]
    async fn rate_limit_once_retries_and_completes() {
        let driver = Arc::new(MockDriver::new());
        let mut req = test_request(ProviderId::Mock);
        req.prompt = "[rate-limit-once]".into();
        let (tx, mut rx) = mpsc::unbounded_channel();
        timeout(
            Duration::from_secs(2),
            run_turn_with_retry(driver, req, tx, true, 1, 0),
        )
        .await
        .unwrap()
        .unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(matches!(
            events[1],
            StreamEvent::RateLimited {
                retry_after_secs: 0,
                ..
            }
        ));
        assert!(matches!(events[2], StreamEvent::SessionStarted { .. }));
        assert!(
            events[3..]
                .iter()
                .all(|event| !matches!(event, StreamEvent::RateLimited { .. }))
        );
        assert!(matches!(events.last(), Some(StreamEvent::Completed { .. })));
    }
}
