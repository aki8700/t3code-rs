use crate::{ServerState, coordinator::RunCoordinator};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use t3_core::*;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::watch,
};
use uuid::Uuid;

const MAX_COMMAND_BYTES: usize = 16 * 1024;
const HEAD_STREAM_BYTES: usize = 64 * 1024;
const TAIL_STREAM_BYTES: usize = 192 * 1024;
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
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ShellStreamStart {
    pub exec_id: Uuid,
    pub command: String,
    pub cwd: String,
}

struct ActiveShellEntry {
    cancel: watch::Sender<bool>,
    #[allow(dead_code)]
    thread_id: Option<Uuid>,
    #[allow(dead_code)]
    project_id: Option<Uuid>,
}

#[derive(Clone, Default)]
pub(crate) struct ActiveShells {
    inner: Arc<Mutex<HashMap<Uuid, ActiveShellEntry>>>,
}

impl ActiveShells {
    pub(crate) fn register(
        &self,
        exec_id: Uuid,
        thread_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) -> (watch::Receiver<bool>, ActiveShellGuard) {
        let (tx, rx) = watch::channel(false);
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).insert(
            exec_id,
            ActiveShellEntry {
                cancel: tx,
                thread_id,
                project_id,
            },
        );
        let guard = ActiveShellGuard {
            shells: self.clone(),
            exec_id,
        };
        (rx, guard)
    }

    pub(crate) fn cancel(&self, exec_id: Uuid) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.get(&exec_id) {
            entry.cancel.send_replace(true);
            true
        } else {
            false
        }
    }

    pub(crate) fn active_count(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn remove(&self, exec_id: Uuid) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&exec_id);
    }
}

pub(crate) struct ActiveShellGuard {
    shells: ActiveShells,
    exec_id: Uuid,
}

impl Drop for ActiveShellGuard {
    fn drop(&mut self) {
        self.shells.remove(self.exec_id);
    }
}

#[derive(Clone)]
struct StreamSink {
    coordinator: RunCoordinator,
    exec_id: Uuid,
    thread_id: Option<Uuid>,
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
    persist_thread_shell_result(state, &thread, started_at, completed_at, &mut result).await?;
    Ok(result)
}

pub(crate) async fn exec_in_thread_streaming(
    state: ServerState,
    shells: ActiveShells,
    thread_id: Uuid,
    req: ShellExecRequest,
) -> CoreResult<ShellStreamStart> {
    let thread = state.store.get_thread(thread_id).await?;
    if thread.archived {
        return Err(CoreError::InvalidArgument("thread is archived".into()));
    }
    let workspace = state.coordinator.workspace(&thread).await?;
    let (command, cwd, timeout_secs) = prepare_shell(&workspace, req).await?;
    let exec_id = Uuid::new_v4();
    let cwd_str = cwd.to_string_lossy().into_owned();
    let start = ShellStreamStart {
        exec_id,
        command: command.clone(),
        cwd: cwd_str,
    };

    let (cancel_rx, active_guard) =
        shells.register(exec_id, Some(thread_id), Some(thread.project_id));

    tokio::spawn(async move {
        let _guard = active_guard;
        let started_at = Utc::now();
        let sink = StreamSink {
            coordinator: state.coordinator.clone(),
            exec_id,
            thread_id: Some(thread_id),
        };
        match run_shell_internal(command, cwd, timeout_secs, cancel_rx, Some(sink)).await {
            Ok(mut result) => {
                let completed_at = Utc::now();
                if let Err(err) = persist_thread_shell_result(
                    &state,
                    &thread,
                    started_at,
                    completed_at,
                    &mut result,
                )
                .await
                {
                    tracing::error!(
                        %exec_id,
                        %thread_id,
                        error = %err,
                        "failed to persist streaming shell turn item"
                    );
                }
                drop(_guard);
                state.coordinator.emit(WsServerMessage::ShellFinished {
                    exec_id,
                    thread_id: Some(thread_id),
                    exit_code: result.exit_code,
                    duration_ms: result.duration_ms,
                    timed_out: result.timed_out,
                    cancelled: result.cancelled,
                    truncated: result.truncated,
                    item_id: result.item_id,
                });
            }
            Err(err) => {
                tracing::warn!(
                    %exec_id,
                    %thread_id,
                    error = %err,
                    "streaming shell execution failed"
                );
                drop(_guard);
                state.coordinator.emit(WsServerMessage::ShellFinished {
                    exec_id,
                    thread_id: Some(thread_id),
                    exit_code: None,
                    duration_ms: 0,
                    timed_out: false,
                    cancelled: false,
                    truncated: false,
                    item_id: None,
                });
            }
        }
    });

    Ok(start)
}

