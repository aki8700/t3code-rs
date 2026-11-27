use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use t3_core::CoreError;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt},
    process::Command,
};

pub(crate) const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

pub(super) fn resolve_cli_bin(configured: &Path) -> Option<PathBuf> {
    if configured.is_file() {
        return Some(configured.to_path_buf());
    }
    // Only search fallback locations for a bare binary name (not an explicit missing path).
    if configured.components().count() != 1 {
        return None;
    }
    let name = configured.file_name()?;
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    if let Some(home) = std::env::home_dir() {
        let local = home.join(".local/bin").join(name);
        if local.is_file() {
            return Some(local);
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let cand = Path::new(dir).join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

pub(crate) fn prepare_probe_cmd(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
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
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
    cmd
}

pub(crate) async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<usize, CoreError> {
    let read = AsyncReadExt::take(&mut *reader, (MAX_LINE_BYTES + 1) as u64)
        .read_until(b'\n', buf)
        .await
        .map_err(|e| CoreError::Provider(format!("probe stdout read failed: {e}")))?;
    if buf.len() > MAX_LINE_BYTES && buf.last() != Some(&b'\n') {
        let mut discard = Vec::new();
        loop {
            discard.clear();
            let n = reader
                .read_until(b'\n', &mut discard)
                .await
                .map_err(|e| CoreError::Provider(format!("probe stdout skip failed: {e}")))?;
            if n == 0 || discard.last() == Some(&b'\n') {
                break;
            }
        }
    }
    Ok(read)
}
