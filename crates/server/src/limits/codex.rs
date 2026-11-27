use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::{Duration as StdDuration, SystemTime},
};

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use t3_core::ProviderId;
use tokio::io::{AsyncWriteExt, BufReader as AsyncBufReader};

use super::{
    LimitSource, LimitStatus, MAX_LINE_BYTES, ProviderUsageLimits, ResetCredits, SESSION_MINS,
    WEEK_MINS, WindowKind, capitalize, cli::resolve_cli_bin, iso, make_window, normalize_iso,
    prepare_probe_cmd, provider_key, provider_label, read_capped_line, unavailable,
};

const CODEX_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(8);
const CODEX_RESET_TIMEOUT: StdDuration = StdDuration::from_secs(20);
const CODEX_LOG_SCAN_FILES: usize = 16;
const MONTH_MINS: u32 = 30 * 24 * 60;

pub(super) async fn probe_codex(
    bin: &Path,
    home: Option<&Path>,
    now: DateTime<Utc>,
) -> ProviderUsageLimits {
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

pub(super) enum CodexResetError {
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

pub(super) async fn consume_codex_reset_credit_rpc(
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
