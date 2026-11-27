use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_turn_with_profile(
    active: &ActiveRunMap,
    run_id: Uuid,
    provider: ProviderId,
    cmd: Command,
    input: Input,
    secret: Option<&str>,
    tool_profile: t3_core::McpToolProfile,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> CoreResult<()> {
    run_turn_inner(
        active,
        run_id,
        provider,
        cmd,
        input,
        secret,
        tool_profile,
        tx,
        configured_idle_timeout(),
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(super) async fn run_turn_with_idle_timeout(
    active: &ActiveRunMap,
    run_id: Uuid,
    provider: ProviderId,
    cmd: Command,
    input: Input,
    secret: Option<&str>,
    tx: mpsc::UnboundedSender<StreamEvent>,
    idle_timeout: Duration,
) -> CoreResult<()> {
    run_turn_inner(
        active,
        run_id,
        provider,
        cmd,
        input,
        secret,
        t3_core::McpToolProfile::Full,
        tx,
        idle_timeout,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_inner(
    active: &ActiveRunMap,
    run_id: Uuid,
    provider: ProviderId,
    mut cmd: Command,
    input: Input,
    secret: Option<&str>,
    tool_profile: t3_core::McpToolProfile,
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
    let steer_supported = matches!(&input, Input::StreamJson(_));
    let control_requests = ControlRequests::default();
    let interrupted = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    let (released, release_rx) = watch::channel(false);
    #[cfg(not(unix))]
    let (stop, stop_rx) = oneshot::channel();
    #[cfg(unix)]
    let spawn = startup::prepare(cmd).await?;
    #[cfg(not(unix))]
    let spawn = move || cmd.spawn();
    let (mut child, guard) = {
        let mut runs = active.lock().unwrap();
        if runs.contains_key(&run_id) {
            return Err(CoreError::Provider(format!(
                "run {run_id} is already active"
            )));
        }
        let child = spawn().map_err(|err| {
            CoreError::Provider(format!(
                "could not start {}: {err}",
                provider.display_name()
            ))
        })?;
        #[cfg(unix)]
        if !startup::has_hook()
            && let Some(pid) = child.id()
            && let Some(start_time) = process_start_time(pid)
        {
            let _ = tx.send(StreamEvent::ProcessStarted {
                process_group_id: pid as i32,
                start_time,
            });
        }
        runs.insert(
            run_id,
            ActiveProcess {
                #[cfg(unix)]
                pid: child.id().expect("newly spawned child"),
                #[cfg(unix)]
                released: release_rx,
                stdin_tx: matches!(input, Input::StreamJson(_)).then_some(steer_tx.clone()),
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
        let stderr_limit = Arc::new(Mutex::new(None::<String>));
        let stderr_stop = Arc::new(tokio::sync::Notify::new());
        let wait = async {
            let wait_notify = Arc::clone(&stderr_stop);
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
                _ = wait_notify.notified() => {
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
                    _ = wait_notify.notified() => {
                        GroupCleanup(Some(pid)).kill();
                        wait_for_exit(pid).await
                    }
                };
                if result.is_ok() || already_reaped(&result) {
                    // Clean up descendants holding pipes before reaping the leader. Kill
                    // again even after reader_stop_rx did: on macOS a child forked while
                    // the first SIGKILL was delivered can survive it and keep stderr open.
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
            read_events(
                stdout,
                provider,
                secret,
                &tx,
                steer_supported.then_some(&steer_tx),
                &control_requests,
                close_tx,
                reader_stop_tx,
                idle_timeout,
                tool_profile,
            ),
            read_tail_watching_limit(
                stderr,
                provider,
                Arc::clone(&stderr_limit),
                Arc::clone(&stderr_stop)
            ),
            write_input(stdin, input, steer_rx, close_rx),
        )
        .map(|(status, terminal, stderr, ())| {
            (
                status,
                terminal,
                stderr,
                stderr_limit.lock().unwrap().clone(),
            )
        })
    };
    let (status, terminal, stderr, fatal_stderr_limit) = tokio::select! {
        _ = tx.closed() => return Ok(()),
        result = collect => result.map_err(process_error)?,
    };

    if let Some(limit_msg) = fatal_stderr_limit {
        let _ = tx.send(StreamEvent::RateLimited {
            retry_after_secs: 0,
            message: limit_msg,
            reset_at: None,
            error_code: Some("usage_limit".into()),
        });
        return Ok(());
    }

    let terminal = match terminal {
        ReadOutcome::Terminal(terminal) => terminal,
        // The turn already finished; t3d only cleaned up a process that lingered,
        // so the SIGKILL exit status must not turn the result into a failure.
        ReadOutcome::StoppedAfterCompleted(completed) if !interrupted.load(Ordering::Acquire) => {
            let notice = format!(
                "t3d stopped the provider process: it stayed alive with no stdout output for {}s after the turn result",
                idle_timeout.as_secs()
            );
            let _ = tx.send(StreamEvent::Item {
                kind: TurnItemKind::SystemNotice,
                summary: Some(notice.clone()),
                payload: json!({ "message": notice }),
            });
            let _ = tx.send(completed);
            return Ok(());
        }
        ReadOutcome::StoppedAfterCompleted(completed) => Some(completed),
        ReadOutcome::Stopped(message) => {
            let error_code = message
                .starts_with("tool_failure_loop:")
                .then(|| TOOL_FAILURE_LOOP_ERROR_CODE.to_owned());
            let _ = tx.send(StreamEvent::Failed {
                message: message.into(),
                reset_at: None,
                error_code,
            });
            return Ok(());
        }
    };
    let event = if interrupted.load(Ordering::Acquire) {
        StreamEvent::Failed {
            message: "interrupted".into(),
            reset_at: None,
            error_code: None,
        }
    } else if matches!(terminal, Some(StreamEvent::RateLimited { .. })) {
        terminal.expect("rate-limit event")
    } else if !status.success() {
        let stderr = redact_stderr(&String::from_utf8_lossy(&stderr), secret);
        let (detail, reset_at, error_code) = match &terminal {
            Some(StreamEvent::Failed {
                message,
                reset_at,
                error_code,
            }) => (Some(message.as_str()), reset_at.clone(), error_code.clone()),
            _ => (None, None, None),
        };
        provider_error(
            exit_message(status, stderr.trim(), detail),
            reset_at,
            error_code,
        )
    } else {
        terminal.unwrap_or_else(|| StreamEvent::Failed {
            message: format!("{} exited without a turn result", provider.display_name()),
            reset_at: None,
            error_code: None,
        })
    };
    let _ = tx.send(event);
    Ok(())
}
