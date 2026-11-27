use std::{sync::Arc, time::Duration};
use t3_core::{
    protocol::StreamEvent,
    traits::{CoreResult, ProviderDriver, TurnRequest},
};
use tokio::sync::mpsc;
use uuid::Uuid;

use t3_core::types::ProviderId;

/// Longest rate-limit wait worth sleeping through; anything longer is treated as a hard stop.
pub const MAX_RETRY_AFTER_SECS: u64 = 120;
pub const MAX_CAPACITY_RETRIES: u32 = 2;

const CAPACITY_RETRY_BASE_SECS: [u64; MAX_CAPACITY_RETRIES as usize] = [5, 20];
pub(crate) const TOOL_FAILURE_LOOP_ERROR_CODE: &str = "tool_failure_loop";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    UsageLimit,
    Capacity,
    Auth,
    ProviderError,
    HarnessError,
    ToolFailureLoop,
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UsageLimit => "usage_limit",
            Self::Capacity => "capacity",
            Self::Auth => "auth",
            Self::ProviderError => "provider_error",
            Self::HarnessError => "harness_error",
            Self::ToolFailureLoop => "tool_failure_loop",
        }
    }
}

/// Classifies an error emitted by a provider's CLI event stream.
pub fn classify_provider_failure(
    provider: ProviderId,
    message: &str,
    error_code: Option<&str>,
) -> FailureClass {
    if error_code == Some(TOOL_FAILURE_LOOP_ERROR_CODE) {
        return FailureClass::ToolFailureLoop;
    }
    let normalized = format!(
        "{} {}",
        normalize(error_code.unwrap_or_default()),
        normalize(message)
    );
    if is_usage_limit(&normalized, provider) {
        FailureClass::UsageLimit
    } else if is_auth_failure(&normalized) {
        FailureClass::Auth
    } else if is_capacity_failure(&normalized, provider) {
        FailureClass::Capacity
    } else {
        FailureClass::ProviderError
    }
}

/// A normalized rate-limit event is transient capacity unless its message signals account quota.
pub fn classify_rate_limit(
    provider: ProviderId,
    message: &str,
    error_code: Option<&str>,
) -> FailureClass {
    match classify_provider_failure(provider, message, error_code) {
        FailureClass::UsageLimit | FailureClass::Auth => {
            classify_provider_failure(provider, message, error_code)
        }
        _ => FailureClass::Capacity,
    }
}

/// Classifies driver/runtime failures that did not arrive as provider error events.
pub fn classify_harness_failure(
    provider: ProviderId,
    message: &str,
    error_code: Option<&str>,
) -> FailureClass {
    match classify_provider_failure(provider, message, error_code) {
        FailureClass::Auth | FailureClass::UsageLimit | FailureClass::Capacity => {
            classify_provider_failure(provider, message, error_code)
        }
        _ => FailureClass::HarnessError,
    }
}

pub fn format_run_error(class: FailureClass, message: &str, reset_at: Option<&str>) -> String {
    let mut formatted = format!("{}: {message}", class.as_str());
    if let Some(reset_at) = reset_at.filter(|reset_at| !reset_at.trim().is_empty()) {
        formatted.push_str(" (reset_at: ");
        formatted.push_str(reset_at);
        formatted.push(')');
    }
    formatted
}

pub fn format_worker_error(
    class: FailureClass,
    provider: ProviderId,
    model: &str,
    reset_at: Option<&str>,
    message: &str,
) -> String {
    format!(
        "{}: provider={} model={} reset_at={}; re-run on another provider. {}",
        class.as_str(),
        provider.as_str(),
        model,
        reset_at
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("unknown"),
        message
    )
}

