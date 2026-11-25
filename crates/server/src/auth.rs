use axum::{
    extract::{ConnectInfo, Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use http::{HeaderMap, Method, StatusCode, Uri, header, uri::Authority};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk::JwkSet};
use reqwest::Url;
use serde::Deserialize;
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use t3_core::{
    AuthConfig, AuthIdentity, CoreError, CoreResult, InteractionMode, RuntimeMode, ServerConfig,
    parse_jwks_override,
};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use uuid::Uuid;

const MCP_TOKEN_PREFIX: &str = "t3mcp_";

#[derive(Clone, Copy)]
pub(crate) struct ActiveMcpToken {
    pub(crate) thread_id: Uuid,
    pub(crate) run_id: Uuid,
    pub(crate) runtime_mode: RuntimeMode,
    pub(crate) interaction_mode: InteractionMode,
}

#[derive(Clone, Default)]
pub(crate) struct McpTokens(Arc<std::sync::RwLock<HashMap<String, ActiveMcpToken>>>);

impl McpTokens {
    pub(crate) fn register(&self, run: ActiveMcpToken) -> CoreResult<RunMcpToken> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| CoreError::Internal(error.to_string()))?;
        let token = format!(
            "{MCP_TOKEN_PREFIX}{}",
            blake3::Hash::from_bytes(bytes).to_hex()
        );
        self.0
            .write()
            .expect("MCP token lock")
            .insert(token.clone(), run);
        Ok(RunMcpToken {
            registry: self.clone(),
            token,
        })
    }

    pub(crate) fn revoke_run(&self, id: Uuid) {
        self.0
            .write()
            .expect("MCP token lock")
            .retain(|_, run| run.run_id != id);
    }
}

pub(crate) struct RunMcpToken {
    registry: McpTokens,
    pub(crate) token: String,
}

impl Drop for RunMcpToken {
    fn drop(&mut self) {
        self.registry
            .0
            .write()
            .expect("MCP token lock")
            .remove(&self.token);
    }
}

#[derive(Clone)]
pub struct AuthLayer {
    mode: AuthConfig,
    cloudflare: Option<Arc<Cloudflare>>,
    tokens: McpTokens,
}

impl AuthLayer {
    pub(crate) fn new(config: &ServerConfig, tokens: McpTokens) -> CoreResult<Self> {
        config
            .validate()
            .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
        let cloudflare = if let AuthConfig::CloudflareAccess {
            team_domain,
            audience,
            allowed_emails,
            jwks_url_override,
        } = &config.auth
        {
            let (url, issuer) = cloudflare_endpoint(team_domain, jwks_url_override.as_deref())?;
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| CoreError::Internal("cannot initialize JWKS client".into()))?;
            Some(Arc::new(Cloudflare {
                url,
                issuer,
                audience: audience.clone(),
                allowed_emails: allowed_emails.clone(),
                client,
                cache: RwLock::new(JwksCache::default()),
                fetch_lock: Mutex::new(()),
            }))
        } else {
            None
        };
        Ok(Self {
            mode: config.auth.clone(),
            cloudflare,
            tokens,
        })
    }

    pub(crate) fn active_run(&self, headers: &HeaderMap) -> Option<ActiveMcpToken> {
        let thread_id = header_text(headers, "x-t3-thread-id")?
            .parse::<Uuid>()
            .ok()?;
        self.tokens
            .0
            .read()
            .ok()?
            .get(bearer(headers)?)
            .copied()
            .filter(|run| run.thread_id == thread_id)
    }

    async fn identity(
        &self,
        headers: &HeaderMap,
        method: &Method,
        uri: &Uri,
        peer: Option<std::net::IpAddr>,
    ) -> Option<AuthIdentity> {
        let loopback = peer.is_some_and(|ip| ip.is_loopback());
        let authority = request_authority(headers, uri);
        if headers.contains_key(header::HOST) && authority.is_none() {
            return None;
        }
        if !same_origin(headers, authority.as_ref()) {
            return None;
        }
        if let Some(token) = bearer(headers)
            && token.starts_with(MCP_TOKEN_PREFIX)
            && !matches!(&self.mode, AuthConfig::Bearer { token: public }
                if matches_token(Some(token), public))
        {
            if !loopback || method != Method::POST || uri.path() != "/mcp" {
                return None;
            }
            let run = self.active_run(headers)?;
            return Some(identity(&run.thread_id.to_string(), "internal-mcp", None));
        }

        match &self.mode {
            AuthConfig::LocalOnly
                if (loopback || peer.is_none()) && authority.as_ref().is_none_or(local_host) =>
            {
                Some(identity("local", "local-only", None))
            }
            AuthConfig::Bearer { token } => {
                if matches_token(bearer(headers), token)
                    || (uri.path() == "/ws"
                        && uri.query().is_some_and(|query| {
                            // Use the same percent-decoding rules as the browser.
                            let mut parsed = Url::parse("http://localhost").expect("literal URL");
                            parsed.set_query(Some(query));
                            parsed.query_pairs().any(|(name, value)| {
                                name == "token" && matches_token(Some(&value), token)
                            })
                        }))
                {
                    Some(identity("bearer", "bearer", None))
                } else {
                    None
                }
            }
            AuthConfig::Tailscale { allowed_logins }
                if loopback && authority.as_ref().is_some_and(tailscale_host) =>
            {
                let login = header_text(headers, "tailscale-user-login")?;
                if login.trim().is_empty() || !allowed_logins.iter().any(|allowed| allowed == login)
                {
                    return None;
                }
                Some(identity(login, "tailscale", Some(login.to_owned())))
            }
            AuthConfig::CloudflareAccess { .. } => {
                let token = header_text(headers, "cf-access-jwt-assertion")
                    .or_else(|| cookie_token(headers))?;
                self.cloudflare.as_ref()?.verify(token).await
            }
            _ => None,
        }
    }
}

