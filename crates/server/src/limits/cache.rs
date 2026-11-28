use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Mutex as StdMutex,
    time::{Duration as StdDuration, Instant},
};

use chrono::{DateTime, Duration, Utc};
use t3_core::{CoreError, ProviderId, ServerConfig};
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use super::{
    CodexResetError, ConsumeResetCreditResponse, LimitSource, LimitStatus, ProviderUsageLimits,
    UsageLimitWindow, UsageLimitsResponse, WindowKind, consume_codex_reset_credit_rpc, iso,
    parse_provider_key, probe_antigravity, probe_claude, probe_codex, provider_key, provider_label,
    unavailable,
};

const LIMITS_CACHE_TTL: StdDuration = StdDuration::from_secs(60);

#[derive(Debug, Clone)]
struct RuntimeRateLimit {
    recorded_at: DateTime<Utc>,
    resets_at: Option<DateTime<Utc>>,
    message: String,
}

struct CachedLimits {
    stamp: Instant,
    antigravity_quota_probe: bool,
    response: UsageLimitsResponse,
}

#[derive(Default)]
pub(crate) struct UsageLimitsCache {
    cached: AsyncMutex<Option<CachedLimits>>,
    /// Serializes Codex reset credit redemption and reuses the pending idempotency key on timeout.
    codex_reset_key: AsyncMutex<Option<String>>,
    runtime: StdMutex<HashMap<ProviderId, RuntimeRateLimit>>,
}

impl UsageLimitsCache {
    pub(crate) fn record_runtime_limit(
        &self,
        provider: ProviderId,
        retry_after_secs: Option<u64>,
        message: &str,
    ) {
        let now = Utc::now();
        let resets_at = retry_after_secs
            .filter(|secs| *secs > 0)
            .and_then(|secs| i64::try_from(secs).ok())
            .map(|secs| now + Duration::seconds(secs));
        if let Ok(mut map) = self.runtime.lock() {
            map.insert(
                provider,
                RuntimeRateLimit {
                    recorded_at: now,
                    resets_at,
                    message: message.to_owned(),
                },
            );
        }
    }

    pub(crate) async fn fetch(
        &self,
        config: &ServerConfig,
        antigravity_quota_probe: bool,
        home: Option<&Path>,
        refresh: bool,
    ) -> UsageLimitsResponse {
        let mut guard = self.cached.lock().await;
        if !refresh
            && let Some(cached) = guard.as_ref()
            && cached.antigravity_quota_probe == antigravity_quota_probe
            && cached.stamp.elapsed() < LIMITS_CACHE_TTL
        {
            return self.overlay_runtime(cached.response.clone());
        }
        let response = probe_all(config, antigravity_quota_probe, home, &self.runtime).await;
        *guard = Some(CachedLimits {
            stamp: Instant::now(),
            antigravity_quota_probe,
            response: response.clone(),
        });
        response
    }

    pub(crate) async fn consume_codex_reset_credit(
        &self,
        config: &ServerConfig,
        antigravity_quota_probe: bool,
        home: Option<&Path>,
    ) -> Result<ConsumeResetCreditResponse, CoreError> {
        let mut pending = self.codex_reset_key.lock().await;
        let idempotency_key = pending
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        *pending = Some(idempotency_key.clone());

        let outcome =
            match consume_codex_reset_credit_rpc(&config.providers.codex_bin, &idempotency_key)
                .await
            {
                Ok(outcome) => {
                    *pending = None;
                    outcome
                }
                Err(CodexResetError::Settled(msg)) => {
                    *pending = None;
                    return Err(CoreError::Provider(msg));
                }
                Err(CodexResetError::Timeout(msg)) => {
                    // Keep pending idempotency key so a user retry is idempotent.
                    return Err(CoreError::Provider(msg));
                }
            };
        drop(pending);

        if outcome == "reset"
            && let Ok(mut map) = self.runtime.lock()
        {
            map.remove(&ProviderId::Codex);
        }
        let limits = self
            .fetch(config, antigravity_quota_probe, home, true)
            .await;
        let message = match outcome.as_str() {
            "reset" => "リセットを適用しました。上限枠が回復しました。",
            "nothingToReset" => "現在リセットが必要な上限枠はありません。",
            "noCredit" => "利用可能なリセットクレジットがありません。",
            "alreadyRedeemed" => "このリセットクレジットは既に使用済みです。",
            _ => "リセットクレジットの処理が完了しました。",
        }
        .to_owned();

        Ok(ConsumeResetCreditResponse {
            outcome,
            message,
            limits,
        })
    }

