use std::{collections::HashSet, path::Path, time::Duration as StdDuration};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use t3_core::{CoreError, ProviderId};
use tokio::io::{AsyncWriteExt, BufReader as AsyncBufReader};

use super::{
    LimitSource, LimitStatus, MAX_LINE_BYTES, ProviderUsageLimits, SESSION_MINS, UsageLimitWindow,
    WEEK_MINS, WindowKind, capitalize, cli::resolve_cli_bin, iso, make_window, normalize_iso,
    prepare_probe_cmd, provider_key, provider_label, read_capped_line, unavailable,
};

const CLAUDE_PROBE_TIMEOUT: StdDuration = StdDuration::from_secs(10);

pub(super) async fn probe_claude(bin: &Path, now: DateTime<Utc>) -> ProviderUsageLimits {
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
