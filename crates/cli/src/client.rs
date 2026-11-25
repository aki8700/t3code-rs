use crate::CliResult;
use reqwest::{Client, Url};
use serde_json::{Value, json};
use std::{
    env,
    io::{self, BufRead},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use t3_core::{CoreError, CoreResult};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, mpsc},
    task::JoinSet,
};
use uuid::Uuid;

const MAX_IN_FLIGHT: usize = 16;
const MAX_STDIN_LINE: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BODY: usize = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
const API_TIMEOUT: Duration = Duration::from_secs(30);
const LIMITS_TIMEOUT: Duration = Duration::from_secs(60);
const TIMEOUT_ENV: &str = "T3_MCP_TIMEOUT_SECS";
const BEARER_TOKEN_ENV: &str = "T3_BEARER_TOKEN";

pub async fn health(server_url: &str) -> CliResult<Value> {
    let client = http_client()?;
    let url = endpoint(server_url, "healthz")?;
    Ok(client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Lists threads from `GET /api/threads`.
pub async fn list_threads(
    server_url: &str,
    token: Option<&str>,
    include_archived: bool,
    limit: usize,
) -> CliResult<Value> {
    let url = threads_url(server_url, include_archived, limit)?;
    let req = with_bearer_auth(http_client()?.get(url).timeout(API_TIMEOUT), token);
    send_json(req).await
}

/// Sends a message to a thread via `POST /api/threads/{id}/messages`.
pub async fn send_message(
    server_url: &str,
    token: Option<&str>,
    thread_id: Uuid,
    prompt: &str,
    behavior: Option<&str>,
) -> CliResult<Value> {
    let url = send_message_url(server_url, thread_id)?;
    let body = send_message_body(prompt, behavior);
    let req = with_bearer_auth(http_client()?.post(url).timeout(API_TIMEOUT), token).json(&body);
    send_json(req).await
}

/// Interrupts the active run on a thread via `POST /api/threads/{id}/interrupt`.
pub async fn interrupt_thread(
    server_url: &str,
    token: Option<&str>,
    thread_id: Uuid,
) -> CliResult<()> {
    let url = interrupt_url(server_url, thread_id)?;
    let req = with_bearer_auth(http_client()?.post(url).timeout(API_TIMEOUT), token);
    send_empty(req).await
}

/// Queries provider usage limits from `GET /api/usage/limits`.
pub async fn usage_limits(
    server_url: &str,
    token: Option<&str>,
    refresh: bool,
) -> CliResult<Value> {
    let url = usage_limits_url(server_url, refresh)?;
    let req = with_bearer_auth(http_client()?.get(url).timeout(LIMITS_TIMEOUT), token);
    send_json(req).await
}

fn with_bearer_auth(
    req: reqwest::RequestBuilder,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    match resolve_token(token, env::var(BEARER_TOKEN_ENV).ok().as_deref()) {
        Some(token) => req.bearer_auth(token),
        None => req,
    }
}

fn resolve_token(explicit: Option<&str>, env_value: Option<&str>) -> Option<String> {
    explicit
        .filter(|value| !value.is_empty())
        .or_else(|| env_value.filter(|value| !value.is_empty()))
        .map(ToOwned::to_owned)
}

fn send_message_body(prompt: &str, behavior: Option<&str>) -> Value {
    match behavior {
        Some(behavior) => json!({ "prompt": prompt, "behavior": behavior }),
        None => json!({ "prompt": prompt }),
    }
}

async fn send_json(req: reqwest::RequestBuilder) -> CliResult<Value> {
    let response = ensure_success(req.send().await?).await?;
    Ok(response.json().await?)
}

async fn send_empty(req: reqwest::RequestBuilder) -> CliResult<()> {
    ensure_success(req.send().await?).await?;
    Ok(())
}

async fn ensure_success(response: reqwest::Response) -> CliResult<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| value.get("error").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| body.trim().to_owned());
    if detail.is_empty() {
        Err(format!("HTTP {status}").into())
    } else {
        Err(format!("HTTP {status}: {detail}").into())
    }
}

fn threads_url(server_url: &str, include_archived: bool, limit: usize) -> CliResult<Url> {
    let mut url = endpoint(server_url, "api/threads")?;
    url.query_pairs_mut()
        .append_pair(
            "include_archived",
            if include_archived { "true" } else { "false" },
        )
        .append_pair("limit", &limit.to_string());
    Ok(url)
}

fn send_message_url(server_url: &str, thread_id: Uuid) -> CliResult<Url> {
    endpoint(server_url, &format!("api/threads/{thread_id}/messages"))
}

