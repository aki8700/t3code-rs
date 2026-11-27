use std::{fs, path::PathBuf};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use t3_core::{AuthConfig, ProviderId, Run, RunStatus, ServerConfig, Store};
use tower::ServiceExt;
use uuid::Uuid;

use super::{
    CodexRunUsage,
    aggregate::aggregate,
    cache::CachedFile,
    codex_run_usage_since, codex_session_usage_snapshot, provider_session_resumable,
    session::{claude_session_resumable, read_session_transcript},
    transcript::parse_transcript,
};
use crate::ServerState;

const CLAUDE: &str = r#"
{"timestamp":"2026-01-02T11:30:00Z","message":{"id":"a","model":"claude-sonnet","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":4,"cache_creation_input_tokens":6}}}
{"timestamp":"2026-01-02T11:30:01Z","message":{"id":"a","model":"claude-sonnet","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":4,"cache_creation_input_tokens":6}}}
{"timestamp":"2026-01-02T11:30:02Z","message":{"id":"a","model":"claude-sonnet","usage":{"output_tokens":9}}}
{"timestamp":"2026-01-02T11:31:00Z","message":{"id":"b","model":"claude-haiku","usage":{"input_tokens":25,"cache_creation_input_tokens":null}}}
{"timestamp":"2026-01-02T11:32:00Z","message":{"id":"c","content":"no usage"}}
"#;

const CODEX: &str = r#"
{"type":"session_meta","model":"gpt-6.2-sol"}
{"timestamp":"2026-01-02T11:30:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":3},"last_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":3}}}}
{"timestamp":"2026-01-02T11:30:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":3},"last_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":3}}}}
{"type":"turn_context","payload":{"model":"gpt-6.2-astra"}}
{"timestamp":"2026-01-02T12:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":180,"cached_input_tokens":120,"cache_write_input_tokens":8,"output_tokens":22,"reasoning_output_tokens":8}}}}
{"type":"event_msg","payload":{"type":"token_count","info":null}}
invalid json
"#;

#[test]
fn claude_streaming_snapshots_and_pricing() {
    let samples = parse_transcript(CLAUDE.as_bytes(), ProviderId::Claude).unwrap();
    assert_eq!(samples.len(), 2);
    let sample = &samples[0];
    assert_eq!(sample.model, "claude-sonnet");
    assert_eq!(sample.tokens.output_tokens, 20);
    assert_eq!(sample.tokens.total(), 130);
    assert!(
        (sample
            .tokens
            .estimated_cost(ProviderId::Claude, &sample.model)
            - 0.000_623_7)
            .abs()
            < 1e-12
    );
    assert!(
        (samples[1]
            .tokens
            .estimated_cost(ProviderId::Claude, "claude-haiku")
            - 0.000_02)
            .abs()
            < 1e-12
    );
}

#[test]
fn codex_usage_uses_last_or_forward_delta_and_tracks_model() {
    let samples = parse_transcript(CODEX.as_bytes(), ProviderId::Codex).unwrap();
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].model, "gpt-6.2-sol");
    assert_eq!(samples[0].tokens.input_tokens, 20);
    assert_eq!(samples[0].tokens.total(), 110);
    let sample = &samples[1];
    assert_eq!(sample.model, "gpt-6.2-astra");
    assert_eq!(sample.tokens.input_tokens, 40);
    assert_eq!(sample.tokens.cache_read_tokens, 40);
    assert_eq!(sample.tokens.cache_write_tokens, 8);
    assert_eq!(sample.tokens.output_tokens, 12);
    assert_eq!(sample.tokens.reasoning_tokens, 5);
    assert_eq!(sample.tokens.total(), 100);
    assert!(
        (sample
            .tokens
            .estimated_cost(ProviderId::Codex, &sample.model)
            - 0.001)
            .abs()
            < 1e-12
    );
    assert_eq!(
        sample
            .tokens
            .estimated_cost(ProviderId::OpenCode, "local/model"),
        0.0
    );
}