pub(crate) async fn exec_in_project_streaming(
    state: ServerState,
    shells: ActiveShells,
    project_id: Uuid,
    req: ShellExecRequest,
) -> CoreResult<ShellStreamStart> {
    let project = state.store.get_project(project_id).await?;
    let (command, cwd, timeout_secs) = prepare_shell(&project.workspace_root, req).await?;
    let exec_id = Uuid::new_v4();
    let cwd_str = cwd.to_string_lossy().into_owned();
    let start = ShellStreamStart {
        exec_id,
        command: command.clone(),
        cwd: cwd_str,
    };

    let (cancel_rx, active_guard) = shells.register(exec_id, None, Some(project_id));

    tokio::spawn(async move {
        let _guard = active_guard;
        let sink = StreamSink {
            coordinator: state.coordinator.clone(),
            exec_id,
            thread_id: None,
        };
        match run_shell_internal(command, cwd, timeout_secs, cancel_rx, Some(sink)).await {
            Ok(result) => {
                drop(_guard);
                state.coordinator.emit(WsServerMessage::ShellFinished {
                    exec_id,
                    thread_id: None,
                    exit_code: result.exit_code,
                    duration_ms: result.duration_ms,
                    timed_out: result.timed_out,
                    cancelled: result.cancelled,
                    truncated: result.truncated,
                    item_id: None,
                });
            }
            Err(err) => {
                tracing::warn!(
                    %exec_id,
                    %project_id,
                    error = %err,
                    "streaming project shell execution failed"
                );
                drop(_guard);
                state.coordinator.emit(WsServerMessage::ShellFinished {
                    exec_id,
                    thread_id: None,
                    exit_code: None,
                    duration_ms: 0,
                    timed_out: false,
                    cancelled: false,
                    truncated: false,
                    item_id: None,
                });
            }
        }
    });

    Ok(start)
}

async fn persist_thread_shell_result(
    state: &ServerState,
    thread: &Thread,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    result: &mut ShellExecOutput,
) -> CoreResult<()> {
    persist_thread_command_result(state, thread, started_at, completed_at, result, None).await
}

pub(crate) async fn persist_thread_command_result(
    state: &ServerState,
    thread: &Thread,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    result: &mut ShellExecOutput,
    selection: Option<&ModelSelection>,
) -> CoreResult<()> {
    let source = if selection.is_some() {
        "gemini_app"
    } else {
        "user_shell"
    };
    let run_id = Uuid::new_v4();
    let ok = !result.timed_out && !result.cancelled && result.exit_code == Some(0);
    let status = if ok {
        RunStatus::Completed
    } else if result.cancelled {
        RunStatus::Cancelled
    } else {
        RunStatus::Failed
    };
    let error_message = if result.cancelled {
        Some("command cancelled".to_owned())
    } else if result.timed_out {
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
            thread_id: thread.id,
            ordinal: 0,
            provider: selection.map_or(thread.model_selection.provider, |s| s.provider),
            model: selection.map_or_else(|| "shell".into(), |s| s.model.clone()),
            effort: selection.and_then(|s| s.effort.clone()),
            status,
            prompt: if source == "user_shell" {
                format!("!{}", result.command)
            } else {
                result.command.clone()
            },
            provider_session_id: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
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
    if result.cancelled {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str("[キャンセルされました]");
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
            thread_id: thread.id,
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
                "cancelled": result.cancelled,
                "truncated": result.truncated,
                "source": source,
            }),
            searchable_text: Some(searchable),
        })
        .await?;
    result.item_id = Some(item.id);
    Ok(())
}

