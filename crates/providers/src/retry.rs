use std::{sync::Arc, time::Duration};
use t3_core::{
    protocol::StreamEvent,
    traits::{CoreResult, ProviderDriver, TurnRequest},
};
use tokio::sync::mpsc;

/// Longest rate-limit wait worth sleeping through; anything longer is treated as a hard stop.
pub const MAX_RETRY_AFTER_SECS: u64 = 120;

/// Account-level exhaustion (weekly or plan quota) cannot be cleared by waiting a few minutes.
pub fn is_quota_exhausted(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["weekly", "usage limit", "hit your limit", "quota"]
        .iter()
        .any(|needle| message.contains(needle))
}

/// Returns true when a rate-limit event should not be retried on the same model.
pub fn is_hard_rate_limit(retry_after_secs: u64, message: &str) -> bool {
    retry_after_secs > MAX_RETRY_AFTER_SECS || is_quota_exhausted(message)
}

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
        let mut hard_limit = false;
        let mut handle_event = |event| {
            match &event {
                StreamEvent::SessionStarted {
                    provider_session_id,
                } => {
                    req.resume_session_id = Some(provider_session_id.clone());
                }
                StreamEvent::RateLimited {
                    retry_after_secs,
                    message,
                } => {
                    hard_limit |= is_hard_rate_limit(*retry_after_secs, message);
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
        let Some(retry_after) =
            retry_after.filter(|_| auto_resume && retries < max_retries && !hard_limit)
        else {
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

    /// Sends one rate-limit event per attempt and counts how many attempts were started.
    struct LimitProbe {
        retry_after_secs: u64,
        message: &'static str,
        attempts: Mutex<usize>,
    }

    #[async_trait]
    impl ProviderDriver for LimitProbe {
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
            _: TurnRequest,
            tx: mpsc::UnboundedSender<StreamEvent>,
        ) -> CoreResult<()> {
            *self.attempts.lock().unwrap() += 1;
            tx.send(StreamEvent::RateLimited {
                retry_after_secs: self.retry_after_secs,
                message: self.message.into(),
            })
            .unwrap();
            Ok(())
        }
    }

    #[tokio::test]
    async fn retry_bails_out_on_long_wait_or_quota_exhaustion() {
        for (retry_after_secs, message) in [
            (MAX_RETRY_AFTER_SECS + 1, "slow down"),
            (0, "You've hit your limit · resets 9am"),
            (0, "Weekly usage limit reached"),
            (30, "quota exceeded for account"),
        ] {
            let probe = Arc::new(LimitProbe {
                retry_after_secs,
                message,
                attempts: Mutex::new(0),
            });
            let (tx, mut rx) = mpsc::unbounded_channel();
            // Bounded so a regression that sleeps the long wait fails fast instead of hanging.
            tokio::time::timeout(
                Duration::from_secs(5),
                run_turn_with_retry(
                    probe.clone(),
                    test_request(ProviderId::Mock),
                    tx,
                    true,
                    3,
                    0,
                ),
            )
            .await
            .expect("must not sleep before bailing")
            .unwrap();
            assert_eq!(*probe.attempts.lock().unwrap(), 1, "{message}");
            assert!(matches!(
                rx.recv().await,
                Some(StreamEvent::RateLimited { .. })
            ));
        }
    }

    #[test]
    fn quota_detection_is_case_insensitive() {
        assert!(is_quota_exhausted("WEEKLY cap"));
        assert!(is_quota_exhausted("Usage Limit"));
        assert!(is_quota_exhausted("You've HIT YOUR LIMIT"));
        assert!(is_quota_exhausted("Account QUOTA exceeded"));
        assert!(!is_quota_exhausted("rate limited, retry shortly"));
    }

    #[test]
    fn hard_rate_limit_triggers_on_long_wait_or_quota_exhaustion() {
        assert!(!is_hard_rate_limit(0, "temporary rate limit"));
        assert!(!is_hard_rate_limit(
            MAX_RETRY_AFTER_SECS,
            "temporary rate limit"
        ));
        assert!(is_hard_rate_limit(
            MAX_RETRY_AFTER_SECS + 1,
            "temporary rate limit"
        ));
        assert!(is_hard_rate_limit(0, "Weekly usage limit reached"));
        assert!(is_hard_rate_limit(15, "You've hit your limit"));
        assert!(is_hard_rate_limit(30, "Quota exceeded"));
    }
}
