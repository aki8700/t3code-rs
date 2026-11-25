use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex as StdMutex,
    time::{Duration as StdDuration, Instant, SystemTime},
};

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};
use t3_core::{CoreError, ProviderId, ServerConfig};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as AsyncBufReader},
    process::Command,
    sync::Mutex as AsyncMutex,
};

use crate::{ServerState, routes::ApiResult};

const LIMITS_CACHE_TTL: StdDuration = StdDuration::from_secs(60);
const CLAUDE_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const CODEX_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(8);
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const CODEX_LOG_SCAN_FILES: usize = 16;
const SESSION_MINS: u32 = 5 * 60;
const WEEK_MINS: u32 = 7 * 24 * 60;
const MONTH_MINS: u32 = 30 * 24 * 60;

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
    let response = state
        .limits_cache
        .fetch(&state.config, state.usage_home.as_deref(), refresh)
        .await;
    Ok(Json(response))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageLimitsResponse {
    pub checked_at: String,
    pub providers: Vec<ProviderUsageLimits>,
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

#[derive(Debug, Clone)]
struct RuntimeRateLimit {
    recorded_at: DateTime<Utc>,
    resets_at: Option<DateTime<Utc>>,
    message: String,
}

struct CachedLimits {
    stamp: Instant,
    response: UsageLimitsResponse,
}

#[derive(Default)]
pub(crate) struct UsageLimitsCache {
    cached: AsyncMutex<Option<CachedLimits>>,
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
        home: Option<&Path>,
        refresh: bool,
    ) -> UsageLimitsResponse {
        let mut guard = self.cached.lock().await;
        if !refresh
            && let Some(cached) = guard.as_ref()
            && cached.stamp.elapsed() < LIMITS_CACHE_TTL
        {
            return self.overlay_runtime(cached.response.clone());
        }
        let response = probe_all(config, home, &self.runtime).await;
        *guard = Some(CachedLimits {
            stamp: Instant::now(),
            response: response.clone(),
        });
        response
    }

    fn overlay_runtime(&self, mut response: UsageLimitsResponse) -> UsageLimitsResponse {
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
    let claude_bin = config.providers.claude_bin.clone();
    let codex_bin = config.providers.codex_bin.clone();
    let home_owned = home.map(Path::to_path_buf);

    let (claude_res, codex_res) = tokio::join!(
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
            ProviderId::Antigravity => ProviderUsageLimits {
                provider: provider_key(provider).into(),
                label: provider_label(provider).into(),
                plan: None,
                status: LimitStatus::Unsupported,
                source: LimitSource::None,
                checked_at: checked_at.clone(),
                message: Some(
                    "Antigravity CLI (agy) はクォータ照会 API を提供していません。実行中にレート制限へ到達した場合はリセット時刻を表示します。"
                        .into(),
                ),
                windows: Vec::new(),
            },
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
                windows: Vec::new(),
            },
            ProviderId::Mock => continue,
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

fn prepare_probe_cmd(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.envs([
        ("DO_NOT_TRACK", "1"),
        ("AGY_DISABLE_TELEMETRY", "1"),
        ("DISABLE_TELEMETRY", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("T3CODE_TELEMETRY_ENABLED", "false"),
        ("T3CODE_OTEL_SDK_DISABLED", "true"),
    ])
    .env_remove("T3_BEARER_TOKEN")
    .env_remove("T3_MCP_TOKEN")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    cmd
}

async fn probe_claude(bin: &Path, now: DateTime<Utc>) -> ProviderUsageLimits {
    if !bin.is_file() {
        return unavailable(
            ProviderId::Claude,
            now,
            "Claude CLI が見つかりません。設定の claude_bin を確認してください。",
        );
    }
    let mut cmd = prepare_probe_cmd(bin);
    cmd.args([
        "--print",
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--setting-sources",
        "",
    ]);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return unavailable(
                ProviderId::Claude,
                now,
                &format!("Claude CLI の起動に失敗しました: {error}"),
            );
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill().await;
        return unavailable(ProviderId::Claude, now, "Claude CLI の標準入力を開けません");
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill().await;
        return unavailable(ProviderId::Claude, now, "Claude CLI の標準出力を開けません");
    };

    let req = json!({
        "type": "control_request",
        "request_id": "t3_limits",
        "request": { "subtype": "get_usage" }
    });
    let mut payload = match serde_json::to_vec(&req) {
        Ok(v) => v,
        Err(e) => {
            let _ = child.kill().await;
            return unavailable(ProviderId::Claude, now, &e.to_string());
        }
    };
    payload.push(b'\n');

    let result = tokio::time::timeout(CLAUDE_PROBE_TIMEOUT, async {
        stdin
            .write_all(&payload)
            .await
            .map_err(|e| CoreError::Provider(format!("Claude stdin write failed: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| CoreError::Provider(format!("Claude stdin flush failed: {e}")))?;
        let mut reader = AsyncBufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let read = read_capped_line(&mut reader, &mut buf).await?;
            if read == 0 {
                return Err(CoreError::Provider(
                    "Claude CLI が上限応答を返さずに終了しました".into(),
                ));
            }
            if buf.len() > MAX_LINE_BYTES {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&buf) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("control_response") {
                continue;
            }
            let Some(resp) = value.get("response") else {
                continue;
            };
            if resp.get("request_id").and_then(Value::as_str) != Some("t3_limits") {
                continue;
            }
            return Ok(parse_claude_usage_response(resp, now));
        }
    })
    .await;

    drop(stdin);
    let _ = child.kill().await;
    let _ = child.wait().await;

    match result {
        Ok(Ok(limits)) => limits,
        Ok(Err(err)) => unavailable(ProviderId::Claude, now, &err.to_string()),
        Err(_) => unavailable(
            ProviderId::Claude,
            now,
            "Claude CLI からの上限取得がタイムアウトしました",
        ),
    }
}

async fn probe_codex(bin: &Path, home: Option<&Path>, now: DateTime<Utc>) -> ProviderUsageLimits {
    if bin.is_file() {
        match probe_codex_app_server(bin, now).await {
            Ok(limits) if !limits.windows.is_empty() => return limits,
            Ok(limits) => {
                if let Some(home) = home
                    && let Some(fallback) = read_codex_session_log_limits(home, now)
                {
                    return fallback;
                }
                return limits;
            }
            Err(err) => {
                if let Some(home) = home
                    && let Some(fallback) = read_codex_session_log_limits(home, now)
                {
                    return fallback;
                }
                return unavailable(ProviderId::Codex, now, &err);
            }
        }
    }
    if let Some(home) = home
        && let Some(fallback) = read_codex_session_log_limits(home, now)
    {
        return fallback;
    }
    unavailable(
        ProviderId::Codex,
        now,
        "Codex CLI が見つかりません。設定の codex_bin を確認してください。",
    )
}

async fn probe_codex_app_server(
    bin: &Path,
    now: DateTime<Utc>,
) -> Result<ProviderUsageLimits, String> {
    let mut cmd = prepare_probe_cmd(bin);
    cmd.arg("app-server");
    let mut child = cmd
        .spawn()
        .map_err(|error| format!("Codex CLI の起動に失敗しました: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Codex CLI の標準入力を開けません".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Codex CLI の標準出力を開けません".to_owned())?;

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "t3code-rs", "version": crate::VERSION }
        }
    });
    let read_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "account/rateLimits/read",
        "params": {}
    });
    let mut init_line = serde_json::to_vec(&init).map_err(|e| e.to_string())?;
    init_line.push(b'\n');
    let mut read_line = serde_json::to_vec(&read_req).map_err(|e| e.to_string())?;
    read_line.push(b'\n');

    let result = tokio::time::timeout(CODEX_PROBE_TIMEOUT, async {
        stdin
            .write_all(&init_line)
            .await
            .map_err(|e| format!("Codex initialize write failed: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("Codex initialize flush failed: {e}"))?;

        let mut reader = AsyncBufReader::new(stdout);
        let mut buf = Vec::new();
        let mut requested = false;
        loop {
            buf.clear();
            let read = read_capped_line(&mut reader, &mut buf)
                .await
                .map_err(|e| e.to_string())?;
            if read == 0 {
                return Err("Codex app-server が上限応答を返さずに終了しました".into());
            }
            if buf.len() > MAX_LINE_BYTES {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&buf) else {
                continue;
            };
            if !requested && value.get("id").and_then(Value::as_i64) == Some(1) {
                requested = true;
                stdin
                    .write_all(&read_line)
                    .await
                    .map_err(|e| format!("Codex rateLimits/read write failed: {e}"))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| format!("Codex rateLimits/read flush failed: {e}"))?;
                continue;
            }
            if value.get("id").and_then(Value::as_i64) == Some(2) {
                if let Some(error) = value.get("error") {
                    let msg = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Codex の上限照会に失敗しました");
                    return Err(msg.to_owned());
                }
                let result = value.get("result").unwrap_or(&Value::Null);
                return Ok(parse_codex_rate_limits_response(result, now));
            }
        }
    })
    .await;

    drop(stdin);
    let _ = child.kill().await;
    let _ = child.wait().await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err("Codex CLI からの上限取得がタイムアウトしました".into()),
    }
}