pub(crate) async fn run_shell(
    workspace: &Path,
    req: ShellExecRequest,
) -> CoreResult<ShellExecOutput> {
    let (_tx, rx) = watch::channel(false);
    run_shell_cancellable(workspace, req, rx).await
}

pub(crate) async fn run_shell_cancellable(
    workspace: &Path,
    req: ShellExecRequest,
    cancel: watch::Receiver<bool>,
) -> CoreResult<ShellExecOutput> {
    let (command, cwd, timeout_secs) = prepare_shell(workspace, req).await?;
    run_shell_internal(command, cwd, timeout_secs, cancel, None).await
}

async fn prepare_shell(
    workspace: &Path,
    req: ShellExecRequest,
) -> CoreResult<(String, PathBuf, u64)> {
    let command = req.command.trim().to_owned();
    if command.is_empty() {
        return Err(CoreError::InvalidArgument("command is required".into()));
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err(CoreError::InvalidArgument(format!(
            "command exceeds {MAX_COMMAND_BYTES} bytes"
        )));
    }
    let cwd = tokio::fs::canonicalize(workspace)
        .await
        .map_err(|_| CoreError::InvalidArgument("workspace directory does not exist".into()))?;
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
    Ok((command, cwd, timeout_secs))
}

async fn run_shell_internal(
    command: String,
    cwd: PathBuf,
    timeout_secs: u64,
    mut cancel: watch::Receiver<bool>,
    sink: Option<StreamSink>,
) -> CoreResult<ShellExecOutput> {
    if *cancel.borrow() {
        return Ok(ShellExecOutput {
            command,
            cwd: cwd.to_string_lossy().into_owned(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: 0,
            timed_out: false,
            cancelled: true,
            truncated: false,
            item_id: None,
        });
    }

    let mut cmd = build_shell_command(&command, &cwd);
    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| CoreError::Internal(format!("failed to spawn shell: {e}")))?;
    #[cfg(unix)]
    let mut group_guard = ProcessGroupGuard(child.id());
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let out_sink = sink.as_ref().map(|s| (s.clone(), "stdout"));
    let err_sink = sink.map(|s| (s, "stderr"));

    let mut out_task = tokio::spawn(async move {
        match stdout_pipe {
            Some(pipe) => read_capped_streaming(pipe, out_sink).await,
            None => (Vec::new(), false),
        }
    });
    let mut err_task = tokio::spawn(async move {
        match stderr_pipe {
            Some(pipe) => read_capped_streaming(pipe, err_sink).await,
            None => (Vec::new(), false),
        }
    });

    let sleep = tokio::time::sleep(Duration::from_secs(timeout_secs));
    tokio::pin!(sleep);

    let (exit_code, timed_out, cancelled) = tokio::select! {
        biased;
        () = wait_cancelled(&mut cancel) => {
            #[cfg(unix)]
            group_guard.kill_now();
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, false, true)
        }
        status = child.wait() => match status {
            Ok(status) => (status.code(), false, false),
            Err(e) => {
                #[cfg(unix)]
                group_guard.kill_now();
                let _ = child.kill().await;
                out_task.abort();
                err_task.abort();
                return Err(CoreError::Internal(format!("shell wait failed: {e}")));
            }
        },
        () = &mut sleep => {
            #[cfg(unix)]
            group_guard.kill_now();
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true, false)
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
        cancelled,
        truncated: out_trunc || err_trunc,
        item_id: None,
    })
}

async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    loop {
        match cancel.changed().await {
            Ok(()) => {
                if *cancel.borrow() {
                    return;
                }
            }
            Err(_) => {
                std::future::pending::<()>().await;
            }
        }
    }
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

