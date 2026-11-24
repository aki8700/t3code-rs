use crate::CliResult;
use reqwest::{Client, Url};
use serde_json::{Value, json};
use std::time::Duration;
use t3_core::{CoreError, CoreResult};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    task::JoinSet,
};
use uuid::Uuid;

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
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut requests = JoinSet::<Option<Vec<u8>>>::new();
    let mut eof = false;
    while !eof || !requests.is_empty() {
        tokio::select! {
            biased;
            result = requests.join_next(), if !requests.is_empty() => {
                if let Some(bytes) = result.expect("an in-flight request").map_err(internal)? {
                    stdout.write_all(&bytes).await.map_err(internal)?;
                    stdout.write_all(b"\n").await.map_err(internal)?;
                    stdout.flush().await.map_err(internal)?;
                }
            }
            line = lines.next_line(), if !eof && requests.len() < 64 => {
                match line.map_err(internal)? {
                    Some(line) if !line.trim().is_empty() => {
                        let mut req = client.post(url.clone())
                            .bearer_auth(token)
                            .header("content-type", "application/json")
                            .header("accept", "application/json, text/event-stream")
                            .body(line.clone());
                        if let Some(thread_id) = thread_id {
                            req = req.header("X-T3-Thread-Id", thread_id.to_string());
                        }
                        requests.spawn(forward(req, line));
                    }
                    Some(_) => {}
                    None => eof = true,
                }
            }
        }
    }
    Ok(())
}

async fn forward(req: reqwest::RequestBuilder, request: String) -> Option<Vec<u8>> {
    let response = req.send().await;
    let error = match response {
        Ok(response) if response.status() == reqwest::StatusCode::ACCEPTED => return None,
        Ok(response) if response.status().is_success() => match response.json::<Value>().await {
            Ok(value) => return serde_json::to_vec(&value).ok(),
            Err(_) => "invalid MCP response from server".to_owned(),
        },
        Ok(response) => format!("MCP HTTP request failed ({})", response.status()),
        Err(_) => "could not reach the MCP server".to_owned(),
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
        .connect_timeout(Duration::from_secs(5))
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