fn interrupt_url(server_url: &str, thread_id: Uuid) -> CliResult<Url> {
    endpoint(server_url, &format!("api/threads/{thread_id}/interrupt"))
}

fn usage_limits_url(server_url: &str, refresh: bool) -> CliResult<Url> {
    let mut url = endpoint(server_url, "api/usage/limits")?;
    if refresh {
        url.query_pairs_mut().append_pair("refresh", "true");
    }
    Ok(url)
}

pub async fn run_stdio_bridge(
    server_url: &str,
    token: &str,
    thread_id: Option<Uuid>,
) -> CoreResult<()> {
    let client = http_client().map_err(internal)?;
    let url = endpoint(server_url, "mcp")
        .map_err(|error| CoreError::InvalidArgument(error.to_string()))?;
    let timeout = mcp_timeout()?;
    let mut lines = stdin_lines().map_err(internal)?;
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let mut requests = JoinSet::<CoreResult<()>>::new();
    let mut eof = false;
    while !eof || !requests.is_empty() {
        tokio::select! {
            biased;
            result = requests.join_next(), if !requests.is_empty() => {
                if let Some(result) = result {
                    result.map_err(internal)??;
                }
            }
            line = lines.recv(), if !eof && requests.len() < MAX_IN_FLIGHT => {
                match line.transpose().map_err(internal)? {
                    Some(line) => {
                        let mut req = client.post(url.clone())
                            .timeout(timeout)
                            .bearer_auth(token)
                            .header("content-type", "application/json")
                            .header("accept", "application/json, text/event-stream")
                            .body(line.clone());
                        if let Some(thread_id) = thread_id {
                            req = req.header("X-T3-Thread-Id", thread_id.to_string());
                        }
                        let stdout = Arc::clone(&stdout);
                        requests.spawn(async move {
                            if let Some(mut bytes) = forward(req, line).await {
                                bytes.push(b'\n');
                                let mut stdout = stdout.lock().await;
                                stdout.write_all(&bytes).await.map_err(internal)?;
                                stdout.flush().await.map_err(internal)?;
                            }
                            Ok(())
                        });
                    }
                    None => eof = true,
                }
            }
        }
    }
    Ok(())
}

fn mcp_timeout() -> CoreResult<Duration> {
    let value = match env::var(TIMEOUT_ENV) {
        Ok(value) => value,
        Err(env::VarError::NotPresent) => return Ok(DEFAULT_TIMEOUT),
        Err(error) => {
            return Err(CoreError::InvalidArgument(format!(
                "{TIMEOUT_ENV}: {error}"
            )));
        }
    };
    value
        .parse::<u64>()
        .ok()
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs)
        .filter(|&timeout| Instant::now().checked_add(timeout).is_some())
        .ok_or_else(|| {
            CoreError::InvalidArgument(format!(
                "{TIMEOUT_ENV} must be a positive number of seconds"
            ))
        })
}

fn stdin_lines() -> io::Result<mpsc::Receiver<io::Result<String>>> {
    let (sender, receiver) = mpsc::channel(1);
    // Tokio waits for its blocking stdin reads during runtime shutdown. Use
    // a dedicated thread so dropping the JoinSet on stdout error aborts the
    // requests and the CLI can exit even while the peer keeps stdin open.
    thread::Builder::new()
        .name("mcp-stdin".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            loop {
                match read_line(&mut stdin) {
                    Ok(Some(line)) if line.trim().is_empty() => {}
                    Ok(Some(line)) => {
                        if sender.blocking_send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = sender.blocking_send(Err(error));
                        break;
                    }
                }
            }
        })?;
    Ok(receiver)
}

fn read_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        let newline = buffer.iter().position(|&byte| byte == b'\n');
        let count = newline.unwrap_or(buffer.len());
        if count > MAX_STDIN_LINE - line.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("MCP stdin line exceeds {MAX_STDIN_LINE} bytes"),
            ));
        }
        line.extend_from_slice(&buffer[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            break;
        }
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn response_bytes(req: reqwest::RequestBuilder) -> Result<Option<Vec<u8>>, String> {
    let mut response = req.send().await.map_err(|error| {
        if error.is_timeout() {
            "MCP HTTP request timed out"
        } else {
            "could not reach the MCP server"
        }
    })?;
    let status = response.status();
    if status == reqwest::StatusCode::ACCEPTED {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("MCP HTTP request failed ({status})"));
    }

    let too_large = || format!("MCP HTTP response exceeds {MAX_RESPONSE_BODY} bytes");
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        if error.is_timeout() {
            "MCP HTTP response timed out"
        } else {
            "invalid MCP response from server"
        }
    })? {
        if chunk.len() > MAX_RESPONSE_BODY - body.len() {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|_| "invalid MCP response from server")?;
    Ok(serde_json::to_vec(&value).ok())
}

