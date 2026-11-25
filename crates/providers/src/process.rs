use crate::{
    parsers::{Parser, provider_error},
    watchdog::RepetitionWatchdog,
};
use serde_json::json;
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
    types::ProviderId,
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

pub(crate) struct ActiveProcess {
    #[cfg(unix)]
    pid: u32,
    #[cfg(unix)]
    released: watch::Receiver<bool>,
    stdin_tx: Option<mpsc::UnboundedSender<String>>,
    interrupted: Arc<AtomicBool>,
    #[cfg(not(unix))]
    stop: oneshot::Sender<()>,
}

pub(crate) type ActiveRunMap = Arc<Mutex<HashMap<Uuid, ActiveProcess>>>;

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
fn signal_group(pid: u32, signal: i32) -> io::Result<()> {
    let pid = checked_pid(pid)?;
    // SAFETY: pre_exec puts the child in its own group and the leader is still unreaped.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(error),
    }
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
    active
        .lock()
        .unwrap()
        .get(&run_id)
        .and_then(|run| run.stdin_tx.as_ref())
        .is_some_and(|tx| tx.send(message).is_ok())
}

pub(crate) async fn run_turn(
    active: &ActiveRunMap,
    run_id: Uuid,
    provider: ProviderId,
    mut cmd: Command,
    input: Input,
    secret: Option<&str>,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> CoreResult<()> {
    cmd.stdin(if matches!(input, Input::None) {
        Stdio::null()
    } else {
        Stdio::piped()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
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
                let result = tokio::select! {
                    result = wait_for_exit(pid) => result,
                    Ok(()) = reader_stop_rx => {
                        GroupCleanup(Some(pid)).kill();
                        wait_for_exit(pid).await
                    }
                };
                if result.is_ok() || already_reaped(&result) {
                    // Clean up descendants holding pipes before reaping the leader.
                    guard.finish(result.is_ok());
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
            read_events(stdout, provider, secret, &tx, close_tx, reader_stop_tx),
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

async fn read_events(
    stdout: impl AsyncRead + Unpin,
    provider: ProviderId,
    secret: Option<&str>,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    close_stdin: oneshot::Sender<()>,
    stop_process: oneshot::Sender<()>,
) -> io::Result<ReadOutcome> {
    let mut lines = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut parser = Parser::new(provider);
    let mut terminal = None;
    let mut close_stdin = Some(close_stdin);
    let mut assistant = RepetitionWatchdog::default();
    let mut reasoning = RepetitionWatchdog::default();
    // Stop reading after one extra byte, even when the producer never sends a newline.
    while (&mut lines)
        .take((STDOUT_LINE_LIMIT + 1) as u64)
        .read_until(b'\n', &mut line)
        .await?
        != 0
    {
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
                let repeated = !matches!(terminal, Some(StreamEvent::Completed { .. }))
                    && match &event {
                        StreamEvent::AssistantDelta { text } => assistant.push(text),
                        StreamEvent::ReasoningDelta { text } => reasoning.push(text),
                        _ => false,
                    };
                if repeated {
                    let _ = stop_process.send(());
                    return Ok(ReadOutcome::Stopped(
                        "repetition loop detected — terminated runaway provider process to save tokens/battery",
                    ));
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
    mut steer: mpsc::UnboundedReceiver<String>,
    close: oneshot::Receiver<()>,
) -> io::Result<()> {
    let Some(mut stdin) = stdin else {
        return Ok(());
    };
    let write = async {
        match input {
            Input::StreamJson(prompt) => {
                stdin.write_all(user_line(&prompt).as_bytes()).await?;
                while let Some(message) = steer.recv().await {
                    stdin.write_all(user_line(&message).as_bytes()).await?;
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
    use t3_core::{traits::ProviderDriver, types::McpInjectionSpec};

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
        let outcome = read_events(input.as_slice(), ProviderId::Claude, None, &tx, close, stop)
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
}