async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<usize, CoreError> {
    let read = AsyncReadExt::take(&mut *reader, (MAX_LINE_BYTES + 1) as u64)
        .read_until(b'\n', buf)
        .await
        .map_err(|e| CoreError::Provider(format!("probe stdout read failed: {e}")))?;
    if buf.len() > MAX_LINE_BYTES && buf.last() != Some(&b'\n') {
        let mut discard = Vec::new();
        loop {
            discard.clear();
            let n = reader
                .read_until(b'\n', &mut discard)
                .await
                .map_err(|e| CoreError::Provider(format!("probe stdout skip failed: {e}")))?;
            if n == 0 || discard.last() == Some(&b'\n') {
                break;
            }
        }
    }
    Ok(read)
}

pub(crate) fn parse_claude_usage_response(resp: &Value, now: DateTime<Utc>) -> ProviderUsageLimits {
    let checked_at = iso(now);
    if resp.get("subtype").and_then(Value::as_str) != Some("success") {
        let msg = resp
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Claude CLI の上限照会に失敗しました");
        return unavailable(ProviderId::Claude, now, msg);
    }
    let body = resp.get("response").unwrap_or(resp);
    let plan = body
        .get("subscription_type")
        .and_then(Value::as_str)
        .and_then(format_plan_name);
    let available = body
        .get("rate_limits_available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(limits) = body.get("rate_limits").filter(|v| v.is_object()) else {
        return ProviderUsageLimits {
            provider: provider_key(ProviderId::Claude).into(),
            label: provider_label(ProviderId::Claude).into(),
            plan,
            status: if available {
                LimitStatus::Unavailable
            } else {
                LimitStatus::Unsupported
            },
            source: LimitSource::ClaudeCli,
            checked_at,
            message: Some(if available {
                "Claude CLI から上限データを取得できませんでした。".into()
            } else {
                "現在の Claude 認証（API キー等）ではサブスクリプション上限が表示されません。"
                    .into()
            }),
            windows: Vec::new(),
        };
    };

    let mut windows = Vec::new();
    let mut seen_ids = HashSet::new();

    if let Some(w) = parse_claude_window(
        limits.get("five_hour"),
        "five_hour",
        WindowKind::Session,
        "セッション (5時間)",
        Some(SESSION_MINS),
    ) {
        seen_ids.insert(w.id.clone());
        windows.push(w);
    }
    if let Some(w) = parse_claude_window(
        limits.get("seven_day"),
        "seven_day",
        WindowKind::Weekly,
        "週間",
        Some(WEEK_MINS),
    ) {
        seen_ids.insert(w.id.clone());
        windows.push(w);
    }
    for (field, label) in [
        ("seven_day_sonnet", "週間 · Sonnet"),
        ("seven_day_opus", "週間 · Opus"),
    ] {
        if let Some(w) = parse_claude_window(
            limits.get(field),
            field,
            WindowKind::Weekly,
            label,
            Some(WEEK_MINS),
        ) && seen_ids.insert(w.id.clone())
        {
            windows.push(w);
        }
    }
    if let Some(scoped) = limits.get("model_scoped").and_then(Value::as_array) {
        for entry in scoped {
            let Some(name) = entry
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let id = format!("seven_day_{}", slug(name));
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            if let Some(w) = parse_claude_window(
                Some(entry),
                &id,
                WindowKind::Weekly,
                &format!("週間 · {name}"),
                Some(WEEK_MINS),
            ) {
                windows.push(w);
            }
        }
    }

    if let Some(spend_pct) = parse_claude_spend_percent(limits) {
        windows.push(make_window(
            "extra_usage".into(),
            WindowKind::Monthly,
            "追加利用枠".into(),
            spend_pct,
            None,
            None,
        ));
    }

    if windows.is_empty() && !available {
        return ProviderUsageLimits {
            provider: provider_key(ProviderId::Claude).into(),
            label: provider_label(ProviderId::Claude).into(),
            plan,
            status: LimitStatus::Unsupported,
            source: LimitSource::ClaudeCli,
            checked_at,
            message: Some("利用可能な上限ウィンドウがありません。".into()),
            windows,
        };
    }

    ProviderUsageLimits {
        provider: provider_key(ProviderId::Claude).into(),
        label: provider_label(ProviderId::Claude).into(),
        plan,
        status: LimitStatus::Ok,
        source: LimitSource::ClaudeCli,
        checked_at,
        message: None,
        windows,
    }
}

