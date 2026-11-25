use crate::{
    parsers::{Parser, provider_error},
    watchdog::{RepetitionWatchdog, ToolLoopWatchdog},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use t3_core::{
    protocol::StreamEvent,
    traits::{CoreError, CoreResult},
    types::{PendingResponse, ProviderId},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::{mpsc, oneshot},
};
#[cfg(unix)]
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};
use uuid::Uuid;

const STDERR_LIMIT: usize = 16 * 1024;
const STDOUT_LINE_LIMIT: usize = 4 * 1024 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_IDLE_STDOUT_TIMEOUT: Duration = Duration::from_secs(900);
const MIN_IDLE_TIMEOUT_SECS: u64 = 60;
const MAX_IDLE_TIMEOUT_SECS: u64 = 3600;
const IDLE_TIMEOUT_ENV: &str = "T3_IDLE_TIMEOUT_SECS";
const LEGACY_IDLE_STDOUT_TIMEOUT_ENV: &str = "T3_IDLE_STDOUT_TIMEOUT_SECS";

/// Messages written to a streaming provider's stdin while the turn is open.
enum StdinMsg {
    /// A mid-turn user message, wrapped as a stream-json `user` event.
    Steer(String),
    /// A complete JSON object written as-is (one line, newline added on write).
    RawLine(String),
}

/// A provider permission request that is waiting for the user's answer.
pub(crate) struct ControlRequest {
    /// The CLI's own `request_id`, echoed back in `control_response`.
    provider_request_id: String,
    /// Original tool input, used as `updatedInput` when the user approves unchanged.
    input: Value,
}

pub(crate) struct ActiveProcess {
    #[cfg(unix)]
    pid: u32,
    #[cfg(unix)]
    released: watch::Receiver<bool>,
    stdin_tx: Option<mpsc::UnboundedSender<StdinMsg>>,
    /// Keyed by the `PendingRequest::id` shown to the user.
    control_requests: ControlRequests,
    interrupted: Arc<AtomicBool>,
    #[cfg(not(unix))]
    stop: oneshot::Sender<()>,
}

pub(crate) type ActiveRunMap = Arc<Mutex<HashMap<Uuid, ActiveProcess>>>;
type ControlRequests = Arc<Mutex<HashMap<Uuid, ControlRequest>>>;

pub(crate) enum Input {
    StreamJson(String),
    Text(String),
    None,
}

pub(crate) fn prepare(cmd: &mut Command) {
    cmd.envs([
        ("DO_NOT_TRACK", "1"),
        ("AGY_DISABLE_TELEMETRY", "1"),
        ("DISABLE_TELEMETRY", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("T3CODE_TELEMETRY_ENABLED", "false"),
        ("T3CODE_OTEL_SDK_DISABLED", "true"),
    ])
    .env_remove("T3_BEARER_TOKEN")
    .env_remove("T3_MCP_TOKEN")
    .kill_on_drop(true);

    #[cfg(unix)]
    // SAFETY: the pre-exec callback only invokes the async-signal-safe setpgid syscall.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub(crate) async fn version(mut cmd: Command) -> Result<String, String> {
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|err| err.to_string())?;
    #[cfg(unix)]
    let mut group = GroupCleanup(child.id());
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let wait = async {
        #[cfg(unix)]
        if let Some(pid) = group.0 {
            let result = wait_for_exit(pid).await;
            if already_reaped(&result) {
                group.0 = None;
            }
            result?;
            group.kill();
        }
        child.wait().await
    };
    let (status, stdout, stderr) = tokio::time::timeout(VERSION_TIMEOUT, async {
        tokio::try_join!(wait, read_tail(stdout), read_tail(stderr))
    })
    .await
    .map_err(|_| "version probe timed out".to_string())?
    .map_err(|err| err.to_string())?;
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    if !status.success() {
        return Err(exit_message(status, stderr.trim(), None));
    }
    Ok(if stdout.trim().is_empty() {
        stderr.trim().to_owned()
    } else {
        stdout.trim().to_owned()
    })
}

struct RunGuard<'a> {
    active: &'a ActiveRunMap,
    run_id: Uuid,
    interrupted: Arc<AtomicBool>,
    #[cfg(unix)]
    released: watch::Sender<bool>,
}

impl RunGuard<'_> {
    fn finish(&self, #[cfg_attr(not(unix), allow(unused_variables))] kill_group: bool) {
        let mut runs = self.active.lock().unwrap();
        if runs
            .get(&self.run_id)
            .is_some_and(|run| Arc::ptr_eq(&run.interrupted, &self.interrupted))
        {
            let run = runs.remove(&self.run_id).expect("matched active run");
            #[cfg(unix)]
            {
                if kill_group {
                    GroupCleanup(Some(run.pid)).kill();
                }
                self.released.send_replace(true);
            }
            #[cfg(not(unix))]
            drop(run);
        }
    }
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        self.finish(true);
    }
}

#[cfg(unix)]
struct GroupCleanup(Option<u32>);

#[cfg(unix)]
impl GroupCleanup {
    fn kill(&mut self) {
        if let Some(pid) = self.0.take()
            && let Err(error) = signal_group(pid, libc::SIGKILL)
        {
            tracing::warn!(pid, %error, "could not terminate CLI process group");
        }
    }
}

#[cfg(unix)]
impl Drop for GroupCleanup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(unix)]
fn checked_pid(pid: u32) -> io::Result<libc::pid_t> {
    if pid <= 1 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid child PID",
        ));
    }
    Ok(pid as libc::pid_t)
}