pub fn format_delegated_error(provider: ProviderId, model: &str, run_error: &str) -> String {
    let (class, provider_message) = [
        FailureClass::UsageLimit,
        FailureClass::Capacity,
        FailureClass::Auth,
        FailureClass::ProviderError,
        FailureClass::HarnessError,
        FailureClass::ToolFailureLoop,
    ]
    .into_iter()
    .find_map(|class| {
        run_error
            .strip_prefix(&format!("{}: ", class.as_str()))
            .map(|message| (class, message))
    })
    .unwrap_or_else(|| {
        (
            classify_provider_failure(provider, run_error, None),
            run_error,
        )
    });
    let (message, reset_at) = provider_message
        .strip_suffix(')')
        .and_then(|message| message.rsplit_once(" (reset_at: "))
        .map_or((provider_message, None), |(message, reset_at)| {
            (message, Some(reset_at))
        });
    format_worker_error(class, provider, model, reset_at, message)
}

/// Account-level exhaustion (weekly or plan quota) cannot be cleared by waiting a few minutes.
pub fn is_quota_exhausted(message: &str) -> bool {
    is_usage_limit(&normalize(message), ProviderId::Mock)
}

/// Returns true when a rate-limit event should not be retried on the same model.
pub fn is_hard_rate_limit(retry_after_secs: u64, message: &str) -> bool {
    retry_after_secs > MAX_RETRY_AFTER_SECS
        || classify_rate_limit(ProviderId::Mock, message, None) == FailureClass::UsageLimit
}