fn parse_claude_window(
    raw: Option<&Value>,
    id: &str,
    kind: WindowKind,
    label: &str,
    duration_mins: Option<u32>,
) -> Option<UsageLimitWindow> {
    let obj = raw?.as_object()?;
    let util = obj.get("utilization").and_then(Value::as_f64)?;
    if !util.is_finite() {
        return None;
    }
    let resets_at = obj
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(normalize_iso);
    Some(make_window(
        id.into(),
        kind,
        label.into(),
        util,
        duration_mins,
        resets_at,
    ))
}

fn parse_claude_spend_percent(limits: &Value) -> Option<f64> {
    if let Some(spend) = limits.get("spend").and_then(Value::as_object)
        && spend.get("enabled").and_then(Value::as_bool) == Some(true)
    {
        let used = spend
            .get("used")
            .and_then(|u| u.get("amount_minor"))
            .and_then(Value::as_f64);
        let cap = spend
            .get("limit")
            .and_then(|l| l.get("amount_minor"))
            .and_then(Value::as_f64);
        let used_cur = spend
            .get("used")
            .and_then(|u| u.get("currency"))
            .and_then(Value::as_str);
        let cap_cur = spend
            .get("limit")
            .and_then(|l| l.get("currency"))
            .and_then(Value::as_str);
        let used_exp = spend
            .get("used")
            .and_then(|u| u.get("exponent"))
            .and_then(Value::as_i64);
        let cap_exp = spend
            .get("limit")
            .and_then(|l| l.get("exponent"))
            .and_then(Value::as_i64);
        if let (Some(u), Some(l)) = (used, cap)
            && l > 0.0
            && u >= 0.0
            && used_cur.is_some()
            && used_cur == cap_cur
            && used_exp == cap_exp
        {
            return Some((u / l) * 100.0);
        }
    }
    if let Some(extra) = limits.get("extra_usage").and_then(Value::as_object)
        && extra.get("is_enabled").and_then(Value::as_bool) == Some(true)
    {
        let used = extra.get("used_credits").and_then(Value::as_f64);
        let cap = extra.get("monthly_limit").and_then(Value::as_f64);
        if let (Some(u), Some(l)) = (used, cap)
            && l > 0.0
            && u >= 0.0
        {
            return Some((u / l) * 100.0);
        }
        if let Some(util) = extra.get("utilization").and_then(Value::as_f64)
            && util.is_finite()
            && util >= 0.0
        {
            return Some(util);
        }
    }
    None
}

