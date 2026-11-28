use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use t3_core::ProviderId;

#[cfg(unix)]
use super::transcript_open_flags;
use super::{
    MAX_JSONL_LINE, SESSION_TAIL_BYTES, SessionContextUsage,
    codex::{codex_context_usage, find_codex_session_file},
    transcript::parse_transcript,
};

/// Reads the latest token snapshot for a Claude or Codex session transcript without following symlinks.
pub(super) fn read_session_transcript(
    path: &Path,
    provider: ProviderId,
) -> Option<SessionContextUsage> {
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(transcript_open_flags().ok()?);
    }
    let mut file = options.open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    // Bound read cost on huge transcripts by tail-parsing the final 2 MiB.
    let len = meta.len();
    let mut reader = if len > SESSION_TAIL_BYTES {
        file.seek(SeekFrom::Start(len - SESSION_TAIL_BYTES)).ok()?;
        let mut r = BufReader::new(file);
        let mut discard = Vec::new();
        let _ = r.read_until(b'\n', &mut discard);
        r
    } else {
        BufReader::new(file)
    };
    if provider == ProviderId::Codex {
        return codex_context_usage(&mut reader).ok()?;
    }
    let samples = parse_transcript(&mut reader, provider).ok()?;
    let last = samples.last()?.tokens;
    let context_tokens = last.total();
    (context_tokens > 0).then_some(SessionContextUsage {
        context_tokens,
        input_tokens: last.input_tokens,
        output_tokens: last.output_tokens,
        cache_read_tokens: last.cache_read_tokens,
        cache_write_tokens: last.cache_write_tokens,
        context_window: None,
    })
}

pub(super) fn find_claude_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    let root = home.join(".claude").join("projects");
    let filename = format!("{session_id}.jsonl");
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let candidate = dir.join(&filename);
        if fs::symlink_metadata(&candidate).is_ok_and(|m| m.is_file()) {
            return Some(candidate);
        }
    }
    None
}

/// Whether the provider can resume `session_id` from `workspace`, judged from the session file
/// the provider keeps on disk. Providers without a stable on-disk session layout (Antigravity,
/// OpenCode, Mock) are assumed resumable.
pub(crate) fn provider_session_resumable(
    provider: ProviderId,
    home: Option<&Path>,
    workspace: &Path,
    session_id: &str,
) -> bool {
    let Some(home) = home else {
        return true;
    };
    match provider {
        ProviderId::Claude => {
            let config = std::env::var_os("CLAUDE_CONFIG_DIR")
                .map_or_else(|| home.join(".claude"), PathBuf::from);
            claude_session_resumable(&config.join("projects"), workspace, session_id)
        }
        ProviderId::Codex => find_codex_session_file(home, session_id).is_some_and(|path| {
            has_session_line(&path, |line| line.kind.as_deref() == Some("session_meta"))
        }),
        ProviderId::Antigravity
        | ProviderId::OpenCode
        | ProviderId::GeminiApp
        | ProviderId::Mock => true,
    }
}

/// Mirrors the check Claude Code makes before `--resume`: the transcript lives in the project
/// directory of the working directory and holds at least one message line.
pub(super) fn claude_session_resumable(
    projects: &Path,
    workspace: &Path,
    session_id: &str,
) -> bool {
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return false;
    }
    // Claude keys projects by the physical working directory, e.g. /tmp becomes /private/tmp.
    let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let dir: String = workspace
        .to_string_lossy()
        .chars()
        .flat_map(|c| {
            let dash = !c.is_ascii_alphanumeric();
            std::iter::repeat_n(if dash { '-' } else { c }, c.len_utf16())
        })
        .collect();
    let path = projects.join(dir).join(format!("{session_id}.jsonl"));
    fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
        && has_session_line(&path, |line| {
            [&line.uuid, &line.message_id, &line.leaf_uuid]
                .iter()
                .any(|id| id.as_deref().is_some_and(|id| !id.is_empty()))
        })
}

#[derive(Deserialize)]
struct SessionLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "messageId")]
    message_id: Option<String>,
    #[serde(rename = "leafUuid")]
    leaf_uuid: Option<String>,
}

fn has_session_line(path: &Path, accept: impl Fn(&SessionLine) -> bool) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut input = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match input
            .by_ref()
            .take((MAX_JSONL_LINE + 1) as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => return false,
            Ok(_) => {}
        }
        if line.len() > MAX_JSONL_LINE {
            if line.last() != Some(&b'\n') && input.skip_until(b'\n').is_err() {
                return false;
            }
        } else if serde_json::from_slice::<SessionLine>(&line).is_ok_and(|parsed| accept(&parsed)) {
            return true;
        }
    }
}
