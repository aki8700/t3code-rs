use crate::ServerState;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use t3_core::*;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use uuid::Uuid;

const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_STREAM_BYTES: usize = 256 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Deserialize)]
pub(crate) struct ShellExecRequest {
    pub command: String,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShellExecOutput {
    pub command: String,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<Uuid>,
}

pub(crate) async fn exec_in_project(
    state: &ServerState,
    project_id: Uuid,
    req: ShellExecRequest,
) -> CoreResult<ShellExecOutput> {
    let project = state.store.get_project(project_id).await?;
    run_shell(&project.workspace_root, req).await
}

pub(crate) async fn exec_in_thread(
    state: &ServerState,
    thread_id: Uuid,
    req: ShellExecRequest,
) -> CoreResult<ShellExecOutput> {
    let thread = state.store.get_thread(thread_id).await?;
    if thread.archived {
        return Err(CoreError::InvalidArgument("thread is archived".into()));
    }
    let workspace = state.coordinator.workspace(&thread).await?;
    let started_at = Utc::now();
    let mut result = run_shell(&workspace, req).await?;
    let completed_at = Utc::now();

    let run_id = Uuid::new_v4();
    let ok = !result.timed_out && result.exit_code == Some(0);
    let status = if ok {
        RunStatus::Completed
    } else {
        RunStatus::Failed
    };
    let error_message = if result.timed_out {
        Some("command timed out".to_owned())
    } else if !ok {
        result.exit_code.map(|c| format!("exit code {c}"))
    } else {
        None
    };

    // Use ordinal 0 (like metadata runs) so shell executions never interfere with
    // AI session resumption (`previous_run` filters `ordinal > 0`) or run timers.
    state
        .store
        .create_run(Run {
            id: run_id,
            thread_id,
            ordinal: 0,
            provider: thread.model_selection.provider,
            model: "shell".into(),
            effort: None,
            status,
            prompt: format!("!{}", result.command),
            provider_session_id: None,
            input_tokens: None,
            output_tokens: None,
            duration_ms: None,
            error_message,
            started_at,
            completed_at: Some(completed_at),
        })
        .await?;

    let mut combined = String::new();
    if !result.stdout.is_empty() {
        combined.push_str(&result.stdout);
    }
    if !result.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&result.stderr);
    }
    if result.timed_out {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str("[タイムアウトにより強制終了しました]");
    }
    if result.truncated {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str("[出力が上限を超えたため一部省略されました]");
    }

    let searchable = format!("{}\n{}", result.command, combined);
    let summary = result.command.chars().take(240).collect::<String>();
    let item = state
        .coordinator
        .append(NewTurnItem {
            thread_id,
            run_id,
            kind: TurnItemKind::CommandExecution,
            summary: Some(summary),
            payload: json!({
                "command": result.command,
                "cwd": result.cwd,
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "output": combined,
                "duration_ms": result.duration_ms,
                "timed_out": result.timed_out,
                "truncated": result.truncated,
                "source": "user_shell",
            }),
            searchable_text: Some(searchable),
        })
        .await?;
    result.item_id = Some(item.id);
    Ok(result)
}

async fn run_shell(workspace: &Path, req: ShellExecRequest) -> CoreResult<ShellExecOutput> {
    let command = req.command.trim().to_owned();
    if command.is_empty() {
        return Err(CoreError::InvalidArgument("command is required".into()));
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err(CoreError::InvalidArgument(format!(
            "command exceeds {MAX_COMMAND_BYTES} bytes"
        )));
    }
    let cwd = tokio::fs::canonicalize(workspace).await.map_err(|_| {
        CoreError::InvalidArgument("workspace directory does not exist".into())
    })?;
    if !tokio::fs::metadata(&cwd)
        .await
        .map_err(|e| CoreError::Internal(e.to_string()))?
        .is_dir()
    {
        return Err(CoreError::InvalidArgument(
            "workspace path is not a directory".into(),
        ));
    }

    let timeout_secs = req
        .timeout_secs
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    let mut cmd = build_shell_command(&command, &cwd);
    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| CoreError::Internal(format!("failed to spawn shell: {e}")))?;
    #[cfg(unix)]
    let mut group_guard = ProcessGroupGuard(child.id());
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let mut out_task = tokio::spawn(async move {
        match stdout_pipe {
            Some(pipe) => read_capped(pipe, MAX_STREAM_BYTES).await,
            None => (Vec::new(), false),
        }
    });
    let mut err_task = tokio::spawn(async move {
        match stderr_pipe {
            Some(pipe) => read_capped(pipe, MAX_STREAM_BYTES).await,
            None => (Vec::new(), false),
        }
    });

    let wait_res = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;
    let (exit_code, timed_out) = match wait_res {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => {
            #[cfg(unix)]
            group_guard.kill_now();
            let _ = child.kill().await;
            out_task.abort();
            err_task.abort();
            return Err(CoreError::Internal(format!("shell wait failed: {e}")));
        }
        Err(_) => {
            #[cfg(unix)]
            group_guard.kill_now();
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true)
        }
    };
    #[cfg(unix)]
    {
        // Reap any lingering backgrounded descendants in the child's process group
        // so inherited stdio pipes cannot hang the request after the shell exits.
        group_guard.kill_now();
        group_guard.disarm();
    }

    let drain_limit = Duration::from_millis(250);
    let (stdout_bytes, out_trunc) = finish_stream(&mut out_task, drain_limit).await;
    let (stderr_bytes, err_trunc) = finish_stream(&mut err_task, drain_limit).await;
    let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;

    Ok(ShellExecOutput {
        command,
        cwd: cwd.to_string_lossy().into_owned(),
        exit_code,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        duration_ms,
        timed_out,
        truncated: out_trunc || err_trunc,
        item_id: None,
    })
}

#[cfg(unix)]
struct ProcessGroupGuard(Option<u32>);

#[cfg(unix)]
impl ProcessGroupGuard {
    fn kill_now(&self) {
        if let Some(pid) = self.0 {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill_now();
    }
}

async fn finish_stream(
    handle: &mut tokio::task::JoinHandle<(Vec<u8>, bool)>,
    limit: Duration,
) -> (Vec<u8>, bool) {
    match tokio::time::timeout(limit, &mut *handle).await {
        Ok(Ok(res)) => res,
        Ok(Err(_)) => (Vec::new(), false),
        Err(_) => {
            handle.abort();
            (Vec::new(), true)
        }
    }
}

fn build_shell_command(command: &str, cwd: &Path) -> Command {
    #[cfg(unix)]
    let mut cmd = {
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        let mut c = Command::new(shell);
        c.arg("-c").arg(command);
        c.process_group(0);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    };

    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_remove("T3_BEARER_TOKEN")
        .env_remove("T3_MCP_TOKEN")
        .env("DO_NOT_TRACK", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("AGY_DISABLE_TELEMETRY", "1")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("OPENCODE_DISABLE_TELEMETRY", "1")
        .env("CODEX_DISABLE_TELEMETRY", "1")
        .env("T3CODE_TELEMETRY_ENABLED", "false")
        .env("T3CODE_OTEL_SDK_DISABLED", "true");
    cmd
}

async fn read_capped<R: AsyncRead + Unpin>(mut reader: R, limit: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.len() < limit {
                    let take = (limit - out.len()).min(n);
                    out.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
        }
    }
    (out, truncated)
}