pub(crate) fn parse_codex_rate_limits_response(
    result: &Value,
    now: DateTime<Utc>,
) -> ProviderUsageLimits {
    let snapshot = result
        .get("rateLimitsByLimitId")
        .and_then(|m| m.get("codex"))
        .filter(|v| v.is_object())
        .or_else(|| result.get("rateLimits").filter(|v| v.is_object()));

    let Some(snapshot) = snapshot else {
        return ProviderUsageLimits {
            provider: provider_key(ProviderId::Codex).into(),
            label: provider_label(ProviderId::Codex).into(),
            plan: None,
            status: LimitStatus::Unsupported,
            source: LimitSource::CodexAppServer,
            checked_at: iso(now),
            message: Some("Codex から上限情報が返されませんでした。".into()),
            windows: Vec::new(),
        };
    };

    parse_codex_snapshot(snapshot, LimitSource::CodexAppServer, now)
}

pub(crate) fn parse_codex_snapshot(
    snapshot: &Value,
    source: LimitSource,
    checked_at: DateTime<Utc>,
) -> ProviderUsageLimits {
    let limit_id = snapshot
        .get("limitId")
        .or_else(|| snapshot.get("limit_id"))
        .and_then(Value::as_str);
    let plan_raw = snapshot
        .get("planType")
        .or_else(|| snapshot.get("plan_type"))
        .and_then(Value::as_str);
    let plan = plan_raw.and_then(format_plan_name);

    if let Some(id) = limit_id
        && id != "codex"
    {
        return ProviderUsageLimits {
            provider: provider_key(ProviderId::Codex).into(),
            label: provider_label(ProviderId::Codex).into(),
            plan,
            status: LimitStatus::Unsupported,
            source,
            checked_at: iso(checked_at),
            message: Some("Codex の標準上限枠が含まれていません。".into()),
            windows: Vec::new(),
        };
    }

    let is_monthly_plan = matches!(plan_raw, Some("free" | "go"));
    let mut windows = Vec::new();
    for (id, field, fallback_mins) in [
        (
            "primary",
            "primary",
            if is_monthly_plan {
                MONTH_MINS
            } else {
                SESSION_MINS
            },
        ),
        ("secondary", "secondary", WEEK_MINS),
    ] {
        let Some(win) = snapshot.get(field).filter(|v| v.is_object()) else {
            continue;
        };
        let Some(used) = win
            .get("usedPercent")
            .or_else(|| win.get("used_percent"))
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
        else {
            continue;
        };
        let duration_mins = win
            .get("windowDurationMins")
            .or_else(|| win.get("window_minutes"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0)
            .unwrap_or(fallback_mins);
        let kind = kind_for_duration(duration_mins);
        let label = label_for_duration(kind, duration_mins);
        let resets_at = win
            .get("resetsAt")
            .or_else(|| win.get("resets_at"))
            .and_then(parse_epoch_or_iso);
        windows.push(make_window(
            id.into(),
            kind,
            label,
            used,
            Some(duration_mins),
            resets_at,
        ));
    }

    if let Some(individual) = snapshot
        .get("individualLimit")
        .or_else(|| snapshot.get("individual_limit"))
        .filter(|v| v.is_object())
        && let Some(rem) = individual
            .get("remainingPercent")
            .or_else(|| individual.get("remaining_percent"))
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
    {
        let resets_at = individual
            .get("resetsAt")
            .or_else(|| individual.get("resets_at"))
            .and_then(parse_epoch_or_iso);
        windows.push(make_window(
            "individual".into(),
            WindowKind::Monthly,
            "月間クレジット上限".into(),
            100.0 - rem,
            None,
            resets_at,
        ));
    }

    ProviderUsageLimits {
        provider: provider_key(ProviderId::Codex).into(),
        label: provider_label(ProviderId::Codex).into(),
        plan,
        status: if windows.is_empty() {
            LimitStatus::Unsupported
        } else {
            LimitStatus::Ok
        },
        source,
        checked_at: iso(checked_at),
        message: if windows.is_empty() {
            Some("Codex の上限ウィンドウが空です。".into())
        } else {
            None
        },
        windows,
    }
}

pub(crate) fn read_codex_session_log_limits(
    home: &Path,
    now: DateTime<Utc>,
) -> Option<ProviderUsageLimits> {
    let sessions_root = home.join(".codex").join("sessions");
    if !fs::symlink_metadata(&sessions_root).is_ok_and(|meta| meta.is_dir()) {
        return None;
    }
    let mut files: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut stack = vec![sessions_root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && path.extension().is_some_and(|ext| ext == "jsonl")
                && let Ok(meta) = entry.metadata()
            {
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((modified, path));
            }
        }
    }
    files.sort_by_key(|a| std::cmp::Reverse(a.0));

    let mut best: Option<(DateTime<Utc>, ProviderUsageLimits)> = None;
    for (_, path) in files.into_iter().take(CODEX_LOG_SCAN_FILES) {
        let Some((ts, limits)) = scan_codex_file_for_limits(&path, now) else {
            continue;
        };
        if best.as_ref().is_none_or(|(best_ts, _)| ts > *best_ts) {
            best = Some((ts, limits));
        }
    }
    best.map(|(_, limits)| limits)
}

