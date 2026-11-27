use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[cfg(unix)]
use super::transcript_open_flags;
use super::{
    CodexRunUsage, CodexSessionUsageSnapshot, MAX_JSONL_LINE, SESSION_TAIL_BYTES,
    SessionContextUsage, transcript::RawTokens,
};

#[derive(Deserialize)]
struct CodexContextLine {
    payload: CodexContextPayload,
}

#[derive(Deserialize)]
struct CodexContextPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    info: Option<CodexContextInfo>,
}

#[derive(Deserialize)]
struct CodexContextInfo {
    last_token_usage: Option<RawTokens>,
    total_token_usage: Option<RawTokens>,
    model_context_window: Option<u64>,
}

/// Codex context size is the latest request's `last_token_usage`, which Codex also rewrites
/// after `/compact` to the size of the replacement history. `total_token_usage` is cumulative.
pub(super) fn codex_context_usage(
    mut input: impl BufRead,
) -> io::Result<Option<SessionContextUsage>> {
    let mut latest = None;
    let mut line = Vec::new();
    while input
        .by_ref()
        .take((MAX_JSONL_LINE + 1) as u64)
        .read_until(b'\n', &mut line)?
        != 0
    {
        if line.len() > MAX_JSONL_LINE {
            if line.last() != Some(&b'\n') {
                input.skip_until(b'\n')?;
            }
        } else if line.windows(13).any(|w| w == b"\"token_count\"")
            && let Ok(parsed) = serde_json::from_slice::<CodexContextLine>(&line)
            && parsed.payload.kind.as_deref() == Some("token_count")
            && let Some(info) = parsed.payload.info
            && let Some(last) = info.last_token_usage
        {
            let context_tokens = if last.total_tokens > 0 {
                last.total_tokens
            } else {
                last.input_tokens
                    .saturating_add(last.cache_write_input_tokens)
                    .saturating_add(last.output_tokens)
            };
            if context_tokens > 0 {
                latest = Some(SessionContextUsage {
                    context_tokens,
                    input_tokens: last.input_tokens.saturating_sub(last.cached_input_tokens),
                    output_tokens: last.output_tokens,
                    cache_read_tokens: last.cached_input_tokens,
                    cache_write_tokens: last.cache_write_input_tokens,
                    context_window: info.model_context_window.filter(|w| *w > 0),
                });
            }
        }
        line.clear();
    }
    Ok(latest)
}

pub(super) fn find_codex_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    let root = home.join(".codex").join("sessions");
    let exact = format!("{session_id}.jsonl");
    let suffixed = format!("-{session_id}.jsonl");
    let mut dirs = vec![root];
    let mut visited = 0usize;
    while let Some(dir) = dirs.pop() {
        visited += 1;
        if visited > 512 || !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name == exact || name.ends_with(&suffixed))
            {
                return Some(path);
            }
        }
    }
    None
}

pub(crate) fn codex_session_usage_snapshot(
    home: Option<&Path>,
    session_id: &str,
) -> Option<CodexSessionUsageSnapshot> {
    let home = home?;
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return None;
    }
    let path = find_codex_session_file(home, session_id)?;
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(transcript_open_flags().ok()?);
    }
    let mut file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut reader = if metadata.len() > SESSION_TAIL_BYTES {
        file.seek(SeekFrom::Start(metadata.len() - SESSION_TAIL_BYTES))
            .ok()?;
        let mut reader = BufReader::new(file);
        let mut discard = Vec::new();
        let _ = reader.read_until(b'\n', &mut discard);
        reader
    } else {
        BufReader::new(file)
    };
    codex_rollout_usage_snapshot(&mut reader).ok().flatten()
}

pub(crate) fn codex_run_usage_since(
    before: Option<CodexSessionUsageSnapshot>,
    after: Option<CodexSessionUsageSnapshot>,
) -> Option<CodexRunUsage> {
    let after = after?;
    let tokens = match (after.total, before.and_then(|snapshot| snapshot.total)) {
        (Some(total), Some(before)) => total.delta(before),
        (Some(total), None) => total,
        (None, _) => after.last?,
    };
    if tokens.input_tokens == 0 && tokens.output_tokens == 0 {
        return None;
    }
    Some(CodexRunUsage {
        // Codex input already includes cached input and output already includes reasoning.
        input_tokens: tokens.input_tokens,
        cache_read_tokens: tokens.cached_input_tokens,
        cache_creation_tokens: tokens.cache_write_input_tokens,
        output_tokens: tokens.output_tokens,
        reasoning_tokens: tokens.reasoning_output_tokens,
    })
}

fn codex_rollout_usage_snapshot(
    mut input: impl BufRead,
) -> io::Result<Option<CodexSessionUsageSnapshot>> {
    let mut latest = None;
    let mut line = Vec::new();
    while input
        .by_ref()
        .take((MAX_JSONL_LINE + 1) as u64)
        .read_until(b'\n', &mut line)?
        != 0
    {
        if line.len() > MAX_JSONL_LINE {
            if line.last() != Some(&b'\n') {
                input.skip_until(b'\n')?;
            }
        } else if line.windows(13).any(|window| window == b"\"token_count\"")
            && let Ok(parsed) = serde_json::from_slice::<CodexContextLine>(&line)
            && parsed.payload.kind.as_deref() == Some("token_count")
            && let Some(info) = parsed.payload.info
            && (info.total_token_usage.is_some() || info.last_token_usage.is_some())
        {
            latest = Some(CodexSessionUsageSnapshot {
                total: info.total_token_usage,
                last: info.last_token_usage,
            });
        }
        line.clear();
    }
    Ok(latest)
}
