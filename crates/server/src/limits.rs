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
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};
use t3_core::{CoreError, ProviderId, ServerConfig};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader as AsyncBufReader},
    process::Command,
    sync::Mutex as AsyncMutex,
};
use uuid::Uuid;

use crate::{ServerState, routes::ApiResult};

const LIMITS_CACHE_TTL: StdDuration = StdDuration::from_secs(60);
const CLAUDE_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const CODEX_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(8);
const CODEX_RESET_TIMEOUT: StdDuration = StdDuration::from_secs(20);
const ANTIGRAVITY_HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const KEYCHAIN_TIMEOUT: StdDuration = StdDuration::from_secs(3);
pub(crate) const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const CODEX_LOG_SCAN_FILES: usize = 16;
const SESSION_MINS: u32 = 5 * 60;
const WEEK_MINS: u32 = 7 * 24 * 60;
const MONTH_MINS: u32 = 30 * 24 * 60;

const ANTIGRAVITY_DAILY_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
const ANTIGRAVITY_PROD_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
const GOOGLE_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

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

pub(crate) fn prepare_probe_cmd(bin: &Path) -> Command {
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

// ───────────────────────── Antigravity probe ─────────────────────────

fn antigravity_user_agent() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("antigravity/hub/2.24.0 (aidev_client; os_type={os}; arch={arch}; cl=963137146)")
}

#[derive(Deserialize)]
struct AgyAcpTokenFile {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    project_id: Option<String>,
}

#[derive(Deserialize)]
struct AgyCliTokenFile {
    token: AgyCliTokenInner,
}

#[derive(Deserialize)]
struct AgyCliTokenInner {
    access_token: String,
    expiry: Option<String>,
}

#[derive(Deserialize)]
struct GoogleTokenRefreshResponse {
    access_token: String,
}

struct ResolvedAntigravityAuth {
    access_token: String,
    project_id: Option<String>,
}

async fn probe_antigravity(
    config: &ServerConfig,
    antigravity_quota_probe: bool,
    now: DateTime<Utc>,
) -> ProviderUsageLimits {
    if !antigravity_quota_probe {
        return ProviderUsageLimits {
            provider: provider_key(ProviderId::Antigravity).into(),
            label: provider_label(ProviderId::Antigravity).into(),
            plan: None,
            status: LimitStatus::Unsupported,
            source: LimitSource::None,
            checked_at: iso(now),
            message: Some(
                "設定で antigravity_quota_probe = true を有効にすると Antigravity のクォータを表示できます。"
                    .into(),
            ),
            reset_credits: None,
            windows: Vec::new(),
        };
    }

    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(ANTIGRAVITY_HTTP_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return unavailable(
                ProviderId::Antigravity,
                now,
                &format!("HTTP クライアントの初期化に失敗しました: {e}"),
            );
        }
    };

    let auth = match resolve_antigravity_auth(&client, &config.antigravity_dir(), now).await {
        Ok(auth) => auth,
        Err(msg) => return unavailable(ProviderId::Antigravity, now, &msg),
    };

    let endpoints: Vec<String> = match config.providers.antigravity_quota_endpoints.as_deref() {
        Some(list) if !list.is_empty() => list
            .iter()
            .map(|e| e.trim_end_matches('/').to_owned())
            .collect(),
        _ => vec![
            ANTIGRAVITY_DAILY_ENDPOINT.to_owned(),
            ANTIGRAVITY_PROD_ENDPOINT.to_owned(),
        ],
    };
    let ua = antigravity_user_agent();
    let body = match auth.project_id.as_deref() {
        Some(project) if !project.is_empty() => json!({ "project": project }),
        _ => json!({}),
    };

    let summary_windows = fetch_antigravity_operation(
        &client,
        &endpoints,
        "retrieveUserQuotaSummary",
        &auth.access_token,
        &ua,
        &body,
        parse_antigravity_summary_windows,
    )
    .await;

    let mut windows = if !summary_windows.is_empty() {
        summary_windows
    } else {
        fetch_antigravity_operation(
            &client,
            &endpoints,
            "fetchAvailableModels",
            &auth.access_token,
            &ua,
            &body,
            |value, _req_at| parse_antigravity_model_windows(value),
        )
        .await
    };

    if windows.is_empty() {
        return unavailable(
            ProviderId::Antigravity,
            now,
            "Antigravity から有効なクォータ情報を取得できませんでした。再更新または Antigravity で 1 ターン実行してください。",
        );
    }

    sort_antigravity_windows(&mut windows);
    ProviderUsageLimits {
        provider: provider_key(ProviderId::Antigravity).into(),
        label: provider_label(ProviderId::Antigravity).into(),
        plan: None,
        status: LimitStatus::Ok,
        source: LimitSource::AntigravityApi,
        checked_at: iso(now),
        message: None,
        reset_credits: None,
        windows,
    }
}