#[cfg(unix)]
fn classify_group_signal_error(pid: libc::pid_t, signal: i32, error: io::Error) -> io::Result<()> {
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        Some(libc::EPERM) if signal != 0 => {
            // macOS XNU can return EPERM instead of ESRCH when signaling a process group
            // whose leader or members are exiting/zombies during teardown.
            tracing::debug!(
                pid,
                signal,
                %error,
                "ignoring EPERM during CLI process group teardown"
            );
            Ok(())
        }
        _ => Err(error),
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) -> io::Result<()> {
    let pid = checked_pid(pid)?;
    // SAFETY: pre_exec puts the child in its own group and the leader is still unreaped.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    classify_group_signal_error(pid, signal, io::Error::last_os_error())
}

#[cfg(unix)]
fn already_reaped(result: &io::Result<()>) -> bool {
    result
        .as_ref()
        .is_err_and(|error| error.raw_os_error() == Some(libc::ECHILD))
}

#[cfg(unix)]
async fn wait_for_exit(pid: u32) -> io::Result<()> {
    let mut exited = signal(SignalKind::child())?;
    let pid = checked_pid(pid)?;
    loop {
        let exited_already = {
            // SAFETY: waitid writes into siginfo. WNOWAIT preserves the PID until
            // cleanup has signaled the group and explicitly reaped its leader.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if result == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: waitid supplied SIGCHLD information or left the struct zeroed.
            unsafe { info.si_pid() != 0 }
        };
        if exited_already {
            return Ok(());
        }
        exited.recv().await;
    }
}

#[cfg(unix)]
pub(crate) async fn interrupt(active: &ActiveRunMap, run_id: Uuid) -> CoreResult<()> {
    let mut released = {
        let mut runs = active.lock().unwrap();
        let Some(run) = runs.get_mut(&run_id) else {
            return Ok(());
        };
        if !run.interrupted.swap(true, Ordering::AcqRel) {
            signal_group(run.pid, libc::SIGTERM).map_err(process_error)?;
            run.stdin_tx = None;
        }
        run.released.clone()
    };
    if tokio::time::timeout(Duration::from_millis(1500), released.wait_for(|done| *done))
        .await
        .is_err()
    {
        let runs = active.lock().unwrap();
        if let Some(run) = runs.get(&run_id)
            && run.released.same_channel(&released)
        {
            signal_group(run.pid, libc::SIGKILL).map_err(process_error)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn interrupt(active: &ActiveRunMap, run_id: Uuid) -> CoreResult<()> {
    if let Some(run) = active.lock().unwrap().remove(&run_id) {
        run.interrupted.store(true, Ordering::Release);
        let _ = run.stop.send(());
    }
    Ok(())
}

pub(crate) fn send_steer(active: &ActiveRunMap, run_id: Uuid, message: String) -> bool {
    send_stdin(active, run_id, StdinMsg::Steer(message))
}

/// Answers a `can_use_tool` control request by writing a `control_response` line to stdin.
pub(crate) fn send_control_response(
    active: &ActiveRunMap,
    run_id: Uuid,
    resp: &PendingResponse,
) -> bool {
    let Some(request) = active.lock().unwrap().get(&run_id).and_then(|run| {
        run.control_requests
            .lock()
            .unwrap()
            .remove(&resp.request_id)
    }) else {
        return false;
    };
    let response = if resp.approved {
        let updated_input = resp
            .payload
            .get("updatedInput")
            .cloned()
            .unwrap_or(request.input);
        json!({"behavior": "allow", "updatedInput": updated_input})
    } else {
        json!({"behavior": "deny", "message": "User denied tool execution"})
    };
    let line = json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request.provider_request_id,
            "response": response,
        }
    });
    send_stdin(active, run_id, StdinMsg::RawLine(line.to_string()))
}

fn send_stdin(active: &ActiveRunMap, run_id: Uuid, msg: StdinMsg) -> bool {
    active
        .lock()
        .unwrap()
        .get(&run_id)
        .and_then(|run| run.stdin_tx.as_ref())
        .is_some_and(|tx| tx.send(msg).is_ok())
}

fn parse_idle_timeout(val: Option<&str>) -> Duration {
    val.and_then(|value| value.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.clamp(MIN_IDLE_TIMEOUT_SECS, MAX_IDLE_TIMEOUT_SECS)))
        .unwrap_or(DEFAULT_IDLE_STDOUT_TIMEOUT)
}

fn configured_idle_timeout() -> Duration {
    let raw = std::env::var(IDLE_TIMEOUT_ENV)
        .ok()
        .or_else(|| std::env::var(LEGACY_IDLE_STDOUT_TIMEOUT_ENV).ok());
    parse_idle_timeout(raw.as_deref())
}

