#![cfg(unix)]

use reqwest::Client;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio, time::Duration};
use t3_core::{ProviderBinaryConfig, ProviderId, ServerConfig, Store};
use t3_store::SqliteStore;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    time::{sleep, timeout},
};
use uuid::Uuid;

const TOKEN: &str = "fixture-bearer-token";
const BRIDGE_TIMEOUT: Duration = Duration::from_secs(5);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("t3-cli-test-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    async fn start(&self) -> (Child, String, ServerConfig) {
        let provider = self.0.join("provider");
        fs::write(
            &provider,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    printf 'fixture 1.0\n'
    exit 0
fi
[ -n "$T3_MCP_TOKEN" ] && [ -z "$T3_BEARER_TOKEN" ] || exit 3
trap '' TERM
/bin/sleep 30 &
printf '%s,%s\n' "$$" "$!" > provider.tmp
mv provider.tmp provider.ready
wait
"#,
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        let home_config = self.0.join(".config/opencode");
        fs::create_dir_all(&home_config).unwrap();
        fs::write(
            home_config.join("config.json"),
            r#"{"provider":{"fixture":{"models":{"external":{}}}}}"#,
        )
        .unwrap();
        let config = ServerConfig {
            data_dir: self.0.join("state"),
            enabled_providers: vec![ProviderId::Claude, ProviderId::OpenCode],
            providers: ProviderBinaryConfig {
                claude_bin: provider.clone(),
                opencode_bin: provider,
                ..Default::default()
            },
            ..Default::default()
        };
        let path = self.0.join("server.toml");
        fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_t3d"))
            .args(["serve", "--port", "0", "--auth", "bearer", "--config"])
            .arg(path)
            .env("T3_BEARER_TOKEN", TOKEN)
            .env("HOME", &self.0)
            .env("RUST_LOG", "t3_server=info")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut log = BufReader::new(child.stderr.take().unwrap()).lines();
        let address = timeout(Duration::from_secs(5), async {
            let mut startup = String::new();
            while let Some(line) = log.next_line().await.unwrap() {
                if let Some((_, address)) = line.split_once("address=") {
                    return address.parse::<std::net::SocketAddr>().unwrap();
                }
                startup.push_str(&line);
            }
            panic!("t3d exited during startup: {startup}");
        })
        .await
        .expect("t3d did not start");
        tokio::spawn(async move { while let Ok(Some(_)) = log.next_line().await {} });
        (child, format!("http://{address}"), config)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn gc_dry_run_leaves_the_data_directory_untouched() {
    let fixture = Fixture::new();
    let cfg = ServerConfig {
        data_dir: fixture.0.join("state ?#"),
        ..Default::default()
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_t3d"));
    command.arg("gc").arg("--data-dir").arg(&cfg.data_dir);
    let empty = command.output().await.unwrap();
    assert!(empty.status.success());
    let report: Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(report["db_bytes_after"], 0);
    assert!(!cfg.data_dir.exists());

    fs::create_dir_all(cfg.attachments_dir()).unwrap();
    let orphan = cfg
        .attachments_dir()
        .join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let unrelated = cfg.attachments_dir().join("keep-me.txt");
    fs::write(&orphan, b"unused").unwrap();
    fs::write(&unrelated, b"user note").unwrap();
    let store = SqliteStore::open(cfg.db_path(), cfg.storage.clone()).unwrap();
    drop(store);
    let original = fs::read(cfg.db_path()).unwrap();
    let modified = fs::metadata(cfg.db_path()).unwrap().modified().unwrap();

    for apply in [false, true] {
        if apply {
            command.arg("--apply");
        }
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["applied"], apply);
        assert_eq!(report["orphan_attachments_count"], 1);
        assert_eq!(report["orphan_attachments_bytes"], 6);
        assert_eq!(report["db_bytes_before"], original.len());
        if !apply {
            assert_eq!(report["db_bytes_after"], original.len());
            assert_eq!(fs::read(cfg.db_path()).unwrap(), original);
            assert_eq!(
                fs::metadata(cfg.db_path()).unwrap().modified().unwrap(),
                modified
            );
            assert_eq!(fs::read_dir(&cfg.data_dir).unwrap().count(), 2);
            assert_eq!(fs::read(&orphan).unwrap(), b"unused");
        }
    }
    assert!(!orphan.exists());
    assert!(unrelated.exists());
}

#[tokio::test]
async fn gc_counts_live_wal_data_and_excludes_apply_while_the_store_is_open() {
    let fixture = Fixture::new();
    let cfg = ServerConfig {
        data_dir: fixture.0.clone(),
        ..Default::default()
    };
    fs::create_dir(cfg.attachments_dir()).unwrap();
    let store = SqliteStore::open(cfg.db_path(), cfg.storage.clone()).unwrap();
    let bytes = b"live fixture";
    let hash = "0000000000000000000000000000000000000000000000000000000000000000";
    let path = cfg.attachments_dir().join(hash);
    fs::write(&path, bytes).unwrap();
    store
        .save_attachment(t3_core::Attachment {
            id: Uuid::new_v4(),
            thread_id: None,
            filename: "live".into(),
            mime_type: "text/plain".into(),
            size_bytes: bytes.len() as u64,
            blake3_hash: hash.into(),
            storage_path: path.clone(),
            created_at: std::time::SystemTime::now().into(),
        })
        .await
        .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_t3d"));
    command.arg("gc").arg("--data-dir").arg(&cfg.data_dir);
    let sqlite_files = ["state.sqlite", "state.sqlite-wal", "state.sqlite-shm"];
    let before = sqlite_files.map(|name| fs::read(cfg.data_dir.join(name)).unwrap());
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["orphan_attachments_count"], 0);
    assert_eq!(
        sqlite_files.map(|name| fs::read(cfg.data_dir.join(name)).unwrap()),
        before
    );

    let output = command.arg("--apply").output().await.unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("stop the server"));
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[tokio::test]
async fn sigterm_stops_the_provider_group_and_persists_cancellation() {
    let fixture = Fixture::new();
    let (mut daemon, url, config) = fixture.start().await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let project: Value = client
        .post(format!("{url}/api/projects"))
        .bearer_auth(TOKEN)
        .json(&json!({"title":"Fixture","workspace_root":fixture.0}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let thread: Value = client
        .post(format!("{url}/api/threads"))
        .bearer_auth(TOKEN)
        .json(&json!({"title":"Fixture","project_id":project["id"]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let thread_id: Uuid = serde_json::from_value(thread["id"].clone()).unwrap();
    client
        .post(format!("{url}/api/threads/{thread_id}/messages"))
        .bearer_auth(TOKEN)
        .json(&json!({"prompt":"fixture"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let pids = timeout(Duration::from_secs(5), async {
        loop {
            match fs::read_to_string(fixture.0.join("provider.ready")) {
                Ok(pids) => break pids.trim().to_owned(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(err) => panic!("cannot read fixture PIDs: {err}"),
            }
        }
    })
    .await
    .expect("provider did not start");

    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &daemon.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        timeout(Duration::from_secs(10), daemon.wait())
            .await
            .expect("t3d did not exit on SIGTERM")
            .unwrap()
            .success()
    );
    let remaining = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pids])
        .output()
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&remaining.stdout)
            .lines()
            .all(|line| line.trim().starts_with('Z'))
    );
    let store = SqliteStore::open(config.db_path(), config.storage).unwrap();
    assert_eq!(
        store.get_thread(thread_id).await.unwrap().status,
        t3_core::ThreadStatus::Interrupted
    );
    assert_eq!(
        store.list_runs(thread_id).await.unwrap()[0].status,
        t3_core::RunStatus::Cancelled
    );
}

#[tokio::test]
async fn mcp_bridge_reads_env_and_opencode_health_returns_only_curated_models() {
    let fixture = Fixture::new();
    let (mut daemon, url, _) = fixture.start().await;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let health: Vec<t3_core::ProviderHealth> = client
        .get(format!("{url}/api/providers"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let opencode = health
        .iter()
        .find(|health| health.provider == ProviderId::OpenCode)
        .unwrap();
    assert!(opencode.available);
    let models: Vec<_> = opencode
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(
        models,
        [
            "opencode/deepseek-v4.1-flash-free",
            "opencode/flint-40b-free",
            "opencode/beam-free",
            "local/gemma4-31b",
            "win10-ds4p/ds4p-9b-heretic",
            "local-uncensored/uncensored-4b",
        ]
    );

    let mut bridge = Command::new(env!("CARGO_BIN_EXE_t3d"))
        .args(["mcp", "--url", &url])
        .env("T3_MCP_TOKEN", TOKEN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = bridge.stdin.take().unwrap();
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    drop(stdin);
    let output = timeout(Duration::from_secs(5), bridge.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"jsonrpc":"2.0","id":1,"result":{}})
    );

    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &daemon.id().unwrap().to_string()])
            .status()
            .await
            .unwrap()
            .success()
    );
    assert!(
        timeout(Duration::from_secs(5), daemon.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn mcp_bridge_completes_fast_requests_first_and_drains_after_eof() {
    timeout(BRIDGE_TIMEOUT, async {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let mut bridge = Command::new(env!("CARGO_BIN_EXE_t3d"))
            .args(["mcp", "--url", &url])
            .env("T3_MCP_TOKEN", TOKEN)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = bridge.stdin.take().unwrap();
        let mut stdout = BufReader::new(bridge.stdout.take().unwrap()).lines();
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"delegate_task\",\"arguments\":{\"task\":\"fixture\",\"mode\":\"wait\"}}}\n")
            .await
            .unwrap();
        let slow = accept_mcp_request(&server, 1).await;

        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"thread_read\",\"arguments\":{}}}\n")
            .await
            .unwrap();
        drop(stdin);
        let fast = accept_mcp_request(&server, 2).await;
        respond_to_mcp(fast, 2).await;
        let first: Value = serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first["id"], 2);

        respond_to_mcp(slow, 1).await;
        let second: Value = serde_json::from_str(&stdout.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(second["id"], 1);
        assert!(stdout.next_line().await.unwrap().is_none());
        assert!(bridge.wait().await.unwrap().success());
    })
    .await
    .expect("the bridge did not complete concurrent requests before the deadline");
}

async fn accept_mcp_request(server: &TcpListener, id: u64) -> TcpStream {
    let (socket, _) = server.accept().await.unwrap();
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim_end(), "POST /mcp HTTP/1.1");
    let mut content_length = None;
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().parse().unwrap());
        }
    }
    let mut body = vec![0; content_length.unwrap()];
    reader.read_exact(&mut body).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["id"], id);
    reader.into_inner()
}

async fn respond_to_mcp(mut socket: TcpStream, id: u64) {
    let body = json!({"jsonrpc": "2.0", "id": id, "result": {}});
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
}

#[tokio::test]
async fn import_history_rejects_source_as_destination() {
    let dir = Fixture::new();
    let config = ServerConfig {
        data_dir: dir.0.clone(),
        ..Default::default()
    };
    let source = dir.0.join("legacy.sqlite");
    fs::write(&source, b"source fixture").unwrap();
    fs::hard_link(&source, config.db_path()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_t3d"))
        .args(["import-history", "--from"])
        .arg(&source)
        .arg("--data-dir")
        .arg(&dir.0)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("source and destination databases must differ")
    );
    assert_eq!(fs::read(source).unwrap(), b"source fixture");
}