async fn forward(req: reqwest::RequestBuilder, request: String) -> Option<Vec<u8>> {
    let error = match response_bytes(req).await {
        Ok(bytes) => return bytes,
        Err(error) => error,
    };
    let message = serde_json::from_str::<Value>(&request).unwrap_or(Value::Null);
    if let Value::Array(messages) = message {
        let responses: Vec<_> = messages
            .into_iter()
            .filter_map(|message| message.get("id").map(|id| transport_error(id, &error)))
            .collect();
        return (!responses.is_empty())
            .then(|| serde_json::to_vec(&responses).expect("JSON serializes"));
    }
    let id = match message.get("id") {
        Some(id) => id,
        None if message.get("method").is_some() => return None,
        None => &Value::Null,
    };
    serde_json::to_vec(&transport_error(id, &error)).ok()
}

fn transport_error(id: &Value, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": message}})
}

fn http_client() -> reqwest::Result<Client> {
    Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
}

fn endpoint(base: &str, path: &str) -> CliResult<Url> {
    let mut url = Url::parse(base)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "server URL must be an HTTP(S) URL without credentials, query, or fragment".into(),
        );
    }
    let prefix = url.path().trim_end_matches('/');
    let path = path.trim_start_matches('/');
    url.set_path(&format!("{prefix}/{path}"));
    Ok(url)
}

fn internal(error: impl std::fmt::Display) -> CoreError {
    CoreError::Internal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalizes_slashes_and_preserves_prefix() {
        assert_eq!(
            endpoint("http://127.0.0.1:3774", "api/threads")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3774/api/threads"
        );
        assert_eq!(
            endpoint("https://example.com/t3/", "/api/usage/limits")
                .unwrap()
                .as_str(),
            "https://example.com/t3/api/usage/limits"
        );
    }

    #[test]
    fn endpoint_rejects_invalid_urls() {
        assert!(endpoint("ftp://127.0.0.1:3774", "healthz").is_err());
        assert!(endpoint("http://user:pass@127.0.0.1:3774", "healthz").is_err());
        assert!(endpoint("http://127.0.0.1:3774?a=1", "healthz").is_err());
        assert!(endpoint("http://127.0.0.1:3774#frag", "healthz").is_err());
    }

    #[test]
    fn thread_and_limit_urls_format_paths_and_queries() {
        let id = Uuid::nil();
        assert_eq!(
            threads_url("http://127.0.0.1:3774", false, 30)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3774/api/threads?include_archived=false&limit=30"
        );
        assert_eq!(
            threads_url("http://127.0.0.1:3774/sub/", true, 10)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3774/sub/api/threads?include_archived=true&limit=10"
        );
        assert_eq!(
            send_message_url("http://127.0.0.1:3774", id)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3774/api/threads/00000000-0000-0000-0000-000000000000/messages"
        );
        assert_eq!(
            interrupt_url("http://127.0.0.1:3774", id)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3774/api/threads/00000000-0000-0000-0000-000000000000/interrupt"
        );
        assert_eq!(
            usage_limits_url("http://127.0.0.1:3774", false)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3774/api/usage/limits"
        );
        assert_eq!(
            usage_limits_url("http://127.0.0.1:3774", true)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:3774/api/usage/limits?refresh=true"
        );
    }

    #[test]
    fn resolve_token_prefers_explicit_and_skips_empty() {
        assert_eq!(
            resolve_token(Some("cli-tok"), Some("env-tok")).as_deref(),
            Some("cli-tok")
        );
        assert_eq!(
            resolve_token(None, Some("env-tok")).as_deref(),
            Some("env-tok")
        );
        assert_eq!(
            resolve_token(Some(""), Some("env-tok")).as_deref(),
            Some("env-tok")
        );
        assert_eq!(resolve_token(None, Some("")), None);
        assert_eq!(resolve_token(None, None), None);
    }

    #[test]
    fn send_message_body_omits_absent_behavior() {
        assert_eq!(send_message_body("hi", None), json!({ "prompt": "hi" }));
        assert_eq!(
            send_message_body("hi", Some("queue")),
            json!({ "prompt": "hi", "behavior": "queue" })
        );
    }
}
