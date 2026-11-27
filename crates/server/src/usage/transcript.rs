use std::{
    collections::HashMap,
    io::{BufRead, Read},
};

use serde::{Deserialize, Deserializer};
#[cfg(unix)]
use std::io;
use t3_core::ProviderId;

use super::{MAX_JSONL_LINE, Sample, pricing::Tokens};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub(super) struct RawTokens {
    #[serde(deserialize_with = "token_count_or_zero")]
    pub(super) input_tokens: u64,
    #[serde(deserialize_with = "token_count_or_zero")]
    pub(super) output_tokens: u64,
    #[serde(alias = "cache_read_input_tokens")]
    #[serde(deserialize_with = "token_count_or_zero")]
    pub(super) cached_input_tokens: u64,
    #[serde(alias = "cache_creation_input_tokens")]
    #[serde(deserialize_with = "token_count_or_zero")]
    pub(super) cache_write_input_tokens: u64,
    #[serde(deserialize_with = "token_count_or_zero")]
    pub(super) reasoning_output_tokens: u64,
    #[serde(deserialize_with = "token_count_or_zero")]
    pub(super) total_tokens: u64,
}

fn token_count_or_zero<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or_default())
}

impl RawTokens {
    pub(super) fn delta(self, before: Self) -> Self {
        if self.input_tokens < before.input_tokens
            || self.output_tokens < before.output_tokens
            || self.total_tokens < before.total_tokens
        {
            return self;
        }
        Self {
            input_tokens: self.input_tokens - before.input_tokens,
            output_tokens: self.output_tokens - before.output_tokens,
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(before.cached_input_tokens),
            cache_write_input_tokens: self
                .cache_write_input_tokens
                .saturating_sub(before.cache_write_input_tokens),
            reasoning_output_tokens: self
                .reasoning_output_tokens
                .saturating_sub(before.reasoning_output_tokens),
            total_tokens: self.total_tokens.saturating_sub(before.total_tokens),
        }
    }

    fn counts(self, provider: ProviderId) -> Tokens {
        let input_tokens = if provider == ProviderId::Codex {
            self.input_tokens
                .checked_sub(self.cached_input_tokens)
                .unwrap_or(self.input_tokens)
        } else {
            self.input_tokens
        };
        Tokens {
            input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cached_input_tokens,
            cache_write_tokens: self.cache_write_input_tokens,
            reasoning_tokens: self.reasoning_output_tokens,
        }
    }
}

#[derive(Deserialize)]
struct ClaudeLine {
    timestamp: String,
    message: ClaudeMessage,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<RawTokens>,
}

#[derive(Deserialize)]
struct CodexLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    model: Option<String>,
    payload: Option<CodexPayload>,
}

#[derive(Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    model: Option<String>,
    info: Option<TokenEvent>,
}

#[derive(Deserialize)]
struct TokenEvent {
    last_token_usage: Option<RawTokens>,
    total_token_usage: Option<RawTokens>,
}

#[derive(Default)]
struct Transcript {
    samples: Vec<Sample>,
    message_index: HashMap<String, usize>,
    codex_model: Option<String>,
    codex_total: Option<RawTokens>,
}

impl Transcript {
    fn claude(&mut self, line: &[u8]) -> Option<()> {
        let line: ClaudeLine = serde_json::from_slice(line).ok()?;
        let tokens = line.message.usage?.counts(ProviderId::Claude);
        let sample = Sample {
            at: line.timestamp.parse().ok()?,
            model: line.message.model.unwrap_or_else(|| "unknown".into()),
            tokens,
        };
        if let Some(id) = line.message.id.filter(|id| !id.is_empty()) {
            if let Some(index) = self.message_index.get(&id) {
                if sample.tokens.output_tokens > self.samples[*index].tokens.output_tokens {
                    self.samples[*index] = sample;
                }
                return Some(());
            }
            self.message_index.insert(id, self.samples.len());
        }
        self.samples.push(sample);
        Some(())
    }

    fn codex(&mut self, line: &[u8]) -> Option<()> {
        let line: CodexLine = serde_json::from_slice(line).ok()?;
        match line.kind.as_str() {
            "turn_context" | "session_meta" => {
                if let Some(model) = line
                    .payload
                    .and_then(|payload| payload.model)
                    .or(line.model)
                {
                    self.codex_model = Some(model);
                }
            }
            "event_msg" => {
                let payload = line.payload?;
                if payload.kind.as_deref() != Some("token_count") {
                    return None;
                }
                let event = payload.info?;
                let at = line.timestamp?.parse().ok()?;
                let current = event.total_token_usage;
                let delta = if let Some(current) = current {
                    if Some(current) == self.codex_total {
                        return None;
                    }
                    let delta = current.delta(self.codex_total.unwrap_or_default());
                    self.codex_total = Some(current);
                    delta
                } else {
                    RawTokens::default()
                };
                let usage = event.last_token_usage.unwrap_or(delta);
                if usage != RawTokens::default() {
                    self.samples.push(Sample {
                        at,
                        model: self.codex_model.clone().unwrap_or_else(|| "unknown".into()),
                        tokens: usage.counts(ProviderId::Codex),
                    });
                }
            }
            _ => {}
        }
        Some(())
    }
}

pub(super) fn parse_transcript(
    mut input: impl BufRead,
    provider: ProviderId,
) -> std::io::Result<Vec<Sample>> {
    let mut transcript = Transcript::default();
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
            line.clear();
            continue;
        }
        match provider {
            ProviderId::Claude => transcript.claude(&line),
            ProviderId::Codex => transcript.codex(&line),
            _ => unreachable!("only native Claude and Codex transcripts are scanned"),
        };
        line.clear();
    }
    Ok(transcript.samples)
}

#[cfg(unix)]
pub(crate) fn transcript_open_flags() -> io::Result<i32> {
    // The server has no libc dependency. Use the O_NOFOLLOW | O_NONBLOCK
    // values from the supported platforms' fcntl.h to reject symlinks and FIFOs.
    if cfg!(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )) {
        Ok(0x100 | 0x4)
    } else if cfg!(any(target_os = "linux", target_os = "android")) {
        let nofollow = if cfg!(any(
            target_arch = "arm",
            target_arch = "aarch64",
            target_arch = "powerpc",
            target_arch = "powerpc64"
        )) {
            0x8000
        } else {
            0x20000
        };
        let nonblock = if cfg!(any(
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "mips32r6",
            target_arch = "mips64r6"
        )) {
            0x80
        } else if cfg!(any(target_arch = "sparc", target_arch = "sparc64")) {
            0x4000
        } else {
            0x800
        };
        Ok(nofollow | nonblock)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "secure transcript open unavailable",
        ))
    }
}