async fn resolve_antigravity_auth(
    client: &reqwest::Client,
    dir: &Path,
    now: DateTime<Utc>,
) -> Result<ResolvedAntigravityAuth, String> {
    let acp_token_path = dir.join("acp_token.json");
    if let Ok(raw) = read_regular_text(&acp_token_path)
        && let Ok(parsed) = serde_json::from_str::<AgyAcpTokenFile>(&raw)
    {
        let resp = client
            .post(GOOGLE_OAUTH_TOKEN_URL)
            .form(&[
                ("client_id", parsed.client_id.as_str()),
                ("client_secret", parsed.client_secret.as_str()),
                ("refresh_token", parsed.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| format!("Antigravity トークン更新に失敗しました: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!(
                "Antigravity トークン更新が拒否されました (HTTP {})",
                resp.status()
            ));
        }
        let body: GoogleTokenRefreshResponse = resp
            .json()
            .await
            .map_err(|e| format!("Antigravity トークン応答の解析に失敗しました: {e}"))?;
        return Ok(ResolvedAntigravityAuth {
            access_token: body.access_token,
            project_id: parsed.project_id,
        });
    }

    let mut saw_expired = false;
    let cli_token_path = dir.join("antigravity-oauth-token");
    if let Ok(raw) = read_regular_text(&cli_token_path)
        && let Ok(parsed) = serde_json::from_str::<AgyCliTokenFile>(&raw)
    {
        if is_token_fresh(parsed.token.expiry.as_deref(), now) {
            return Ok(ResolvedAntigravityAuth {
                access_token: parsed.token.access_token,
                project_id: None,
            });
        }
        saw_expired = true;
    }

    #[cfg(target_os = "macos")]
    if let Some(raw) = read_macos_antigravity_keychain().await
        && let Ok(parsed) = serde_json::from_str::<AgyCliTokenFile>(&raw)
    {
        if is_token_fresh(parsed.token.expiry.as_deref(), now) {
            return Ok(ResolvedAntigravityAuth {
                access_token: parsed.token.access_token,
                project_id: None,
            });
        }
        saw_expired = true;
    }

    if saw_expired {
        Err("Antigravity のサインイン期限が切れています。Antigravity で 1 ターン実行すると自動更新されます。".into())
    } else {
        Err(
            "Antigravity の認証トークンが見つかりません。agy で Google サインインしてください。"
                .into(),
        )
    }
}

fn is_token_fresh(expiry: Option<&str>, now: DateTime<Utc>) -> bool {
    let Some(raw) = expiry else {
        return false;
    };
    raw.parse::<DateTime<Utc>>()
        .is_ok_and(|exp| exp > now + Duration::seconds(60))
}

fn read_regular_text(path: &Path) -> std::io::Result<String> {
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(crate::usage::transcript_open_flags()?);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut buf = String::new();
    file.by_ref().take(64 * 1024).read_to_string(&mut buf)?;
    Ok(buf)
}

#[cfg(target_os = "macos")]
async fn read_macos_antigravity_keychain() -> Option<String> {
    let mut cmd = Command::new("/usr/bin/security");
    cmd.args([
        "find-generic-password",
        "-s",
        "gemini",
        "-a",
        "antigravity",
        "-w",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    let output = tokio::time::timeout(KEYCHAIN_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    decode_go_keyring_secret(raw.trim())
}

pub(crate) fn decode_go_keyring_secret(raw: &str) -> Option<String> {
    if let Some(b64) = raw.strip_prefix("go-keyring-base64:") {
        let bytes = BASE64.decode(b64.trim()).ok()?;
        return String::from_utf8(bytes).ok();
    }
    if let Some(hex) = raw.strip_prefix("go-keyring-encoded:") {
        let bytes = decode_hex(hex.trim())?;
        return String::from_utf8(bytes).ok();
    }
    Some(raw.to_owned())
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    let (pairs, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    for pair in pairs {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

async fn fetch_antigravity_operation<F>(
    client: &reqwest::Client,
    endpoints: &[String],
    operation: &str,
    access_token: &str,
    user_agent: &str,
    body: &Value,
    parse: F,
) -> Vec<UsageLimitWindow>
where
    F: Fn(&Value, DateTime<Utc>) -> Vec<UsageLimitWindow> + Copy,
{
    let futures = endpoints.iter().map(|endpoint| async move {
        let requested_at = Utc::now();
        let url = format!("{endpoint}/v1internal:{operation}");
        let resp = client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("User-Agent", user_agent)
            .json(body)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let value: Value = resp.json().await.ok()?;
        Some(parse(&value, requested_at))
    });
    let results = futures_util::future::join_all(futures).await;
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for list in results.into_iter().flatten() {
        for win in list {
            if seen.insert(win.id.clone()) {
                merged.push(win);
            }
        }
    }
    merged
}

pub(crate) fn parse_antigravity_summary_windows(
    value: &Value,
    requested_at: DateTime<Utc>,
) -> Vec<UsageLimitWindow> {
    let mut groups: Vec<(Option<&str>, &[Value])> = Vec::new();
    if let Some(arr) = value.get("groups").and_then(Value::as_array) {
        let any_buckets = arr.iter().any(|g| {
            g.get("buckets")
                .and_then(Value::as_array)
                .is_some_and(|b| !b.is_empty())
        });
        if any_buckets {
            for g in arr {
                let name = g.get("displayName").and_then(Value::as_str);
                if let Some(buckets) = g.get("buckets").and_then(Value::as_array) {
                    groups.push((name, buckets.as_slice()));
                }
            }
        }
    }
    if groups.is_empty()
        && let Some(buckets) = value.get("buckets").and_then(Value::as_array)
    {
        groups.push((None, buckets.as_slice()));
    }

    let mut seen = HashSet::new();
    let mut windows = Vec::new();
    for (group_name, buckets) in groups {
        for bucket in buckets {
            if bucket.get("disabled").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let Some(bucket_id) = bucket
                .get("bucketId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let Some(rem_frac) = bucket
                .get("remainingFraction")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
            else {
                continue;
            };
            let family = if bucket_id.starts_with("gemini-argon-") {
                group_name
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Gemini Argon")
            } else if bucket_id.starts_with("gemini-carbon-") {
                group_name
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Gemini Carbon")
            } else if bucket_id.starts_with("gemini-") {
                "Gemini"
            } else if bucket_id.starts_with("3p-") {
                "Claude & GPT (shared)"
            } else {
                group_name
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(bucket_id)
            };

            let window_tag = bucket.get("window").and_then(Value::as_str);
            let (kind, duration_mins, suffix) = match window_tag {
                Some("5h") => (WindowKind::Session, Some(SESSION_MINS), "5-hour"),
                Some("weekly") => (WindowKind::Weekly, Some(WEEK_MINS), "Weekly"),
                _ => (
                    WindowKind::Other,
                    None,
                    bucket
                        .get("displayName")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("Quota"),
                ),
            };
            let resets_at =
                antigravity_summary_reset_time(bucket, rem_frac, duration_mins, requested_at);
            let used = (1.0 - rem_frac.clamp(0.0, 1.0)) * 100.0;
            if seen.insert(bucket_id.to_owned()) {
                windows.push(make_window(
                    bucket_id.to_owned(),
                    kind,
                    format!("{family} · {suffix}"),
                    used,
                    duration_mins,
                    resets_at,
                ));
            }
        }
    }
    windows
}

fn antigravity_summary_reset_time(
    bucket: &Value,
    remaining_fraction: f64,
    duration_mins: Option<u32>,
    requested_at: DateTime<Utc>,
) -> Option<String> {
    let raw = bucket.get("resetTime").and_then(Value::as_str)?;
    let parsed = raw.parse::<DateTime<Utc>>().ok()?;
    let description_missing = bucket.get("description").and_then(Value::as_str).is_none();
    if (remaining_fraction - 1.0).abs() < f64::EPSILON
        && description_missing
        && let Some(mins) = duration_mins
    {
        let expected = requested_at + Duration::minutes(i64::from(mins));
        if (parsed - expected).num_seconds().abs() <= 60 {
            return None;
        }
    }
    Some(iso(parsed))
}

pub(crate) fn parse_antigravity_model_windows(value: &Value) -> Vec<UsageLimitWindow> {
    let Some(models) = value.get("models").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut by_id: HashMap<String, UsageLimitWindow> = HashMap::new();
    for (model_id, model) in models {
        let provider = model.get("modelProvider").and_then(Value::as_str);
        let family = if provider == Some("MODEL_PROVIDER_GOOGLE") || model_id.starts_with("gemini-")
        {
            "Gemini"
        } else if provider == Some("MODEL_PROVIDER_ANTHROPIC") || model_id.starts_with("claude-") {
            "Claude"
        } else if provider == Some("MODEL_PROVIDER_OPENAI") || model_id.starts_with("gpt-") {
            "GPT"
        } else {
            continue;
        };
        for (suffix, field, kind, duration_mins, label_suffix) in [
            ("quota", "quotaInfo", WindowKind::Other, None, "Quota"),
            (
                "weekly",
                "weeklyQuotaInfo",
                WindowKind::Weekly,
                Some(WEEK_MINS),
                "Weekly",
            ),
        ] {
            let Some(quota) = model.get(field).and_then(Value::as_object) else {
                continue;
            };
            let Some(rem_frac) = quota
                .get("remainingFraction")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
            else {
                continue;
            };
            let resets_at = quota
                .get("resetTime")
                .and_then(Value::as_str)
                .and_then(normalize_iso);
            let id = format!("{}-{suffix}", family.to_ascii_lowercase());
            let used = (1.0 - rem_frac.clamp(0.0, 1.0)) * 100.0;
            let candidate = make_window(
                id.clone(),
                kind,
                format!("{family} · {label_suffix}"),
                used,
                duration_mins,
                resets_at,
            );
            let replace = by_id
                .get(&id)
                .is_none_or(|prev| candidate.used_percent > prev.used_percent);
            if replace {
                by_id.insert(id, candidate);
            }
        }
    }
    by_id.into_values().collect()
}

pub(crate) fn sort_antigravity_windows(windows: &mut [UsageLimitWindow]) {
    windows.sort_by_key(|w| {
        let not_gemini = !w.id.starts_with("gemini-");
        let base =
            w.id.strip_suffix("-5h")
                .or_else(|| w.id.strip_suffix("-weekly"))
                .or_else(|| w.id.strip_suffix("-quota"))
                .unwrap_or(&w.id)
                .to_owned();
        (not_gemini, base, w.kind.sort_order())
    });
}

// ───────────────────────── Claude CLI probe ─────────────────────────

fn resolve_cli_bin(configured: &Path) -> Option<PathBuf> {
    if configured.is_file() {
        return Some(configured.to_path_buf());
    }
    // Only search fallback locations for a bare binary name (not an explicit missing path).
    if configured.components().count() != 1 {
        return None;
    }
    let name = configured.file_name()?;
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Some(home) = std::env::home_dir() {
        let local = home.join(".local/bin").join(name);
        if local.is_file() {
            return Some(local);
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let cand = Path::new(dir).join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

async fn probe_claude(bin: &Path, now: DateTime<Utc>) -> ProviderUsageLimits {
    let Some(resolved) = resolve_cli_bin(bin) else {
        return unavailable(
            ProviderId::Claude,
            now,
            "Claude CLI が見つかりません。設定の claude_bin を確認してください。",
        );
    };
    let mut cmd = prepare_probe_cmd(&resolved);
    // Claude Code 2.1.330+ suppresses `rate_limits` in `get_usage` when
    // CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 is set; keep telemetry,
    // error reporting, and auto-updater explicitly disabled instead.
    cmd.env_remove("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
        .env("DISABLE_ERROR_REPORTING", "1")
        .env("DISABLE_AUTOUPDATER", "1");
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

// ───────────────────────── Codex CLI probe & reset credit ─────────────────────────

async fn probe_codex(bin: &Path, home: Option<&Path>, now: DateTime<Utc>) -> ProviderUsageLimits {
    if let Some(resolved) = resolve_cli_bin(bin) {
        match probe_codex_app_server(&resolved, now).await {
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
    let result = codex_app_server_rpc(
        bin,
        "account/rateLimits/read",
        json!({}),
        CODEX_PROBE_TIMEOUT,
    )
    .await
    .map_err(|e| e.into_message())?;
    Ok(parse_codex_rate_limits_response(&result, now))
}

enum CodexResetError {
    Settled(String),
    Timeout(String),
}

impl CodexResetError {
    fn into_message(self) -> String {
        match self {
            CodexResetError::Settled(m) | CodexResetError::Timeout(m) => m,
        }
    }
}

async fn consume_codex_reset_credit_rpc(
    bin: &Path,
    idempotency_key: &str,
) -> Result<String, CodexResetError> {
    if !bin.is_file() {
        return Err(CodexResetError::Settled(
            "Codex CLI が見つかりません。設定の codex_bin を確認してください。".into(),
        ));
    }
    let result = codex_app_server_rpc(
        bin,
        "account/rateLimitResetCredit/consume",
        json!({ "idempotencyKey": idempotency_key }),
        CODEX_RESET_TIMEOUT,
    )
    .await?;
    let outcome = result
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            CodexResetError::Settled("Codex からリセット結果が返されませんでした。".into())
        })?;
    Ok(outcome.to_owned())
}

async fn codex_app_server_rpc(
    bin: &Path,
    method: &str,
    params: Value,
    timeout: StdDuration,
) -> Result<Value, CodexResetError> {
    let mut cmd = prepare_probe_cmd(bin);
    cmd.arg("app-server");
    let mut child = cmd.spawn().map_err(|error| {
        CodexResetError::Settled(format!("Codex CLI の起動に失敗しました: {error}"))
    })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CodexResetError::Settled("Codex CLI の標準入力を開けません".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CodexResetError::Settled("Codex CLI の標準出力を開けません".into()))?;

    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": { "name": "t3code-rs", "version": crate::VERSION }
        }
    });
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": method,
        "params": params
    });
    let mut init_line =
        serde_json::to_vec(&init).map_err(|e| CodexResetError::Settled(e.to_string()))?;
    init_line.push(b'\n');
    let mut req_line =
        serde_json::to_vec(&req).map_err(|e| CodexResetError::Settled(e.to_string()))?;
    req_line.push(b'\n');

    let result =
        tokio::time::timeout(timeout, async {
            stdin.write_all(&init_line).await.map_err(|e| {
                CodexResetError::Settled(format!("Codex initialize write failed: {e}"))
            })?;
            stdin.flush().await.map_err(|e| {
                CodexResetError::Settled(format!("Codex initialize flush failed: {e}"))
            })?;

            let mut reader = AsyncBufReader::new(stdout);
            let mut buf = Vec::new();
            let mut requested = false;
            loop {
                buf.clear();
                let read = read_capped_line(&mut reader, &mut buf)
                    .await
                    .map_err(|e| CodexResetError::Settled(e.to_string()))?;
                if read == 0 {
                    return Err(CodexResetError::Settled(
                        "Codex app-server が応答を返さずに終了しました".into(),
                    ));
                }
                if buf.len() > MAX_LINE_BYTES {
                    continue;
                }
                let Ok(value) = serde_json::from_slice::<Value>(&buf) else {
                    continue;
                };
                if !requested && value.get("id").and_then(Value::as_i64) == Some(1) {
                    requested = true;
                    stdin.write_all(&req_line).await.map_err(|e| {
                        CodexResetError::Settled(format!("Codex RPC write failed: {e}"))
                    })?;
                    stdin.flush().await.map_err(|e| {
                        CodexResetError::Settled(format!("Codex RPC flush failed: {e}"))
                    })?;
                    continue;
                }
                if value.get("id").and_then(Value::as_i64) == Some(2) {
                    if let Some(error) = value.get("error") {
                        let msg = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("Codex の RPC 実行に失敗しました");
                        return Err(CodexResetError::Settled(msg.to_owned()));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        })
        .await;

    drop(stdin);
    let _ = child.kill().await;
    let _ = child.wait().await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(CodexResetError::Timeout(
            "Codex CLI からの応答がタイムアウトしました".into(),
        )),
    }
}

pub(crate) async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
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

// ───────────────────────── Parsers ─────────────────────────

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
        .and_then(format_claude_plan_name);
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
            reset_credits: None,
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
            reset_credits: None,
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
        reset_credits: None,
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
    let reset_credits = result
        .get("rateLimitResetCredits")
        .and_then(parse_codex_reset_credits);
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
            reset_credits,
            windows: Vec::new(),
        };
    };

    let mut limits = parse_codex_snapshot(snapshot, LimitSource::CodexAppServer, now);
    limits.reset_credits = reset_credits;
    limits
}

fn parse_codex_reset_credits(value: &Value) -> Option<ResetCredits> {
    let obj = value.as_object()?;
    let available_count = obj
        .get("availableCount")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())?;
    let next_expires_at = obj
        .get("credits")
        .and_then(Value::as_array)
        .and_then(|credits| {
            credits
                .iter()
                .filter(|c| c.get("status").and_then(Value::as_str) == Some("available"))
                .filter_map(|c| c.get("expiresAt").and_then(Value::as_i64))
                .filter(|secs| *secs > 0)
                .min()
        })
        .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
        .map(iso);
    Some(ResetCredits {
        available_count,
        next_expires_at,
    })
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
    let plan = plan_raw.and_then(format_codex_plan_name);

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
            reset_credits: None,
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
        reset_credits: None,
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

// ───────────────────────── Helpers ─────────────────────────

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

fn format_claude_plan_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let formatted = match trimmed.to_ascii_lowercase().as_str() {
        "pro" => "Pro".to_owned(),
        "max" => "Max".to_owned(),
        "ultra" => "Ultra".to_owned(),
        "team" => "Team".to_owned(),
        "enterprise" => "Enterprise".to_owned(),
        _ => capitalize(trimmed)?,
    };
    Some(formatted)
}

fn format_codex_plan_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let formatted = match trimmed.to_ascii_lowercase().as_str() {
        "plus" => "ChatGPT Plus Subscription".to_owned(),
        "pro" => "ChatGPT Pro Subscription".to_owned(),
        "free" => "ChatGPT Free".to_owned(),
        "go" => "ChatGPT Go".to_owned(),
        "team" => "ChatGPT Team".to_owned(),
        "enterprise" => "ChatGPT Enterprise".to_owned(),
        _ => capitalize(trimmed)?,
    };
    Some(formatted)
}

fn capitalize(trimmed: &str) -> Option<String> {
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    Some(format!("{}{}", first.to_uppercase(), chars.as_str()))
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
        assert_eq!(parsed.windows[0].remaining_percent, 50.0);
        assert_eq!(parsed.windows[1].remaining_percent, 42.6);
        assert_eq!(parsed.windows[2].id, "seven_day_sonnet");
        assert_eq!(parsed.windows[3].used_percent, 25.0);
    }

    #[test]
    fn parses_codex_app_server_rate_limits_and_reset_credits() {
        let now = Utc.with_ymd_and_hms(2026, 11, 25, 6, 0, 0).unwrap();
        let result = json!({
            "ordinaryUsageAllowed": false,
            "rateLimits": {
                "limitId": "codex",
                "primary": { "usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1795593846 },
                "secondary": { "usedPercent": 80, "windowDurationMins": 10080, "resetsAt": 1795702939 },
                "planType": "plus"
            },
            "rateLimitResetCredits": {
                "availableCount": 3,
                "credits": [
                    { "id": "c1", "status": "used", "expiresAt": 1795000000 },
                    { "id": "c2", "status": "available", "expiresAt": 1796163165 },
                    { "id": "c3", "status": "available", "expiresAt": 1796071038 }
                ]
            }
        });
        let parsed = parse_codex_rate_limits_response(&result, now);
        assert_eq!(parsed.status, LimitStatus::Ok);
        assert_eq!(parsed.plan.as_deref(), Some("ChatGPT Plus Subscription"));
        assert_eq!(parsed.source, LimitSource::CodexAppServer);
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0].remaining_percent, 0.0);
        assert_eq!(parsed.windows[1].remaining_percent, 20.0);
        let credits = parsed.reset_credits.expect("reset credits");
        assert_eq!(credits.available_count, 3);
        assert_eq!(
            credits.next_expires_at.as_deref(),
            Some("2026-11-30T20:37:18Z")
        );
    }

    #[test]
    fn parses_antigravity_summary_buckets_with_untouched_reset_and_sort_order() {
        let requested_at = Utc.with_ymd_and_hms(2026, 11, 25, 6, 0, 0).unwrap();
        let raw = json!({
            "groups": [
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        {
                            "bucketId": "3p-weekly",
                            "window": "weekly",
                            "remainingFraction": 1.0,
                            "resetTime": "2026-12-02T06:00:05Z"
                        },
                        {
                            "bucketId": "3p-5h",
                            "window": "5h",
                            "remainingFraction": 1.0,
                            "resetTime": "2026-11-25T11:00:02Z"
                        }
                    ]
                },
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        {
                            "bucketId": "gemini-weekly",
                            "window": "weekly",
                            "remainingFraction": 0.8,
                            "description": "Used some weekly limit",
                            "resetTime": "2026-12-01T08:28:09Z"
                        },
                        {
                            "bucketId": "gemini-5h",
                            "window": "5h",
                            "remainingFraction": 0.58,
                            "description": "Used some 5h limit",
                            "resetTime": "2026-11-25T08:00:23Z"
                        }
                    ]
                }
            ]
        });
        let mut windows = parse_antigravity_summary_windows(&raw, requested_at);
        sort_antigravity_windows(&mut windows);
        assert_eq!(windows.len(), 4);
        assert_eq!(windows[0].id, "gemini-5h");
        assert_eq!(windows[0].label, "Gemini · 5-hour");
        assert_eq!(windows[0].remaining_percent, 58.0);
        assert_eq!(
            windows[0].resets_at.as_deref(),
            Some("2026-11-25T08:00:23Z")
        );
        assert_eq!(windows[1].id, "gemini-weekly");
        assert_eq!(windows[1].label, "Gemini · Weekly");
        assert_eq!(windows[1].remaining_percent, 80.0);
        assert_eq!(windows[2].id, "3p-5h");
        assert_eq!(windows[2].label, "Claude & GPT (shared) · 5-hour");
        assert_eq!(windows[2].remaining_percent, 100.0);
        assert_eq!(windows[2].resets_at, None);
        assert_eq!(windows[3].id, "3p-weekly");
        assert_eq!(windows[3].label, "Claude & GPT (shared) · Weekly");
        assert_eq!(windows[3].resets_at, None);
    }

    #[test]
    fn decodes_go_keyring_base64_and_hex_secrets() {
        let plain = r#"{"token":{"access_token":"abc"}}"#;
        let b64 = format!("go-keyring-base64:{}", BASE64.encode(plain));
        assert_eq!(decode_go_keyring_secret(&b64).as_deref(), Some(plain));
        let hex: String = plain
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let encoded = format!("go-keyring-encoded:{hex}");
        assert_eq!(decode_go_keyring_secret(&encoded).as_deref(), Some(plain));
        assert_eq!(decode_go_keyring_secret(plain).as_deref(), Some(plain));
    }

    #[test]
    fn falls_back_to_codex_session_jsonl_rate_limits() {
        let dir = std::env::temp_dir().join(format!("t3-limits-test-{}", Uuid::new_v4()));
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
        assert_eq!(parsed.plan.as_deref(), Some("ChatGPT Plus Subscription"));
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
                reset_credits: None,
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
