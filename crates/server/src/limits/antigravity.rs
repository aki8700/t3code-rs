#[cfg(target_os = "macos")]
use std::process::Stdio;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
    time::Duration as StdDuration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use t3_core::{ProviderId, ServerConfig};
#[cfg(target_os = "macos")]
use tokio::process::Command;

use super::{
    LimitSource, LimitStatus, ProviderUsageLimits, SESSION_MINS, UsageLimitWindow, WEEK_MINS,
    WindowKind, iso, make_window, normalize_iso, provider_key, provider_label, unavailable,
};

const ANTIGRAVITY_HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(10);
#[cfg(target_os = "macos")]
const KEYCHAIN_TIMEOUT: StdDuration = StdDuration::from_secs(3);

const ANTIGRAVITY_DAILY_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
const ANTIGRAVITY_PROD_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
const GOOGLE_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

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

pub(super) async fn probe_antigravity(
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