async fn read_capped_streaming<R: AsyncRead + Unpin>(
    mut reader: R,
    sink: Option<(StreamSink, &'static str)>,
) -> (Vec<u8>, bool) {
    let mut head = Vec::with_capacity(HEAD_STREAM_BYTES);
    let mut tail = VecDeque::with_capacity(TAIL_STREAM_BYTES);
    let mut tail_bytes_seen = 0usize;
    let mut pending_utf8 = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let head_take = (HEAD_STREAM_BYTES - head.len()).min(n);
                let head_slice = &buf[..head_take];
                head.extend_from_slice(head_slice);
                if !head_slice.is_empty()
                    && let Some((ref stream_sink, stream_name)) = sink
                {
                    pending_utf8.extend_from_slice(head_slice);
                    let chunk = take_valid_utf8_prefix(&mut pending_utf8);
                    if !chunk.is_empty() {
                        stream_sink.coordinator.emit(WsServerMessage::ShellOutput {
                            exec_id: stream_sink.exec_id,
                            thread_id: stream_sink.thread_id,
                            stream: stream_name.to_owned(),
                            chunk,
                        });
                    }
                }

                let tail_slice = &buf[head_take..n];
                tail_bytes_seen = tail_bytes_seen.saturating_add(tail_slice.len());
                if !tail_slice.is_empty() {
                    if tail_slice.len() >= TAIL_STREAM_BYTES {
                        tail.clear();
                        tail.extend(&tail_slice[tail_slice.len() - TAIL_STREAM_BYTES..]);
                    } else {
                        let excess = tail
                            .len()
                            .saturating_add(tail_slice.len())
                            .saturating_sub(TAIL_STREAM_BYTES);
                        tail.drain(..excess);
                        tail.extend(tail_slice);
                    }
                }
            }
        }
    }

    let omitted_bytes = tail_bytes_seen.saturating_sub(tail.len());
    let marker =
        (omitted_bytes > 0).then(|| format!("\n[... {omitted_bytes} bytes omitted ...]\n"));
    if let Some((stream_sink, stream_name)) = sink {
        let emit = |chunk: String| {
            if !chunk.is_empty() {
                stream_sink.coordinator.emit(WsServerMessage::ShellOutput {
                    exec_id: stream_sink.exec_id,
                    thread_id: stream_sink.thread_id,
                    stream: stream_name.to_owned(),
                    chunk,
                });
            }
        };
        if !pending_utf8.is_empty() {
            emit(String::from_utf8_lossy(&pending_utf8).into_owned());
        }
        if let Some(marker) = &marker {
            emit(marker.clone());
        }
        if !tail.is_empty() {
            emit(String::from_utf8_lossy(tail.make_contiguous()).into_owned());
        }
    }

    let mut out = head;
    if let Some(marker) = marker {
        out.extend_from_slice(marker.as_bytes());
    }
    out.extend(tail);
    (out, omitted_bytes > 0)
}

