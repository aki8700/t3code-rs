mod antigravity;
mod cache;
mod claude;
mod cli;
mod codex;
#[cfg(test)]
mod tests;

pub(crate) use antigravity::*;
pub(crate) use cache::UsageLimitsCache;
pub(crate) use claude::*;
pub(crate) use cli::{MAX_LINE_BYTES, prepare_probe_cmd, read_capped_line};
pub(crate) use codex::*;

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, de};
use t3_core::{CoreError, ProviderId};

use crate::{ServerState, routes::ApiResult};

const SESSION_MINS: u32 = 5 * 60;
const WEEK_MINS: u32 = 7 * 24 * 60;

#[derive(Deserialize)]
pub(crate) struct LimitsQuery {
    #[serde(default, deserialize_with = "parse_refresh")]
    refresh: Option<bool>,
}

fn parse_refresh<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    match String::deserialize(deserializer)?.as_str() {
        "true" | "1" => Ok(Some(true)),
        "false" | "0" => Ok(Some(false)),
        _ => Err(de::Error::custom("refresh must be a boolean")),
    }
}

pub(crate) async fn get(
    State(state): State<ServerState>,
    Query(query): Query<LimitsQuery>,
) -> ApiResult<Json<UsageLimitsResponse>> {
    let refresh = query.refresh.unwrap_or(false);
    let antigravity_quota_probe = state.settings.get().antigravity_quota_probe;
    let response = state
        .limits_cache
        .fetch(
            &state.config,
            antigravity_quota_probe,
            state.usage_home.as_deref(),
            refresh,
        )
        .await;
    Ok(Json(response))
}

#[derive(Deserialize)]
pub(crate) struct ConsumeResetCreditRequest {
    provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConsumeResetCreditResponse {
    pub outcome: String,
    pub message: String,
    pub limits: UsageLimitsResponse,
}

pub(crate) async fn consume_reset_credit(
    State(state): State<ServerState>,
    Json(body): Json<ConsumeResetCreditRequest>,
) -> ApiResult<Json<ConsumeResetCreditResponse>> {
    if parse_provider_key(&body.provider) != Some(ProviderId::Codex) {
        return Err(CoreError::InvalidArgument(
            "リセットクレジットの消費は現在 Codex のみ対応しています".into(),
        )
        .into());
    }
    let antigravity_quota_probe = state.settings.get().antigravity_quota_probe;
    let response = state
        .limits_cache
        .consume_codex_reset_credit(
            &state.config,
            antigravity_quota_probe,
            state.usage_home.as_deref(),
        )
        .await?;
    if response.outcome == "reset" {
        state
            .coordinator
            .release_provider_hold(ProviderId::Codex)
            .await;
    }
    Ok(Json(response))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageLimitsResponse {
    pub checked_at: String,
    pub providers: Vec<ProviderUsageLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResetCredits {
    pub available_count: u32,
    pub next_expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderUsageLimits {
    pub provider: String,
    pub label: String,
    pub plan: Option<String>,
    pub status: LimitStatus,
    pub source: LimitSource,
    pub checked_at: String,
    pub message: Option<String>,
    pub reset_credits: Option<ResetCredits>,
    pub windows: Vec<UsageLimitWindow>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LimitStatus {
    Ok,
    Unavailable,
    Unsupported,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LimitSource {
    ClaudeCli,
    CodexAppServer,
    CodexSessionLog,
    AntigravityApi,
    Runtime,
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WindowKind {
    Session,
    Weekly,
    Monthly,
    Other,
}

impl WindowKind {
    fn sort_order(self) -> u8 {
        match self {
            WindowKind::Session => 0,
            WindowKind::Weekly => 1,
            WindowKind::Monthly => 2,
            WindowKind::Other => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageLimitWindow {
    pub id: String,
    pub kind: WindowKind,
    pub label: String,
    pub used_percent: f64,
    pub remaining_percent: f64,
    pub window_duration_mins: Option<u32>,
    pub resets_at: Option<String>,
}

fn unavailable(provider: ProviderId, now: DateTime<Utc>, message: &str) -> ProviderUsageLimits {
    ProviderUsageLimits {
        provider: provider_key(provider).into(),
        label: provider_label(provider).into(),
        plan: None,
        status: LimitStatus::Unavailable,
        source: LimitSource::None,
        checked_at: iso(now),
        message: Some(message.to_owned()),
        reset_credits: None,
        windows: Vec::new(),
    }
}

fn make_window(
    id: String,
    kind: WindowKind,
    label: String,
    used_percent: f64,
    window_duration_mins: Option<u32>,
    resets_at: Option<String>,
) -> UsageLimitWindow {
    let used = used_percent.clamp(0.0, 100.0);
    let remaining = (100.0 - used).clamp(0.0, 100.0);
    UsageLimitWindow {
        id,
        kind,
        label,
        used_percent: (used * 10.0).round() / 10.0,
        remaining_percent: (remaining * 10.0).round() / 10.0,
        window_duration_mins,
        resets_at,
    }
}

fn capitalize(trimmed: &str) -> Option<String> {
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    Some(format!("{}{}", first.to_uppercase(), chars.as_str()))
}

fn normalize_iso(raw: &str) -> Option<String> {
    raw.parse::<DateTime<Utc>>().ok().map(iso)
}

fn iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn provider_key(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Claude => "claude",
        ProviderId::Codex => "codex",
        ProviderId::Antigravity => "antigravity",
        ProviderId::OpenCode => "open-code",
        ProviderId::Mock => "mock",
    }
}

fn parse_provider_key(key: &str) -> Option<ProviderId> {
    match key {
        "claude" => Some(ProviderId::Claude),
        "codex" => Some(ProviderId::Codex),
        "antigravity" => Some(ProviderId::Antigravity),
        "open-code" | "opencode" => Some(ProviderId::OpenCode),
        _ => None,
    }
}

fn provider_label(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Claude => "Claude Code",
        ProviderId::Codex => "Codex",
        ProviderId::Antigravity => "Antigravity",
        ProviderId::OpenCode => "OpenCode",
        ProviderId::Mock => "Mock",
    }
}