pub(crate) async fn run_turn(
    active: &ActiveRunMap,
    run_id: Uuid,
    provider: ProviderId,
    cmd: Command,
    input: Input,
    secret: Option<&str>,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> CoreResult<()> {
    run_turn_with_idle_timeout(
        active,
        run_id,
        provider,
        cmd,
        input,
        secret,
        tx,
        configured_idle_timeout(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_with_idle_timeout(
    active: &ActiveRunMap,
    run_id: Uuid,
    provider: ProviderId,
    mut cmd: Command,
    input: Input,
    secret: Option<&str>,
    tx: mpsc::UnboundedSender<StreamEvent>,
    idle_timeout: Duration,
) -> CoreResult<()> {
    cmd.stdin(if matches!(input, Input::None) {
        Stdio::null()
    } else {
        Stdio::piped()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
    let control_requests = ControlRequests::default();
    let interrupted = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    let (released, release_rx) = watch::channel(false);
    #[cfg(not(unix))]
    let (stop, stop_rx) = oneshot::channel();
    let (mut child, guard) = {
        let mut runs = active.lock().unwrap();
        if runs.contains_key(&run_id) {
            return Err(CoreError::Provider(format!(
                "run {run_id} is already active"
            )));
        }
        let child = cmd.spawn().map_err(|err| {
            CoreError::Provider(format!(
                "could not start {}: {err}",
                provider.display_name()
            ))
        })?;
        runs.insert(
            run_id,
            ActiveProcess {
                #[cfg(unix)]
                pid: child.id().expect("newly spawned child"),
                #[cfg(unix)]
                released: release_rx,
                stdin_tx: matches!(input, Input::StreamJson(_)).then_some(steer_tx),
                control_requests: Arc::clone(&control_requests),
                interrupted: Arc::clone(&interrupted),
                #[cfg(not(unix))]
                stop,
            },
        );
        (
            child,
            RunGuard {
                active,
                run_id,
                interrupted: Arc::clone(&interrupted),
                #[cfg(unix)]
                released,
            },
        )
    };

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdin = child.stdin.take();
    let (close_tx, close_rx) = oneshot::channel();
    let (reader_stop_tx, reader_stop_rx) = oneshot::channel();
    let collect = async {
        let wait = async {
            #[cfg(not(unix))]
            let status = tokio::select! {
                status = child.wait() => status,
                _ = stop_rx => {
                    child.start_kill()?;
                    child.wait().await
                }
                Ok(()) = reader_stop_rx => {
                    child.start_kill()?;
                    child.wait().await
                }
            }?;
            #[cfg(unix)]
            {
                let pid = child.id().expect("unreaped child");
                let mut killed_group = false;
                let result = tokio::select! {
                    result = wait_for_exit(pid) => result,
                    Ok(()) = reader_stop_rx => {
                        GroupCleanup(Some(pid)).kill();
                        killed_group = true;
                        wait_for_exit(pid).await
                    }
                };
                if result.is_ok() || already_reaped(&result) {
                    // Clean up descendants holding pipes before reaping the leader; skip
                    // a second kill when reader_stop_rx already terminated the group.
                    guard.finish(result.is_ok() && !killed_group);
                }
                result?;
                child.wait().await
            }
            #[cfg(not(unix))]
            {
                guard.finish(false);
                Ok(status)
            }
        };
        tokio::try_join!(
            wait,
            read_events(
                stdout,
                provider,
                secret,
                &tx,
                &control_requests,
                close_tx,
                reader_stop_tx,
                idle_timeout,
            ),
            read_tail(stderr),
            write_input(stdin, input, steer_rx, close_rx),
        )
    };
    let (status, terminal, stderr, ()) = tokio::select! {
        _ = tx.closed() => return Ok(()),
        result = collect => result.map_err(process_error)?,
    };

    let terminal = match terminal {
        ReadOutcome::Terminal(terminal) => terminal,
        ReadOutcome::Stopped(message) => {
            let _ = tx.send(StreamEvent::Failed {
                message: message.into(),
            });
            return Ok(());
        }
    };
    let event = if interrupted.load(Ordering::Acquire) {
        StreamEvent::Failed {
            message: "interrupted".into(),
        }
    } else if matches!(terminal, Some(StreamEvent::RateLimited { .. })) {
        terminal.expect("rate-limit event")
    } else if !status.success() {
        let stderr = redact_stderr(&String::from_utf8_lossy(&stderr), secret);
        let detail = match &terminal {
            Some(StreamEvent::Failed { message }) => Some(message.as_str()),
            _ => None,
        };
        provider_error(exit_message(status, stderr.trim(), detail))
    } else {
        terminal.unwrap_or_else(|| StreamEvent::Failed {
            message: format!("{} exited without a turn result", provider.display_name()),
        })
    };
    let _ = tx.send(event);
    Ok(())
}

fn exit_message(status: std::process::ExitStatus, stderr: &str, detail: Option<&str>) -> String {
    let mut message = format!("provider exited with {status}");
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        message.push_str(": ");
        message.push_str(detail);
    }
    if !stderr.is_empty() && detail != Some(stderr) {
        message.push_str(if detail.is_some() { "\nstderr: " } else { ": " });
        message.push_str(stderr);
    }
    message
}

fn process_error(error: io::Error) -> CoreError {
    CoreError::Provider(format!("CLI process: {error}"))
}

fn redact_stderr(text: &str, secret: Option<&str>) -> String {
    let Some(secret) = secret.filter(|secret| !secret.is_empty()) else {
        return text.to_owned();
    };
    let encoded = json!(secret).to_string();
    text.replace(&encoded[1..encoded.len() - 1], "[redacted]")
        .replace(secret, "[redacted]")
}

fn redact_value(value: &mut serde_json::Value, secret: &str) {
    match value {
        serde_json::Value::String(text) if text.contains(secret) => {
            *text = text.replace(secret, "[redacted]");
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_value(value, secret);
            }
        }
        serde_json::Value::Object(map) => {
            if map.keys().any(|key| key.contains(secret)) {
                *map = std::mem::take(map)
                    .into_iter()
                    .map(|(key, value)| (key.replace(secret, "[redacted]"), value))
                    .collect();
            }
            for value in map.values_mut() {
                redact_value(value, secret);
            }
        }
        _ => {}
    }
}

enum ReadOutcome {
    Terminal(Option<StreamEvent>),
    Stopped(&'static str),
}

#[allow(clippy::too_many_arguments)]
async fn read_events(
    stdout: impl AsyncRead + Unpin,
    provider: ProviderId,
    secret: Option<&str>,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    control_requests: &ControlRequests,
    close_stdin: oneshot::Sender<()>,
    stop_process: oneshot::Sender<()>,
    idle_timeout: Duration,
) -> io::Result<ReadOutcome> {
    let mut lines = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut parser = Parser::new(provider);
    let mut terminal = None;
    let mut close_stdin = Some(close_stdin);
    let mut assistant = RepetitionWatchdog::default();
    let mut reasoning = RepetitionWatchdog::default();
    let mut tool_loop = ToolLoopWatchdog::default();
    // Stop reading after one extra byte, even when the producer never sends a newline.
    loop {
        let bytes = match tokio::time::timeout(
            idle_timeout,
            (&mut lines)
                .take((STDOUT_LINE_LIMIT + 1) as u64)
                .read_until(b'\n', &mut line),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                let _ = stop_process.send(());
                if terminal.is_some() {
                    return Ok(ReadOutcome::Terminal(terminal));
                }
                return Ok(ReadOutcome::Stopped(
                    "provider produced no stdout output before the idle timeout",
                ));
            }
        };
        if bytes == 0 {
            break;
        }
        if line.len() > STDOUT_LINE_LIMIT && line.last() != Some(&b'\n') {
            let _ = stop_process.send(());
            return Ok(ReadOutcome::Stopped(
                "provider stdout line exceeds the 4 MiB limit",
            ));
        }
        if let Ok(mut value) = serde_json::from_slice(&line) {
            if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
                redact_value(&mut value, secret);
            }
            for event in parser.parse(&value) {
                if let StreamEvent::PendingApproval { request } = &event
                    && let Some(provider_request_id) =
                        request.payload["_provider_request_id"].as_str()
                {
                    control_requests.lock().unwrap().insert(
                        request.id,
                        ControlRequest {
                            provider_request_id: provider_request_id.to_owned(),
                            input: request.payload["input"].clone(),
                        },
                    );
                }
                if !matches!(terminal, Some(StreamEvent::Completed { .. })) {
                    let stop_reason = match &event {
                        StreamEvent::AssistantDelta { text } if assistant.push(text) => Some(
                            "repetition loop detected — terminated runaway provider process to save tokens/battery",
                        ),
                        StreamEvent::ReasoningDelta { text } if reasoning.push(text) => Some(
                            "repetition loop detected — terminated runaway provider process to save tokens/battery",
                        ),
                        StreamEvent::Item {
                            kind,
                            summary,
                            payload,
                        } if tool_loop.push(*kind, summary.as_deref(), payload) => Some(
                            "tool call repetition loop detected — terminated runaway provider process to save tokens/battery",
                        ),
                        _ => None,
                    };
                    if let Some(reason) = stop_reason {
                        let _ = stop_process.send(());
                        return Ok(ReadOutcome::Stopped(reason));
                    }
                }
                match event {
                    StreamEvent::Completed { .. }
                    | StreamEvent::Failed { .. }
                    | StreamEvent::RateLimited { .. } => {
                        // Keep rate-limit metadata if the provider also emits a generic failure.
                        if !matches!(terminal, Some(StreamEvent::RateLimited { .. })) {
                            terminal = Some(event);
                        }
                        if let Some(close) = close_stdin.take() {
                            let _ = close.send(());
                        }
                    }
                    _ => {
                        let _ = tx.send(event);
                    }
                }
            }
        }
        line.clear();
    }
    Ok(ReadOutcome::Terminal(terminal.or_else(|| parser.finish())))
}

async fn read_tail(mut input: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut tail = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let size = input.read(&mut buf).await?;
        if size == 0 {
            return Ok(tail);
        }
        let excess = (tail.len() + size).saturating_sub(STDERR_LIMIT);
        tail.drain(..excess);
        tail.extend_from_slice(&buf[..size]);
    }
}

fn user_line(text: &str) -> String {
    let mut line =
        json!({"type": "user", "message": {"role": "user", "content": text}}).to_string();
    line.push('\n');
    line
}

async fn write_input(
    stdin: Option<ChildStdin>,
    input: Input,
    mut steer: mpsc::UnboundedReceiver<StdinMsg>,
    close: oneshot::Receiver<()>,
) -> io::Result<()> {
    let Some(mut stdin) = stdin else {
        return Ok(());
    };
    let write = async {
        match input {
            Input::StreamJson(prompt) => {
                stdin.write_all(user_line(&prompt).as_bytes()).await?;
                while let Some(msg) = steer.recv().await {
                    let line = match msg {
                        StdinMsg::Steer(text) => user_line(&text),
                        StdinMsg::RawLine(mut line) => {
                            line.push('\n');
                            line
                        }
                    };
                    stdin.write_all(line.as_bytes()).await?;
                }
            }
            Input::Text(prompt) => stdin.write_all(prompt.as_bytes()).await?,
            Input::None => {}
        }
        stdin.shutdown().await
    };
    match tokio::select! {
        result = write => result,
        _ = close => Ok(()),
    } {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

pub(crate) fn command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    prepare(&mut cmd);
    cmd
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{AntigravityDriver, ClaudeDriver, CodexDriver, OpenCodeDriver, test_request};
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
    use t3_core::{
        traits::ProviderDriver,
        types::{McpInjectionSpec, PendingResponse},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(script: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("t3-provider-fixture-{}", Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            let file = dir.join("cli");
            fs::write(&file, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
            Self(dir)
        }

        fn binary(&self) -> PathBuf {
            self.0.join("cli")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn receive(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> Option<StreamEvent> {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn streaming_providers_keep_stdin_open_for_steering_then_close_it() {
        let fixture = Fixture::new(
            r#"
check_privacy() {
    [ "$DO_NOT_TRACK:$AGY_DISABLE_TELEMETRY:$DISABLE_TELEMETRY:$CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC" = '1:1:1:1' ]
    [ "$T3CODE_TELEMETRY_ENABLED" = false ]
    [ "$T3CODE_OTEL_SDK_DISABLED" = true ]
}
check_privacy
if [ "$1" = --version ]; then
    printf 'fixture 1.0\n'
    exit
fi
IFS= read -r first
[ "$first" = '{"message":{"content":"hello","role":"user"},"type":"user"}' ]
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-stream","conversation_id":"fake-stream"}'
IFS= read -r next
[ "$next" = '{"message":{"content":"follow up","role":"user"},"type":"user"}' ]
printf '%s\n' '{"type":"result","result":"ok"}'
if IFS= read -r unexpected; then
    exit 17
fi
"#,
        );
        for driver in [Arc::new(ClaudeDriver::new(fixture.binary())) as Arc<dyn ProviderDriver>] {
            let health = driver.health_check().await;
            assert!(health.available, "{:?}", health.error);
            assert_eq!(health.version.as_deref(), Some("fixture 1.0"));
            let req = test_request(driver.provider_id());
            let id = req.run_id;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let running = Arc::clone(&driver);
            let task = tokio::spawn(async move { running.run_turn(req, tx).await });
            assert!(
                matches!(receive(&mut rx).await, Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "fake-stream")
            );
            assert!(driver.send_steer(id, "follow up".into()).await.unwrap());
            assert!(
                matches!(receive(&mut rx).await, Some(StreamEvent::Completed { final_text: Some(text) }) if text == "ok")
            );
            task.await.unwrap().unwrap();
            assert!(!driver.send_steer(id, "late".into()).await.unwrap());
        }
    }

    #[tokio::test]
    async fn antigravity_passes_prompt_via_print_and_parses_event_stream() {
        let fixture = Fixture::new(
            r#"
last=""
prev=""
for arg in "$@"; do
    prev="$last"
    last="$arg"
done
[ "$prev" = --print ]
[ "$last" = hello ]
printf '%s\n' \
  '{"event":"init","conversation_id":"fake-agy"}' \
  '{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"OK"}}' \
  '{"event":"result","result":{"conversation_id":"fake-agy","status":"SUCCESS","response":"OK\n","usage":{"input_tokens":5,"output_tokens":2}}}'
"#,
        );
        let driver = AntigravityDriver::new(fixture.binary());
        let req = test_request(ProviderId::Antigravity);
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(Duration::from_secs(5), driver.run_turn(req, tx))
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(receive(&mut rx).await, Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "fake-agy")
        );
        assert!(
            matches!(receive(&mut rx).await, Some(StreamEvent::AssistantDelta { text }) if text == "OK")
        );
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Usage {
                input_tokens: 5,
                output_tokens: 2
            })
        ));
        assert!(
            matches!(receive(&mut rx).await, Some(StreamEvent::Completed { final_text: Some(t) }) if t == "OK\n")
        );
    }

    #[tokio::test]
    async fn codex_receives_plain_text_and_eof() {
        let fixture = Fixture::new(
            r#"
[ "$1" = exec ]
prompt=$(/bin/cat)
[ "$prompt" = 'line one
line two' ]
printf '%s\n' '{"type":"thread.started","thread_id":"fake-codex"}' '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}'
"#,
        );
        let driver = CodexDriver::new(fixture.binary());
        let mut req = test_request(ProviderId::Codex);
        req.prompt = "line one\nline two".into();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(Duration::from_secs(5), driver.run_turn(req, tx))
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(receive(&mut rx).await, Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "fake-codex")
        );
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Usage {
                input_tokens: 1,
                output_tokens: 2
            })
        ));
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Completed { .. })
        ));
        assert!(receive(&mut rx).await.is_none());
    }

    #[tokio::test]
    async fn opencode_reads_no_stdin_and_finishes_on_exit() {
        let fixture = Fixture::new(
            r#"
[ "$1" = run ]
if IFS= read -r unexpected; then exit 17; fi
printf '%s\n' '{"type":"step_start","sessionID":"fake-opencode"}' '{"type":"step_finish","part":{"reason":"stop","tokens":{"input":3,"output":4}}}'
"#,
        );
        let driver = OpenCodeDriver::new(fixture.binary());
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            driver.run_turn(test_request(ProviderId::OpenCode), tx),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(receive(&mut rx).await, Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "fake-opencode")
        );
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Usage {
                input_tokens: 3,
                output_tokens: 4
            })
        ));
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Completed { .. })
        ));
    }

    #[tokio::test]
    async fn nonzero_exit_returns_actionable_stderr_and_redacts_mcp_token() {
        let fixture = Fixture::new(
            r#"
printf '%s\n' '{"type":"turn.failed","error":{"message":"tool failed: fixture-token"}}'
printf '%s\n' 'fixture-token: stderr diagnostic' >&2
exit 7
"#,
        );
        let driver = CodexDriver::new(fixture.binary());
        let mut req = test_request(ProviderId::Codex);
        req.mcp_injection = Some(McpInjectionSpec {
            binary_path: "/fixture/t3d".into(),
            server_url: "http://127.0.0.1:3780".into(),
            internal_token: "fixture-token".into(),
            thread_id: req.thread_id,
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        driver.run_turn(req, tx).await.unwrap();
        let Some(StreamEvent::Failed { message }) = receive(&mut rx).await else {
            panic!("expected one terminal failure");
        };
        assert!(message.contains("exit status: 7"));
        assert!(message.contains("tool failed: [redacted]"));
        assert!(message.contains("[redacted]: stderr diagnostic"));
        assert!(!message.contains("fixture-token"));
        assert!(receive(&mut rx).await.is_none());
    }

    async fn assert_processes_exited(pid: u32, grandchild: i32) {
        let pids = format!("{pid},{grandchild}");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let output = command(Path::new("ps"))
                    .args(["-o", "stat=", "-p", &pids])
                    .output()
                    .await
                    .unwrap();
                // A killed descendant can briefly remain as a zombie until its new parent reaps it.
                if output.stderr.is_empty()
                    && String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .all(|status| status.trim_start().starts_with('Z'))
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("a process is still running after group cleanup");
    }

    async fn check_group_cleanup(interrupt_turn: bool) {
        let fixture = Fixture::new(
            r#"
trap '' TERM
/bin/sleep 60 &
printf '{"type":"system","session_id":"%s:%s"}\n' "$$" "$!"
wait
"#,
        );
        let driver = ClaudeDriver::new(fixture.binary());
        let req = test_request(ProviderId::Claude);
        let id = req.run_id;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let running = driver.clone();
        let task = tokio::spawn(async move { running.run_turn(req, tx).await });
        let Some(StreamEvent::SessionStarted {
            provider_session_id,
        }) = receive(&mut rx).await
        else {
            panic!("fixture did not start");
        };
        let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
        let pid = pid.parse().unwrap();
        let grandchild = grandchild.parse().unwrap();
        // SAFETY: getpgid only queries process metadata.
        assert_eq!(unsafe { libc::getpgid(grandchild) }, pid as i32);
        if interrupt_turn {
            driver.interrupt(id).await.unwrap();
            let event = receive(&mut rx).await;
            assert!(
                matches!(event, Some(StreamEvent::Failed { message }) if message == "interrupted")
            );
            task.await.unwrap().unwrap();
        } else {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert!(!driver.send_steer(id, "late".into()).await.unwrap());
        assert_processes_exited(pid, grandchild).await;
    }

    #[tokio::test]
    async fn interrupt_escalates_to_kill_for_the_entire_group() {
        check_group_cleanup(true).await;
    }

    #[tokio::test]
    async fn dropping_the_turn_kills_the_entire_group() {
        check_group_cleanup(false).await;
    }

    #[test]
    fn group_signals_reject_invalid_pids() {
        for pid in [0, 1, i32::MAX as u32 + 1, u32::MAX] {
            assert_eq!(
                signal_group(pid, 0).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[tokio::test]
    async fn provider_exit_releases_descendants_that_keep_pipes_open() {
        let fixture = Fixture::new(
            r#"
/bin/sleep 60 &
printf '{"type":"system","session_id":"%s:%s"}\n' "$$" "$!"
printf '{"type":"result","subtype":"success","result":"finished"}\n'
exit 0
"#,
        );
        let driver = ClaudeDriver::new(fixture.binary());
        let req = test_request(ProviderId::Claude);
        let run_id = req.run_id;
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(Duration::from_secs(5), driver.run_turn(req, tx))
            .await
            .expect("a descendant held the provider pipes open")
            .unwrap();
        let Some(StreamEvent::SessionStarted {
            provider_session_id,
        }) = receive(&mut rx).await
        else {
            panic!("provider failed to start");
        };
        let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
        assert_processes_exited(pid.parse().unwrap(), grandchild.parse().unwrap()).await;
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Completed { .. })
        ));
        driver.interrupt(run_id).await.unwrap();
    }

    #[tokio::test]
    async fn repetitive_stream_terminates_the_provider_group() {
        let line = "The model repeats the same line indefinitely.\n";
        let phrase = "We still need to inspect the exact same file again. ";
        for event in [
            json!({
                "type": "stream_event", "event": {"type": "content_block_delta",
                    "delta": {"type": "text_delta", "text": line.repeat(8)}}
            }),
            json!({
                "type": "stream_event", "event": {"type": "content_block_delta",
                    "delta": {"type": "thinking_delta", "thinking": phrase.repeat(6)}}
            }),
        ] {
            let fixture = Fixture::new(&format!(
                r#"
/bin/sleep 60 &
printf '{{"type":"system","session_id":"%s:%s"}}\n' "$$" "$!"
printf '%s\n' '{event}'
wait
"#
            ));
            let driver = ClaudeDriver::new(fixture.binary());
            let req = test_request(ProviderId::Claude);
            let id = req.run_id;
            let (tx, mut rx) = mpsc::unbounded_channel();
            tokio::time::timeout(Duration::from_secs(2), driver.run_turn(req, tx))
                .await
                .expect("repetition should stop the provider before it exits")
                .unwrap();
            let Some(StreamEvent::SessionStarted {
                provider_session_id,
            }) = receive(&mut rx).await
            else {
                panic!("fixture did not start");
            };
            let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
            assert_processes_exited(pid.parse().unwrap(), grandchild.parse().unwrap()).await;
            assert!(matches!(
                receive(&mut rx).await,
                Some(StreamEvent::Failed { message }) if message == "repetition loop detected — terminated runaway provider process to save tokens/battery"
            ));
            assert!(receive(&mut rx).await.is_none());
            assert!(!driver.send_steer(id, "late".into()).await.unwrap());
        }
    }

    #[tokio::test]
    async fn completed_response_is_not_tested_for_repetition() {
        let answer = "The final answer intentionally repeats this same example.\n".repeat(10);
        let event = json!({"type": "result", "subtype": "success", "result": answer});
        let fixture = Fixture::new(&format!("printf '%s\\n' '{event}'"));
        let driver = ClaudeDriver::new(fixture.binary());
        let (tx, mut rx) = mpsc::unbounded_channel();
        driver
            .run_turn(test_request(ProviderId::Claude), tx)
            .await
            .unwrap();
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Completed { final_text: Some(text) }) if text == answer
        ));
    }

    #[tokio::test]
    async fn oversized_unterminated_stdout_line_fails_the_turn() {
        let fixture = Fixture::new(
            r#"
/usr/bin/head -c 4194305 /dev/zero
/bin/sleep 60
"#,
        );
        let driver = ClaudeDriver::new(fixture.binary());
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(2),
            driver.run_turn(test_request(ProviderId::Claude), tx),
        )
        .await
        .expect("stdout must be capped before the line ends")
        .unwrap();
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Failed { message }) if message.contains("4 MiB")
        ));
    }

    #[tokio::test]
    async fn stdout_accepts_a_line_at_the_limit_and_an_unterminated_final_line() {
        let mut input = br#"{"type":"system","session_id":"boundary"}"#.to_vec();
        input.resize(4 * 1024 * 1024, b' ');
        input.push(b'\n');
        input.extend_from_slice(br#"{"type":"result","subtype":"success","result":"ok"}"#);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (close, _) = oneshot::channel();
        let (stop, _) = oneshot::channel();
        let outcome = read_events(
            input.as_slice(),
            ProviderId::Claude,
            None,
            &tx,
            &ControlRequests::default(),
            close,
            stop,
            DEFAULT_IDLE_STDOUT_TIMEOUT,
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            ReadOutcome::Terminal(Some(StreamEvent::Completed { .. }))
        ));
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "boundary"
        ));
    }

    #[test]
    fn group_signal_treats_teardown_eperm_and_esrch_as_benign() {
        assert!(
            classify_group_signal_error(
                12345,
                libc::SIGKILL,
                io::Error::from_raw_os_error(libc::EPERM),
            )
            .is_ok()
        );
        assert!(
            classify_group_signal_error(
                12345,
                libc::SIGTERM,
                io::Error::from_raw_os_error(libc::EPERM),
            )
            .is_ok()
        );
        assert!(
            classify_group_signal_error(
                12345,
                libc::SIGKILL,
                io::Error::from_raw_os_error(libc::ESRCH),
            )
            .is_ok()
        );
        assert_eq!(
            classify_group_signal_error(12345, 0, io::Error::from_raw_os_error(libc::EPERM))
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            classify_group_signal_error(
                12345,
                libc::SIGKILL,
                io::Error::from_raw_os_error(libc::EINVAL),
            )
            .unwrap_err()
            .raw_os_error(),
            Some(libc::EINVAL)
        );
    }

    #[tokio::test]
    async fn run_guard_finish_false_skips_second_group_kill() {
        let mut cmd = command(Path::new("/bin/sleep"));
        cmd.arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let pid = child.id().unwrap();
        let active = ActiveRunMap::default();
        let run_id = Uuid::new_v4();
        let interrupted = Arc::new(AtomicBool::new(false));
        let (released, release_rx) = watch::channel(false);
        active.lock().unwrap().insert(
            run_id,
            ActiveProcess {
                pid,
                released: release_rx,
                stdin_tx: None,
                control_requests: ControlRequests::default(),
                interrupted: Arc::clone(&interrupted),
            },
        );
        let guard = RunGuard {
            active: &active,
            run_id,
            interrupted,
            released,
        };

        // Calling finish(false) removes the active entry and marks released without killing.
        guard.finish(false);
        assert!(active.lock().unwrap().is_empty());
        assert!(*guard.released.borrow());
        // Child should still be alive because finish(false) did not call GroupCleanup::kill().
        assert!(child.try_wait().unwrap().is_none());

        // Dropping guard after finish(false) must also be a no-op; child stays alive until killed.
        drop(guard);
        assert!(child.try_wait().unwrap().is_none());

        GroupCleanup(Some(pid)).kill();
        wait_for_exit(pid).await.unwrap();
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn repeated_tool_calls_terminate_the_provider_group() {
        let mut script = String::from(
            "/bin/sleep 60 &\nprintf '{\"type\":\"system\",\"session_id\":\"%s:%s\"}\\n' \"$$\" \"$!\"\n",
        );
        for idx in 0..8 {
            let call = json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "tool_use",
                        "id": format!("tool-{idx}"),
                        "name": "Bash",
                        "input": {"command": "cargo test"}
                    }]
                }
            });
            let result = json!({
                "type": "user",
                "message": {
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": format!("tool-{idx}"),
                        "content": "failed"
                    }]
                }
            });
            script.push_str(&format!("printf '%s\\n' '{call}'\n"));
            script.push_str(&format!("printf '%s\\n' '{result}'\n"));
        }
        script.push_str("wait\n");

        let fixture = Fixture::new(&script);
        let driver = ClaudeDriver::new(fixture.binary());
        let req = test_request(ProviderId::Claude);
        let id = req.run_id;
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(Duration::from_secs(2), driver.run_turn(req, tx))
            .await
            .expect("tool loop should stop the provider before it exits")
            .unwrap();
        let Some(StreamEvent::SessionStarted {
            provider_session_id,
        }) = receive(&mut rx).await
        else {
            panic!("fixture did not start");
        };
        let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
        assert_processes_exited(pid.parse().unwrap(), grandchild.parse().unwrap()).await;

        let mut failed_message = None;
        while let Some(event) = receive(&mut rx).await {
            if let StreamEvent::Failed { message } = event {
                failed_message = Some(message);
            }
        }
        assert_eq!(
            failed_message.as_deref(),
            Some(
                "tool call repetition loop detected — terminated runaway provider process to save tokens/battery"
            )
        );
        assert!(!driver.send_steer(id, "late".into()).await.unwrap());
    }

    #[tokio::test]
    async fn idle_stdout_timeout_terminates_hung_cli_and_descendants() {
        let fixture = Fixture::new(
            r#"
/bin/sleep 60 &
printf '{"type":"system","session_id":"%s:%s"}\n' "$$" "$!"
wait
"#,
        );
        let active = ActiveRunMap::default();
        let run_id = Uuid::new_v4();
        let mut cmd = command(&fixture.binary());
        cmd.arg("--print").arg("hello");
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(3),
            run_turn_with_idle_timeout(
                &active,
                run_id,
                ProviderId::Claude,
                cmd,
                Input::None,
                None,
                tx,
                Duration::from_millis(350),
            ),
        )
        .await
        .expect("idle timeout must terminate hung CLI")
        .unwrap();

        let Some(StreamEvent::SessionStarted {
            provider_session_id,
        }) = receive(&mut rx).await
        else {
            panic!("fixture did not start");
        };
        let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
        assert_processes_exited(pid.parse().unwrap(), grandchild.parse().unwrap()).await;
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Failed { message }) if message.contains("idle timeout")
        ));
        assert!(receive(&mut rx).await.is_none());
        assert!(active.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn idle_stdout_timeout_resets_when_events_stream() {
        let fixture = Fixture::new(
            r#"
printf '{"type":"system","session_id":"active-stream"}\n'
for i in 1 2 3 4 5; do
    /bin/sleep 0.15
    printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"tick "}}}\n'