pub(crate) async fn authenticate(
    State(auth): State<AuthLayer>,
    mut req: Request,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip());
    let Some(identity) = auth
        .identity(req.headers(), req.method(), req.uri(), peer)
        .await
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    req.extensions_mut().insert(identity);
    next.run(req).await
}

fn identity(subject: &str, method: &str, email: Option<String>) -> AuthIdentity {
    AuthIdentity {
        subject: subject.to_owned(),
        method: method.to_owned(),
        email,
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, value) = header_text(headers, "authorization")?.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(value)
}

fn matches_token(value: Option<&str>, expected: &str) -> bool {
    value.is_some_and(|value| {
        let actual = blake3::hash(value.as_bytes());
        let expected = blake3::hash(expected.as_bytes());
        bool::from(actual.as_bytes().ct_eq(expected.as_bytes()))
    })
}

fn same_origin(headers: &HeaderMap, authority: Option<&Authority>) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Some(origin), Some(host)) = (origin.to_str().ok(), authority.map(Authority::as_str))
    else {
        return false;
    };
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    matches!(scheme, "http" | "https") && authority.eq_ignore_ascii_case(host)
}

pub(crate) fn request_authority(headers: &HeaderMap, uri: &Uri) -> Option<Authority> {
    if headers.get_all(header::HOST).iter().count() > 1 {
        return None;
    }
    let authority = match headers.get(header::HOST) {
        Some(host) => Authority::try_from(host.as_bytes()).ok()?,
        None => uri.authority()?.clone(),
    };
    (!authority.as_str().contains('@')).then_some(authority)
}

fn local_host(host: &Authority) -> bool {
    host.host().eq_ignore_ascii_case("localhost")
        || host
            .host()
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn tailscale_host(authority: &Authority) -> bool {
    let host = authority.host();
    if authority.as_str().len() != host.len() && authority.port_u16().is_none() {
        return false;
    }
    let host = host.to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]")
        || (host.ends_with(".ts.net")
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }))
}

fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|value| value.trim().split_once('='))
        .find_map(|(name, value)| (name == "CF_Authorization").then_some(value))
}

