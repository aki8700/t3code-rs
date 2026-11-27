//! In-memory "limited until reset" state per provider (or per model when the limit names it).
//!
//! A restart forgets every hold; the first run after it rediscovers the limit.

use chrono::{DateTime, Duration, Utc};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use t3_core::{ModelSelection, ProviderId};
use uuid::Uuid;

pub(crate) const CRASH_LOOP_WINDOW_SECS: i64 = 300;
pub(crate) const CRASH_LOOP_THRESHOLD: usize = 3;
pub(crate) const CRASH_LOOP_HOLD_SECS: i64 = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderHold {
    pub(crate) provider: ProviderId,
    /// `None` holds every model of the provider.
    pub(crate) model: Option<String>,
    pub(crate) until: DateTime<Utc>,
    pub(crate) reason: String,
}

struct Entry {
    hold: ProviderHold,
    announced: HashSet<Uuid>,
}

type FailureHistory = HashMap<(ProviderId, String), VecDeque<DateTime<Utc>>>;

#[derive(Default)]
pub(crate) struct ProviderHolds(
    Mutex<Vec<Entry>>,
    AtomicBool,
    Mutex<HashSet<Uuid>>,
    Mutex<FailureHistory>,
);

impl ProviderHolds {
    pub(crate) fn set_paused(&self, paused: bool) {
        self.1.store(paused, Ordering::Release);
        if !paused {
            self.2.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.1.load(Ordering::Acquire)
    }

    /// Returns true after the run has already received the pause reason.
    pub(crate) fn pause_blocking(&self, run_id: Uuid) -> Option<bool> {
        if !self.is_paused() {
            return None;
        }
        let mut announced = self.2.lock().unwrap_or_else(|e| e.into_inner());
        self.is_paused().then(|| !announced.insert(run_id))
    }

    /// Records a limit and returns true when it extends the hold, so the caller schedules a wake.
    pub(crate) fn record(&self, hold: ProviderHold) -> bool {
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match entries
            .iter_mut()
            .find(|entry| entry.hold.provider == hold.provider && entry.hold.model == hold.model)
        {
            Some(entry) if entry.hold.until >= hold.until => false,
            Some(entry) => {
                entry.hold = hold;
                true
            }
            None => {
                entries.push(Entry {
                    hold,
                    announced: HashSet::new(),
                });
                true
            }
        }
    }

    /// The hold that blocks `selection` now, with whether `run_id` has already been told about it.
    pub(crate) fn blocking(
        &self,
        selection: &ModelSelection,
        run_id: Option<Uuid>,
    ) -> Option<(ProviderHold, bool)> {
        let now = Utc::now();
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|entry| entry.hold.until > now);
        let entry = entries
            .iter_mut()
            .filter(|entry| {
                entry.hold.provider == selection.provider
                    && entry
                        .hold
                        .model
                        .as_ref()
                        .is_none_or(|model| *model == selection.model)
            })
            .max_by_key(|entry| entry.hold.until)?;
        let announced = run_id.is_some_and(|id| !entry.announced.insert(id));
        Some((entry.hold.clone(), announced))
    }

    pub(crate) fn release(&self, provider: ProviderId) {
        let mut entries = self.0.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|entry| entry.hold.provider != provider);
    }

    /// Tracks recent consecutive failures per (provider, model). When `CRASH_LOOP_THRESHOLD`
    /// failures occur within `CRASH_LOOP_WINDOW_SECS`, automatically records a temporary hold
    /// so broken/unauthenticated providers stop burning tokens in tight retry loops.
    pub(crate) fn record_failure(
        &self,
        selection: &ModelSelection,
        reason: &str,
        now: DateTime<Utc>,
    ) -> Option<ProviderHold> {
        let cutoff = now - Duration::seconds(CRASH_LOOP_WINDOW_SECS);
        let triggered_count = {
            let mut failures = self.3.lock().unwrap_or_else(|e| e.into_inner());
            let history = failures
                .entry((selection.provider, selection.model.clone()))
                .or_default();
            history.retain(|&ts| ts >= cutoff);
            history.push_back(now);
            if history.len() >= CRASH_LOOP_THRESHOLD {
                let count = history.len();
                history.clear();
                Some(count)
            } else {
                None
            }
        };
        let count = triggered_count?;
        let hold = ProviderHold {
            provider: selection.provider,
            model: Some(selection.model.clone()),
            until: now + Duration::seconds(CRASH_LOOP_HOLD_SECS),
            reason: format!(
                "crash_loop: {count} consecutive failures within {CRASH_LOOP_WINDOW_SECS}s ({reason})"
            ),
        };
        self.record(hold.clone());
        Some(hold)
    }

    pub(crate) fn record_success(&self, selection: &ModelSelection) {
        let mut failures = self.3.lock().unwrap_or_else(|e| e.into_inner());
        failures.remove(&(selection.provider, selection.model.clone()));
    }
}

