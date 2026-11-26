use axum::{
    Router,
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Value, json};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use t3_core::{
    AuthConfig, NewTurnItem, ProviderId, Run, RunStatus, ServerConfig, Store, ThreadStatus,
    TurnItemKind, WsServerMessage,
};
use t3_server::{ServerState, router};
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

const TOKEN: &str = "test-bearer-token +!%";
const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const REMOTE: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("t3-server-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn state(&self, auth: AuthConfig) -> ServerState {
        ServerState::new(
            ServerConfig {
                data_dir: self.0.join("server"),
                enabled_providers: vec![ProviderId::Mock],
                auth,
                ..Default::default()
            },
            "http://127.0.0.1:3774".into(),
        )
        .unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn request(app: &Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.unwrap()
}

async fn status(app: &Router, uri: &str, peer: IpAddr, headers: &[(&str, &str)]) -> StatusCode {
    let mut req = Request::builder()
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::new(peer, 49152)));
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    request(app, req.body(Body::empty()).unwrap())
        .await
        .status()
}

async fn json_req(app: &Router, method: Method, uri: &str, data: Option<Value>) -> Value {
    let mut req = Request::builder().method(method).uri(uri);
    if data.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let response = request(
        app,
        req.body(data.map_or(Body::empty(), |data| Body::from(data.to_string())))
            .unwrap(),
    )
    .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    assert!(
        status.is_success(),
        "{uri} returned {status}: {}",
        String::from_utf8_lossy(&body)
    );
    if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    }
}