done
printf '{"type":"result","subtype":"success","result":"done"}\n'
"#,
        );
        let active = ActiveRunMap::default();
        let run_id = Uuid::new_v4();
        let cmd = command(&fixture.binary());
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            run_turn_with_idle_timeout(
                &active,
                run_id,
                ProviderId::Claude,
                cmd,
                Input::None,
                None,
                tx,
                Duration::from_millis(600),
            ),
        )
        .await
        .expect("active stream should complete")
        .unwrap();

        let mut completed = false;
        while let Some(event) = receive(&mut rx).await {
            if matches!(event, StreamEvent::Completed { final_text: Some(ref text) } if text == "done")
            {
                completed = true;
            }
        }
        assert!(completed);
    }

    /// Runs a Claude turn whose CLI asks permission to run `ls -la`, answers it through
    /// `respond_request`, and returns the single line the CLI read back from stdin.
    async fn answer_control_request(approved: bool, payload: Value) -> Value {
        let fixture = Fixture::new(
            r#"
IFS= read -r first
printf '%s\n' '{"type":"control_request","request_id":"cli-req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls -la"}}}'
IFS= read -r answer
printf '%s\n' "$answer" > "$(dirname "$0")/answer.json"
printf '%s\n' '{"type":"result","result":"ok"}'
if IFS= read -r unexpected; then
    exit 17
fi
"#,
        );
        let driver = ClaudeDriver::new(fixture.binary());
        let req = test_request(ProviderId::Claude);
        let run_id = req.run_id;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let running = driver.clone();
        let task = tokio::spawn(async move { running.run_turn(req, tx).await });

        let Some(StreamEvent::PendingApproval { request }) = receive(&mut rx).await else {
            panic!("expected a pending approval");
        };
        assert_eq!(request.title, "Bash");
        assert_eq!(request.payload["command"], "ls -la");
        let resp = PendingResponse {
            request_id: request.id,
            approved,
            payload,
        };
        assert!(driver.respond_request(run_id, resp.clone()).await.unwrap());
        // The request is consumed once answered, so a repeated answer is refused.
        assert!(!driver.respond_request(run_id, resp).await.unwrap());
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Completed { .. })
        ));
        task.await.unwrap().unwrap();

        let answer = fs::read_to_string(fixture.0.join("answer.json")).unwrap();
        serde_json::from_str(&answer).unwrap()
    }

    #[tokio::test]
    async fn approved_control_request_writes_allow_response_to_stdin() {
        let answer = answer_control_request(true, json!({})).await;
        assert_eq!(
            answer,
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "cli-req-1",
                    "response": {"behavior": "allow", "updatedInput": {"command": "ls -la"}}
                }
            })
        );
    }

    #[tokio::test]
    async fn approval_with_updated_input_allows_the_edited_call() {
        let answer = answer_control_request(true, json!({"updatedInput": {"command": "ls"}})).await;
        assert_eq!(
            answer["response"]["response"],
            json!({"behavior": "allow", "updatedInput": {"command": "ls"}})
        );
    }

    #[tokio::test]
    async fn denied_control_request_writes_deny_response_to_stdin() {
        let answer = answer_control_request(false, json!({})).await;
        assert_eq!(
            answer,
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "cli-req-1",
                    "response": {"behavior": "deny", "message": "User denied tool execution"}
                }
            })
        );
    }

    #[test]
    fn control_response_for_unknown_run_is_refused() {
        let active = ActiveRunMap::default();
        let resp = PendingResponse {
            request_id: Uuid::new_v4(),
            approved: true,
            payload: json!({}),
        };
        assert!(!send_control_response(&active, Uuid::new_v4(), &resp));
    }
}

