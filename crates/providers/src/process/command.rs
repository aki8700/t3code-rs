use super::*;

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
