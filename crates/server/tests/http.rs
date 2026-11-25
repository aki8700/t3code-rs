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
                rate_limit_initial_backoff_secs: 0,
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
    tokio::time::timeout(Duration::from_secs(5), async {
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
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
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
    wait_for_thread(
        &mut events,
        fork_id.parse().unwrap(),
        ThreadStatus::Completed,
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
    assert_eq!(models[0]["total_runs"], 3);
    assert_eq!(models[0]["completed_runs"], 3);
    assert_eq!(models[0]["total_output_tokens"], 3);

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
    for _ in 0..3 {
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
    }
    let app = router(tmp.state(AuthConfig::LocalOnly));
    for (i, id) in threads.into_iter().enumerate() {
        let response = json_req(&app, Method::GET, &format!("/api/threads/{id}"), None).await;
        let (thread_status, run_status) = if i < 2 {
            ("interrupted", "cancelled")
        } else {
            ("completed", "completed")
        };
        assert_eq!(response["thread"]["status"], thread_status);
        assert_eq!(response["runs"][0]["status"], run_status);
        assert!(response["runs"][0]["completed_at"].is_string());
    }
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
    assert_eq!(before["coordinator_thread_id"], json!(id));
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
    wait_for_thread(&mut events, id, ThreadStatus::Completed).await;
    let messages = mcp_tool(
        &app,
        "swarm_read_board",
        json!({
            "thread_id":worker_id, "since":note["head_seq"], "limit":1,
        }),
    )
    .await;
    assert_eq!(messages.as_array().unwrap().len(), 1);
    assert_eq!(messages[0]["sender"], "reviewer");
    assert_eq!(messages[0]["mentions"], json!(["coordinator"]));
    let detail = json_req(&app, Method::GET, &thread, None).await;
    assert_eq!(detail["runs"].as_array().unwrap().len(), 2);
    let prompt = detail["runs"][1]["prompt"].as_str().unwrap();
    assert!(prompt.contains("Fixture context from the user"));
    assert!(prompt.contains("@coordinator [done] Fixture checked"));
    let board = json_req(&app, Method::GET, &format!("{worker_path}/swarm"), None).await;
    assert_eq!(board["board_id"], before["board_id"]);
    assert_eq!(board["members"][0]["last_read_seq"], done["head_seq"]);
    assert_eq!(board["members"][1]["state"], "waiting");
    let killed = json_req(
        &app,
        Method::POST,
        &format!("{swarm}/workers/reviewer/kill"),
        None,
    )
    .await;
    assert_eq!(killed["members"][1]["state"], "killed");
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