fn scan_codex_file_for_limits(
    path: &Path,
    fallback_now: DateTime<Utc>,
) -> Option<(DateTime<Utc>, ProviderUsageLimits)> {
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(crate::usage::transcript_open_flags().ok()?);
    }
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut latest: Option<(DateTime<Utc>, ProviderUsageLimits)> = None;
    loop {
        line.clear();
        let n = reader
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .ok()?;
        if n == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            if line.last() != Some(&b'\n') {
                let _ = reader.skip_until(b'\n');
            }
            continue;
        }
        if !line
            .windows(b"rate_limits".len())
            .any(|w| w == b"rate_limits")
        {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        let Some(rl) = payload.get("rate_limits").filter(|v| v.is_object()) else {
            continue;
        };
        let ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<DateTime<Utc>>().ok())
            .unwrap_or(fallback_now);
        let parsed = parse_codex_snapshot(rl, LimitSource::CodexSessionLog, ts);
        if !parsed.windows.is_empty() {
            latest = Some((ts, parsed));
        }
    }
    latest
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

fn kind_for_duration(mins: u32) -> WindowKind {
    if mins >= MONTH_MINS {
        WindowKind::Monthly
    } else if mins >= WEEK_MINS {
        WindowKind::Weekly
    } else {
        WindowKind::Session
    }
}