fn take_valid_utf8_prefix(pending: &mut Vec<u8>) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                out.push_str(valid);
                pending.clear();
                break;
            }
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to > 0 {
                    if let Ok(valid) = std::str::from_utf8(&pending[..valid_up_to]) {
                        out.push_str(valid);
                    }
                    pending.drain(..valid_up_to);
                }
                match err.error_len() {
                    Some(invalid_len) => {
                        out.push('\u{FFFD}');
                        pending.drain(..invalid_len);
                    }
                    None => break,
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_shell_cancellable_completes_normally() {
        let dir = std::env::temp_dir();
        let (_tx, rx) = watch::channel(false);
        let out = run_shell_cancellable(
            &dir,
            ShellExecRequest {
                command: "echo hello-shell".into(),
                timeout_secs: Some(5),
            },
            rx,
        )
        .await
        .expect("shell should run");

        assert_eq!(out.exit_code, Some(0));
        assert!(!out.timed_out);
        assert!(!out.cancelled);
        assert!(!out.truncated);
        assert_eq!(out.stdout.trim(), "hello-shell");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_keeps_tail_when_output_exceeds_cap() {
        let dir = std::env::temp_dir();
        let (_tx, rx) = watch::channel(false);
        let out = run_shell_cancellable(
            &dir,
            ShellExecRequest {
                command:
                    "head -c 300000 /dev/zero | tr '\\000' 'x'; printf '\\nFINAL_LINE_MARKER\\n'"
                        .into(),
                timeout_secs: Some(5),
            },
            rx,
        )
        .await
        .expect("shell should run");

        assert_eq!(out.exit_code, Some(0));
        assert!(out.truncated);
        assert!(out.stdout.starts_with(&"x".repeat(64 * 1024)));
        assert!(out.stdout.ends_with("FINAL_LINE_MARKER\n"));
        assert!(out.stdout.contains("[... 37875 bytes omitted ...]"));
    }

    #[tokio::test]
    async fn run_shell_cancellable_already_cancelled_skips_spawn() {
        let dir = std::env::temp_dir();
        let marker = dir.join(format!("t3-shell-cancel-test-{}", Uuid::new_v4()));
        let (_tx, rx) = watch::channel(true);
        let out = run_shell_cancellable(
            &dir,
            ShellExecRequest {
                command: format!("touch '{}'", marker.display()),
                timeout_secs: Some(5),
            },
            rx,
        )
        .await
        .expect("already-cancelled shell returns ok");

        assert!(out.cancelled);
        assert!(!out.timed_out);
        assert_eq!(out.exit_code, None);
        assert!(
            !marker.exists(),
            "command must not execute when already cancelled"
        );
    }

    #[tokio::test]
    async fn run_shell_cancellable_stops_running_process_immediately() {
        let dir = std::env::temp_dir();
        let (tx, rx) = watch::channel(false);
        let started = Instant::now();

        let task = tokio::spawn(async move {
            run_shell_cancellable(
                &dir,
                ShellExecRequest {
                    command: "sleep 30 & wait".into(),
                    timeout_secs: Some(30),
                },
                rx,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(80)).await;
        tx.send_replace(true);

        let out = task
            .await
            .expect("join ok")
            .expect("cancelled shell returns ok");
        assert!(out.cancelled, "expected cancelled=true");
        assert!(!out.timed_out, "expected timed_out=false");
        assert_eq!(out.exit_code, None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancellation should stop process promptly, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn active_shells_tracks_cancels_and_cleans_up_on_drop() {
        let shells = ActiveShells::default();
        assert_eq!(shells.active_count(), 0);

        let unknown = Uuid::new_v4();
        assert!(!shells.cancel(unknown));

        let exec_id = Uuid::new_v4();
        let thread_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let (rx, guard) = shells.register(exec_id, Some(thread_id), Some(project_id));
        assert_eq!(shells.active_count(), 1);
        assert!(!*rx.borrow());

        assert!(shells.cancel(exec_id));
        assert!(*rx.borrow());

        drop(guard);
        assert_eq!(shells.active_count(), 0);
        assert!(!shells.cancel(exec_id));
    }

    #[tokio::test]
    async fn active_shells_cancels_background_shell_and_removes_entry() {
        let shells = ActiveShells::default();
        let exec_id = Uuid::new_v4();
        let (rx, guard) = shells.register(exec_id, None, Some(Uuid::new_v4()));
        assert_eq!(shells.active_count(), 1);

        let dir = std::env::temp_dir();
        let handle = tokio::spawn(async move {
            let _guard = guard;
            run_shell_cancellable(
                &dir,
                ShellExecRequest {
                    command: "sleep 30".into(),
                    timeout_secs: Some(30),
                },
                rx,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(shells.cancel(exec_id));

        let out = handle.await.expect("join ok").expect("shell result ok");
        assert!(out.cancelled);
        assert_eq!(shells.active_count(), 0);
        assert!(!shells.cancel(exec_id));
    }
}
