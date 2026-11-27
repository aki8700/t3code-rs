use std::fs;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{TimeZone, Utc};
use serde_json::json;
use t3_core::ProviderId;
use uuid::Uuid;

use super::{
    LimitSource, LimitStatus, ProviderUsageLimits, UsageLimitsCache, UsageLimitsResponse,
    decode_go_keyring_secret, iso, parse_antigravity_summary_windows, parse_claude_usage_response,
    parse_codex_rate_limits_response, read_codex_session_log_limits, sort_antigravity_windows,
};

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