pub(crate) fn cloudflare_endpoint(
    domain: &str,
    override_url: Option<&str>,
) -> CoreResult<(Url, Option<String>)> {
    let invalid =
        || CoreError::InvalidArgument("invalid Cloudflare Access JWKS configuration".into());
    if let Some(url) = override_url {
        return parse_jwks_override(url)
            .map(|parsed| (parsed, None))
            .map_err(|_| invalid());
    }
    let domain = domain
        .strip_prefix("https://")
        .unwrap_or(domain)
        .trim_end_matches('/');
    if domain.is_empty()
        || !domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
    {
        return Err(invalid());
    }
    let issuer = format!("https://{domain}");
    let url = Url::parse(&format!("{issuer}/cdn-cgi/access/certs")).map_err(|_| invalid())?;
    Ok((url, Some(issuer)))
}

const JWKS_TTL: Duration = Duration::from_secs(15 * 60);
const JWKS_REFRESH_COOLDOWN: Duration = Duration::from_secs(60);
const MAX_JWKS_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct JwksCache {
    keys: Option<JwkSet>,
    fetched_at: Option<Instant>,
    last_attempt: Option<Instant>,
}

impl JwksCache {
    fn fresh_key(&self, kid: &str) -> Option<DecodingKey> {
        (self.fetched_at?.elapsed() < JWKS_TTL)
            .then(|| cached_key(self, kid))
            .flatten()
    }
}

struct Cloudflare {
    url: Url,
    issuer: Option<String>,
    audience: String,
    allowed_emails: Vec<String>,
    client: reqwest::Client,
    cache: RwLock<JwksCache>,
    fetch_lock: Mutex<()>,
}

#[derive(Deserialize)]
struct Claims {
    #[serde(default)]
    sub: String,
    email: Option<String>,
}

impl Cloudflare {
    async fn verify(&self, token: &str) -> Option<AuthIdentity> {
        let header = jsonwebtoken::decode_header(token).ok()?;
        if header.alg != Algorithm::RS256 {
            return None;
        }
        let key = self.key(header.kid.as_deref()?).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 60;
        validation.validate_nbf = true;
        validation.set_audience(&[&self.audience]);
        validation.required_spec_claims.insert("aud".into());
        if let Some(issuer) = &self.issuer {
            validation.set_issuer(&[issuer]);
            validation.required_spec_claims.insert("iss".into());
        }
        let claims = jsonwebtoken::decode::<Claims>(token, &key, &validation)
            .ok()?
            .claims;
        if !self.allowed_emails.is_empty()
            && !claims.email.as_ref().is_some_and(|email| {
                self.allowed_emails
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(email))
            })
        {
            return None;
        }
        let subject = if claims.sub.is_empty() {
            claims.email.as_deref().unwrap_or("cloudflare-access")
        } else {
            &claims.sub
        };
        Some(identity(subject, "cloudflare-access", claims.email.clone()))
    }

    async fn key(&self, kid: &str) -> Option<DecodingKey> {
        {
            let cache = self.cache.read().await;
            if let Some(key) = cache.fresh_key(kid) {
                return Some(key);
            }
        }

        // Requests that miss the cache can fail immediately while a refresh is in flight.
        let _guard = self.fetch_lock.try_lock().ok()?;
        {
            let mut cache = self.cache.write().await;
            if let Some(key) = cache.fresh_key(kid) {
                return Some(key);
            }
            if cache
                .last_attempt
                .is_some_and(|attempt| attempt.elapsed() < JWKS_REFRESH_COOLDOWN)
            {
                return None;
            }
            cache.last_attempt = Some(Instant::now());
        }
        let keys = self.fetch_keys().await;
        let mut cache = self.cache.write().await;
        let now = Instant::now();
        cache.last_attempt = Some(now);
        if let Some(keys) = keys {
            cache.keys = Some(keys);
            cache.fetched_at = Some(now);
        }
        cache.fresh_key(kid)
    }

    async fn fetch_keys(&self) -> Option<JwkSet> {
        let mut response = self.client.get(self.url.clone()).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if chunk.len() > MAX_JWKS_BYTES - body.len() {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).ok()
    }
}