#[test]
fn codex_rollout_run_usage_uses_total_deltas_and_rebases_after_resume_reset() {
    let home = Fixture::new();
    let sessions = home.0.join(".codex/sessions/2026/11/26");
    fs::create_dir_all(&sessions).unwrap();
    let path = sessions.join("rollout-2026-11-26T10-00-00-codex-meter-1.jsonl");
    let line = |input, cached, written, output, reasoning, total| {
        json!({
            "timestamp": "2026-11-26T10:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": cached,
                        "cache_write_input_tokens": written,
                        "output_tokens": output,
                        "reasoning_output_tokens": reasoning,
                        "total_tokens": total
                    },
                    "last_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": cached,
                        "cache_write_input_tokens": written,
                        "output_tokens": output,
                        "reasoning_output_tokens": reasoning,
                        "total_tokens": total
                    }
                }
            }
        })
        .to_string()
    };
    fs::write(&path, line(600, 500, 20, 80, 20, 700)).unwrap();
    let before = codex_session_usage_snapshot(Some(&home.0), "codex-meter-1").unwrap();
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            line(600, 500, 20, 80, 20, 700),
            line(950, 750, 35, 120, 30, 1070)
        ),
    )
    .unwrap();
    let after = codex_session_usage_snapshot(Some(&home.0), "codex-meter-1").unwrap();
    assert_eq!(
        codex_run_usage_since(Some(before), Some(after)),
        Some(CodexRunUsage {
            input_tokens: 350,
            cache_read_tokens: 250,
            cache_creation_tokens: 15,
            output_tokens: 40,
            reasoning_tokens: 10,
        })
    );

    fs::write(&path, line(120, 80, 5, 20, 8, 145)).unwrap();
    let resumed = codex_session_usage_snapshot(Some(&home.0), "codex-meter-1").unwrap();
    assert_eq!(
        codex_run_usage_since(Some(after), Some(resumed)),
        Some(CodexRunUsage {
            input_tokens: 120,
            cache_read_tokens: 80,
            cache_creation_tokens: 5,
            output_tokens: 20,
            reasoning_tokens: 8,
        })
    );
}

