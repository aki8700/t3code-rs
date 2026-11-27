use super::*;

#[cfg(unix)]
pub(super) fn process_start_time(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| normalize_process_start_time(&String::from_utf8_lossy(&output.stdout)))
        .filter(|start| !start.is_empty())
}

pub(super) fn normalize_process_start_time(start: &str) -> String {
    start.split_whitespace().collect::<Vec<_>>().join(" ")
}
pub(super) fn process_error(error: io::Error) -> CoreError {
    CoreError::Provider(format!("CLI process: {error}"))
}

pub(super) fn exit_message(
    status: std::process::ExitStatus,
    stderr: &str,
    detail: Option<&str>,
) -> String {
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

pub(super) fn redact_stderr(text: &str, secret: Option<&str>) -> String {
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

pub(super) enum ReadOutcome {
    Terminal(Option<StreamEvent>),
    /// t3d stopped a process that had already sent `Completed`.
    StoppedAfterCompleted(StreamEvent),
    Stopped(Cow<'static, str>),
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn read_events(
    stdout: impl AsyncRead + Unpin,
    provider: ProviderId,
    secret: Option<&str>,
    tx: &mpsc::UnboundedSender<StreamEvent>,
    steer: Option<&mpsc::UnboundedSender<StdinMsg>>,
    control_requests: &ControlRequests,
    close_stdin: oneshot::Sender<()>,
    mut stop_process: oneshot::Sender<()>,
    idle_timeout: Duration,
    tool_profile: t3_core::McpToolProfile,
) -> io::Result<ReadOutcome> {
    let mut lines = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut parser = Parser::new(provider);
    let mut terminal = None;
    let mut close_stdin = Some(close_stdin);
    let mut assistant = RepetitionWatchdog::default();
    let mut reasoning = RepetitionWatchdog::default();
    let mut tool_loop = ToolLoopWatchdog::default();
    let mut failed_tools = ToolFailureWatchdog::default();
    let mut budget = TurnBudgetWatchdog::for_profile(tool_profile);
    let mut in_flight = InFlightTools::default();
    let startup_limit = startup_timeout(idle_timeout);
    let mut saw_first_event = false;
    loop {
        let wait = if !saw_first_event {
            startup_limit.min(idle_timeout)
        } else {
            match in_flight.oldest() {
                Some(_) => idle_timeout * IN_FLIGHT_TOOL_IDLE_FACTOR,
                None => idle_timeout,
            }
        };
        // A CLI waiting for the user's approval is silent by design, so an unanswered
        // request (or one answered during this wait) re-arms the idle limit.
        let awaiting_approval = !control_requests.lock().unwrap().is_empty();
        let chunk = match tokio::time::timeout(wait, lines.fill_buf()).await {
            Ok(result) => result?,
            Err(_) if awaiting_approval || !control_requests.lock().unwrap().is_empty() => {
                continue;
            }
            Err(_) => {
                let stopped = stop_process.send(()).is_ok();
                if let Some(event) = terminal {
                    return Ok(after_stop(stopped, event));
                }
                if !saw_first_event {
                    return Ok(ReadOutcome::Stopped(
                        format!(
                            "provider produced no startup output within {}s",
                            wait.as_secs()
                        )
                        .into(),
                    ));
                }
                return Ok(ReadOutcome::Stopped(match in_flight.oldest() {
                    Some(tool) => format!(
                        "provider tool `{tool}` was still running with no stdout output after {}s",
                        wait.as_secs()
                    )
                    .into(),
                    None => "provider produced no stdout output before the idle timeout".into(),
                }));
            }
        };
        if chunk.is_empty() {
            if line.is_empty() {
                break;
            }
        } else {
            let newline = chunk.iter().position(|&byte| byte == b'\n');
            let len = newline.unwrap_or(chunk.len());
            if len > STDOUT_LINE_LIMIT - line.len() {
                let _ = stop_process.send(());
                return Ok(ReadOutcome::Stopped(
                    "provider stdout line exceeds the 4 MiB limit".into(),
                ));
            }
            line.extend_from_slice(&chunk[..len]);
            lines.consume(len + usize::from(newline.is_some()));
            if newline.is_none() {
                continue;
            }
        }
        if let Ok(mut value) = serde_json::from_slice(&line) {
            saw_first_event = true;
            if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
                redact_value(&mut value, secret);
            }
            in_flight.observe(provider, &value);
            if value["type"] == "control_cancel_request"
                && let Some(provider_request_id) = value["request_id"].as_str()
            {
                control_requests
                    .lock()
                    .unwrap()
                    .retain(|&request_id, request| {
                        let cancelled = request.provider_request_id == provider_request_id;
                        if cancelled {
                            let _ = tx.send(StreamEvent::PendingResolved { request_id });
                        }
                        !cancelled
                    });
            }
            for event in parser.parse(&value) {
                saw_first_event = true;
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
                    let failure_action = match &event {
                        StreamEvent::Item {
                            kind,
                            summary,
                            payload,
                        } => failed_tools.push(*kind, summary.as_deref(), payload),
                        _ => ToolFailureAction::None,
                    };
                    match failure_action {
                        ToolFailureAction::Warn => {
                            warn_agent_of_tool_failure(steer, tx);
                        }
                        ToolFailureAction::Stop => {
                            let _ = stop_process.send(());
                            return Ok(ReadOutcome::Stopped(
                                "tool_failure_loop: same tool call failed four times; inspect the error and change the command, inputs, or approach before retrying".into(),
                            ));
                        }
                        ToolFailureAction::None => {}
                    }
                    let budget_action = match &event {
                        StreamEvent::Item {
                            kind: TurnItemKind::ToolCall | TurnItemKind::CommandExecution,
                            ..
                        } => budget.on_tool_call(),
                        StreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cache_read_tokens,
                            ..
                        } => budget.on_usage(
                            input_tokens.unwrap_or(0),
                            output_tokens.unwrap_or(0),
                            cache_read_tokens.unwrap_or(0),
                        ),
                        _ => BudgetAction::None,
                    };
                    match budget_action {
                        BudgetAction::None => {}
                        BudgetAction::Warn(msg) => warn_agent(steer, tx, &msg),
                        BudgetAction::Stop(msg) => {
                            let _ = stop_process.send(());
                            return Ok(ReadOutcome::Stopped(msg.into()));
                        }
                    }
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
                        return Ok(ReadOutcome::Stopped(reason.into()));
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
    // Some streaming CLIs wait for stdin EOF before exiting. The process waiter
    // drops the stop receiver on exit, even when stdout closes first.
    drop(close_stdin);
    if tokio::time::timeout(idle_timeout, stop_process.closed())
        .await
        .is_err()
        && stop_process.send(()).is_ok()
    {
        return Ok(match terminal {
            Some(event) => after_stop(true, event),
            None => ReadOutcome::Stopped(
                "provider stdout closed but the process did not exit before the idle timeout"
                    .into(),
            ),
        });
    }
    Ok(ReadOutcome::Terminal(terminal.or_else(|| parser.finish())))
}

/// Outcome when the provider already emitted a terminal event and t3d may have
/// had to stop the process afterwards.
fn after_stop(stopped: bool, terminal: StreamEvent) -> ReadOutcome {
    match terminal {
        StreamEvent::Completed { .. } if stopped => ReadOutcome::StoppedAfterCompleted(terminal),
        terminal => ReadOutcome::Terminal(Some(terminal)),
    }
}

pub(super) async fn read_tail(mut input: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
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

pub(crate) fn detect_fatal_stderr_limit(provider: ProviderId, line: &str) -> Option<&'static str> {
    if provider == ProviderId::OpenCode
        && line
            .to_ascii_lowercase()
            .contains("free usage limit reached")
    {
        Some("Free usage limit reached")
    } else {
        None
    }
}

pub(super) async fn read_tail_watching_limit(
    mut input: impl AsyncRead + Unpin,
    provider: ProviderId,
    fatal_limit: Arc<Mutex<Option<String>>>,
    stop_notify: Arc<tokio::sync::Notify>,
) -> io::Result<Vec<u8>> {
    let mut tail = Vec::new();
    let mut line_buf = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let size = input.read(&mut buf).await?;
        if size == 0 {
            if !line_buf.is_empty() {
                check_stderr_limit_line(&line_buf, provider, &fatal_limit, &stop_notify);
            }
            return Ok(tail);
        }
        let chunk = &buf[..size];
        let excess = (tail.len() + size).saturating_sub(STDERR_LIMIT);
        tail.drain(..excess);
        tail.extend_from_slice(chunk);

        for &byte in chunk {
            if byte == b'\n' {
                check_stderr_limit_line(&line_buf, provider, &fatal_limit, &stop_notify);
                line_buf.clear();
            } else if line_buf.len() < 4096 {
                line_buf.push(byte);
            }
        }
    }
}

fn check_stderr_limit_line(
    raw: &[u8],
    provider: ProviderId,
    fatal_limit: &Mutex<Option<String>>,
    stop_notify: &tokio::sync::Notify,
) {
    let line = String::from_utf8_lossy(raw);
    if let Some(reason) = detect_fatal_stderr_limit(provider, &line) {
        let mut guard = fatal_limit
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(format!("{reason}: {}", line.trim()));
        }
        stop_notify.notify_waiters();
    }
}

fn user_line(content: impl serde::Serialize) -> String {
    let mut line =
        json!({"type": "user", "message": {"role": "user", "content": content}}).to_string();
    line.push('\n');
    line
}

pub(super) async fn write_input(
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
            Input::StreamJson(content) => {
                stdin.write_all(user_line(content).as_bytes()).await?;
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