fn label_for_duration(kind: WindowKind, mins: u32) -> String {
    match kind {
        WindowKind::Session => {
            if mins == SESSION_MINS {
                "セッション (5時間)".into()
            } else if mins >= 60 && mins.is_multiple_of(60) {
                format!("セッション ({}時間)", mins / 60)
            } else {
                "セッション".into()
            }
        }
        WindowKind::Weekly => "週間".into(),
        WindowKind::Monthly => "月間".into(),
        WindowKind::Other => "上限".into(),
    }
}

fn format_plan_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let formatted = match trimmed.to_ascii_lowercase().as_str() {
        "pro" => "Pro".to_owned(),
        "plus" => "Plus".to_owned(),
        "max" => "Max".to_owned(),
        "ultra" => "Ultra".to_owned(),
        "free" => "Free".to_owned(),
        "go" => "Go".to_owned(),
        "team" => "Team".to_owned(),
        "enterprise" => "Enterprise".to_owned(),
        _ => {
            let mut chars = trimmed.chars();
            let first = chars.next()?;
            format!("{}{}", first.to_uppercase(), chars.as_str())
        }
    };
    Some(formatted)
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut prev_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !prev_underscore && !out.is_empty() {
            out.push('_');
            prev_underscore = true;
        }
    }
    out.trim_end_matches('_').to_owned()
}