#[cfg(test)]
mod idle_timeout_tests {
    use super::*;

    #[test]
    fn default_idle_timeout_is_fifteen_minutes() {
        assert_eq!(DEFAULT_IDLE_STDOUT_TIMEOUT, Duration::from_secs(900));
    }

    #[test]
    fn parse_idle_timeout_falls_back_when_unset_or_invalid() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("not-a-number"),
            Some("-60"),
            Some("120.5"),
            Some("18446744073709551616"),
        ] {
            assert_eq!(
                parse_idle_timeout(raw),
                DEFAULT_IDLE_STDOUT_TIMEOUT,
                "expected default for {raw:?}"
            );
        }
    }

    #[test]
    fn parse_idle_timeout_parses_and_clamps_range() {
        assert_eq!(parse_idle_timeout(Some("0")), Duration::from_secs(60));
        assert_eq!(parse_idle_timeout(Some("15")), Duration::from_secs(60));
        assert_eq!(parse_idle_timeout(Some("59")), Duration::from_secs(60));
        assert_eq!(parse_idle_timeout(Some("60")), Duration::from_secs(60));
        assert_eq!(parse_idle_timeout(Some("300")), Duration::from_secs(300));
        assert_eq!(
            parse_idle_timeout(Some(" 1200 \n")),
            Duration::from_secs(1200)
        );
        assert_eq!(parse_idle_timeout(Some("3600")), Duration::from_secs(3600));
        assert_eq!(parse_idle_timeout(Some("3601")), Duration::from_secs(3600));
        assert_eq!(parse_idle_timeout(Some("99999")), Duration::from_secs(3600));
    }
}