fn normalize(message: &str) -> String {
    message
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_usage_limit(message: &str, provider: ProviderId) -> bool {
    let lower_camel_usage_limit =
        provider == ProviderId::Codex && message.contains("usagelimitexceeded");
    lower_camel_usage_limit
        || [
            "usage limit",
            "weekly",
            "individual quota",
            "quota reached",
            "quota exceeded",
            "weekly quota",
            "monthly quota",
            "hit your limit",
            "exceeded your plan",
            "out of quota",
        ]
        .iter()
        .any(|needle| message.contains(needle))
        || (message.contains("quota") && !message.contains("quota available"))
}

fn is_auth_failure(message: &str) -> bool {
    [
        "authentication",
        "auth error",
        "autherror",
        "unauthorized",
        "unauthenticated",
        "invalid api key",
        "invalidapikey",
        "api key",
        "apikey",
        "credential",
        "api key is invalid",
        "not authenticated",
        "not logged in",
        "login required",
        "token expired",
        "expired token",
        "access token expired",
        "forbidden",
        "permission denied",
        "access denied",
        "http 401",
        "status 401",
        "401",
        "http 403",
        "status 403",
        "403",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn is_capacity_failure(message: &str, provider: ProviderId) -> bool {
    (provider == ProviderId::Claude && message.contains("api error 429"))
        || [
            "429",
            "rate limit",
            "too many requests",
            "resource exhausted",
            "overloaded",
            "at capacity",
            "capacity exceeded",
            "temporarily unavailable",
        ]
        .iter()
        .any(|needle| message.contains(needle))
}

fn jittered_capacity_delay(retry_index: u32) -> Duration {
    let base = CAPACITY_RETRY_BASE_SECS[retry_index as usize];
    let jitter_range = (base / 5).max(1);
    let width = jitter_range * 2 + 1;
    let random = Uuid::new_v4();
    let value = u64::from(random.as_bytes()[0]) % width;
    Duration::from_secs(base - jitter_range + value)
}

/// Stream each attempt and resume the latest provider session after rate limiting.
pub async fn run_turn_with_retry(
    driver: Arc<dyn ProviderDriver>,
    mut req: TurnRequest,
    tx: mpsc::UnboundedSender<StreamEvent>,
    auto_resume: bool,
    max_retries: u32,
) -> CoreResult<()> {
    let mut retries = 0;
    loop {
        let (attempt_tx, mut attempt_rx) = mpsc::unbounded_channel();
        let turn = driver.run_turn(req.clone(), attempt_tx);
        tokio::pin!(turn);

        let mut retry_after: Option<u64> = None;
        let mut failure_class = None;
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
                    error_code,
                    ..
                } => {
                    let class =
                        classify_rate_limit(driver.provider_id(), message, error_code.as_deref());
                    failure_class = Some(class);
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
        let Some(retry_after) = retry_after.filter(|retry_after| {
            auto_resume
                && retries < max_retries.min(MAX_CAPACITY_RETRIES)
                && failure_class == Some(FailureClass::Capacity)
                && *retry_after <= MAX_RETRY_AFTER_SECS
        }) else {
            return Ok(());
        };

        let delay = jittered_capacity_delay(retries);
        let provider_wait = Duration::from_secs(retry_after.min(MAX_RETRY_AFTER_SECS));
        let wait = delay.max(provider_wait);
        tokio::select! {
            _ = tx.closed() => return Ok(()),
            _ = tokio::time::sleep(wait) => {}
        }
        retries += 1;
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
                    message: "429 at capacity".into(),
                    reset_at: None,
                    error_code: None,
                })
                .unwrap();
            } else {
                tx.send(StreamEvent::Completed { final_text: None })
                    .unwrap();
            }
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_carries_new_session_id_to_second_attempt() {
        let probe = Arc::new(ResumeProbe::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_turn_with_retry(probe.clone(), test_request(ProviderId::Mock), tx, true, 1)
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

    #[tokio::test(start_paused = true)]
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
            )
            .await
            .unwrap();
            assert_eq!(probe.attempts.lock().unwrap().len(), expected_attempts);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_retry_count_is_capped_at_two() {
        let probe = Arc::new(ResumeProbe {
            always_limited: true,
            ..ResumeProbe::default()
        });
        let (tx, _rx) = mpsc::unbounded_channel();

        run_turn_with_retry(probe.clone(), test_request(ProviderId::Claude), tx, true, 9)
            .await
            .unwrap();

        assert_eq!(probe.attempts.lock().unwrap().len(), 3);
    }

    #[test]
    fn capacity_retry_backoff_has_jitter_around_five_and_twenty_seconds() {
        for _ in 0..256 {
            assert!((4..=6).contains(&jittered_capacity_delay(0).as_secs()));
            assert!((16..=24).contains(&jittered_capacity_delay(1).as_secs()));
        }
    }

    #[test]
    fn classifies_provider_error_shapes_and_formats_run_errors() {
        assert_eq!(
            classify_provider_failure(
                ProviderId::Claude,
                "same tool call failed repeatedly",
                Some(TOOL_FAILURE_LOOP_ERROR_CODE)
            ),
            FailureClass::ToolFailureLoop
        );
        assert_eq!(
            classify_rate_limit(
                ProviderId::Claude,
                "API Error 429 at capacity",
                Some("api_error_429")
            ),
            FailureClass::Capacity
        );
        assert_eq!(
            classify_rate_limit(
                ProviderId::Codex,
                "request failed",
                Some("usageLimitExceeded")
            ),
            FailureClass::UsageLimit
        );
        assert_eq!(
            classify_rate_limit(
                ProviderId::Antigravity,
                "Individual quota reached",
                Some("RESOURCE_EXHAUSTED")
            ),
            FailureClass::UsageLimit
        );
        assert_eq!(
            classify_provider_failure(ProviderId::Claude, "invalid_api_key", None),
            FailureClass::Auth
        );
        assert_eq!(
            classify_provider_failure(ProviderId::Codex, "unsupported model", None),
            FailureClass::ProviderError
        );
        assert_eq!(
            classify_harness_failure(ProviderId::Antigravity, "CLI process: file not found", None),
            FailureClass::HarnessError
        );
        assert_eq!(
            format_run_error(
                FailureClass::UsageLimit,
                "Individual quota reached",
                Some("2026-11-27T03:00:00Z")
            ),
            "usage_limit: Individual quota reached (reset_at: 2026-11-27T03:00:00Z)"
        );
        assert_eq!(
            format_delegated_error(
                ProviderId::Antigravity,
                "gemini-test",
                "usage_limit: Individual quota reached (reset_at: 2026-11-27T03:00:00Z)"
            ),
            "usage_limit: provider=antigravity model=gemini-test reset_at=2026-11-27T03:00:00Z; re-run on another provider. Individual quota reached"
        );
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
                reset_at: None,
                error_code: None,
            })
            .unwrap();
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
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
                run_turn_with_retry(probe.clone(), test_request(ProviderId::Mock), tx, true, 3),
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
