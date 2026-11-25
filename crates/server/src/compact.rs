//! In-place context compaction for Codex sessions.
//!
//! `codex exec resume` hands `/compact` to the model as ordinary prompt text, so the
//! compaction goes through the app-server `thread/compact/start` RPC instead. The session id
//! stays the same, which keeps the next `exec resume` turn on the compacted history.

use std::{path::Path, time::Duration};

use serde_json::{Value, json};
use t3_core::{CoreError, CoreResult};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::ChildStdin,
};

use crate::limits::{MAX_LINE_BYTES, prepare_probe_cmd, read_capped_line};

const COMPACT_TIMEOUT: Duration = Duration::from_secs(180);
const EXIT_GRACE: Duration = Duration::from_secs(5);

const ID_INITIALIZE: i64 = 1;
const ID_RESUME: i64 = 2;
const ID_COMPACT: i64 = 3;

pub(crate) async fn compact_codex_session(
    bin: &Path,
    session_id: &str,
    cwd: &Path,
) -> CoreResult<()> {
    compact_with_timeout(bin, session_id, cwd, COMPACT_TIMEOUT).await
}

async fn compact_with_timeout(
    bin: &Path,
    session_id: &str,
    cwd: &Path,
    timeout: Duration,
) -> CoreResult<()> {
    if !bin.is_file() {
        return Err(CoreError::Provider(
            "Codex CLI が見つかりません。設定の codex_bin を確認してください。".into(),
        ));
    }
    let mut cmd = prepare_probe_cmd(bin);
    cmd.arg("app-server").current_dir(cwd);
    let mut child = cmd
        .spawn()
        .map_err(|error| CoreError::Provider(format!("Codex CLI の起動に失敗しました: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| CoreError::Provider("Codex CLI の標準入力を開けません".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| CoreError::Provider("Codex CLI の標準出力を開けません".into()))?;

    let result = tokio::time::timeout(
        timeout,
        drive(&mut stdin, BufReader::new(stdout), session_id, cwd),
    )
    .await;

    // The server exits on stdin EOF; give it a moment to flush the session file before killing it.
    drop(stdin);
    if tokio::time::timeout(EXIT_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    result.unwrap_or_else(|_| {
        Err(CoreError::Provider(
            "Codex の圧縮がタイムアウトしました".into(),
        ))
    })
}

async fn drive<R: tokio::io::AsyncBufRead + Unpin>(
    stdin: &mut ChildStdin,
    mut reader: R,
    session_id: &str,
    cwd: &Path,
) -> CoreResult<()> {
    send(
        stdin,
        json!({
            "jsonrpc": "2.0", "id": ID_INITIALIZE, "method": "initialize",
            "params": { "clientInfo": { "name": "t3code-rs", "version": crate::VERSION } }
        }),
    )
    .await?;

    let mut compacted = false;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = read_capped_line(&mut reader, &mut buf).await?;
        if read == 0 {
            return Err(CoreError::Provider(
                "Codex app-server が圧縮の完了前に終了しました".into(),
            ));
        }
        if buf.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };

        // Responses carry an id and no method; server-initiated requests carry both.
        if msg.get("method").is_none()
            && let Some(id) = msg.get("id").and_then(Value::as_i64)
        {
            if let Some(error) = msg.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex の RPC 実行に失敗しました");
                return Err(CoreError::Provider(message.to_owned()));
            }
            match id {
                ID_INITIALIZE => {
                    send(stdin, json!({ "jsonrpc": "2.0", "method": "initialized" })).await?;
                    send(
                        stdin,
                        json!({
                            "jsonrpc": "2.0", "id": ID_RESUME, "method": "thread/resume",
                            "params": {
                                "threadId": session_id,
                                "cwd": cwd.to_string_lossy(),
                                "excludeTurns": true
                            }
                        }),
                    )
                    .await?;
                }
                ID_RESUME => {
                    send(
                        stdin,
                        json!({
                            "jsonrpc": "2.0", "id": ID_COMPACT, "method": "thread/compact/start",
                            "params": { "threadId": session_id }
                        }),
                    )
                    .await?;
                }
                _ => {}
            }
            continue;
        }

        let Some(params) = msg.get("params") else {
            continue;
        };
        if params.get("threadId").and_then(Value::as_str) != Some(session_id) {
            continue;
        }
        match msg.get("method").and_then(Value::as_str) {
            Some("item/completed")
                if params.pointer("/item/type").and_then(Value::as_str)
                    == Some("contextCompaction") =>
            {
                compacted = true;
            }
            Some("turn/completed") => {
                if compacted {
                    return Ok(());
                }
                let reason = params
                    .pointer("/turn/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("圧縮が実行されませんでした");
                return Err(CoreError::Provider(reason.to_owned()));
            }
            _ => {}
        }
    }
}

async fn send(stdin: &mut ChildStdin, value: Value) -> CoreResult<()> {
    let mut line = serde_json::to_vec(&value).map_err(|e| CoreError::Internal(e.to_string()))?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .await
        .map_err(|e| CoreError::Provider(format!("Codex への書き込みに失敗しました: {e}")))?;
    stdin
        .flush()
        .await
        .map_err(|e| CoreError::Provider(format!("Codex への書き込みに失敗しました: {e}")))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    use uuid::Uuid;

    /// A stand-in `codex` that speaks just enough app-server JSON-RPC over stdio. `script`
    /// runs after the shared prelude, which logs every request line it receives.
    fn fake_codex(script: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("t3-compact-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("codex");
        fs::write(
            &bin,
            format!("#!/bin/sh\nlog=\"$(dirname \"$0\")/received.log\"\n{script}\n"),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, bin)
    }

    const HAPPY: &str = r#"
read init; echo '{"id":1,"result":{}}'
read initialized
read resume; echo '{"id":2,"result":{}}'
read compact; echo '{"id":3,"result":{}}'
echo '{"method":"item/completed","params":{"threadId":"sess-1","item":{"type":"contextCompaction"}}}'
echo '{"method":"turn/completed","params":{"threadId":"sess-1","turn":{"status":"completed"}}}'
printf '%s\n' "$init" "$initialized" "$resume" "$compact" > "$log"
"#;

    #[tokio::test]
    async fn compacts_the_session_in_place() {
        let (dir, bin) = fake_codex(HAPPY);
        compact_with_timeout(&bin, "sess-1", &dir, Duration::from_secs(10))
            .await
            .unwrap();
        let log = fs::read_to_string(dir.join("received.log")).unwrap();
        let methods: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["method"].clone())
            .collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "thread/resume",
                "thread/compact/start"
            ]
        );
        let resume: Value = serde_json::from_str(log.lines().nth(2).unwrap()).unwrap();
        assert_eq!(resume["params"]["threadId"], "sess-1");
    }

    #[tokio::test]
    async fn reports_a_json_rpc_error() {
        let (dir, bin) = fake_codex(
            r#"read init; echo '{"id":1,"result":{}}'
read initialized
read resume; echo '{"id":2,"error":{"code":-32600,"message":"no rollout found"}}'"#,
        );
        let error = compact_with_timeout(&bin, "sess-1", &dir, Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no rollout found"), "{error}");
    }

    #[tokio::test]
    async fn turn_without_compaction_is_an_error() {
        let (dir, bin) = fake_codex(
            r#"read init; echo '{"id":1,"result":{}}'
read initialized
read resume; echo '{"id":2,"result":{}}'
read compact; echo '{"id":3,"result":{}}'
echo '{"method":"turn/completed","params":{"threadId":"sess-1","turn":{"status":"failed","error":{"message":"context window exceeded"}}}}'"#,
        );
        let error = compact_with_timeout(&bin, "sess-1", &dir, Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("context window exceeded"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn ignores_other_threads_and_survives_early_exit() {
        let (dir, bin) = fake_codex(
            r#"read init; echo '{"id":1,"result":{}}'
read initialized
read resume; echo '{"id":2,"result":{}}'
read compact; echo '{"id":3,"result":{}}'
echo '{"method":"turn/completed","params":{"threadId":"other","turn":{"status":"completed"}}}'"#,
        );
        let error = compact_with_timeout(&bin, "sess-1", &dir, Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("完了前に終了"), "{error}");
    }

    #[tokio::test]
    async fn times_out_when_the_server_goes_silent() {
        let (dir, bin) = fake_codex("read init; sleep 30");
        let error = compact_with_timeout(&bin, "sess-1", &dir, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("タイムアウト"), "{error}");
    }

    /// Manual check against the real CLI (spends a little quota):
    /// `T3_TEST_CODEX_SESSION=<id> T3_TEST_CODEX_CWD=<dir> cargo test -p t3-server --lib real_codex -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn real_codex_compacts_an_existing_session() {
        let session = std::env::var("T3_TEST_CODEX_SESSION").unwrap();
        let cwd = std::env::var("T3_TEST_CODEX_CWD").unwrap();
        let bin = std::env::var("T3_TEST_CODEX_BIN").unwrap_or("/opt/homebrew/bin/codex".into());
        compact_codex_session(Path::new(&bin), &session, Path::new(&cwd))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn missing_binary_is_reported() {
        let error = compact_with_timeout(
            Path::new("/nonexistent/codex"),
            "sess-1",
            Path::new("/tmp"),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("見つかりません"), "{error}");
    }
}