fn parse_epoch_or_iso(value: &Value) -> Option<String> {
    if let Some(secs) = value.as_i64()
        && secs > 0
    {
        return Utc.timestamp_opt(secs, 0).single().map(iso);
    }
    if let Some(secs) = value.as_f64()
        && secs.is_finite()
        && secs > 0.0
    {
        return Utc.timestamp_opt(secs as i64, 0).single().map(iso);
    }
    value.as_str().and_then(normalize_iso)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_get_usage_windows_and_plan() {
        let now = Utc.with_ymd_and_hms(2026, 11, 25, 6, 0, 0).unwrap();
        let resp = json!({
            "subtype": "success",
            "request_id": "t3_limits",
            "response": {
                "subscription_type": "pro",
                "rate_limits_available": true,
                "rate_limits": {
                    "five_hour": {
                        "utilization": 50,
                        "resets_at": "2026-11-25T08:00:00.459482+00:00"
                    },
                    "seven_day": {
                        "utilization": 57.4,
                        "resets_at": "2026-11-27T00:00:00.459503+00:00"
                    },
                    "model_scoped": [
                        {
                            "display_name": "Sonnet",
                            "utilization": 12.0,
                            "resets_at": "2026-11-27T00:00:00Z"
                        }
                    ],
                    "spend": {
                        "enabled": true,
                        "used": { "amount_minor": 2500, "currency": "USD", "exponent": 2 },
                        "limit": { "amount_minor": 10000, "currency": "USD", "exponent": 2 }
                    }
                }
            }
        });
        let parsed = parse_claude_usage_response(&resp, now);
        assert_eq!(parsed.status, LimitStatus::Ok);
        assert_eq!(parsed.plan.as_deref(), Some("Pro"));
        assert_eq!(parsed.source, LimitSource::ClaudeCli);
        assert_eq!(parsed.windows.len(), 4);
        assert_eq!(parsed.windows[0].id, "five_hour");
        assert_eq!(parsed.windows[0].used_percent, 50.0);
        assert_eq!(parsed.windows[0].remaining_percent, 50.0);
        assert_eq!(parsed.windows[0].window_duration_mins, Some(300));
        assert_eq!(
            parsed.windows[0].resets_at.as_deref(),
            Some("2026-11-25T08:00:00Z")
        );
        assert_eq!(parsed.windows[1].id, "seven_day");
        assert_eq!(parsed.windows[1].remaining_percent, 42.6);
        assert_eq!(parsed.windows[2].id, "seven_day_sonnet");
        assert_eq!(parsed.windows[2].label, "週間 · Sonnet");
        assert_eq!(parsed.windows[3].id, "extra_usage");
        assert_eq!(parsed.windows[3].used_percent, 25.0);
    }

    #[test]
    fn parses_codex_app_server_rate_limits() {
        let now = Utc.with_ymd_and_hms(2026, 11, 25, 6, 0, 0).unwrap();
        let result = json!({
            "ordinaryUsageAllowed": false,
            "rateLimits": {
                "limitId": "codex",
                "primary": { "usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1795593846 },
                "secondary": { "usedPercent": 80, "windowDurationMins": 10080, "resetsAt": 1795702939 },
                "planType": "plus"
            }
        });
        let parsed = parse_codex_rate_limits_response(&result, now);
        assert_eq!(parsed.status, LimitStatus::Ok);
        assert_eq!(parsed.plan.as_deref(), Some("Plus"));
        assert_eq!(parsed.source, LimitSource::CodexAppServer);
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0].used_percent, 100.0);
        assert_eq!(parsed.windows[0].remaining_percent, 0.0);
        assert_eq!(parsed.windows[0].kind, WindowKind::Session);
        assert_eq!(parsed.windows[1].used_percent, 80.0);
        assert_eq!(parsed.windows[1].remaining_percent, 20.0);
        assert_eq!(parsed.windows[1].kind, WindowKind::Weekly);
    }

    #[test]
    fn falls_back_to_codex_session_jsonl_rate_limits() {
        let dir = std::env::temp_dir().join(format!("t3-limits-test-{}", uuid::Uuid::new_v4()));
        let sess = dir.join(".codex/sessions/2026/11/25");
        fs::create_dir_all(&sess).unwrap();
        let path = sess.join("rollout.jsonl");
        let lines = [
            json!({
                "timestamp": "2026-11-25T03:04:05Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "rate_limits": {
                        "limit_id": "codex",
                        "plan_type": "plus",
                        "primary": { "used_percent": 25.0, "window_minutes": 300, "resets_at": 1795593843 },
                        "secondary": { "used_percent": 64.0, "window_minutes": 10080, "resets_at": 1795702939 }
                    }
                }
            })
            .to_string(),
            json!({
                "timestamp": "2026-11-25T05:12:32Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "rate_limits": {
                        "limit_id": "premium",
                        "plan_type": "plus",
                        "primary": null,
                        "secondary": null
                    }
                }
            })
            .to_string(),
        ]
        .join("\n");
        fs::write(&path, lines).unwrap();

        let now = Utc.with_ymd_and_hms(2026, 11, 25, 6, 0, 0).unwrap();
        let parsed = read_codex_session_log_limits(&dir, now).expect("codex log fallback");
        assert_eq!(parsed.source, LimitSource::CodexSessionLog);
        assert_eq!(parsed.plan.as_deref(), Some("Plus"));
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0].remaining_percent, 75.0);
        assert_eq!(parsed.windows[1].remaining_percent, 36.0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn runtime_limit_overlays_unsupported_provider() {
        let cache = UsageLimitsCache::default();
        cache.record_runtime_limit(
            ProviderId::Antigravity,
            Some(120),
            "Rate limited by Antigravity",
        );
        let resp = UsageLimitsResponse {
            checked_at: iso(Utc::now()),
            providers: vec![ProviderUsageLimits {
                provider: "antigravity".into(),
                label: "Antigravity".into(),
                plan: None,
                status: LimitStatus::Unsupported,
                source: LimitSource::None,
                checked_at: iso(Utc::now()),
                message: Some("unsupported".into()),
                windows: Vec::new(),
            }],
        };
        let merged = cache.overlay_runtime(resp);
        assert_eq!(merged.providers[0].status, LimitStatus::Ok);
        assert_eq!(merged.providers[0].source, LimitSource::Runtime);
        assert_eq!(merged.providers[0].windows.len(), 1);
        assert_eq!(merged.providers[0].windows[0].remaining_percent, 0.0);
        assert!(merged.providers[0].windows[0].resets_at.is_some());
    }
}
