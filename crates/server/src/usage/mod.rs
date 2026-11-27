mod aggregate;
mod cache;
mod codex;
mod local_runs;
mod pricing;
mod session;
mod transcript;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Deserializer, Serialize, de};
use t3_core::CoreError;

use crate::{ServerState, routes::ApiResult};

pub(crate) use cache::UsageScanCache;
pub(crate) use codex::{codex_run_usage_since, codex_session_usage_snapshot};
pub(crate) use pricing::{estimate_tokens_cost, model_rates_per_million};
pub(crate) use session::provider_session_resumable;
#[cfg(unix)]
pub(crate) use transcript::transcript_open_flags;

use pricing::Tokens;
use transcript::RawTokens;

const MAX_JSONL_LINE: usize = 4 * 1024 * 1024;
const SESSION_TAIL_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Deserialize)]
pub(crate) struct UsageQuery {
    days: Option<u32>,
    tz_offset_mins: Option<i32>,
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
    Query(query): Query<UsageQuery>,
) -> ApiResult<Json<UsageResponse>> {
    let days = query.days.unwrap_or(7);
    if !matches!(days, 1 | 7 | 30 | 90) {
        return Err(CoreError::InvalidArgument("days must be 1, 7, 30, or 90".into()).into());
    }
    let offset = query
        .tz_offset_mins
        .unwrap_or_else(|| Local::now().offset().local_minus_utc() / 60)
        .clamp(-720, 840);
    let response = tokio::task::spawn_blocking(move || {
        let mut cache = state
            .usage_cache
            .lock()
            .map_err(|_| CoreError::Internal("usage cache lock poisoned".into()))?;
        cache.report(
            state.usage_home.as_deref(),
            &state.config.db_path(),
            days,
            offset,
            query.refresh.unwrap_or_default(),
        )
    })
    .await
    .map_err(|error| CoreError::Internal(error.to_string()))??;
    Ok(Json(response))
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(crate) struct SessionContextUsage {
    pub context_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Window the provider itself reported for this session (Codex `model_context_window`).
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CodexSessionUsageSnapshot {
    total: Option<RawTokens>,
    last: Option<RawTokens>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodexRunUsage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

struct Sample {
    at: DateTime<Utc>,
    model: String,
    tokens: Tokens,
}

#[derive(Clone, Serialize)]
pub(crate) struct UsageResponse {
    window: UsageWindow,
    totals: Totals,
    providers: Vec<UsageProviderRow>,
    models: Vec<UsageModelRow>,
    buckets: Vec<UsageBucket>,
    scanned_at: String,
}

#[derive(Clone, Serialize)]
struct UsageWindow {
    days: u32,
    resolution: &'static str,
    since: String,
    until: String,
}

#[derive(Clone, Copy, Serialize)]
struct Totals {
    cost_usd: f64,
    total_tokens: u64,
    #[serde(flatten)]
    tokens: Tokens,
    sessions: usize,
    requests: u64,
}

#[derive(Clone, Serialize)]
struct UsageProviderRow {
    provider: &'static str,
    label: &'static str,
    color: &'static str,
    #[serde(flatten)]
    totals: Totals,
    cost_share: f64,
    token_share: f64,
}

#[derive(Clone, Serialize)]
struct UsageModelRow {
    provider: &'static str,
    model: String,
    #[serde(flatten)]
    totals: Totals,
    cost_share: f64,
    token_share: f64,
}

#[derive(Clone, Serialize)]
struct UsageBucket {
    key: String,
    label: String,
    total_cost_usd: f64,
    total_tokens: u64,
    by_provider: BTreeMap<&'static str, BucketProvider>,
}

#[derive(Clone, Default, Serialize)]
struct BucketProvider {
    cost_usd: f64,
    total_tokens: u64,
}