fn cached_key(cache: &JwksCache, kid: &str) -> Option<DecodingKey> {
    use jsonwebtoken::jwk::{AlgorithmParameters, KeyAlgorithm};
    let jwk = cache.keys.as_ref()?.find(kid)?;
    if !matches!(jwk.algorithm, AlgorithmParameters::RSA(_))
        || jwk
            .common
            .key_algorithm
            .is_some_and(|alg| alg != KeyAlgorithm::RS256)
    {
        return None;
    }
    DecodingKey::from_jwk(jwk).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use serde_json::{Value, json};
    use t3_core::{
        DelegationSpec, InteractionMode, IsolationMode, ModelSelection, ProviderId, RuntimeMode,
        Store,
    };
    use t3_mcp::{DelegateMode, OrchestratorBackend};
    use tower::ServiceExt;

    fn fixture_token(layer: &AuthLayer, thread_id: Uuid) -> RunMcpToken {
        layer
            .tokens
            .register(ActiveMcpToken {
                thread_id,
                run_id: Uuid::new_v4(),
                runtime_mode: RuntimeMode::ApprovalRequired,
                interaction_mode: InteractionMode::Plan,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn internal_token_only_opens_mcp_for_loopback_peers() {
        let config = ServerConfig {
            auth: AuthConfig::CloudflareAccess {
                team_domain: "fixture.cloudflareaccess.com".into(),
                audience: "fixture".into(),
                allowed_emails: vec![],
                jwks_url_override: None,
            },
            ..Default::default()
        };
        let layer = AuthLayer::new(&config, McpTokens::default()).unwrap();
        let id = Uuid::from_u128(1);
        let token = fixture_token(&layer, id);
        let bearer = &token.token;
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {bearer}").parse().unwrap());
        headers.insert("x-t3-thread-id", id.to_string().parse().unwrap());
        let loopback = Some("127.0.0.1".parse().unwrap());
        let external = Some("192.0.2.1".parse().unwrap());
        let mcp = Uri::from_static("/mcp");
        assert_eq!(
            layer
                .identity(&headers, &Method::POST, &mcp, loopback)
                .await
                .unwrap()
                .method,
            "internal-mcp"
        );
        for (method, path, peer) in [
            (Method::POST, mcp.clone(), external),
            (Method::POST, mcp.clone(), None),
            (Method::GET, mcp, loopback),
            (Method::GET, Uri::from_static("/api/meta"), loopback),
            (
                Method::POST,
                Uri::from_static("/mcp?token=test-internal-token"),
                external,
            ),
        ] {
            assert!(
                layer
                    .identity(&headers, &method, &path, peer)
                    .await
                    .is_none()
            );
        }
    }

    #[tokio::test]
    async fn bearer_query_token_is_accepted_only_for_websocket() {
        let config = ServerConfig {
            auth: AuthConfig::Bearer {
                token: "test-bearer-token +!%".into(),
            },
            ..Default::default()
        };
        let layer = AuthLayer::new(&config, McpTokens::default()).unwrap();
        for (path, authorized) in [
            ("/ws?token=test-bearer-token%20%2B!%25", true),
            ("/ws?token=invalid", false),
            ("/api/meta?token=test-bearer-token%20%2B!%25", false),
            ("/mcp?token=test-bearer-token%20%2B!%25", false),
        ] {
            assert_eq!(
                layer
                    .identity(
                        &HeaderMap::new(),
                        &Method::GET,
                        &path.parse().unwrap(),
                        None
                    )
                    .await
                    .is_some(),
                authorized,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn authenticated_mcp_scope_limits_projects_and_child_permissions() {
        struct TempState(std::path::PathBuf);
        impl Drop for TempState {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let temp = TempState(std::env::temp_dir().join(format!("t3-mcp-scope-{}", Uuid::new_v4())));
        let config = ServerConfig {
            data_dir: temp.0.clone(),
            enabled_providers: vec![ProviderId::Mock],
            default_model: ModelSelection {
                provider: ProviderId::Mock,
                model: "mock".into(),
                effort: None,
                agent: None,
            },
            ..Default::default()
        };
        let state = crate::ServerState::new(config, "http://localhost".into()).unwrap();
        let project_a = state
            .store
            .create_project("A".into(), temp.0.clone())
            .await
            .unwrap();
        let project_b = state
            .store
            .create_project("B".into(), temp.0.clone())
            .await
            .unwrap();
        let create = |project_id, runtime_mode, interaction_mode| {
            state
                .coordinator
                .create_thread(crate::threads::CreateThread {
                    project_id,
                    title: "fixture".into(),
                    model_selection: None,
                    runtime_mode,
                    interaction_mode,
                    isolation: IsolationMode::Shared,
                    base_ref: None,
                })
        };
        let caller = create(
            project_a.id,
            RuntimeMode::ApprovalRequired,
            InteractionMode::Plan,
        )
        .await
        .unwrap();
        let same_project = create(
            project_a.id,
            RuntimeMode::FullAccess,
            InteractionMode::Default,
        )
        .await
        .unwrap();
        let other = create(
            project_b.id,
            RuntimeMode::ApprovalRequired,
            InteractionMode::Plan,
        )
        .await
        .unwrap();
        let foreign_task = state
            .coordinator
            .delegate(
                DelegationSpec {
                    parent_thread_id: other.id,
                    title: "Foreign child".into(),
                    task: "fixture".into(),
                    model_selection: other.model_selection,
                    isolation: IsolationMode::Shared,
                    base_ref: None,
                    runtime_mode: RuntimeMode::ApprovalRequired,
                    interaction_mode: InteractionMode::Plan,
                },
                DelegateMode::Wait,
            )
            .await
            .unwrap()
            .task_id;
        let token = fixture_token(&state.auth, caller.id);
        let bearer = &token.token;
        let app = crate::router(state);
        let call = |name, arguments: Value, header_id: Option<Uuid>| {
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/mcp")
                .extension(ConnectInfo(
                    "127.0.0.1:49300".parse::<SocketAddr>().unwrap(),
                ))
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
            if let Some(id) = header_id {
                request = request.header("x-t3-thread-id", id.to_string());
            }
            let req = request
                .body(Body::from(
                    json!({"jsonrpc":"2.0","id":1,
                "method":"tools/call","params":{"name":name,"arguments":arguments}})
                    .to_string(),
                ))
                .unwrap();
            app.clone().oneshot(req)
        };
        for id in [None, Some(other.id)] {
            assert_eq!(
                call("thread_list", json!({}), id).await.unwrap().status(),
                StatusCode::UNAUTHORIZED
            );
        }
        let call = &call;
        let tool = |name, arguments| async move {
            let response = call(name, arguments, Some(caller.id)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap())
                    .unwrap();
            body["result"].clone()
        };
        let delegate = |parent, runtime, interaction| {
            json!({
                "parent_thread_id": parent,"title":"Child","task":"fixture","provider":"mock","model":"mock",
                "runtime_mode":runtime,"interaction_mode":interaction,"mode":"wait"
            })
        };
        let list = tool("thread_list", json!({})).await;
        let entries: Vec<t3_core::Thread> =
            serde_json::from_str(list["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| entry.project_id == caller.project_id)
        );
        assert_eq!(tool("thread_read", json!({})).await["isError"], Value::Null);
        for (tool_name, args) in [
            ("thread_list", json!({"project_id": project_b.id})),
            ("thread_read", json!({"thread_id": other.id})),
            (
                "thread_send",
                json!({"thread_id": other.id, "prompt":"fixture"}),
            ),
            ("thread_fork", json!({"thread_id": other.id})),
            (
                "delegate_task",
                delegate(other.id, "approval-required", "plan"),
            ),
            ("task_status", json!({"task_id": foreign_task})),
            ("task_cancel", json!({"task_id": foreign_task})),
            ("delegate_task", delegate(caller.id, "full-access", "plan")),
            (
                "delegate_task",
                delegate(caller.id, "approval-required", "default"),
            ),
            (
                "delegate_task",
                delegate(same_project.id, "full-access", "default"),
            ),
            (
                "thread_send",
                json!({"thread_id": same_project.id, "prompt":"fixture"}),
            ),
            ("thread_fork", json!({"thread_id": same_project.id})),
        ] {
            let result = tool(tool_name, args).await;
            assert_eq!(result["isError"], true, "{tool_name}: {result}");
        }
        let allowed = tool(
            "delegate_task",
            delegate(caller.id, "approval-required", "plan"),
        )
        .await;
        assert_eq!(allowed["isError"], Value::Null, "{allowed}");
    }
}
