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
const TIMEOUT_ENV: &str = "T3_MCP_TIMEOUT_SECS";

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
    url.set_path(&format!("{prefix}/{path}"));
    Ok(url)
}

fn internal(error: impl std::fmt::Display) -> CoreError {
    CoreError::Internal(error.to_string())
}