#[tokio::test]
async fn bearer_tailscale_and_local_peer_checks() {
    let dir = TempDir::new();
    let app = router(dir.state(AuthConfig::Bearer {
        token: TOKEN.into(),
    }));
    assert_eq!(
        status(
            &app,
            "/api/meta",
            REMOTE,
            &[("authorization", &format!("Bearer {TOKEN}"))]
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        status(
            &app,
            "/api/meta?token=test-bearer-token%20%2B!%25",
            REMOTE,
            &[]
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    for headers in [&[][..], &[("authorization", "Bearer invalid")][..]] {
        assert_eq!(
            status(&app, "/api/meta", LOCAL, headers).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(status(&app, "/healthz", REMOTE, &[]).await, StatusCode::OK);

    drop(app);
    let app = router(dir.state(AuthConfig::Tailscale {
        allowed_logins: vec!["dev@example.test".into()],
    }));
    let headers = &[
        ("tailscale-user-login", "dev@example.test"),
        ("host", "dev.example.ts.net:3774"),
    ];
    for host in ["evil.example:3774", "ts.net", "bad..ts.net", "127.0.0.2"] {
        assert_eq!(
            status(&app, "/api/meta", LOCAL, &[headers[0], ("host", host)]).await,
            StatusCode::UNAUTHORIZED
        );
    }
    for host in ["localhost:3774", "127.0.0.1", "[::1]:3774"] {
        assert_eq!(
            status(&app, "/api/meta", LOCAL, &[headers[0], ("host", host)]).await,
            StatusCode::OK
        );
    }
    assert_eq!(
        status(&app, "/api/meta", LOCAL, headers).await,
        StatusCode::OK
    );
    assert_eq!(
        status(&app, "/api/meta", "::1".parse().unwrap(), headers).await,
        StatusCode::OK
    );
    assert_eq!(
        status(&app, "/api/meta", REMOTE, headers).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(&app, "/api/meta", LOCAL, &[]).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            &app,
            "/api/meta",
            LOCAL,
            &[("tailscale-user-login", "other@example.test")]
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    drop(app);
    let app = router(dir.state(AuthConfig::LocalOnly));
    assert_eq!(
        status(&app, "/api/meta", REMOTE, &[]).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(status(&app, "/api/meta", LOCAL, &[]).await, StatusCode::OK);
    assert_eq!(
        status(
            &app,
            "/api/meta",
            LOCAL,
            &[
                ("origin", "https://external.example"),
                ("host", "localhost:3774")
            ]
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn tailscale_requires_an_explicit_login_allowlist() {
    let cfg = ServerConfig {
        auth: AuthConfig::Tailscale {
            allowed_logins: vec![],
        },
        ..Default::default()
    };
    assert!(cfg.validate().is_err());
}

fn jwt(kid: &str, pem: &[u8], claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.into());
    jsonwebtoken::encode(&header, claims, &EncodingKey::from_rsa_pem(pem).unwrap()).unwrap()
}

const FIRST_PEM: &[u8] = include_bytes!("fixtures/first.test.pem");
const ROTATED_PEM: &[u8] = include_bytes!("fixtures/rotated.test.pem");

#[tokio::test]
async fn cloudflare_rs256_header_cookie_websocket_and_rotation() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let jwks = Router::new().route("/certs", axum::routing::get(move || {
        let request_no = requests.fetch_add(1, Ordering::SeqCst);
        async move {
            let (kid, n) = if request_no == 0 {
                ("first", include_str!("fixtures/first.modulus"))
            } else {
                ("rotated", include_str!("fixtures/rotated.modulus"))
            };
            axum::Json(json!({"keys":[{"kid":kid,"kty":"RSA","use":"sig","alg":"RS256","n":n,"e":"AQAB"}]}))
        }
    }));
    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let url = format!("http://{}/certs", listener.local_addr().unwrap());
    let jwks_task = tokio::spawn(async move { axum::serve(listener, jwks).await.unwrap() });
    let tmp = TempDir::new();
    let app = router(tmp.state(AuthConfig::CloudflareAccess {
        team_domain: "example.cloudflareaccess.com".into(),
        audience: "t3-audience".into(),
        allowed_emails: vec!["test@example.test".into()],
        jwks_url_override: Some(url),
    }));
    let now = jsonwebtoken::get_current_timestamp();
    let claims = json!({"sub":"test-subject","email":"TEST@EXAMPLE.TEST",
        "aud":["another-audience","t3-audience"],"exp":now+120,"nbf":now-1,"iss":"https://issuer.example.test"});
    let token = jwt("first", FIRST_PEM, &claims);
    assert_eq!(
        status(
            &app,
            "/api/meta",
            REMOTE,
            &[("cf-access-jwt-assertion", &token)]
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        status(
            &app,
            "/api/meta",
            REMOTE,
            &[(
                "cookie",
                &format!("other=x; CF_Authorization={token}; more=y")
            )]
        )
        .await,
        StatusCode::OK
    );

    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let server_url = format!("http://{}", listener.local_addr().unwrap());
    let web_app = app.clone();
    let server_task = tokio::spawn(async move {
        axum::serve(
            listener,
            web_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let upgrade = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{server_url}/ws"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("origin", &server_url)
        .header("cookie", format!("CF_Authorization={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(upgrade.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    let (head, original_sig) = token.rsplit_once('.').unwrap();
    let mut signature = URL_SAFE_NO_PAD.decode(original_sig).unwrap();
    signature[0] ^= 1;
    let tampered = format!("{head}.{}", URL_SAFE_NO_PAD.encode(signature));
    let none = format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"none","kid":"first"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let mut invalid = vec![tampered, none];
    for (key, value) in [
        ("aud", json!("wrong")),
        ("exp", json!(now - 61)),
        ("nbf", json!(now + 61)),
        ("email", json!("blocked@example.test")),
    ] {
        let mut claims = claims.clone();
        claims[key] = value;
        invalid.push(jwt("first", FIRST_PEM, &claims));
    }
    for token in &invalid {
        assert_eq!(
            status(
                &app,
                "/api/meta",
                REMOTE,
                &[("cf-access-jwt-assertion", token)]
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        status(&app, "/api/meta", REMOTE, &[]).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let rotated = jwt("rotated", ROTATED_PEM, &claims);
    for token in [&rotated, &jwt("unknown", ROTATED_PEM, &claims)] {
        assert_eq!(
            status(
                &app,
                "/api/meta",
                REMOTE,
                &[("cf-access-jwt-assertion", token)]
            )
            .await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    advance_jwks_cooldown().await;
    assert_eq!(
        status(
            &app,
            "/api/meta",
            REMOTE,
            &[("cf-access-jwt-assertion", &rotated)]
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    server_task.abort();
    jwks_task.abort();
}

async fn advance_jwks_cooldown() {
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
}

#[tokio::test]
async fn cloudflare_retries_a_failed_jwks_fetch_only_after_cooldown() {
    let count = Arc::new(AtomicUsize::new(0));
    let requests = count.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (fetch_started, release_fetch) = (started.clone(), release.clone());
    let jwks = Router::new().route(
        "/certs",
        axum::routing::get(move || {
            let request_no = requests.fetch_add(1, Ordering::SeqCst);
            let (started, release) = (fetch_started.clone(), release_fetch.clone());
            async move {
                if request_no == 0 {
                    started.notify_one();
                    release.notified().await;
                    return (StatusCode::SERVICE_UNAVAILABLE, axum::Json(Value::Null));
                }
                (
                    StatusCode::OK,
                    axum::Json(json!({"keys":[{
                        "kid":"first", "kty":"RSA", "alg":"RS256",
                        "n":include_str!("fixtures/first.modulus"), "e":"AQAB"
                    }]})),
                )
            }
        }),
    );
    let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
    let url = format!("http://{}/certs", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, jwks).await.unwrap() });
    let tmp = TempDir::new();
    let app = router(tmp.state(AuthConfig::CloudflareAccess {
        team_domain: "example.cloudflareaccess.com".into(),
        audience: "fixture".into(),
        allowed_emails: vec![],
        jwks_url_override: Some(url),
    }));
    let claims = json!({"aud":"fixture", "exp":jsonwebtoken::get_current_timestamp()+120});
    let token = jwt("first", FIRST_PEM, &claims);
    let headers = &[("cf-access-jwt-assertion", token.as_str())];
    let (first_app, first_token) = (app.clone(), token.clone());
    let first = tokio::spawn(async move {
        status(
            &first_app,
            "/api/meta",
            REMOTE,
            &[("cf-access-jwt-assertion", &first_token)],
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let competing = tokio::time::timeout(
        Duration::from_secs(2),
        status(&app, "/api/meta", REMOTE, headers),
    )
    .await
    .expect("another request waited for the JWKS fetch");
    assert_eq!(competing, StatusCode::UNAUTHORIZED);
    release.notify_one();
    assert_eq!(first.await.unwrap(), StatusCode::UNAUTHORIZED);
    for _ in 0..3 {
        assert_eq!(
            status(&app, "/api/meta", REMOTE, headers).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    advance_jwks_cooldown().await;
    assert_eq!(
        status(&app, "/api/meta", REMOTE, headers).await,
        StatusCode::OK
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    task.abort();
}

fn git(path: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn wait_for_thread(
    events: &mut tokio::sync::broadcast::Receiver<WsServerMessage>,
    thread_id: Uuid,
    status: ThreadStatus,
) {
    wait_for_thread_timeout(events, thread_id, status, Duration::from_secs(5)).await;
}

async fn wait_for_thread_timeout(
    events: &mut tokio::sync::broadcast::Receiver<WsServerMessage>,
    thread_id: Uuid,
    status: ThreadStatus,
    timeout: Duration,
) {
    tokio::time::timeout(timeout, async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event, WsServerMessage::ThreadStatusChanged { thread_id: id, status: seen } if id == thread_id && seen == status) {
                break;
            }
        }
    }).await.unwrap();
}

async fn mcp_tool(app: &Router, name: &str, arguments: Value) -> Value {
    let response = json_req(app, Method::POST, "/mcp", Some(json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments},
    }))).await;
    assert_ne!(response["result"]["isError"], true, "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn create_parent(app: &Router, workspace: &Path) -> Uuid {
    let project = json_req(
        app,
        Method::POST,
        "/api/projects",
        Some(json!({"title":"Test","workspace_root":workspace})),
    )
    .await;
    let thread = json_req(app, Method::POST, "/api/threads", Some(json!({
        "project_id":project["id"],"title":"Parent","model_selection":{"provider":"mock","model":"mock"}
    }))).await;
    assert_eq!(thread["runtime_mode"], "approval-required");
    serde_json::from_value(thread["id"].clone()).unwrap()
}

#[tokio::test]
async fn rest_delegation_respects_runtime_and_plan_permissions() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let project = json_req(
        &app,
        Method::POST,
        "/api/projects",
        Some(json!({
            "title":"Test", "workspace_root":tmp.0
        })),
    )
    .await;
    let parent = json_req(&app, Method::POST, "/api/threads", Some(json!({
        "project_id":project["id"], "title":"Plan", "model_selection":{"provider":"mock","model":"mock"},
        "interaction_mode":"plan", "runtime_mode":"auto-accept-edits"
    }))).await;
    let id = parent["id"].as_str().unwrap();
    let path = format!("/api/threads/{id}/delegate");
    let spec = |runtime, interaction| {
        json!({
            "parent_thread_id":id, "title":"Child", "task":"fixture",
            "model_selection":{"provider":"mock","model":"mock"}, "isolation":"shared",
            "runtime_mode":runtime, "interaction_mode":interaction
        })
    };
    for (runtime, interaction) in [("full-access", "plan"), ("approval-required", "default")] {
        let response = request(
            &app,
            Request::post(&path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(spec(runtime, interaction).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let children = json_req(&app, Method::GET, "/api/threads", None).await;
    assert_eq!(children.as_array().unwrap().len(), 1);
    let child = json_req(
        &app,
        Method::POST,
        &path,
        Some(spec("approval-required", "plan")),
    )
    .await;
    assert_eq!(child["thread"]["runtime_mode"], "approval-required");
    assert_eq!(child["thread"]["interaction_mode"], "plan");
    wait_for_thread(
        &mut events,
        serde_json::from_value(child["thread"]["id"].clone()).unwrap(),
        ThreadStatus::Completed,
    )
    .await;
}

#[tokio::test]
async fn fork_handoff_keeps_recent_failures_and_redacts_credentials() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let store = state.store.clone();
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    let now = chrono::Utc::now();
    let secret = "sk-fixture_0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    for (ordinal, status, error) in [
        (
            1,
            RunStatus::Failed,
            Some(format!("Build failed using {secret}")),
        ),
        (2, RunStatus::Cancelled, None),
        (
            3,
            RunStatus::RateLimited,
            Some("Retry quota exhausted".into()),
        ),
        (4, RunStatus::Completed, None),
    ] {
        let run_id = Uuid::new_v4();
        store
            .create_run(Run {
                id: run_id,
                thread_id: id,
                ordinal,
                provider: ProviderId::Mock,
                model: "mock".into(),
                effort: None,
                status,
                prompt: "Finish migration".into(),
                provider_session_id: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
                duration_ms: None,
                error_message: error,
                started_at: now,
                completed_at: Some(now),
            })
            .await
            .unwrap();
        if ordinal == 4 {
            store
                .append_turn_item(NewTurnItem {
                    thread_id: id,
                    run_id,
                    kind: TurnItemKind::ToolResult,
                    summary: Some("migration tool".into()),
                    payload: json!({"error": {"message": "Missing migration directory"}}),
                    searchable_text: None,
                })
                .await
                .unwrap();
            store
                .append_turn_item(NewTurnItem {
                    thread_id: id,
                    run_id,
                    kind: TurnItemKind::UserMessage,
                    summary: None,
                    payload: json!({"text": format!("{} {secret}", "a".repeat(336))}),
                    searchable_text: None,
                })
                .await
                .unwrap();
        }
    }
    let fork = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/fork"),
        Some(json!({})),
    )
    .await;
    let fork_id = fork["id"].as_str().unwrap();
    let fork_detail = json_req(&app, Method::GET, &format!("/api/threads/{fork_id}"), None).await;
    let summary = fork_detail["handoffs"][0]["summary"].as_str().unwrap();
    let (_, errors) = summary.split_once("## Unresolved / Recent Errors").unwrap();
    assert!(errors.contains("Build failed using [REDACTED]"));
    assert!(errors.contains("interrupted") && errors.contains("Finish migration"));
    assert!(errors.contains("Retry quota exhausted"));
    assert!(errors.contains("Missing migration directory"));
    assert!(!fork_detail.to_string().contains("sk-fixture"));
}

#[tokio::test]
async fn run_without_provider_session_gets_previous_context_handoff() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({"prompt":"Remember the earlier migration constraints."})),
    )
    .await;
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({
            "prompt": "Continue the migration.",
            "model_selection": {"provider": "mock", "model": "mock-2"}
        })),
    )
    .await;
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    let detail = json_req(&app, Method::GET, &format!("/api/threads/{id}"), None).await;
    let reply = detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|item| item["kind"] == "assistant_message")
        .and_then(|item| item["payload"]["text"].as_str())
        .unwrap();
    assert!(reply.contains("Context from the previous run:"));
    assert!(reply.contains("Remember the earlier migration constraints."));
    assert!(reply.contains("Current request:\nContinue the migration."));
}

#[tokio::test]
async fn mock_rest_mcp_worktrees_handoff_search_compression_and_attachments() {
    let tmp = TempDir::new();
    let workspace = tmp.0.join("repo");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=T3 Test",
            "-c",
            "user.email=t3@example.test",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    );
    let state = tmp.state(AuthConfig::LocalOnly);
    #[cfg(unix)]
    let config = state.config.clone();
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let id = create_parent(&app, &workspace).await;
    let detail_path = format!("/api/threads/{id}");
    let message_path = format!("{detail_path}/messages");
    let result = json_req(
        &app,
        Method::POST,
        &message_path,
        Some(json!({"prompt":"文字起こし [large-output] [rate-limit-once]"})),
    )
    .await;
    assert!(result["run_id"].is_string());
    wait_for_thread_timeout(
        &mut events,
        id,
        ThreadStatus::Completed,
        Duration::from_secs(12),
    )
    .await;
    let stats = json_req(&app, Method::GET, "/api/stats/db", None).await;
    assert!(stats["raw_payload_bytes"].as_u64().unwrap() > 100 * 1024);
    assert!(stats["compression_ratio"].as_f64().unwrap() < 0.1);
    assert_eq!(stats["dedup_blobs_count"], 1);
    let hits = json_req(
        &app,
        Method::GET,
        "/api/search?q=%E6%96%87%E5%AD%97%E8%B5%B7%E3%81%93%E3%81%97",
        None,
    )
    .await;
    assert!(
        hits.as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["thread_id"] == json!(id))
    );
    let pinned = json_req(
        &app,
        Method::PATCH,
        &detail_path,
        Some(json!({"pinned":true})),
    )
    .await;
    assert_eq!(pinned["pinned"], true);
    let fork = json_req(
        &app,
        Method::POST,
        &format!("{detail_path}/fork"),
        Some(json!({"title":"Fork"})),
    )
    .await;
    let fork_id = fork["id"].as_str().unwrap();
    let fork_path = format!("/api/threads/{fork_id}");
    let fork_detail = json_req(&app, Method::GET, &fork_path, None).await;
    assert_eq!(fork["parent_thread_id"], json!(id));
    assert_eq!(fork_detail["items"][0]["kind"], "handoff");
    assert!(
        fork_detail["handoffs"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("文字起こし")
    );
    json_req(
        &app,
        Method::POST,
        &format!("{fork_path}/messages"),
        Some(json!({"prompt":"continue fork"})),
    )
    .await;
    wait_for_thread_timeout(
        &mut events,
        fork_id.parse().unwrap(),
        ThreadStatus::Completed,
        Duration::from_secs(12),
    )
    .await;
    let fork_detail = json_req(&app, Method::GET, &fork_path, None).await;
    assert!(fork_detail["items"].as_array().unwrap().iter().any(|item| {
        item["kind"] == "assistant_message"
            && item["payload"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("Current request:\ncontinue fork"))
    }));

    let child = mcp_tool(
        &app,
        "delegate_task",
        json!({
            "parent_thread_id": id, "title":"Child","task":"child result","provider":"mock",
            "model":"mock", "isolation":"worktree", "base_ref":"HEAD", "mode":"wait"
        }),
    )
    .await;
    assert_eq!(child["status"], "completed");
    assert_eq!(child["summary"], "child result");
    assert!(
        Path::new(child["worktree_path"].as_str().unwrap())
            .join(".git")
            .exists()
    );
    let child_id: Uuid = serde_json::from_value(child["child_thread_id"].clone()).unwrap();
    let mut child_event = false;
    let mut item_event = false;
    while let Ok(event) = events.try_recv() {
        child_event |= matches!(&event, WsServerMessage::SubagentUpdated {
            parent_thread_id, child_thread_id, status: ThreadStatus::Completed
        } if *parent_thread_id == id && *child_thread_id == child_id);
        item_event |= matches!(&event, WsServerMessage::TurnItemAdded { thread_id, item }
            if *thread_id == id && item.kind == TurnItemKind::Subagent);
    }
    assert!(child_event && item_event);
    let detail = json_req(&app, Method::GET, &detail_path, None).await;
    assert!(
        detail["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "subagent" && item["summary"] == "child result")
    );

    json_req(
        &app,
        Method::PATCH,
        "/api/settings",
        Some(json!({"auto_resume_limited_threads": false})),
    )
    .await;
    let limited_child = mcp_tool(
        &app,
        "delegate_task",
        json!({
            "parent_thread_id": id, "title":"Limited child","task":"[rate-limit-once]",
            "provider":"mock", "model":"mock", "mode":"wait"
        }),
    )
    .await;
    assert_eq!(limited_child["status"], "rate-limited");
    assert_eq!(
        limited_child["summary"],
        "capacity: provider=mock model=mock reset_at=unknown; re-run on another provider. mock limit"
    );

    let content = format!(
        "--t3\r\nContent-Disposition: form-data; name=\"file\"; filename=\"hello.txt\"\r\nContent-Type: text/plain\r\n\r\nattachment bytes\r\n--t3\r\nContent-Disposition: form-data; name=\"thread_id\"\r\n\r\n{id}\r\n--t3--\r\n"
    );
    let upload = |content| {
        Request::post("/api/attachments")
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=t3")
            .body(Body::from(content))
            .unwrap()
    };
    let response = request(&app, upload(content.clone())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let attachment: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(attachment["thread_id"], json!(id));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for (path, mode) in [
            (config.data_dir.clone(), 0o700),
            (config.worktrees_dir(), 0o700),
            (config.attachments_dir(), 0o700),
            (config.db_path(), 0o600),
            (
                PathBuf::from(attachment["storage_path"].as_str().unwrap()),
                0o600,
            ),
        ] {
            assert_eq!(
                path.metadata().unwrap().permissions().mode() & 0o777,
                mode,
                "{path:?}"
            );
        }
    }
    let response = request(
        &app,
        Request::get(format!(
            "/api/attachments/{}",
            attachment["id"].as_str().unwrap()
        ))
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"hello.txt\"; filename*=UTF-8''hello.txt"
    );
    assert_eq!(
        to_bytes(response.into_body(), 65536)
            .await
            .unwrap()
            .as_ref(),
        b"attachment bytes"
    );
    assert_eq!(
        request(
            &app,
            upload(content.replace("hello.txt", "../traversal.txt"))
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );

    let models = json_req(&app, Method::GET, "/api/stats/orch", None).await;
    assert_eq!(models[0]["total_runs"], 4);
    assert_eq!(models[0]["completed_runs"], 3);
    assert_eq!(models[0]["total_output_tokens"], 3);
    assert_eq!(models[0]["total_cache_read_tokens"], 0);
    assert_eq!(models[0]["total_cache_creation_tokens"], 0);
    assert_eq!(models[0]["total_reasoning_tokens"], 0);

    let meta = json_req(&app, Method::GET, "/api/meta", None).await;
    assert_eq!(meta["telemetry_enabled"], false);
    assert_eq!(meta["privacy"]["allowed_outbound_urls"], json!([]));
    let index = request(&app, Request::get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(index.status(), StatusCode::OK);
    assert_eq!(index.headers()["x-content-type-options"], "nosniff");
    assert_eq!(index.headers()["referrer-policy"], "no-referrer");
    assert_eq!(index.headers()["x-frame-options"], "DENY");
    assert!(
        index.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    assert_eq!(
        to_bytes(index.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .as_ref(),
        include_bytes!("../../../web/index.html")
    );
}

#[tokio::test]
async fn restart_recovers_stale_running_and_rate_limited_runs() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let cfg = state.config.clone();
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let mut threads = Vec::new();
    for _ in 0..4 {
        let id = create_parent(&app, &tmp.0).await;
        json_req(
            &app,
            Method::POST,
            &format!("/api/threads/{id}/messages"),
            Some(json!({"prompt":"fixture"})),
        )
        .await;
        wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
        threads.push(id);
    }
    drop(app);
    {
        let db = rusqlite::Connection::open(cfg.db_path()).unwrap();
        for (id, status) in threads.iter().zip(["running", "rate-limited"]) {
            let id = id.to_string();
            let status = json!(status).to_string();
            db.execute(
                "UPDATE threads SET status = ?1 WHERE id = ?2",
                [&status, &id],
            )
            .unwrap();
            db.execute(
                "UPDATE runs SET status = ?1, completed_at = NULL WHERE thread_id = ?2",
                [&status, &id],
            )
            .unwrap();
        }
        // A run that ended rate limited is not in flight.
        let ended = threads[3].to_string();
        let limited = json!("rate-limited").to_string();
        db.execute(
            "UPDATE threads SET status = ?1 WHERE id = ?2",
            [&limited, &ended],
        )
        .unwrap();
        db.execute(
            "UPDATE runs SET status = ?1 WHERE thread_id = ?2",
            [&limited, &ended],
        )
        .unwrap();
    }
    let app = router(tmp.state(AuthConfig::LocalOnly));
    for (i, id) in threads.into_iter().enumerate() {
        let response = json_req(&app, Method::GET, &format!("/api/threads/{id}"), None).await;
        let (thread_status, run_status) = match i {
            0 | 1 => ("interrupted", "cancelled"),
            2 => ("completed", "completed"),
            _ => ("rate-limited", "rate-limited"),
        };
        assert_eq!(response["thread"]["status"], thread_status);
        assert_eq!(response["runs"][0]["status"], run_status);
        assert!(response["runs"][0]["completed_at"].is_string());
    }
}

/// A crash leaves the run in flight: the first server's runtime goes away without letting the
/// run finish, and a new server on the same data directory resumes the run's session.
#[test]
fn restart_resumes_an_interrupted_run_with_its_session() {
    let tmp = TempDir::new();
    let crashed = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (thread_id, run_id) = crashed.block_on(async {
        let state = tmp.state(AuthConfig::LocalOnly);
        let mut events = state.coordinator.subscribe();
        let app = router(state);
        let id = create_parent(&app, &tmp.0).await;
        let run = json_req(
            &app,
            Method::POST,
            &format!("/api/threads/{id}/messages"),
            Some(json!({"prompt": "[slow] original request"})),
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(
                events.recv().await.unwrap(),
                WsServerMessage::Delta { thread_id, .. } if thread_id == id
            ) {}
        })
        .await
        .unwrap();
        let run_id: Uuid = run["run_id"].as_str().unwrap().parse().unwrap();
        (id, run_id)
    });
    crashed.shutdown_background();
    // The mock names a new session after the run, so give the provider session a distinct id to
    // tell a resume apart from a new session.
    rusqlite::Connection::open(tmp.0.join("server").join("state.sqlite"))
        .unwrap()
        .execute(
            "UPDATE threads SET provider_session_id = 'crashed-session' WHERE id = ?1",
            [thread_id.to_string()],
        )
        .unwrap();

    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let state = tmp.state(AuthConfig::LocalOnly);
        let coordinator = state.coordinator.clone();
        let mut events = coordinator.subscribe();
        let app = router(state);
        let path = format!("/api/threads/{thread_id}");
        let detail = json_req(&app, Method::GET, &path, None).await;
        assert_eq!(detail["runs"][0]["status"], "queued");
        coordinator.resume_queued_runs().await.unwrap();
        wait_for_thread(&mut events, thread_id, ThreadStatus::Completed).await;

        let detail = json_req(&app, Method::GET, &path, None).await;
        let runs = detail["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["id"], run_id.to_string());
        assert_eq!(runs[0]["status"], "completed");
        assert_eq!(runs[0]["provider_session_id"], "crashed-session");
        let items = detail["items"].as_array().unwrap();
        let reply = items
            .iter()
            .rfind(|item| item["kind"] == "assistant_message")
            .and_then(|item| item["payload"]["text"].as_str())
            .unwrap();
        assert!(
            reply.starts_with("The harness restarted and interrupted your previous turn."),
            "{reply}"
        );
        assert!(!reply.contains("original request"), "{reply}");
        assert!(!reply.contains("Project instructions:") && !reply.contains("Context from"));
        assert!(items.iter().any(|item| {
            item["kind"] == "system_notice"
                && item["payload"]["prompt"]
                    .as_str()
                    .is_some_and(|p| p == reply)
        }));
    });
}

#[tokio::test]
async fn queue_and_delegated_cancellation() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    let path = format!("/api/threads/{id}");
    let messages = format!("{path}/messages");
    let first = json_req(
        &app,
        Method::POST,
        &messages,
        Some(json!({"prompt":"[slow]"})),
    )
    .await;
    let queued = json_req(
        &app,
        Method::POST,
        &messages,
        Some(json!({"prompt":"queued","behavior":"queue"})),
    )
    .await;
    let busy = json_req(&app, Method::GET, &path, None).await;
    assert_eq!(busy["runs"][1]["status"], "queued");
    assert!(
        busy["items"].as_array().unwrap().iter().any(|item| {
            item["run_id"] == queued["run_id"] && item["payload"]["text"] == "queued"
        })
    );
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    let detail = json_req(&app, Method::GET, &path, None).await;
    assert_eq!(detail["runs"][0]["id"], first["run_id"]);
    assert_eq!(detail["runs"][1]["id"], queued["run_id"]);

    let child = mcp_tool(
        &app,
        "delegate_task",
        json!({
            "parent_thread_id":id,"title":"Cancel","task":"[slow]","provider":"mock","model":"mock",
        }),
    )
    .await;
    assert_eq!(child["status"], "running");
    let child = mcp_tool(&app, "task_cancel", json!({"task_id": child["task_id"]})).await;
    assert_eq!(child["status"], "interrupted");
    let status = mcp_tool(&app, "task_status", json!({"task_id": child["task_id"]})).await;
    assert_eq!(status["status"], "interrupted");
    let detail = json_req(
        &app,
        Method::GET,
        &format!(
            "/api/threads/{}",
            child["child_thread_id"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(detail["runs"][0]["status"], "cancelled");
}

#[tokio::test]
async fn usage_route_requires_authentication_and_valid_window() {
    let dir = TempDir::new();
    let app = router(dir.state(AuthConfig::Bearer {
        token: TOKEN.into(),
    }));
    assert_eq!(
        status(&app, "/api/usage?days=7", REMOTE, &[]).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            &app,
            "/api/usage?days=2",
            REMOTE,
            &[("authorization", &format!("Bearer {TOKEN}"))],
        )
        .await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn patch_thread_modes_and_walk_turn_item_pages() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    let path = format!("/api/threads/{id}");
    let patched = json_req(
        &app,
        Method::PATCH,
        &path,
        Some(json!({
            "runtime_mode":"auto-accept-edits","interaction_mode":"plan"
        })),
    )
    .await;
    assert_eq!(patched["runtime_mode"], "auto-accept-edits");
    assert_eq!(patched["interaction_mode"], "plan");
    json_req(
        &app,
        Method::POST,
        &format!("{path}/messages"),
        Some(json!({"prompt":"fixture"})),
    )
    .await;
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;

    let full = json_req(&app, Method::GET, &path, None).await;
    let detail = json_req(&app, Method::GET, &format!("{path}?limit=2"), None).await;
    let items = detail["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(detail["has_more"], true);
    assert_eq!(
        detail["total_items"],
        full["items"].as_array().unwrap().len()
    );
    assert_eq!(detail["thread"]["interaction_mode"], "plan");
    let cursor = &items[0]["ordinal"];
    let older = json_req(
        &app,
        Method::GET,
        &format!("{path}?limit=2&before_ordinal={cursor}"),
        None,
    )
    .await;
    assert_eq!(older["items"].as_array().unwrap().len(), 2);
    assert!(older["items"][1]["ordinal"].as_i64() < cursor.as_i64());
    let oldest = json_req(
        &app,
        Method::GET,
        &format!(
            "{path}?limit=2&before_ordinal={}",
            older["items"][0]["ordinal"]
        ),
        None,
    )
    .await;
    assert_eq!(oldest["has_more"], false);
    assert_eq!(oldest["items"][0]["kind"], "user_message");
}

#[tokio::test]
async fn usage_route_buckets_both_sides_of_jst_midnight() {
    use chrono::{Duration, FixedOffset, Utc};
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let store = state.store.clone();
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    let offset = FixedOffset::east_opt(9 * 3600).unwrap();
    let today_jst = Utc::now().with_timezone(&offset).date_naive();
    let midnight = (today_jst - Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(offset)
        .unwrap()
        .to_utc();
    for (ordinal, at, tokens, model) in [
        (1, midnight - Duration::seconds(1), 11, "fixture"),
        (2, midnight, 22, "fixture"),
        (3, midnight, 0, "zero-token-model"),
        (4, midnight, 1, "<synthetic>"),
    ] {
        store
            .create_run(Run {
                id: Uuid::new_v4(),
                thread_id: id,
                ordinal,
                provider: ProviderId::Mock,
                model: model.into(),
                effort: None,
                status: RunStatus::Completed,
                prompt: String::new(),
                provider_session_id: None,
                input_tokens: Some(tokens),
                output_tokens: Some(0),
                cache_read_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
                duration_ms: Some(1),
                error_message: None,
                started_at: at,
                completed_at: Some(at),
            })
            .await
            .unwrap();
    }
    let report = json_req(
        &app,
        Method::GET,
        "/api/usage?days=7&tz_offset_mins=540",
        None,
    )
    .await;
    let buckets = report["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 7);
    assert_eq!(buckets[4]["by_provider"]["mock"]["total_tokens"], 11);
    assert_eq!(buckets[5]["by_provider"]["mock"]["total_tokens"], 23);
    assert_eq!(buckets[6]["key"], today_jst.to_string());
    assert!(
        report["window"]["since"]
            .as_str()
            .unwrap()
            .ends_with("T15:00:00Z")
    );
    let models = report["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["provider"] == "mock")
        .map(|row| row["model"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(models, vec!["fixture"]);
}

#[tokio::test]
async fn attachment_download_sanitizes_and_encodes_stored_filename() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let bytes = b"legacy attachment";
    let hash = blake3::hash(bytes).to_hex().to_string();
    let path = state.config.attachments_dir().join(&hash);
    std::fs::create_dir_all(state.config.attachments_dir()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    let id = Uuid::new_v4();
    state
        .store
        .save_attachment(t3_core::Attachment {
            id,
            thread_id: None,
            filename: "報告\r\n\"/\\file.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: bytes.len() as u64,
            blake3_hash: hash,
            storage_path: path,
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let app = router(state);
    let response = request(
        &app,
        Request::get(format!("/api/attachments/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"_______file.txt\"; filename*=UTF-8''%E5%A0%B1%E5%91%8A_____file.txt"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn internal_mcp_token_is_revoked_as_soon_as_run_finishes() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new();
    let driver = tmp.0.join("fixture-codex");
    std::fs::write(
        &driver,
        r#"#!/bin/sh
umask 077
cat > /dev/null
printf '%s' "$T3_MCP_TOKEN" > mcp-token
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"active"}}'
while [ ! -f release ]; do sleep 0.02; done
printf '%s\n' '{"type":"turn.completed"}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = ServerConfig {
        data_dir: tmp.0.join("server"),
        enabled_providers: vec![ProviderId::Mock, ProviderId::Codex],
        ..Default::default()
    };
    config.providers.codex_bin = driver;
    let state = ServerState::new(config, "http://127.0.0.1:3774".into()).unwrap();
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({
            "prompt":"fixture","model_selection":{"provider":"codex","model":"fixture"}
        })),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(events.recv().await.unwrap(), WsServerMessage::Delta { thread_id, .. } if thread_id == id) {
                break;
            }
        }
    }).await.unwrap();
    let token = std::fs::read_to_string(tmp.0.join("mcp-token")).unwrap();
    let call = |thread_id: Uuid| {
        request(
            &app,
            Request::post("/mcp")
                .extension(ConnectInfo(SocketAddr::new(LOCAL, 49152)))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header("x-t3-thread-id", thread_id.to_string())
                .body(Body::from(
                    r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                ))
                .unwrap(),
        )
    };
    assert_eq!(call(id).await.status(), StatusCode::OK);
    assert_eq!(
        call(Uuid::new_v4()).await.status(),
        StatusCode::UNAUTHORIZED
    );
    std::fs::write(tmp.0.join("release"), []).unwrap();
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    assert_eq!(call(id).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn exclusive_daemon_lock_is_held_by_the_router() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let config = (*state.config).clone();
    let app = router(state);
    let attempt = || ServerState::new(config.clone(), "http://127.0.0.1:3774".into());
    assert!(
        attempt()
            .err()
            .unwrap()
            .to_string()
            .contains("another daemon")
    );
    drop(app);
    assert!(attempt().is_ok());
}

#[tokio::test]
async fn swarm_mock_workers_post_results_and_wake_the_coordinator() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    let thread = format!("/api/threads/{id}");
    let swarm = format!("{thread}/swarm");
    assert_eq!(json_req(&app, Method::GET, &swarm, None).await, Value::Null);
    json_req(
        &app,
        Method::PATCH,
        &thread,
        Some(json!({"interaction_mode":"plan"})),
    )
    .await;
    json_req(
        &app,
        Method::POST,
        &format!("{thread}/messages"),
        Some(json!({"prompt":"/swarm Review the fixture"})),
    )
    .await;
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    let before = mcp_tool(&app, "swarm_status", json!({"thread_id":id})).await;
    assert_eq!(before["board_id"].as_str().unwrap().len(), 36);
    assert!(before.get("messages").is_none());
    let rejected = json_req(&app, Method::POST, "/mcp", Some(json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"swarm_add_worker","arguments":{"thread_id":id,"name":"unsafe","brief":"check","isolation":"worktree"}
        }
    }))).await;
    assert_eq!(rejected["result"]["isError"], true);
    // MockDriver echoes the prompt. The test sends each participant's MCP calls
    // over the same tool endpoint used by the external providers.
    let worker = mcp_tool(&app, "swarm_add_worker", json!({
        "thread_id":id,"name":"reviewer","brief":"Review the fixture","provider":"mock","model":"mock",
    })).await;
    assert_eq!(worker["brief_chars"], 18);
    assert!(worker.get("brief").is_none());
    let worker_id: Uuid = serde_json::from_value(worker["thread_id"].clone()).unwrap();
    let worker_path = format!("/api/threads/{worker_id}");
    wait_for_thread(&mut events, worker_id, ThreadStatus::Completed).await;
    let worker_detail = json_req(&app, Method::GET, &worker_path, None).await;
    assert_eq!(worker_detail["thread"]["parent_thread_id"], json!(id));
    assert_eq!(worker_detail["thread"]["interaction_mode"], "plan");
    assert!(
        worker_detail["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["kind"] == "assistant_message"
                    && item["payload"]["text"].as_str().is_some_and(|text| {
                        text.contains("swarm_post_message") && text.contains("Review the fixture")
                    })
            })
    );
    let note = json_req(
        &app,
        Method::POST,
        &format!("{swarm}/messages"),
        Some(json!({"text":"Fixture context from the user"})),
    )
    .await;
    let done = mcp_tool(
        &app,
        "swarm_post_message",
        json!({
            "thread_id":worker_id,"text":"@coordinator [done] Fixture checked",
        }),
    )
    .await;
    assert_eq!(done["woken"], json!(["coordinator"]));
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    let messages = mcp_tool(
        &app,
        "swarm_read_board",
        json!({
            "thread_id":worker_id,
        }),
    )
    .await;
    assert_eq!(messages.as_array().unwrap().len(), 2);
    assert_eq!(messages[0]["sender"], "user");
    assert_eq!(messages[1]["sender"], "reviewer");
    assert_eq!(messages[1]["mentions"], json!(["coordinator"]));
    let after_note = mcp_tool(
        &app,
        "swarm_read_board",
        json!({
            "thread_id":worker_id, "since_seq":note["head_seq"], "limit":1,
        }),
    )
    .await;
    assert_eq!(after_note.as_array().unwrap().len(), 1);
    assert_eq!(after_note[0]["sender"], "reviewer");
    let detail = json_req(&app, Method::GET, &thread, None).await;
    assert_eq!(detail["runs"].as_array().unwrap().len(), 2);
    let prompt = detail["runs"][1]["prompt"].as_str().unwrap();
    assert!(prompt.contains("Fixture context from the user"));
    assert!(prompt.contains("@coordinator [done] Fixture checked"));
    let board = json_req(&app, Method::GET, &format!("{worker_path}/swarm"), None).await;
    assert_eq!(board["board_id"], before["board_id"]);
    assert_eq!(board["members"][0]["last_read_seq"], done["seq"]);
    assert_eq!(board["members"][1]["state"], "waiting");
    let killed = mcp_tool(
        &app,
        "swarm_kill_worker",
        json!({"thread_id":id,"name":"reviewer"}),
    )
    .await;
    assert_eq!(killed["state"], "killed");
    assert!(killed.get("members").is_none());
    json_req(
        &app,
        Method::POST,
        &format!("{swarm}/messages"),
        Some(json!({"text":"@reviewer Please check again"})),
    )
    .await;
    let detail = json_req(&app, Method::GET, &worker_path, None).await;
    assert_eq!(detail["runs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn swarm_mcp_status_and_reader_return_compact_truncated_messages() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({"prompt":"/swarm inspect board"})),
    )
    .await;

    let long_text = "x".repeat(2_500);
    let mut first_post = Value::Null;
    for index in 0..21 {
        let text = if index == 0 {
            long_text.clone()
        } else {
            format!("post {index}")
        };
        let post = mcp_tool(
            &app,
            "swarm_post_message",
            json!({"thread_id":id,"text":text}),
        )
        .await;
        assert!(post.to_string().len() < 500, "{post}");
        if index == 0 {
            first_post = post;
        }
    }

    let status = mcp_tool(&app, "swarm_status", json!({"thread_id":id})).await;
    assert_eq!(status["message_count"], 21);
    assert!(status.get("messages").is_none());
    assert_eq!(
        status["members"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["last_read_seq", "model", "name", "provider", "state"]
    );
    let messages = mcp_tool(
        &app,
        "swarm_read_board",
        json!({"thread_id":id,"since_seq":0}),
    )
    .await;
    let messages = messages.as_array().unwrap();
    assert_eq!(messages.len(), 20);
    assert_eq!(messages[0]["text"].as_str().unwrap().len(), 2_000);
    assert_eq!(messages[0]["truncated"], true);

    let full = mcp_tool(
        &app,
        "swarm_read_board",
        json!({"thread_id":id,"seq":first_post["seq"]}),
    )
    .await;
    assert_eq!(full[0]["text"], long_text);
    assert_eq!(full[0]["truncated"], false);
}

#[tokio::test]
async fn coordinator_wake_prompt_excludes_posts_from_the_coordinator() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({"prompt":"/swarm coordinate"})),
    )
    .await;

    let worker = mcp_tool(
        &app,
        "swarm_add_worker",
        json!({"thread_id":id,"name":"reviewer","brief":"Review the fixture"}),
    )
    .await;
    let worker_id: Uuid = serde_json::from_value(worker["thread_id"].clone()).unwrap();
    let worker_path = format!("/api/threads/{worker_id}");
    let detail = json_req(&app, Method::GET, &worker_path, None).await;
    assert_eq!(detail["runs"].as_array().unwrap().len(), 1);

    let own_post = "coordinator private context that must not be replayed";
    mcp_tool(
        &app,
        "swarm_post_message",
        json!({"thread_id":id,"text":own_post}),
    )
    .await;
    mcp_tool(
        &app,
        "swarm_post_message",
        json!({"thread_id":worker_id,"text":"@coordinator [done] Fixture checked"}),
    )
    .await;

    let detail = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let detail = json_req(&app, Method::GET, &format!("/api/threads/{id}"), None).await;
            let runs = detail["runs"].as_array().unwrap();
            if runs.len() >= 2 && runs.iter().all(|run| run["status"] != "running") {
                break detail;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("coordinator wake did not finish");
    let prompt = detail["runs"][1]["prompt"].as_str().unwrap();
    assert!(prompt.contains("@coordinator [done] Fixture checked"));
    assert!(
        !prompt.contains(own_post),
        "coordinator's own post was replayed"
    );
}

// A coordinator without MCP emits T3_SWARM_ACTION lines that run after its turn.
// Calls up to `failing_turns` emit a worktree worker (fails: the workspace is not
// a git repository) followed by a plain post.
#[cfg(unix)]
async fn swarm_text_action_fixture(tmp: &TempDir, failing_turns: u32) -> (Router, Uuid) {
    use std::os::unix::fs::PermissionsExt;
    let driver = tmp.0.join("fixture-claude");
    std::fs::write(
        &driver,
        format!(
            r#"#!/bin/sh
IFS= read -r prompt
n=$(cat calls 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > calls
marker=$(printf '%s' "$prompt" | grep -o 'T3_SWARM_ACTION [0-9a-f-]*' | head -n 1)
if [ "$n" -le {failing_turns} ]; then
  text='planned\n'"$marker"' {{\"tool\":\"swarm_add_worker\",\"name\":\"w1\",\"brief\":\"x\",\"isolation\":\"worktree\"}}\n'"$marker"' {{\"tool\":\"swarm_post_message\",\"text\":\"after-failure\"}}'
else
  text="noted"
fi
printf '{{"type":"result","subtype":"success","result":"%s"}}\n' "$text"
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = ServerConfig {
        data_dir: tmp.0.join("server"),
        enabled_providers: vec![ProviderId::Mock, ProviderId::Claude],
        ..Default::default()
    };
    config.providers.claude_bin = driver;
    let state = ServerState::new(config, "http://127.0.0.1:3774".into()).unwrap();
    let app = router(state);
    let id = create_parent(&app, &tmp.0).await;
    (app, id)
}

#[cfg(unix)]
async fn start_swarm(app: &Router, id: Uuid) {
    json_req(
        app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({
            "prompt":"/swarm split the work",
            "model_selection":{"provider":"claude","model":"fixture"}
        })),
    )
    .await;
}

// One failing action must not fail the turn or drop later actions, and the
// coordinator (whose turn already ended) must be woken with the error.
#[cfg(unix)]
#[tokio::test]
async fn failed_swarm_text_action_is_reported_to_the_coordinator() {
    let tmp = TempDir::new();
    let (app, id) = swarm_text_action_fixture(&tmp, 1).await;
    start_swarm(&app, id).await;
    let detail = wait_for_runs(&app, id, 2).await;
    let runs = detail["runs"].as_array().unwrap();
    assert_eq!(runs[0]["status"], "completed", "{}", runs[0]);
    let messages = mcp_tool(
        &app,
        "swarm_read_board",
        json!({"thread_id":id,"since_seq":0}),
    )
    .await;
    let messages = messages.as_array().unwrap();
    assert!(
        messages.iter().any(|m| m["text"] == "after-failure"),
        "{messages:?}"
    );
    let notice = messages
        .iter()
        .find(|m| m["sender"] == "system")
        .unwrap_or_else(|| panic!("{messages:?}"));
    assert_eq!(notice["mentions"], json!(["coordinator"]));
    let text = notice["text"].as_str().unwrap();
    assert!(text.contains("swarm_add_worker w1"), "{text}");
    assert!(text.contains(&tmp.0.display().to_string()), "{text}");
    assert!(
        runs[1]["prompt"]
            .as_str()
            .unwrap()
            .contains("swarm_add_worker w1")
    );
}

// A coordinator that repeats the same failing action must stop being woken.
#[cfg(unix)]
#[tokio::test]
async fn repeated_failing_swarm_action_stops_at_the_wake_depth_limit() {
    let tmp = TempDir::new();
    let (app, id) = swarm_text_action_fixture(&tmp, u32::MAX).await;
    start_swarm(&app, id).await;
    wait_for_runs(&app, id, 2).await;
    let mut last = 0;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let calls: u32 = std::fs::read_to_string(tmp.0.join("calls"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        if calls == last && calls > 1 {
            break;
        }
        last = calls;
    }
    assert!((2..=10).contains(&last), "coordinator woken {last} times");
}

#[cfg(unix)]
async fn wait_for_runs(app: &Router, id: Uuid, count: usize) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let detail = json_req(app, Method::GET, &format!("/api/threads/{id}"), None).await;
            let runs = detail["runs"].as_array().unwrap();
            if runs.len() >= count && runs.iter().all(|run| run["status"] != "running") {
                return detail;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("swarm runs did not finish")
}

#[cfg(unix)]
#[tokio::test]
async fn usage_limits_probes_claude_and_codex_clis_with_cache_and_refresh() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new();
    let claude_counter = tmp.0.join("claude-count");
    let fake_claude = tmp.0.join("fake-claude");
    std::fs::write(
        &fake_claude,
        format!(
            r#"#!/bin/sh
printf '1\n' >> "{}"
read -r _line
printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"t3_limits","response":{{"subscription_type":"pro","rate_limits_available":true,"rate_limits":{{"five_hour":{{"utilization":40,"resets_at":"2026-11-25T08:00:00Z"}},"seven_day":{{"utilization":55,"resets_at":"2026-11-27T00:00:00Z"}}}}}}}}}}'
"#,
            claude_counter.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o700)).unwrap();

    let fake_codex = tmp.0.join("fake-codex");
    let reset_flag = tmp.0.join("codex-reset-applied");
    let idempotency_log = tmp.0.join("codex-idempotency-key");
    std::fs::write(
        &fake_codex,
        format!(
            r#"#!/bin/sh
read -r _init
printf '%s\n' '{{"id":1,"result":{{}}}}'
read -r req
case "$req" in
  *rateLimitResetCredit/consume*)
    printf '%s\n' "$req" > "{}"
    touch "{}"
    printf '%s\n' '{{"id":2,"result":{{"outcome":"reset"}}}}'
    ;;
  *)
    if [ -f "{}" ]; then
      printf '%s\n' '{{"id":2,"result":{{"rateLimits":{{"limitId":"codex","planType":"plus","primary":{{"usedPercent":0,"windowDurationMins":300,"resetsAt":1795593846}},"secondary":{{"usedPercent":0,"windowDurationMins":10080,"resetsAt":1795702939}}}},"rateLimitResetCredits":{{"availableCount":2,"credits":[{{"id":"c2","status":"available","expiresAt":1796163165}}]}}}}}}'
    else
      printf '%s\n' '{{"id":2,"result":{{"rateLimits":{{"limitId":"codex","planType":"plus","primary":{{"usedPercent":100,"windowDurationMins":300,"resetsAt":1795593846}},"secondary":{{"usedPercent":80,"windowDurationMins":10080,"resetsAt":1795702939}}}},"rateLimitResetCredits":{{"availableCount":3,"credits":[{{"id":"c1","status":"available","expiresAt":1796071038}}]}}}}}}'
    fi
    ;;
esac
"#,
            idempotency_log.display(),
            reset_flag.display(),
            reset_flag.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Local mock server for Antigravity retrieveUserQuotaSummary
    let agy_listener = tokio::net::TcpListener::bind((LOCAL, 0)).await.unwrap();
    let agy_addr = agy_listener.local_addr().unwrap();
    let agy_app = axum::Router::new().route(
        "/v1internal:retrieveUserQuotaSummary",
        axum::routing::post(|| async {
            axum::Json(json!({
                "groups": [
                    {
                        "displayName": "Gemini Models",
                        "buckets": [
                            { "bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 0.8, "description": "used", "resetTime": "2026-12-01T08:28:09Z" },
                            { "bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.58, "description": "used", "resetTime": "2026-11-25T08:00:23Z" }
                        ]
                    },
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            { "bucketId": "3p-5h", "window": "5h", "remainingFraction": 1.0 },
                            { "bucketId": "3p-weekly", "window": "weekly", "remainingFraction": 1.0 }
                        ]
                    }
                ]
            }))
        }),
    );
    let agy_task = tokio::spawn(async move {
        let _ = axum::serve(agy_listener, agy_app).await;
    });

    let agy_dir = tmp.0.join("agy-data");
    std::fs::create_dir_all(&agy_dir).unwrap();
    let future_expiry = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    std::fs::write(
        agy_dir.join("antigravity-oauth-token"),
        json!({ "token": { "access_token": "fixture-token", "expiry": future_expiry } })
            .to_string(),
    )
    .unwrap();

    let mut config = ServerConfig {
        data_dir: tmp.0.join("server"),
        enabled_providers: vec![
            ProviderId::Claude,
            ProviderId::Codex,
            ProviderId::Antigravity,
            ProviderId::OpenCode,
        ],
        antigravity_quota_probe: true,
        ..Default::default()
    };
    config.providers.claude_bin = fake_claude;
    config.providers.codex_bin = fake_codex;
    config.providers.antigravity_data_dir = Some(agy_dir);
    config.providers.antigravity_quota_endpoints = Some(vec![format!("http://{agy_addr}")]);
    let state = ServerState::new(config, "http://127.0.0.1:3774".into()).unwrap();
    let app = router(state);

    let first = json_req(&app, Method::GET, "/api/usage/limits", None).await;
    let providers = first["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 4);
    assert_eq!(providers[0]["provider"], "claude");
    assert_eq!(providers[0]["status"], "ok");
    assert_eq!(providers[0]["plan"], "Pro");
    assert_eq!(providers[0]["windows"][0]["remaining_percent"], 60.0);
    assert_eq!(providers[0]["windows"][1]["remaining_percent"], 45.0);
    assert_eq!(providers[1]["provider"], "codex");
    assert_eq!(providers[1]["status"], "ok");
    assert_eq!(providers[1]["plan"], "ChatGPT Plus Subscription");
    assert_eq!(providers[1]["windows"][0]["remaining_percent"], 0.0);
    assert_eq!(providers[1]["windows"][1]["remaining_percent"], 20.0);
    assert_eq!(providers[1]["reset_credits"]["available_count"], 3);
    assert_eq!(providers[2]["provider"], "antigravity");
    assert_eq!(providers[2]["status"], "ok");
    assert_eq!(providers[2]["source"], "antigravity-api");
    assert_eq!(providers[2]["windows"].as_array().unwrap().len(), 4);
    assert_eq!(providers[2]["windows"][0]["label"], "Gemini · 5-hour");
    assert_eq!(providers[2]["windows"][0]["remaining_percent"], 58.0);
    assert_eq!(providers[2]["windows"][1]["label"], "Gemini · Weekly");
    assert_eq!(providers[2]["windows"][1]["remaining_percent"], 80.0);
    assert_eq!(
        providers[2]["windows"][2]["label"],
        "Claude & GPT (shared) · 5-hour"
    );
    assert_eq!(providers[3]["provider"], "open-code");
    assert_eq!(providers[3]["status"], "unsupported");

    // Cached call should not re-spawn the CLI probe.
    let second = json_req(&app, Method::GET, "/api/usage/limits", None).await;
    assert_eq!(second["checked_at"], first["checked_at"]);
    assert_eq!(
        std::fs::read_to_string(&claude_counter)
            .unwrap()
            .lines()
            .count(),
        1
    );

    // Explicit refresh re-runs the probe.
    let _refreshed = json_req(&app, Method::GET, "/api/usage/limits?refresh=1", None).await;
    assert_eq!(
        std::fs::read_to_string(&claude_counter)
            .unwrap()
            .lines()
            .count(),
        2
    );

    // Redeem Codex reset credit via POST /api/usage/limits/reset-credit.
    let redeemed = json_req(
        &app,
        Method::POST,
        "/api/usage/limits/reset-credit",
        Some(json!({ "provider": "codex" })),
    )
    .await;
    assert_eq!(redeemed["outcome"], "reset");
    assert_eq!(
        redeemed["limits"]["providers"][1]["windows"][0]["remaining_percent"],
        100.0
    );
    assert_eq!(
        redeemed["limits"]["providers"][1]["reset_credits"]["available_count"],
        2
    );
    let logged_req = std::fs::read_to_string(&idempotency_log).unwrap();
    assert!(logged_req.contains("idempotencyKey"));
    agy_task.abort();
}

#[tokio::test]
async fn thread_detail_returns_context_and_items_breakdown() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let app = router(state);
    let providers = json_req(&app, Method::GET, "/api/providers", None).await;
    let mock_models = providers
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "mock")
        .unwrap()["models"]
        .as_array()
        .unwrap();
    assert_eq!(mock_models[0]["context_window"], 200_000);

    let id = create_parent(&app, &tmp.0).await;
    let _ = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({ "prompt": "context panel check" })),
    )
    .await;
    // Wait briefly for the mock run to complete.
    let mut detail = Value::Null;
    for _ in 0..40 {
        detail = json_req(&app, Method::GET, &format!("/api/threads/{id}"), None).await;
        if detail["thread"]["status"] == "completed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(detail["thread"]["status"], "completed");
    let ctx = &detail["context"];
    assert_eq!(ctx["context_window"], 200_000);
    assert_eq!(ctx["source"], "live");
    assert_eq!(ctx["used_tokens"], 2);
    assert_eq!(ctx["total_input_tokens"], 1);
    assert_eq!(ctx["total_output_tokens"], 1);
    let breakdown = ctx["items_breakdown"].as_array().unwrap();
    assert!(
        breakdown
            .iter()
            .any(|b| b["kind"] == "user_message" && b["count"].as_u64().unwrap() >= 1)
    );
    assert!(
        breakdown
            .iter()
            .any(|b| b["kind"] == "assistant_message" && b["count"].as_u64().unwrap() >= 1)
    );

    // Drop in-memory live context to simulate restart; stored DB context must survive.
    drop(app);
    let reopened = tmp.state(AuthConfig::LocalOnly);
    let app2 = router(reopened);
    let after_restart = json_req(&app2, Method::GET, &format!("/api/threads/{id}"), None).await;
    assert_eq!(after_restart["context"]["source"], "stored");
    assert_eq!(after_restart["context"]["used_tokens"], 2);
    assert_eq!(after_restart["context"]["context_window"], 200_000);

    // Embedded ES modules under /js/{name} serve .js files and reject traversal/non-js paths.
    let js_ok = request(
        &app2,
        Request::get("/js/util.js").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(js_ok.status(), StatusCode::OK);
    let js_bad = request(
        &app2,
        Request::get("/js/..%2Findex.html")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(js_bad.status(), StatusCode::NOT_FOUND);
    let js_non = request(
        &app2,
        Request::get("/js/util.txt").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(js_non.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn shell_exec_runs_in_thread_and_project_and_records_history() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let app = router(state);
    let project = json_req(
        &app,
        Method::POST,
        "/api/projects",
        Some(json!({"title": "Shell Test", "workspace_root": tmp.0})),
    )
    .await;
    let project_id = project["id"].as_str().unwrap();

    // Project-level exec runs in workspace and disables telemetry.
    let proj_res = json_req(
        &app,
        Method::POST,
        &format!("/api/projects/{project_id}/exec"),
        Some(json!({"command": "printf '%s:%s' \"$DO_NOT_TRACK\" \"$T3CODE_TELEMETRY_ENABLED\""})),
    )
    .await;
    assert_eq!(proj_res["exit_code"], 0);
    assert_eq!(proj_res["stdout"], "1:false");
    assert_eq!(proj_res["timed_out"], false);

    // Empty command returns 400.
    let bad = request(
        &app,
        Request::post(format!("/api/projects/{project_id}/exec"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"command":"   "}"#))
            .unwrap(),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    // Thread-level exec persists a CommandExecution item in the thread timeline.
    let thread = json_req(
        &app,
        Method::POST,
        "/api/threads",
        Some(json!({
            "project_id": project_id,
            "title": "Shell Thread",
            "model_selection": {"provider": "mock", "model": "mock"}
        })),
    )
    .await;
    let thread_id = thread["id"].as_str().unwrap();
    let thr_res = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{thread_id}/exec"),
        Some(json!({"command": "echo hello-shell && echo err-line >&2"})),
    )
    .await;
    assert_eq!(thr_res["exit_code"], 0);
    assert!(thr_res["stdout"].as_str().unwrap().contains("hello-shell"));
    assert!(thr_res["stderr"].as_str().unwrap().contains("err-line"));
    assert!(thr_res["item_id"].is_string());

    let detail = json_req(
        &app,
        Method::GET,
        &format!("/api/threads/{thread_id}"),
        None,
    )
    .await;
    let items = detail["items"].as_array().unwrap();
    assert!(items.iter().any(|it| {
        it["kind"] == "command_execution"
            && it["payload"]["source"] == "user_shell"
            && it["payload"]["output"]
                .as_str()
                .unwrap_or("")
                .contains("hello-shell")
    }));

    // Timeout and backgrounded descendants must terminate cleanly without hanging.
    let to_res = json_req(
        &app,
        Method::POST,
        &format!("/api/projects/{project_id}/exec"),
        Some(json!({"command": "sleep 10 & sleep 10", "timeout_secs": 1})),
    )
    .await;
    assert_eq!(to_res["timed_out"], true);
    assert!(to_res["exit_code"].is_null());

    // Shell exec in a thread must not disrupt subsequent AI run session continuity.
    let _ = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{thread_id}/messages"),
        Some(json!({"prompt": "after shell"})),
    )
    .await;
    let mut after = Value::Null;
    for _ in 0..40 {
        after = json_req(
            &app,
            Method::GET,
            &format!("/api/threads/{thread_id}"),
            None,
        )
        .await;
        if after["thread"]["status"] == "completed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(after["thread"]["status"], "completed");
}

#[tokio::test]
async fn checkpoints_git_actions_and_queue_endpoints_work() {
    let tmp = TempDir::new();
    let workspace = tmp.0.join("repo");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q"]);
    std::fs::write(workspace.join("a.txt"), "initial\n").unwrap();
    git(&workspace, &["add", "a.txt"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=T3 Test",
            "-c",
            "user.email=t3@example.test",
            "commit",
            "-qm",
            "init",
        ],
    );

    let state = tmp.state(AuthConfig::LocalOnly);
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let id = create_parent(&app, &workspace).await;

    let msg = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({"prompt": "checkpoint run"})),
    )
    .await;
    let run_id = msg["run_id"].as_str().unwrap();
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;

    let detail = json_req(&app, Method::GET, &format!("/api/threads/{id}"), None).await;
    assert!(
        detail["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some(run_id))
    );

    // Mutate file & add untracked file, then rollback to pre-run checkpoint.
    std::fs::write(workspace.join("a.txt"), "mutated\n").unwrap();
    std::fs::write(workspace.join("extra.txt"), "temp\n").unwrap();
    let rb = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/runs/{run_id}/rollback"),
        Some(json!({})),
    )
    .await;
    assert_eq!(rb["ok"], true);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "initial\n"
    );
    assert!(!workspace.join("extra.txt").exists());

    // Stage, discard, and commit endpoints.
    std::fs::write(workspace.join("a.txt"), "discard-me\n").unwrap();
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/git/discard"),
        Some(json!({"path": "a.txt"})),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "initial\n"
    );
    std::fs::write(workspace.join("a.txt"), "committed\n").unwrap();
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/git/stage"),
        Some(json!({"path": "a.txt"})),
    )
    .await;
    let committed = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/git/commit"),
        Some(json!({"message": "update a.txt"})),
    )
    .await;
    assert!(committed["sha"].as_str().unwrap().len() >= 7);

    // Queue list & individual cancel while a slow run is active.
    let _slow = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({"prompt": "[slow] active turn"})),
    )
    .await;
    let queued = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({"prompt": "queued follow-up", "behavior": "queue"})),
    )
    .await;
    let q_id = queued["run_id"].as_str().unwrap();
    let q_list = json_req(&app, Method::GET, &format!("/api/threads/{id}/queue"), None).await;
    assert!(
        q_list
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"].as_str() == Some(q_id))
    );
    let del = request(
        &app,
        Request::delete(format!("/api/threads/{id}/queue/{q_id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    let _ = request(
        &app,
        Request::post(format!("/api/threads/{id}/interrupt"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
}

#[tokio::test]
async fn project_patch_workspace_meta_and_instructions_injection() {
    let tmp = TempDir::new();
    let workspace = tmp.0.join("ws");
    std::fs::create_dir_all(workspace.join(".claude/commands")).unwrap();
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join(".claude/commands/deploy.md"),
        "# Deploy helper\nDeploys safely\n",
    )
    .unwrap();
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();

    let state = tmp.state(AuthConfig::LocalOnly);
    let store = state.store.clone();
    let mut events = state.coordinator.subscribe();
    let app = router(state);

    let proj = json_req(
        &app,
        Method::POST,
        "/api/projects",
        Some(json!({"title": "Meta Proj", "workspace_root": workspace})),
    )
    .await;
    let pid = proj["id"].as_str().unwrap();

    let updated = json_req(
        &app,
        Method::PATCH,
        &format!("/api/projects/{pid}"),
        Some(json!({
            "title": "Meta Proj v2",
            "instructions": "Always respond in Japanese.",
            "verify_command": "echo ok"
        })),
    )
    .await;
    assert_eq!(updated["title"], "Meta Proj v2");
    assert_eq!(updated["instructions"], "Always respond in Japanese.");
    assert_eq!(updated["verify_command"], "echo ok");

    let meta = json_req(
        &app,
        Method::GET,
        &format!("/api/projects/{pid}/workspace-meta"),
        None,
    )
    .await;
    assert!(
        meta["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "deploy")
    );
    assert!(
        meta["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "src/main.rs")
    );

    // First turn in a new thread receives the project instructions prefix.
    let thread = json_req(
        &app,
        Method::POST,
        "/api/threads",
        Some(json!({
            "project_id": pid,
            "title": "Instructions Check",
            "model_selection": {"provider": "mock", "model": "mock"}
        })),
    )
    .await;
    let tid: Uuid = thread["id"].as_str().unwrap().parse().unwrap();
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{tid}/messages"),
        Some(json!({"prompt": "say hi"})),
    )
    .await;
    wait_for_thread(&mut events, tid, ThreadStatus::Completed).await;
    let detail = json_req(&app, Method::GET, &format!("/api/threads/{tid}"), None).await;
    let reply = detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|item| item["kind"] == "assistant_message")
        .and_then(|item| item["payload"]["text"].as_str())
        .unwrap();
    assert!(reply.contains("Project instructions:\nAlways respond in Japanese."));

    // Native resume should reuse provider context without sending the instructions again.
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{tid}/messages"),
        Some(json!({"prompt": "continue naturally"})),
    )
    .await;
    wait_for_thread(&mut events, tid, ThreadStatus::Completed).await;
    let detail = json_req(&app, Method::GET, &format!("/api/threads/{tid}"), None).await;
    let replies: Vec<_> = detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "assistant_message")
        .filter_map(|item| item["payload"]["text"].as_str())
        .collect();
    assert_eq!(replies.len(), 2);
    assert!(!replies[1].contains("Project instructions:"));
    assert!(replies[1].contains("continue naturally"));

    // A model switch starts a fresh provider session and must keep instructions
    // ahead of the previous-run handoff and current request.
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{tid}/messages"),
        Some(json!({
            "prompt": "continue after switching models",
            "model_selection": {"provider": "mock", "model": "mock-2"}
        })),
    )
    .await;
    wait_for_thread(&mut events, tid, ThreadStatus::Completed).await;
    let detail = json_req(&app, Method::GET, &format!("/api/threads/{tid}"), None).await;
    let reply = detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|item| item["kind"] == "assistant_message")
        .and_then(|item| item["payload"]["text"].as_str())
        .unwrap();
    assert!(reply.starts_with("Project instructions:\nAlways respond in Japanese."));
    assert!(reply.contains("Context from the previous run:"));
    assert!(reply.contains("Current request:\ncontinue after switching models"));

    // A prior completed run without a provider session also starts fresh.
    store
        .create_run(Run {
            id: Uuid::new_v4(),
            thread_id: tid,
            ordinal: 4,
            provider: ProviderId::Mock,
            model: "mock-2".into(),
            effort: None,
            status: RunStatus::Completed,
            prompt: "synthetic prior run without a session".into(),
            provider_session_id: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            duration_ms: None,
            error_message: None,
            started_at: chrono::Utc::now(),
            completed_at: Some(chrono::Utc::now()),
        })
        .await
        .unwrap();
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{tid}/messages"),
        Some(json!({"prompt": "continue without a provider session"})),
    )
    .await;
    wait_for_thread(&mut events, tid, ThreadStatus::Completed).await;
    let detail = json_req(&app, Method::GET, &format!("/api/threads/{tid}"), None).await;
    let reply = detail["items"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|item| item["kind"] == "assistant_message")
        .and_then(|item| item["payload"]["text"].as_str())
        .unwrap();
    assert!(reply.starts_with("Project instructions:\nAlways respond in Japanese."));
    assert!(reply.contains("Context from the previous run:"));
    assert!(reply.contains("Current request:\ncontinue without a provider session"));
}

/// Writes the transcript Claude keeps for `session`, so the pre-resume check accepts it.
fn claude_transcript(home: &Path, workspace: &Path, session: &str) {
    let workspace = std::fs::canonicalize(workspace).unwrap();
    let project: String = workspace
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = home.join(".claude").join("projects").join(project);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{session}.jsonl")),
        "{\"type\":\"user\",\"uuid\":\"fixture-message\"}\n",
    )
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn claude_follow_up_resumes_only_a_session_with_a_transcript() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new();
    let driver = tmp.0.join("fixture-claude");
    std::fs::write(
        &driver,
        r#"#!/bin/sh
set -eu
IFS= read -r line
printf '%s\n' "$line" >> stdin.jsonl
printf '%s\n' "$*" >> argv.txt
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fixture-session"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"fixture complete"}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = ServerConfig {
        data_dir: tmp.0.join("server"),
        enabled_providers: vec![ProviderId::Claude],
        ..Default::default()
    };
    config.providers.claude_bin = driver;
    let state = ServerState::new(config, "http://127.0.0.1:3774".into()).unwrap();
    let home = tmp.0.join("home");
    state.coordinator.set_session_home(Some(home.clone()));
    let mut events = state.coordinator.subscribe();
    let app = router(state);
    let project = json_req(
        &app,
        Method::POST,
        "/api/projects",
        Some(json!({"title": "Resume fixture", "workspace_root": tmp.0})),
    )
    .await;
    json_req(
        &app,
        Method::PATCH,
        &format!("/api/projects/{}", project["id"].as_str().unwrap()),
        Some(json!({"instructions": "Project instruction fixture."})),
    )
    .await;
    let thread = json_req(
        &app,
        Method::POST,
        "/api/threads",
        Some(json!({
            "project_id": project["id"],
            "title": "Resume fixture",
            "model_selection": {"provider": "claude", "model": "fixture"}
        })),
    )
    .await;
    let id = thread["id"].as_str().unwrap();
    let id: Uuid = id.parse().unwrap();
    for prompt in ["first", "second"] {
        json_req(
            &app,
            Method::POST,
            &format!("/api/threads/{id}/messages"),
            Some(json!({"prompt": prompt})),
        )
        .await;
        wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    }
    claude_transcript(&home, &tmp.0, "fixture-session");
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{id}/messages"),
        Some(json!({"prompt": "third"})),
    )
    .await;
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;

    let argv = std::fs::read_to_string(tmp.0.join("argv.txt")).unwrap();
    let argv: Vec<_> = argv.lines().collect();
    assert!(!argv[0].contains("--resume"));
    // No transcript: the stored session is not passed on, and the new session gets the
    // project instructions and a handoff of the previous run.
    assert!(!argv[1].contains("--resume"), "{}", argv[1]);
    assert!(argv[2].contains("--resume fixture-session"), "{}", argv[2]);
    let input = std::fs::read_to_string(tmp.0.join("stdin.jsonl")).unwrap();
    let messages: Vec<Value> = input
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let second = messages[1]["message"]["content"].as_str().unwrap();
    assert!(second.starts_with("Project instructions:\nProject instruction fixture."));
    assert!(second.contains("Context from the previous run:"));
    assert!(second.ends_with("Current request:\nsecond"));
    assert_eq!(messages[2]["message"]["content"], "third");
}

#[cfg(unix)]
#[tokio::test]
async fn claude_pstack_slash_keeps_first_turn_context_and_native_resume() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new();
    let driver = tmp.0.join("fixture-claude");
    std::fs::write(
        &driver,
        r#"#!/bin/sh
set -eu
IFS= read -r line
printf '%s\n' "$line" >> stdin.jsonl
printf '%s\n' "$*" >> argv.txt
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fixture-session"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"fixture complete"}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = ServerConfig {
        data_dir: tmp.0.join("server"),
        enabled_providers: vec![ProviderId::Claude],
        ..Default::default()
    };
    config.providers.claude_bin = driver;
    let state = ServerState::new(config, "http://127.0.0.1:3774".into()).unwrap();
    let home = tmp.0.join("home");
    claude_transcript(&home, &tmp.0, "fixture-session");
    state.coordinator.set_session_home(Some(home));
    let mut events = state.coordinator.subscribe();
    let store = state.store.clone();
    let app = router(state);
    let project = json_req(
        &app,
        Method::POST,
        "/api/projects",
        Some(json!({
            "title": "Pstack fixture",
            "workspace_root": tmp.0
        })),
    )
    .await;
    json_req(
        &app,
        Method::PATCH,
        &format!("/api/projects/{}", project["id"].as_str().unwrap()),
        Some(json!({"instructions": "Project instruction fixture."})),
    )
    .await;
    let parent = json_req(
        &app,
        Method::POST,
        "/api/threads",
        Some(json!({
            "project_id": project["id"],
            "title": "Parent fixture",
            "model_selection": {"provider": "claude", "model": "fixture"}
        })),
    )
    .await;
    let fork = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{}/fork", parent["id"].as_str().unwrap()),
        Some(json!({"handoff_summary": "Prior thread fixture."})),
    )
    .await;
    let id: Uuid = fork["id"].as_str().unwrap().parse().unwrap();
    let attachment_id = Uuid::new_v4();
    let attachment_path = tmp.0.join("notes.txt");
    let notes = b"Fixture notes.";
    std::fs::write(&attachment_path, notes).unwrap();
    store
        .save_attachment(t3_core::Attachment {
            id: attachment_id,
            thread_id: Some(id),
            filename: "notes.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: notes.len() as u64,
            blake3_hash: blake3::hash(notes).to_hex().to_string(),
            storage_path: attachment_path.clone(),
            created_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let help = "/pstack:poteto-help Which skill handles a synthetic task?";
    let mode = "/pstack:poteto-mode Explain the fixture.";
    let verify = "/verify-t3code Check persistence";
    let playbook = "/pstack:create-verification-skill Inspect this fixture";
    for body in [
        json!({"prompt": help, "attachment_ids": [attachment_id]}),
        json!({"prompt": mode}),
        json!({"prompt": "Continue the conversation."}),
        json!({"prompt": verify, "attachment_ids": [attachment_id]}),
        json!({"prompt": playbook, "attachment_ids": [attachment_id]}),
    ] {
        json_req(
            &app,
            Method::POST,
            &format!("/api/threads/{id}/messages"),
            Some(body),
        )
        .await;
        wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    }

    let input = std::fs::read_to_string(tmp.0.join("stdin.jsonl")).unwrap();
    let messages: Vec<Value> = input
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let first = &messages[0]["message"]["content"];
    assert_eq!(first[1], json!({"type": "text", "text": help}), "{first}");
    let context = first[0]["text"].as_str().unwrap();
    assert!(context.contains("Project instructions:\nProject instruction fixture."));
    assert!(context.contains("Context from the previous thread:\nPrior thread fixture."));
    assert!(context.contains(&format!(
        "Attachments (file paths):\n- {}",
        attachment_path.display()
    )));
    assert_eq!(messages[1]["message"]["content"], mode);
    assert_eq!(
        messages[2]["message"]["content"],
        "Continue the conversation."
    );
    for (message, command) in messages[3..].iter().zip([verify, playbook]) {
        let content = &message["message"]["content"];
        assert_eq!(content[1]["text"], command);
        assert!(content[0]["text"].as_str().unwrap().contains("notes.txt"));
    }
    let argv = std::fs::read_to_string(tmp.0.join("argv.txt")).unwrap();
    let mut argv = argv.lines();
    assert!(!argv.next().unwrap().contains("--resume"));
    assert!(argv.next().unwrap().contains("--resume fixture-session"));
    assert!(argv.next().unwrap().contains("--resume fixture-session"));
}

#[tokio::test]
async fn tier4_streaming_shell_cancel_monitor_and_worktree_merge_routes() {
    let tmp = TempDir::new();
    let workspace = tmp.0.join("repo");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q"]);
    std::fs::write(workspace.join("base.txt"), "base\n").unwrap();
    git(&workspace, &["add", "base.txt"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=T3 Test",
            "-c",
            "user.email=t3@example.test",
            "commit",
            "-qm",
            "init",
        ],
    );

    let state = tmp.state(AuthConfig::LocalOnly);
    let mut events = state.coordinator.subscribe();
    let app = router(state);

    let proj = json_req(
        &app,
        Method::POST,
        "/api/projects",
        Some(json!({"title": "Tier4 Proj", "workspace_root": workspace})),
    )
    .await;
    let project_id = proj["id"].as_str().unwrap();

    let parent = json_req(
        &app,
        Method::POST,
        "/api/threads",
        Some(json!({
            "project_id": project_id,
            "title": "Parent",
            "model_selection": {"provider": "mock", "model": "mock"},
            "runtime_mode": "full-access"
        })),
    )
    .await;
    let parent_id: Uuid = parent["id"].as_str().unwrap().parse().unwrap();

    // 1. Streaming shell in thread emits ShellOutput + ShellFinished and persists 1 TurnItem.
    let started = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{parent_id}/exec/stream"),
        Some(json!({"command": "echo stream-ok"})),
    )
    .await;
    let exec_id: Uuid = started["exec_id"].as_str().unwrap().parse().unwrap();
    let mut saw_chunk = false;
    let mut saw_finished = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !saw_finished {
        if let Ok(Ok(msg)) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await {
            match msg {
                WsServerMessage::ShellOutput {
                    exec_id: id, chunk, ..
                } if id == exec_id && chunk.contains("stream-ok") => {
                    saw_chunk = true;
                }
                WsServerMessage::ShellFinished {
                    exec_id: id,
                    exit_code,
                    cancelled,
                    item_id,
                    ..
                } if id == exec_id => {
                    assert_eq!(exit_code, Some(0));
                    assert!(!cancelled);
                    assert!(item_id.is_some());
                    saw_finished = true;
                }
                _ => {}
            }
        }
    }
    assert!(saw_chunk, "expected ShellOutput chunk");
    assert!(saw_finished, "expected ShellFinished event");

    // 2. Streaming project shell can be cancelled via POST /api/exec/{exec_id}/cancel.
    let slow_shell = json_req(
        &app,
        Method::POST,
        &format!("/api/projects/{project_id}/exec/stream"),
        Some(json!({"command": "sleep 10", "timeout_secs": 20})),
    )
    .await;
    let slow_exec_id: Uuid = slow_shell["exec_id"].as_str().unwrap().parse().unwrap();
    let mon = json_req(&app, Method::GET, "/api/monitor", None).await;
    assert!(mon["active_shells"].as_u64().unwrap() >= 1);
    let cancel = json_req(
        &app,
        Method::POST,
        &format!("/api/exec/{slow_exec_id}/cancel"),
        None,
    )
    .await;
    assert_eq!(cancel["cancelled"], true);
    let mut saw_cancelled = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !saw_cancelled {
        if let Ok(Ok(WsServerMessage::ShellFinished {
            exec_id: id,
            cancelled,
            ..
        })) = tokio::time::timeout(Duration::from_secs(2), events.recv()).await
            && id == slow_exec_id
        {
            assert!(cancelled);
            saw_cancelled = true;
        }
    }
    assert!(saw_cancelled, "expected cancelled ShellFinished event");

    // 3. Worktree child thread HTTP diff + merge endpoints.
    let delegated = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{parent_id}/delegate"),
        Some(json!({
            "parent_thread_id": parent_id,
            "title": "Child Feature",
            "task": "prepare feature",
            "model_selection": {"provider": "mock", "model": "mock"},
            "runtime_mode": "full-access",
            "interaction_mode": "default",
            "isolation": "worktree"
        })),
    )
    .await;
    let child_id: Uuid = delegated["thread"]["id"].as_str().unwrap().parse().unwrap();
    let child_wt = std::path::PathBuf::from(
        delegated["thread"]["worktree_path"]
            .as_str()
            .expect("child worktree"),
    );
    wait_for_thread(&mut events, child_id, ThreadStatus::Completed).await;

    std::fs::write(child_wt.join("feature.txt"), "hello from worktree\n").unwrap();
    git(&child_wt, &["add", "feature.txt"]);
    git(
        &child_wt,
        &[
            "-c",
            "user.name=T3 Test",
            "-c",
            "user.email=t3@example.test",
            "commit",
            "-qm",
            "add feature",
        ],
    );

    let child_diff = json_req(
        &app,
        Method::GET,
        &format!("/api/threads/{parent_id}/children/{child_id}/diff"),
        None,
    )
    .await;
    assert!(
        child_diff["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "feature.txt")
    );

    let merged = json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{parent_id}/children/{child_id}/merge"),
        Some(json!({"cleanup": true})),
    )
    .await;
    assert_eq!(merged["merged"], true);
    assert_eq!(merged["cleaned_up"], true);
    assert_eq!(
        std::fs::read_to_string(workspace.join("feature.txt")).unwrap(),
        "hello from worktree\n"
    );

    // 4. Thread detail includes compaction_econ payload.
    let detail = json_req(
        &app,
        Method::GET,
        &format!("/api/threads/{parent_id}"),
        None,
    )
    .await;
    assert!(detail["context"]["compaction"]["utilization_pct"].is_f64());
    assert!(detail["context"]["compaction"]["recommended"].is_boolean());
}

#[cfg(unix)]
#[tokio::test]
async fn tier4_verify_autofix_loop_and_quota_fallback() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new();
    let workspace = tmp.0.join("repo");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q"]);
    std::fs::write(workspace.join("a.txt"), "v0\n").unwrap();
    git(&workspace, &["add", "a.txt"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=T3 Test",
            "-c",
            "user.email=t3@example.test",
            "commit",
            "-qm",
            "init",
        ],
    );

    // Fake Claude CLI:
    // - On prompt containing "quota-trigger": emits a weekly quota RateLimited error with no side effects.
    // - On normal prompt: modifies a.txt (triggering git tree diff) without creating ok.txt.
    // - On verify auto-fix prompt (contains "verify command"): creates ok.txt so verify_command passes!
    let fake_claude = tmp.0.join("fake-claude-t4");
    let count_file = tmp.0.join("claude-runs.txt");
    std::fs::write(
        &fake_claude,
        format!(
            r#"#!/bin/sh
IFS= read -r line
printf 'run\n' >> "{count}"
case "$line" in
  *quota-trigger*)
    printf '%s\n' '{{"type":"error","error":{{"type":"usage_limit","message":"You have hit your weekly usage limit","reset_at":"2026-11-27T03:00:00Z"}}}}'
    exit 0
    ;;
  *"verify command"*)
    printf 'fixed\n' > ok.txt
    printf '%s\n' '{{"type":"system","session_id":"sess-fix"}}'
    printf '%s\n' '{{"type":"result","subtype":"success","result":"fixed ok.txt"}}'
    exit 0
    ;;
  *)
    printf 'v1\n' > a.txt
    printf '%s\n' '{{"type":"system","session_id":"sess-1"}}'
    printf '%s\n' '{{"type":"result","subtype":"success","result":"edited a.txt"}}'
    exit 0
    ;;
esac
"#,
            count = count_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut cfg = ServerConfig {
        data_dir: tmp.0.join("server"),
        enabled_providers: vec![ProviderId::Claude, ProviderId::Mock],
        verify_max_auto_fixes: 2,
        fallback_models: vec![t3_core::ModelSelection {
            provider: ProviderId::Mock,
            model: "mock".into(),
            effort: None,
            agent: None,
        }],
        ..Default::default()
    };
    cfg.providers.claude_bin = fake_claude;
    let state = ServerState::new(cfg, "http://127.0.0.1:3774".into()).unwrap();
    let mut events = state.coordinator.subscribe();
    let app = router(state);

    let proj = json_req(
        &app,
        Method::POST,
        "/api/projects",
        Some(json!({"title": "Verify Proj", "workspace_root": workspace})),
    )
    .await;
    let pid = proj["id"].as_str().unwrap();
    json_req(
        &app,
        Method::PATCH,
        &format!("/api/projects/{pid}"),
        Some(json!({"verify_command": "test -f ok.txt"})),
    )
    .await;

    // 1. Verify auto-fix loop: run 1 mutates a.txt (fails verify), auto-fix run 2 creates ok.txt and succeeds!
    let thread = json_req(
        &app,
        Method::POST,
        "/api/threads",
        Some(json!({
            "project_id": pid,
            "title": "AutoFix Thread",
            "model_selection": {"provider": "claude", "model": "claude-sonnet-4-5"},
            "runtime_mode": "full-access"
        })),
    )
    .await;
    let tid: Uuid = thread["id"].as_str().unwrap().parse().unwrap();
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{tid}/messages"),
        Some(json!({"prompt": "edit a.txt"})),
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && !workspace.join("ok.txt").exists() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        workspace.join("ok.txt").exists(),
        "auto-fix loop should have created ok.txt"
    );
    // Wait until the auto-fix run settles to Completed.
    let mut detail = Value::Null;
    for _ in 0..60 {
        detail = json_req(&app, Method::GET, &format!("/api/threads/{tid}"), None).await;
        if detail["runs"].as_array().unwrap().len() >= 2
            && detail["thread"]["status"] == "completed"
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(detail["thread"]["status"], "completed");
    assert_eq!(detail["runs"].as_array().unwrap().len(), 2);

    // 2. Quota-exhausted automatic provider fallback: Claude hits weekly limit -> falls back to Mock!
    let fb_thread = json_req(
        &app,
        Method::POST,
        "/api/threads",
        Some(json!({
            "project_id": pid,
            "title": "Fallback Thread",
            "model_selection": {"provider": "claude", "model": "claude-sonnet-4-5"},
            "runtime_mode": "full-access"
        })),
    )
    .await;
    let fb_tid: Uuid = fb_thread["id"].as_str().unwrap().parse().unwrap();
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{fb_tid}/messages"),
        Some(json!({"prompt": "quota-trigger please answer"})),
    )
    .await;
    wait_for_thread(&mut events, fb_tid, ThreadStatus::Completed).await;
    let fb_detail = json_req(&app, Method::GET, &format!("/api/threads/{fb_tid}"), None).await;
    let fb_runs = fb_detail["runs"].as_array().unwrap();
    assert_eq!(fb_runs.len(), 2);
    assert_eq!(fb_runs[0]["status"], "rate-limited");
    assert_eq!(
        fb_runs[0]["error_message"],
        "usage_limit: You have hit your weekly usage limit (reset_at: 2026-11-27T03:00:00Z)"
    );
    assert_eq!(fb_runs[1]["provider"], "mock");
    assert_eq!(fb_runs[1]["status"], "completed");
}

#[tokio::test]
async fn tier5_settings_get_patch_persistence_validation_and_live_meta() {
    let tmp = TempDir::new();
    let make_cfg = || ServerConfig {
        data_dir: tmp.0.join("server"),
        enabled_providers: vec![ProviderId::Mock, ProviderId::Claude],
        default_model: t3_core::ModelSelection {
            provider: ProviderId::Mock,
            model: "mock".into(),
            effort: None,
            agent: None,
        },
        auth: AuthConfig::LocalOnly,
        ..Default::default()
    };

    {
        let state = ServerState::new(make_cfg(), "http://127.0.0.1:3774".into()).unwrap();
        let app = router(state);

        // Initial GET /api/settings returns defaults and enabled_providers.
        let initial = json_req(&app, Method::GET, "/api/settings", None).await;
        assert_eq!(initial["settings"]["default_model"]["provider"], "mock");
        assert_eq!(initial["settings"]["default_model"]["model"], "mock");
        assert_eq!(initial["settings"]["follow_up_behavior"], "steer");
        assert_eq!(initial["settings"]["verify_max_auto_fixes"], 2);
        assert_eq!(initial["settings"]["antigravity_mcp_enabled"], false);
        assert_eq!(initial["settings"]["antigravity_quota_probe"], false);
        assert_eq!(initial["enabled_providers"], json!(["mock", "claude"]));

        // Reject disabled provider atomically without mutating state.
        let bad_provider = request(
            &app,
            Request::builder()
                .method(Method::PATCH)
                .uri("/api/settings")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "default_model": {"provider": "codex", "model": "gpt-6.2-sol"},
                        "verify_max_auto_fixes": 5
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(bad_provider.status(), StatusCode::BAD_REQUEST);

        let after_bad = json_req(&app, Method::GET, "/api/settings", None).await;
        assert_eq!(after_bad["settings"]["default_model"]["provider"], "mock");
        assert_eq!(after_bad["settings"]["verify_max_auto_fixes"], 2);

        // Valid PATCH updates settings live.
        let patched = json_req(
            &app,
            Method::PATCH,
            "/api/settings",
            Some(json!({
                "default_model": {"provider": "claude", "model": "claude-sonnet-5-5", "effort": "medium"},
                "follow_up_behavior": "queue",
                "auto_resume_limited_threads": false,
                "rate_limit_max_retries": 2,
                "fallback_models": [{"provider": "mock", "model": "mock-fallback"}],
                "verify_max_auto_fixes": 4,
                "antigravity_mcp_enabled": true,
                "antigravity_quota_probe": true
            })),
        )
        .await;
        assert_eq!(patched["settings"]["default_model"]["provider"], "claude");
        assert_eq!(
            patched["settings"]["default_model"]["model"],
            "claude-sonnet-5-5"
        );
        assert_eq!(patched["settings"]["follow_up_behavior"], "queue");
        assert_eq!(patched["settings"]["auto_resume_limited_threads"], false);
        assert_eq!(patched["settings"]["rate_limit_max_retries"], 2);
        assert_eq!(patched["settings"]["verify_max_auto_fixes"], 4);
        assert_eq!(patched["settings"]["antigravity_mcp_enabled"], true);
        assert_eq!(patched["settings"]["antigravity_quota_probe"], true);
        assert_eq!(patched["enabled_providers"], json!(["mock", "claude"]));

        // GET /api/meta reflects live follow_up_behavior and default_model immediately.
        let meta = json_req(&app, Method::GET, "/api/meta", None).await;
        assert_eq!(meta["follow_up_behavior"], "queue");
        assert_eq!(meta["default_model"]["provider"], "claude");
        assert_eq!(meta["default_model"]["model"], "claude-sonnet-5-5");
        assert_eq!(meta["default_model"]["effort"], "medium");

        // Creating a new thread without explicit model_selection uses the live default_model.
        let proj = json_req(
            &app,
            Method::POST,
            "/api/projects",
            Some(json!({"title": "Live Defaults", "workspace_root": tmp.0})),
        )
        .await;
        let thread = json_req(
            &app,
            Method::POST,
            "/api/threads",
            Some(json!({"project_id": proj["id"], "title": "Default Model Thread"})),
        )
        .await;
        assert_eq!(thread["model_selection"]["provider"], "claude");
        assert_eq!(thread["model_selection"]["model"], "claude-sonnet-5-5");
    }

    // Reopen ServerState from the same data_dir and confirm persistence across restart.
    {
        let reopened = ServerState::new(make_cfg(), "http://127.0.0.1:3774".into()).unwrap();
        let app = router(reopened);

        let persisted = json_req(&app, Method::GET, "/api/settings", None).await;
        assert_eq!(persisted["settings"]["default_model"]["provider"], "claude");
        assert_eq!(
            persisted["settings"]["default_model"]["model"],
            "claude-sonnet-5-5"
        );
        assert_eq!(persisted["settings"]["follow_up_behavior"], "queue");
        assert_eq!(persisted["settings"]["auto_resume_limited_threads"], false);
        assert_eq!(persisted["settings"]["rate_limit_max_retries"], 2);
        assert_eq!(persisted["settings"]["verify_max_auto_fixes"], 4);
        assert_eq!(persisted["settings"]["antigravity_mcp_enabled"], true);
        assert_eq!(persisted["settings"]["antigravity_quota_probe"], true);

        let meta = json_req(&app, Method::GET, "/api/meta", None).await;
        assert_eq!(meta["follow_up_behavior"], "queue");
        assert_eq!(meta["default_model"]["model"], "claude-sonnet-5-5");
    }
}

#[tokio::test]
async fn tier5_thread_export_markdown_headers_and_credential_redaction() {
    let tmp = TempDir::new();
    let state = tmp.state(AuthConfig::LocalOnly);
    let store = state.store.clone();
    let app = router(state);

    let thread_id = create_parent(&app, &tmp.0).await;
    let run_id = Uuid::new_v4();
    let now = chrono::Utc::now();
    let sk_secret = "sk-fixture_0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let ghp_secret = "ghp_1234567890abcdef1234";
    let bearer_secret = "Bearer abcdefgh012345._~+/-";

    store
        .create_run(Run {
            id: run_id,
            thread_id,
            ordinal: 1,
            provider: ProviderId::Mock,
            model: "mock".into(),
            effort: None,
            status: RunStatus::Completed,
            prompt: "Export test".into(),
            provider_session_id: None,
            input_tokens: Some(120),
            output_tokens: Some(45),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            duration_ms: Some(300),
            error_message: Some(format!("transient auth warning {ghp_secret}")),
            started_at: now,
            completed_at: Some(now),
        })
        .await
        .unwrap();

    store
        .append_turn_item(NewTurnItem {
            thread_id,
            run_id,
            kind: TurnItemKind::UserMessage,
            summary: Some("user prompt".into()),
            payload: json!({"text": format!("Please rotate {sk_secret}")}),
            searchable_text: None,
        })
        .await
        .unwrap();
    store
        .append_turn_item(NewTurnItem {
            thread_id,
            run_id,
            kind: TurnItemKind::AssistantMessage,
            summary: Some("assistant reply".into()),
            payload: json!({"text": format!("Checked header {bearer_secret} and completed.")}),
            searchable_text: None,
        })
        .await
        .unwrap();

    let response = request(
        &app,
        Request::builder()
            .method(Method::GET)
            .uri(format!("/api/threads/{thread_id}/export"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/markdown; charset=utf-8")
    );
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok()),
        Some(format!("attachment; filename=\"thread-{thread_id}.md\"").as_str())
    );

    let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let md = String::from_utf8(body.to_vec()).unwrap();
    assert!(md.starts_with("# Parent\n"));
    assert!(md.contains("- **Runs**: 1"));
    assert!(md.contains("- **Total Tokens**: 165 (120 input / 45 output)"));
    assert!(md.contains("Please rotate [REDACTED]"));
    assert!(md.contains("Checked header [REDACTED] and completed."));
    assert!(md.contains("transient auth warning [REDACTED]"));
    assert!(!md.contains(sk_secret));
    assert!(!md.contains(ghp_secret));
    assert!(!md.contains(bearer_secret));

    // Missing thread returns 404.
    let missing = request(
        &app,
        Request::builder()
            .method(Method::GET)
            .uri(format!("/api/threads/{}/export", Uuid::new_v4()))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn verify_auto_fix_escalates_on_same_result_twice_and_tracks_dead_ends() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new();
    let workspace = tmp.0.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&workspace)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(workspace.join("a.txt"), "init\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=T3",
        "-c",
        "user.email=t3@example.test",
        "commit",
        "-qm",
        "init",
    ]);

    // Fake Claude CLI mutates `a.txt` differently on every run (so post_tree changes each time!),
    // while the verify command emits the exact same error message (with a different elapsed duration).
    // Even though verify_max_auto_fixes=4, the loop MUST escalate after run 2 (same-result-twice).
    let fake_claude = tmp.0.join("fake-claude-same-err");
    let count_file = tmp.0.join("runs.txt");
    let prompts_file = tmp.0.join("prompts.log");
    std::fs::write(
        &fake_claude,
        format!(
            r#"#!/bin/sh
IFS= read -r line
printf 'run\n' >> "{count}"
printf '%s\n' "$line" >> "{prompts}"
n=$(wc -l < "{count}" | tr -d ' ')
printf 'attempt-%s\n' "$n" > a.txt
printf '%s\n' '{{"type":"system","session_id":"sess-repeat"}}'
printf '%s\n' '{{"type":"result","subtype":"success","result":"tried fix"}}'
exit 0
"#,
            count = count_file.display(),
            prompts = prompts_file.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut cfg = ServerConfig {
        data_dir: tmp.0.join("server"),
        enabled_providers: vec![ProviderId::Claude],
        verify_max_auto_fixes: 4,
        ..Default::default()
    };
    cfg.providers.claude_bin = fake_claude;
    let state = ServerState::new(cfg, "http://127.0.0.1:3774".into()).unwrap();
    let app = router(state);

    let proj = json_req(
        &app,
        Method::POST,
        "/api/projects",
        Some(json!({"title": "SameResult Proj", "workspace_root": workspace})),
    )
    .await;
    let pid = proj["id"].as_str().unwrap();
    // Emit identical error message with varying elapsed duration (`0.1s` vs `0.2s` etc.)
    let verify_cmd = format!(
        "n=$(wc -l < \"{}\" | tr -d ' '); echo \"error[E0308]: mismatched types in 0.${{n}}s\" >&2; exit 1",
        count_file.display()
    );
    json_req(
        &app,
        Method::PATCH,
        &format!("/api/projects/{pid}"),
        Some(json!({"verify_command": verify_cmd})),
    )
    .await;

    let thread = json_req(
        &app,
        Method::POST,
        "/api/threads",
        Some(json!({
            "project_id": pid,
            "title": "SameResult Thread",
            "model_selection": {"provider": "claude", "model": "claude-sonnet-4-5"},
            "runtime_mode": "full-access"
        })),
    )
    .await;
    let tid: Uuid = thread["id"].as_str().unwrap().parse().unwrap();
    json_req(
        &app,
        Method::POST,
        &format!("/api/threads/{tid}/messages"),
        Some(json!({"prompt": "edit a.txt"})),
    )
    .await;

    let mut detail = Value::Null;
    for _ in 0..80 {
        detail = json_req(&app, Method::GET, &format!("/api/threads/{tid}"), None).await;
        if detail["thread"]["status"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(detail["thread"]["status"], "failed");
    // Run 1 fails verify -> schedules auto-fix attempt 1 (Run 2).
    // Run 2 mutates a.txt differently but hits the same normalized verify error -> escalates immediately!
    // Total runs must be 2 (NOT 5).
    let runs = detail["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "expected same-result-twice escalation at run 2, got {} runs", runs.len());
    assert!(
        runs[1]["error_message"]
            .as_str()
            .unwrap_or("")
            .contains("repeated the same failure"),
        "expected escalation error message, got: {:?}",
        runs[1]["error_message"]
    );
    let items = detail["items"].as_array().unwrap();
    assert!(
        items.iter().any(|it| {
            it["kind"] == "system_notice" && it["payload"]["reason"] == "same_result_twice"
        }),
        "expected same_result_twice system_notice"
    );
    let prompts = std::fs::read_to_string(&prompts_file).unwrap();
    assert!(prompts.contains("EXPECTED  exit_code=0, timed_out=false"));
    assert!(prompts.contains("SAW       exit_code=Some(1), timed_out=false"));
}