#[test]
fn hourly_and_daily_windows_include_empty_buckets() {
    let file = CachedFile {
        modified: None,
        len: 0,
        provider: ProviderId::Claude,
        samples: parse_transcript(CLAUDE.as_bytes(), ProviderId::Claude).unwrap(),
    };
    let now = "2026-01-02T12:30:00Z".parse().unwrap();
    let hourly = aggregate(1, 0, now, std::iter::once(&file), &[]);
    assert_eq!(hourly.window.resolution, "hour");
    assert_eq!(hourly.window.since, "2026-01-01T13:00:00Z");
    assert_eq!(hourly.buckets.len(), 24);
    assert_eq!(hourly.buckets[22].by_provider["claude"].total_tokens, 155);
    assert_eq!(hourly.buckets[23].label, "12:00");
    assert_eq!(hourly.buckets[23].total_tokens, 0);
    assert_eq!(hourly.buckets[0].total_tokens, 0);
    let daily = aggregate(7, 0, now, std::iter::once(&file), &[]);
    assert_eq!(daily.window.resolution, "day");
    assert_eq!(daily.buckets.len(), 7);
    assert_eq!(daily.buckets[0].key, "2025-12-27");
    assert_eq!(daily.buckets[6].key, "2026-01-02");
    assert_eq!(daily.totals.total_tokens, 155);
    assert_eq!(daily.totals.sessions, 1);
    assert_eq!(daily.totals.requests, 2);
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("t3-usage-{}", Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn claude_resume_needs_a_transcript_with_a_message_in_the_workspace_project() {
    let home = Fixture::new();
    let workspace = home.0.join("work space");
    fs::create_dir(&workspace).unwrap();
    let projects = home.0.join("projects");
    let session = "4f1c2d3e-0000-4000-8000-000000000001";
    assert!(!claude_session_resumable(&projects, &workspace, session));

    let physical = fs::canonicalize(&workspace).unwrap();
    let name: String = physical
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    assert!(name.ends_with("-work-space"));
    let dir = projects.join(name);
    fs::create_dir_all(&dir).unwrap();
    let transcript = dir.join(format!("{session}.jsonl"));
    fs::write(&transcript, "").unwrap();
    assert!(!claude_session_resumable(&projects, &workspace, session));
    fs::write(
            &transcript,
            "{\"type\":\"queue-operation\",\"sessionId\":\"x\"}\nnot json\n{\"type\":\"user\",\"uuid\":\"\"}\n",
        )
        .unwrap();
    assert!(!claude_session_resumable(&projects, &workspace, session));
    fs::write(
            &transcript,
            "{\"type\":\"queue-operation\"}\n{\"type\":\"user\",\"uuid\":\"m-1\",\"message\":{\"content\":\"hi\"}}\n",
        )
        .unwrap();
    assert!(claude_session_resumable(&projects, &workspace, session));
    assert!(!claude_session_resumable(&projects, &workspace, "../x"));
    // A transcript of another working directory is not resumable from this one.
    let other = home.0.join("other");
    fs::create_dir(&other).unwrap();
    assert!(!claude_session_resumable(&projects, &other, session));
}

#[test]
fn claude_project_names_count_utf16_units_like_claude() {
    let home = Fixture::new();
    let workspace = home.0.join("日本😀");
    fs::create_dir(&workspace).unwrap();
    let projects = home.0.join("projects");
    let physical = fs::canonicalize(&home.0).unwrap();
    let prefix: String = physical
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = projects.join(format!("{prefix}-----"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("s.jsonl"), "{\"leafUuid\":\"l\"}\n").unwrap();
    assert!(claude_session_resumable(&projects, &workspace, "s"));
}

#[test]
fn codex_resume_needs_a_rollout_with_session_meta() {
    let home = Fixture::new();
    let workspace = home.0.clone();
    let session = "01a0fa7c-d795-72e1-a07d-4cea8e87567d";
    let resumable =
        || provider_session_resumable(ProviderId::Codex, Some(&home.0), &workspace, session);
    assert!(!resumable());
    let dir = home.0.join(".codex/sessions/2026/10/02");
    fs::create_dir_all(&dir).unwrap();
    let rollout = dir.join(format!("rollout-2026-10-02T11-41-20-{session}.jsonl"));
    fs::write(&rollout, "").unwrap();
    assert!(!resumable());
    fs::write(
        &rollout,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"01a0fa7c\"}}\n",
    )
    .unwrap();
    assert!(resumable());
    assert!(provider_session_resumable(
        ProviderId::OpenCode,
        Some(&home.0),
        &workspace,
        "anything"
    ));
}

#[tokio::test]
async fn usage_route_aggregates_local_sources_and_revalidates_on_refresh() {
    let home = Fixture::new();
    let projects = home.0.join(".claude/projects/fixture");
    fs::create_dir_all(&projects).unwrap();
    let now = Utc::now() - Duration::minutes(15);
    let line = |id: &str, output| {
        json!({
            "timestamp": now,
            "message": {"id": id, "model": "claude-opus", "usage": {"input_tokens": 10, "output_tokens": output}}
        }).to_string()
    };
    let log = projects.join("session.jsonl");
    fs::write(&log, line("first", 5)).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&log, projects.join("symlink.jsonl")).unwrap();
    fs::write(projects.join("ignore.json"), line("ignored", 9_000)).unwrap();

    let mut state = ServerState::new(
        ServerConfig {
            data_dir: home.0.join("server"),
            auth: AuthConfig::Bearer {
                token: "fixture-usage-token".into(),
            },
            ..Default::default()
        },
        "http://127.0.0.1:3774".into(),
    )
    .unwrap();
    state.usage_home = Some(home.0.clone());
    let project = state
        .store
        .create_project("fixture".into(), home.0.clone())
        .await
        .unwrap();
    let thread_id = Uuid::new_v4();
    state
        .store
        .create_thread(t3_core::Thread {
            id: thread_id,
            project_id: project.id,
            parent_thread_id: None,
            relationship: None,
            title: "fixture".into(),
            model_selection: Default::default(),
            runtime_mode: Default::default(),
            interaction_mode: Default::default(),
            status: Default::default(),
            worktree_path: None,
            branch: None,
            provider_session_id: None,
            archived: false,
            pinned: false,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    for (ordinal, provider, model) in [
        (1, ProviderId::Antigravity, "gemini-neon"),
        (2, ProviderId::OpenCode, "local/example"),
        (3, ProviderId::Claude, "claude-opus"),
    ] {
        state
            .store
            .create_run(Run {
                id: Uuid::new_v4(),
                thread_id,
                ordinal,
                provider,
                model: model.into(),
                effort: None,
                status: RunStatus::Completed,
                prompt: String::new(),
                provider_session_id: None,
                input_tokens: Some(30),
                output_tokens: Some(10),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
                duration_ms: Some(1),
                error_message: None,
                started_at: now,
                completed_at: Some(now),
            })
            .await
            .unwrap();
    }
    let app = crate::router(state);
    let get = |query: &'static str| {
        let app = app.clone();
        async move {
            let response = app
                .oneshot(
                    Request::get(query)
                        .header("authorization", "Bearer fixture-usage-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
            serde_json::from_slice::<Value>(&body).unwrap()
        }
    };
    let first = get("/api/usage?days=7").await;
    assert_eq!(first["window"]["days"], 7);
    assert_eq!(first["totals"]["total_tokens"], 95);
    assert_eq!(first["totals"]["requests"], 3);
    assert_eq!(first["totals"]["sessions"], 2);
    assert_eq!(first["providers"][0]["provider"], "claude");
    assert_eq!(first["providers"][2]["provider"], "open-code");
    assert_eq!(first["models"][0]["model"], "claude-opus");
    assert_eq!(first["buckets"].as_array().unwrap().len(), 7);
    assert!(
        first["scanned_at"]
            .as_str()
            .unwrap()
            .parse::<DateTime<Utc>>()
            .is_ok()
    );
    let warm = get("/api/usage").await;
    assert_eq!(warm, first);

    fs::write(
        &log,
        format!("{}\n{}", line("first", 5), line("second", 30)),
    )
    .unwrap();
    let refreshed = get("/api/usage?days=7&refresh=1").await;
    assert_eq!(refreshed["totals"]["total_tokens"], 135);
    fs::remove_file(&log).unwrap();
    let removed = get("/api/usage?days=7&refresh=true").await;
    assert_eq!(removed["totals"]["total_tokens"], 80);
}

#[test]
fn codex_session_context_uses_latest_request_not_cumulative_total() {
    let dir = std::env::temp_dir().join(format!("t3-ctx-{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout-x.jsonl");
    let token_count = |total_in: u64, last: &str| {
        format!(
            r#"{{"timestamp":"2026-01-02T11:30:00Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total_in},"cached_input_tokens":0,"output_tokens":3658,"total_tokens":{}}},"last_token_usage":{last},"model_context_window":258400}}}}}}"#,
            total_in + 3658
        )
    };
    let before = token_count(
        1_148_907,
        r#"{"input_tokens":62029,"cached_input_tokens":61312,"output_tokens":85,"total_tokens":62114}"#,
    );
    fs::write(&path, format!("{before}\n")).unwrap();
    let usage = read_session_transcript(&path, ProviderId::Codex).unwrap();
    assert_eq!(usage.context_tokens, 62_114);
    assert_eq!(usage.input_tokens, 717);
    assert_eq!(usage.cache_read_tokens, 61_312);
    assert_eq!(usage.output_tokens, 85);
    assert_eq!(usage.context_window, Some(258_400));

    // After an in-place compaction Codex reports only the estimated size of the new history.
    let after = token_count(
        1_148_907,
        r#"{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"total_tokens":25284}"#,
    );
    let compacted = r#"{"timestamp":"2026-01-02T11:31:00Z","type":"compacted","payload":{"message":"","replacement_history":[]}}"#;
    fs::write(&path, format!("{before}\n{compacted}\n{after}\n")).unwrap();
    let usage = read_session_transcript(&path, ProviderId::Codex).unwrap();
    assert_eq!(usage.context_tokens, 25_284);
    assert_eq!(usage.context_window, Some(258_400));
    fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn codex_thread_context_comes_from_session_file_not_stored_turn_totals() {
    let home = Fixture::new();
    let sessions = home.0.join(".codex/sessions/2026/01/02");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
            sessions.join("rollout-2026-01-02T11-30-00-codex-ctx-1.jsonl"),
            r#"{"timestamp":"2026-01-02T11:31:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1148907,"output_tokens":3658,"total_tokens":1152565},"last_token_usage":{"input_tokens":0,"output_tokens":0,"total_tokens":25284},"model_context_window":258400}}}
"#,
        )
        .unwrap();
    let mut state = ServerState::new(
        ServerConfig {
            data_dir: home.0.join("server"),
            auth: AuthConfig::Bearer {
                token: "fixture-ctx-token".into(),
            },
            ..Default::default()
        },
        "http://127.0.0.1:3774".into(),
    )
    .unwrap();
    state.usage_home = Some(home.0.clone());
    let project = state
        .store
        .create_project("fixture".into(), home.0.clone())
        .await
        .unwrap();
    let now = Utc::now();
    let thread_id = Uuid::new_v4();
    let model = "gpt-6.2-astra".to_owned();
    state
        .store
        .create_thread(t3_core::Thread {
            id: thread_id,
            project_id: project.id,
            parent_thread_id: None,
            relationship: None,
            title: "fixture".into(),
            model_selection: t3_core::ModelSelection {
                provider: ProviderId::Codex,
                model: model.clone(),
                effort: None,
                agent: None,
            },
            runtime_mode: Default::default(),
            interaction_mode: Default::default(),
            status: Default::default(),
            worktree_path: None,
            branch: None,
            provider_session_id: Some("codex-ctx-1".into()),
            archived: false,
            pinned: false,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let run_id = Uuid::new_v4();
    state
        .store
        .create_run(Run {
            id: run_id,
            thread_id,
            ordinal: 1,
            provider: ProviderId::Codex,
            model: model.clone(),
            effort: None,
            status: RunStatus::Completed,
            prompt: String::new(),
            provider_session_id: Some("codex-ctx-1".into()),
            input_tokens: Some(1_148_907),
            output_tokens: Some(3_658),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            duration_ms: Some(1),
            error_message: None,
            started_at: now,
            completed_at: Some(now),
        })
        .await
        .unwrap();
    // A snapshot an older build stored from `turn.completed` must not win.
    state
        .coordinator
        .persist_context(
            thread_id,
            &crate::coordinator::LiveContextUsage {
                run_id,
                provider: ProviderId::Codex,
                model,
                session_id: Some("codex-ctx-1".into()),
                context_tokens: 1_152_565,
                input_tokens: 52_075,
                output_tokens: 3_658,
                cache_read_tokens: 1_096_832,
                cache_write_tokens: 0,
            },
        )
        .await;
    let response = crate::router(state)
        .oneshot(
            Request::get(format!("/api/threads/{thread_id}"))
                .header("authorization", "Bearer fixture-ctx-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let context = &serde_json::from_slice::<Value>(&body).unwrap()["context"];
    assert_eq!(context["source"], "session");
    assert_eq!(context["used_tokens"], 25_284);
    assert_eq!(context["context_window"], 258_400);
    assert_eq!(context["compaction"]["recommended"], false);
}
