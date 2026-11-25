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
