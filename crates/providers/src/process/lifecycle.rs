use super::*;

pub(super) struct RunGuard<'a> {
    pub(super) active: &'a ActiveRunMap,
    pub(super) run_id: Uuid,
    pub(super) interrupted: Arc<AtomicBool>,
    #[cfg(unix)]
    pub(super) released: watch::Sender<bool>,
}

impl RunGuard<'_> {
    pub(super) fn finish(&self, #[cfg_attr(not(unix), allow(unused_variables))] kill_group: bool) {
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
pub(super) struct GroupCleanup(pub(super) Option<u32>);

#[cfg(unix)]
impl GroupCleanup {
    pub(super) fn kill(&mut self) {
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
pub(super) fn classify_group_signal_error(
    pid: libc::pid_t,
    signal: i32,
    error: io::Error,
) -> io::Result<()> {
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
pub(super) fn signal_group(pid: u32, signal: i32) -> io::Result<()> {
    let pid = checked_pid(pid)?;
    // SAFETY: pre_exec puts the child in its own group and the leader is still unreaped.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    classify_group_signal_error(pid, signal, io::Error::last_os_error())
}

#[cfg(unix)]
pub(super) fn already_reaped(result: &io::Result<()>) -> bool {
    result
        .as_ref()
        .is_err_and(|error| error.raw_os_error() == Some(libc::ECHILD))
}

#[cfg(unix)]
pub(super) async fn wait_for_exit(pid: u32) -> io::Result<()> {
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

pub(super) fn parse_idle_timeout(val: Option<&str>) -> Duration {
    val.and_then(|value| value.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.clamp(MIN_IDLE_TIMEOUT_SECS, MAX_IDLE_TIMEOUT_SECS)))
        .unwrap_or(DEFAULT_IDLE_STDOUT_TIMEOUT)
}

pub(super) fn configured_idle_timeout() -> Duration {
    let raw = std::env::var(IDLE_TIMEOUT_ENV)
        .ok()
        .or_else(|| std::env::var(LEGACY_IDLE_STDOUT_TIMEOUT_ENV).ok());
    parse_idle_timeout(raw.as_deref())
}