    pub(super) fn overlay_runtime(&self, mut response: UsageLimitsResponse) -> UsageLimitsResponse {
        let now = Utc::now();
        let active = active_runtime_limits(&self.runtime, now);
        if active.is_empty() {
            return response;
        }
        for entry in &mut response.providers {
            let Some(pid) = parse_provider_key(&entry.provider) else {
                continue;
            };
            if let Some(rt) = active.get(&pid)
                && entry.windows.is_empty()
            {
                entry.status = LimitStatus::Ok;
                entry.source = LimitSource::Runtime;
                entry.checked_at = iso(rt.recorded_at);
                entry.message = Some(rt.message.clone());
                entry.windows = vec![runtime_window(rt)];
            }
        }
        response
    }
}

fn active_runtime_limits(
    store: &StdMutex<HashMap<ProviderId, RuntimeRateLimit>>,
    now: DateTime<Utc>,
) -> HashMap<ProviderId, RuntimeRateLimit> {
    let Ok(mut map) = store.lock() else {
        return HashMap::new();
    };
    map.retain(|_, entry| match entry.resets_at {
        Some(resets) => resets > now,
        None => now - entry.recorded_at < Duration::minutes(15),
    });
    map.clone()
}

fn runtime_window(rt: &RuntimeRateLimit) -> UsageLimitWindow {
    UsageLimitWindow {
        id: "runtime".into(),
        kind: WindowKind::Session,
        label: "レート制限中".into(),
        used_percent: 100.0,
        remaining_percent: 0.0,
        window_duration_mins: None,
        resets_at: rt.resets_at.map(iso),
    }
}

async fn probe_all(
    config: &ServerConfig,
    antigravity_quota_probe: bool,
    home: Option<&Path>,
    runtime: &StdMutex<HashMap<ProviderId, RuntimeRateLimit>>,
) -> UsageLimitsResponse {
    let now = Utc::now();
    let checked_at = iso(now);
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    for provider in &config.enabled_providers {
        if *provider != ProviderId::Mock && seen.insert(*provider) {
            order.push(*provider);
        }
    }

    let wants_claude = order.contains(&ProviderId::Claude);
    let wants_codex = order.contains(&ProviderId::Codex);
    let wants_antigravity = order.contains(&ProviderId::Antigravity);
    let claude_bin = config.providers.claude_bin.clone();
    let codex_bin = config.providers.codex_bin.clone();
    let home_owned = home.map(Path::to_path_buf);

    let (claude_res, codex_res, antigravity_res) = tokio::join!(
        async {
            if wants_claude {
                Some(probe_claude(&claude_bin, now).await)
            } else {
                None
            }
        },
        async {
            if wants_codex {
                Some(probe_codex(&codex_bin, home_owned.as_deref(), now).await)
            } else {
                None
            }
        },
        async {
            if wants_antigravity {
                Some(probe_antigravity(config, antigravity_quota_probe, now).await)
            } else {
                None
            }
        },
    );

    let active_rt = active_runtime_limits(runtime, now);
    let mut providers = Vec::with_capacity(order.len());
    for provider in order {
        let mut item = match provider {
            ProviderId::Claude => claude_res
                .clone()
                .unwrap_or_else(|| unavailable(provider, now, "Claude CLI の確認に失敗しました")),
            ProviderId::Codex => codex_res
                .clone()
                .unwrap_or_else(|| unavailable(provider, now, "Codex CLI の確認に失敗しました")),
            ProviderId::Antigravity => antigravity_res.clone().unwrap_or_else(|| {
                unavailable(provider, now, "Antigravity の上限確認に失敗しました")
            }),
            ProviderId::OpenCode => ProviderUsageLimits {
                provider: provider_key(provider).into(),
                label: provider_label(provider).into(),
                plan: None,
                status: LimitStatus::Unsupported,
                source: LimitSource::None,
                checked_at: checked_at.clone(),
                message: Some(
                    "OpenCode はローカルモデル・外部プロバイダー経由のため上限照会に非対応です。"
                        .into(),
                ),
                reset_credits: None,
                windows: Vec::new(),
            },
            ProviderId::GeminiApp | ProviderId::Mock => continue,
        };
        if item.windows.is_empty()
            && let Some(rt) = active_rt.get(&provider)
        {
            item.status = LimitStatus::Ok;
            item.source = LimitSource::Runtime;
            item.checked_at = iso(rt.recorded_at);
            item.message = Some(rt.message.clone());
            item.windows = vec![runtime_window(rt)];
        }
        providers.push(item);
    }

    UsageLimitsResponse {
        checked_at,
        providers,
    }
}
