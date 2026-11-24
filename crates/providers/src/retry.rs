use std::{sync::Arc, time::Duration};
use t3_core::{
    protocol::StreamEvent,
    traits::{CoreResult, ProviderDriver, TurnRequest},
};
use tokio::sync::mpsc;

/// Stream each attempt and resume the latest provider session after rate limiting.
pub async fn run_turn_with_retry(
    driver: Arc<dyn ProviderDriver>,
    mut req: TurnRequest,
    tx: mpsc::UnboundedSender<StreamEvent>,
    auto_resume: bool,
    max_retries: u32,
    initial_backoff_secs: u64,
) -> CoreResult<()> {
    let mut retries = 0;
    let mut backoff = initial_backoff_secs;
    loop {
        let (attempt_tx, mut attempt_rx) = mpsc::unbounded_channel();
        let turn = driver.run_turn(req.clone(), attempt_tx);
        tokio::pin!(turn);

        let mut retry_after: Option<u64> = None;
        let mut handle_event = |event| {
            match &event {
                StreamEvent::SessionStarted {
                    provider_session_id,
                } => {
                    req.resume_session_id = Some(provider_session_id.clone());
                }
                StreamEvent::RateLimited {
                    retry_after_secs, ..
                } => {
                    retry_after = Some(
                        retry_after.map_or(*retry_after_secs, |secs| secs.max(*retry_after_secs)),
                    );
                }
                _ => {}
            }
            let _ = tx.send(event);
        };

        let result = loop {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                result = &mut turn => break result,
                Some(event) = attempt_rx.recv() => handle_event(event),
            }
        };
        // Preserve events buffered in the same poll in which the driver returned.
        attempt_rx.close();
        while let Ok(event) = attempt_rx.try_recv() {
            handle_event(event);
        }
        result?;
        let Some(retry_after) = retry_after.filter(|_| auto_resume && retries < max_retries) else {
            return Ok(());
        };

        let wait = retry_after.max(backoff);
        if wait != 0 {
            tokio::select! {
                _ = tx.closed() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            }
        }
        retries += 1;
        backoff = backoff.saturating_mul(2).min(initial_backoff_secs.max(120));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_request;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use t3_core::types::{ProviderHealth, ProviderId};
    use uuid::Uuid;

    #[derive(Default)]
    struct ResumeProbe {
        attempts: Mutex<Vec<Option<String>>>,
        always_limited: bool,
    }

    #[async_trait]
    impl ProviderDriver for ResumeProbe {
        fn provider_id(&self) -> ProviderId {
            ProviderId::Mock
        }
        async fn health_check(&self) -> ProviderHealth {
            unreachable!()
        }
        async fn interrupt(&self, _: Uuid) -> CoreResult<()> {
            Ok(())
        }
        async fn run_turn(
            &self,
            req: TurnRequest,
            tx: mpsc::UnboundedSender<StreamEvent>,
        ) -> CoreResult<()> {
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push(req.resume_session_id);
            if self.always_limited || attempts.len() == 1 {
                tx.send(StreamEvent::SessionStarted {
                    provider_session_id: "provider-generated-id".into(),
                })
                .unwrap();
                tx.send(StreamEvent::RateLimited {
                    retry_after_secs: 0,
                    message: "limit".into(),
                })
                .unwrap();
            } else {
                tx.send(StreamEvent::Completed { final_text: None })
                    .unwrap();
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn retry_carries_new_session_id_to_second_attempt() {
        let probe = Arc::new(ResumeProbe::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_turn_with_retry(
            probe.clone(),
            test_request(ProviderId::Mock),
            tx,
            true,
            1,
            0,
        )
        .await
        .unwrap();
        assert_eq!(
            *probe.attempts.lock().unwrap(),
            vec![None, Some("provider-generated-id".into())]
        );
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::SessionStarted { .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::RateLimited { .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::Completed { .. })
        ));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn retry_respects_opt_out_and_retry_limit() {
        for (auto_resume, max_retries, expected_attempts) in
            [(false, 3, 1), (true, 0, 1), (true, 2, 3)]
        {
            let probe = Arc::new(ResumeProbe {
                always_limited: true,
                ..ResumeProbe::default()
            });
            let (tx, _rx) = mpsc::unbounded_channel();
            run_turn_with_retry(
                probe.clone(),
                test_request(ProviderId::Mock),
                tx,
                auto_resume,
                max_retries,
                0,
            )
            .await
            .unwrap();
            assert_eq!(probe.attempts.lock().unwrap().len(), expected_attempts);
        }
    }
}