/// Parses `reset_at` as Unix seconds, Unix milliseconds, or RFC 3339.
pub(crate) fn parse_reset_at(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(epoch) = raw.parse::<i64>() {
        return if epoch >= 1_000_000_000_000 {
            DateTime::from_timestamp_millis(epoch)
        } else {
            DateTime::from_timestamp(epoch, 0)
        };
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// When the provider limit ends, from `reset_at` or else `retry_after_secs` counted from `now`.
pub(crate) fn limited_until(
    reset_at: Option<&str>,
    retry_after_secs: u64,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    reset_at.and_then(parse_reset_at).or_else(|| {
        i64::try_from(retry_after_secs)
            .ok()
            .filter(|secs| *secs > 0)
            .map(|secs| now + Duration::seconds(secs))
    })
}

/// A limit is model-scoped when its message names the model family (for example "Opus limit").
pub(crate) fn model_scoped(model: &str, message: &str) -> bool {
    const GENERIC: [&str; 6] = ["claude", "gemini", "codex", "latest", "preview", "mock"];
    let message = message.to_ascii_lowercase();
    model
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| {
            token.len() >= 4
                && token.bytes().all(|b| b.is_ascii_alphabetic())
                && !GENERIC.contains(token)
        })
        .any(|token| {
            message
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word == token)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(provider: ProviderId, model: &str) -> ModelSelection {
        ModelSelection {
            provider,
            model: model.into(),
            effort: None,
            agent: None,
        }
    }

    fn hold(model: Option<&str>, secs: i64) -> ProviderHold {
        ProviderHold {
            provider: ProviderId::Claude,
            model: model.map(str::to_owned),
            until: Utc::now() + Duration::seconds(secs),
            reason: "limit".into(),
        }
    }

    #[test]
    fn reset_at_accepts_epoch_seconds_millis_and_rfc3339() {
        let expected = DateTime::from_timestamp(1_795_682_400, 0).unwrap();
        assert_eq!(parse_reset_at("1795682400"), Some(expected));
        assert_eq!(parse_reset_at("1795682400000"), Some(expected));
        assert_eq!(
            parse_reset_at("2026-11-27T03:00:00Z"),
            parse_reset_at("1795748400")
        );
        assert_eq!(parse_reset_at("soon"), None);
        let now = Utc::now();
        assert_eq!(
            limited_until(None, 60, now),
            Some(now + Duration::seconds(60))
        );
        assert_eq!(limited_until(None, 0, now), None);
        assert_eq!(limited_until(Some("1795682400"), 60, now), Some(expected));
    }

    #[test]
    fn model_scope_follows_the_model_family_named_in_the_message() {
        assert!(model_scoped(
            "claude-opus-5-5",
            "You've reached your Opus limit"
        ));
        assert!(!model_scoped(
            "claude-opus-5-5",
            "Claude usage limit reached"
        ));
        assert!(!model_scoped("gpt-6-luna", "You've hit your usage limit"));
        // The run error a real Claude limit leaves (five_hour / seven_day are account-wide).
        assert!(!model_scoped(
            "sonnet",
            "capacity: rate_limit_event (reset_at: 1795702200)"
        ));
        assert!(model_scoped(
            "gemini-4-hydrogen",
            "Individual quota reached for gemini-4-hydrogen"
        ));
    }

    #[test]
    fn holds_block_matching_selections_until_expiry_and_announce_once() {
        let holds = ProviderHolds::default();
        assert!(holds.record(hold(None, 60)));
        assert!(!holds.record(hold(None, 30)));
        let run = Uuid::new_v4();
        let opus = selection(ProviderId::Claude, "claude-opus-5-5");
        assert!(matches!(holds.blocking(&opus, Some(run)), Some((_, false))));
        assert!(matches!(holds.blocking(&opus, Some(run)), Some((_, true))));
        assert!(
            holds
                .blocking(&selection(ProviderId::Codex, "gpt-6-luna"), None)
                .is_none()
        );
        holds.release(ProviderId::Claude);
        assert!(holds.blocking(&opus, None).is_none());

        assert!(holds.record(hold(Some("claude-opus-5-5"), 60)));
        assert!(holds.blocking(&opus, None).is_some());
        assert!(
            holds
                .blocking(&selection(ProviderId::Claude, "claude-sonnet-5-5"), None)
                .is_none()
        );
        assert!(holds.record(hold(Some("claude-sonnet-5-5"), -1)));
        assert!(
            holds
                .blocking(&selection(ProviderId::Claude, "claude-sonnet-5-5"), None)
                .is_none()
        );
    }

    #[test]
    fn pause_all_blocks_unattended_runs_and_announces_each_once() {
        let holds = ProviderHolds::default();
        let run = Uuid::new_v4();
        assert!(!holds.is_paused());
        holds.set_paused(true);
        assert!(holds.is_paused());
        assert_eq!(holds.pause_blocking(run), Some(false));
        assert_eq!(holds.pause_blocking(run), Some(true));
        holds.set_paused(false);
        assert!(!holds.is_paused());
        assert_eq!(holds.pause_blocking(run), None);
    }

    #[test]
    fn crash_loop_holds_on_third_failure_within_window() {
        let holds = ProviderHolds::default();
        let sel = selection(ProviderId::Codex, "gpt-6-luna");
        let t0 = Utc::now();

        assert!(holds.record_failure(&sel, "exit 1", t0).is_none());
        assert!(
            holds
                .record_failure(&sel, "exit 1", t0 + Duration::seconds(60))
                .is_none()
        );
        assert!(holds.blocking(&sel, None).is_none());

        let t_trigger = t0 + Duration::seconds(120);
        let hold = holds
            .record_failure(&sel, "exit 1", t_trigger)
            .expect("third failure should trigger crash_loop hold");
        assert_eq!(hold.provider, ProviderId::Codex);
        assert_eq!(hold.model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(
            hold.until,
            t_trigger + Duration::seconds(CRASH_LOOP_HOLD_SECS)
        );
        assert!(
            hold.reason
                .starts_with("crash_loop: 3 consecutive failures")
        );
        assert!(holds.blocking(&sel, None).is_some());
        assert!(
            holds
                .record_failure(&sel, "exit 1", t_trigger + Duration::seconds(10))
                .is_none()
        );
    }

    #[test]
    fn record_success_resets_crash_loop_counter() {
        let holds = ProviderHolds::default();
        let sel = selection(ProviderId::Claude, "claude-haiku-5-5");
        let t0 = Utc::now();

        assert!(holds.record_failure(&sel, "sigkill", t0).is_none());
        assert!(
            holds
                .record_failure(&sel, "sigkill", t0 + Duration::seconds(30))
                .is_none()
        );
        holds.record_success(&sel);
        assert!(
            holds
                .record_failure(&sel, "sigkill", t0 + Duration::seconds(60))
                .is_none()
        );
        assert!(
            holds
                .record_failure(&sel, "sigkill", t0 + Duration::seconds(90))
                .is_none()
        );
        assert!(
            holds
                .record_failure(&sel, "sigkill", t0 + Duration::seconds(120))
                .is_some()
        );
    }

    #[test]
    fn failures_outside_window_are_not_counted() {
        let holds = ProviderHolds::default();
        let sel = selection(ProviderId::Codex, "gpt-6-luna");
        let t0 = Utc::now();

        assert!(holds.record_failure(&sel, "err", t0).is_none());
        assert!(
            holds
                .record_failure(&sel, "err", t0 + Duration::seconds(1))
                .is_none()
        );
        let t_later = t0 + Duration::seconds(CRASH_LOOP_WINDOW_SECS + 5);
        assert!(holds.record_failure(&sel, "err", t_later).is_none());
        assert!(
            holds
                .record_failure(&sel, "err", t_later + Duration::seconds(10))
                .is_none()
        );
        assert!(
            holds
                .record_failure(&sel, "err", t_later + Duration::seconds(20))
                .is_some()
        );
    }
}
