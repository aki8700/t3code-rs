use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration as StdDuration, Instant, SystemTime},
};

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Duration, Local, SecondsFormat, Timelike, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Deserializer, Serialize, de};
use t3_core::{CoreError, CoreResult, ProviderId};

use crate::{ServerState, routes::ApiResult};

const MAX_JSONL_LINE: usize = 4 * 1024 * 1024;
const SNAPSHOT_TTL: StdDuration = StdDuration::from_secs(2);

#[derive(Deserialize)]
pub(crate) struct UsageQuery {
    days: Option<u32>,
    tz_offset_mins: Option<i32>,
    #[serde(default, deserialize_with = "parse_refresh")]
    refresh: Option<bool>,
}

fn parse_refresh<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    match String::deserialize(deserializer)?.as_str() {
        "true" | "1" => Ok(Some(true)),
        "false" | "0" => Ok(Some(false)),
        _ => Err(de::Error::custom("refresh must be a boolean")),
    }
}

const SESSION_CTX_CACHE_MAX: usize = 256;
const SESSION_CTX_MISS_TTL: StdDuration = StdDuration::from_secs(10);
const SESSION_TAIL_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(crate) struct SessionContextUsage {
    pub context_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

enum CachedSessionContext {
    Hit {
        path: PathBuf,
        modified: Option<SystemTime>,
        len: u64,
        usage: Option<SessionContextUsage>,
    },
    Miss {
        at: Instant,
    },
}

/// Reads the latest token snapshot for a Claude or Codex session transcript without following symlinks.
fn read_session_transcript(path: &Path, provider: ProviderId) -> Option<SessionContextUsage> {
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
    let samples = parse_transcript(&mut reader, provider).ok()?;
    let last = samples.last()?.tokens;
    let context_tokens = last.total();
    (context_tokens > 0).then_some(SessionContextUsage {
        context_tokens,
        input_tokens: last.input_tokens,
        output_tokens: last.output_tokens,
        cache_read_tokens: last.cache_read_tokens,
        cache_write_tokens: last.cache_write_tokens,
    })
}

fn find_claude_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
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

fn find_codex_session_file(home: &Path, session_id: &str) -> Option<PathBuf> {
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

pub(crate) async fn get(
    State(state): State<ServerState>,
    Query(query): Query<UsageQuery>,
) -> ApiResult<Json<UsageResponse>> {
    let days = query.days.unwrap_or(7);
    if !matches!(days, 1 | 7 | 30 | 90) {
        return Err(CoreError::InvalidArgument("days must be 1, 7, 30, or 90".into()).into());
    }
    let offset = query
        .tz_offset_mins
        .unwrap_or_else(|| Local::now().offset().local_minus_utc() / 60)
        .clamp(-720, 840);
    let response = tokio::task::spawn_blocking(move || {
        let mut cache = state
            .usage_cache
            .lock()
            .map_err(|_| CoreError::Internal("usage cache lock poisoned".into()))?;
        cache.report(
            state.usage_home.as_deref(),
            &state.config.db_path(),
            days,
            offset,
            query.refresh.unwrap_or_default(),
        )
    })
    .await
    .map_err(|error| CoreError::Internal(error.to_string()))??;
    Ok(Json(response))
}

#[derive(Default)]
pub(crate) struct UsageScanCache {
    files: HashMap<PathBuf, CachedFile>,
    snapshot: Option<UsageSnapshot>,
    session_ctx: HashMap<(ProviderId, String), CachedSessionContext>,
}

impl UsageScanCache {
    pub(crate) fn session_context_usage(
        &mut self,
        home: Option<&Path>,
        provider: ProviderId,
        session_id: &str,
    ) -> Option<SessionContextUsage> {
        let home = home?;
        if session_id.is_empty()
            || session_id.len() > 128
            || !session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return None;
        }
        let key = (provider, session_id.to_owned());
        if let Some(entry) = self.session_ctx.get(&key) {
            match entry {
                CachedSessionContext::Hit {
                    path,
                    modified,
                    len,
                    usage,
                } => {
                    if let Ok(meta) = fs::symlink_metadata(path)
                        && meta.is_file()
                        && meta.modified().ok() == *modified
                        && meta.len() == *len
                    {
                        return *usage;
                    }
                }
                CachedSessionContext::Miss { at } => {
                    if at.elapsed() < SESSION_CTX_MISS_TTL {
                        return None;
                    }
                }
            }
        }
        let path = match provider {
            ProviderId::Claude => find_claude_session_file(home, session_id),
            ProviderId::Codex => find_codex_session_file(home, session_id),
            _ => None,
        };
        if self.session_ctx.len() >= SESSION_CTX_CACHE_MAX && !self.session_ctx.contains_key(&key) {
            self.session_ctx.clear();
        }
        let Some(path) = path else {
            self.session_ctx
                .insert(key, CachedSessionContext::Miss { at: Instant::now() });
            return None;
        };
        let meta = fs::symlink_metadata(&path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let usage = read_session_transcript(&path, provider);
        self.session_ctx.insert(
            key,
            CachedSessionContext::Hit {
                path,
                modified: meta.modified().ok(),
                len: meta.len(),
                usage,
            },
        );
        usage
    }
}

struct CachedFile {
    modified: Option<SystemTime>,
    len: u64,
    provider: ProviderId,
    samples: Vec<Sample>,
}

struct UsageSnapshot {
    scanned_at: DateTime<Utc>,
    stamp: Instant,
    runs: Vec<LocalRun>,
    reports: HashMap<(u32, i32), UsageResponse>,
}

impl UsageScanCache {
    fn report(
        &mut self,
        home: Option<&Path>,
        db: &Path,
        days: u32,
        offset: i32,
        refresh: bool,
    ) -> CoreResult<UsageResponse> {
        if refresh
            || self
                .snapshot
                .as_ref()
                .is_none_or(|snapshot| snapshot.stamp.elapsed() >= SNAPSHOT_TTL)
        {
            let mut seen = HashSet::new();
            if let Some(home) = home {
                self.scan(home, ".claude", "projects", ProviderId::Claude, &mut seen);
                self.scan(home, ".codex", "sessions", ProviderId::Codex, &mut seen);
            }
            self.files.retain(|path, _| seen.contains(path));
            let scanned_at = Utc::now();
            self.snapshot = Some(UsageSnapshot {
                scanned_at,
                runs: read_local_runs(db, scanned_at - Duration::days(90))?,
                stamp: Instant::now(),
                reports: HashMap::new(),
            });
        }
        let snapshot = self.snapshot.as_mut().expect("usage snapshot initialized");
        Ok(snapshot
            .reports
            .entry((days, offset))
            .or_insert_with(|| {
                aggregate(
                    days,
                    offset,
                    snapshot.scanned_at,
                    self.files.values(),
                    &snapshot.runs,
                )
            })
            .clone())
    }

    fn scan(
        &mut self,
        home: &Path,
        config: &str,
        transcripts: &str,
        provider: ProviderId,
        seen: &mut HashSet<PathBuf>,
    ) {
        let config = home.join(config);
        if fs::symlink_metadata(&config).is_ok_and(|metadata| metadata.is_dir()) {
            self.scan_directory(config.join(transcripts), provider, seen);
        }
    }

    fn scan_directory(&mut self, root: PathBuf, provider: ProviderId, seen: &mut HashSet<PathBuf>) {
        let mut directories = vec![root];
        while let Some(path) = directories.pop() {
            if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                continue;
            }
            let Ok(entries) = fs::read_dir(path) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    directories.push(path);
                } else if kind.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "jsonl")
                    && self.scan_file(&path, provider).is_ok()
                {
                    seen.insert(path);
                }
            }
        }
    }

    fn scan_file(&mut self, path: &Path, provider: ProviderId) -> io::Result<()> {
        let mut options = File::options();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(transcript_open_flags()?);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a regular transcript",
            ));
        }
        let modified = metadata.modified().ok();
        let len = metadata.len();
        if self
            .files
            .get(path)
            .is_some_and(|cached| cached.modified == modified && cached.len == len)
        {
            return Ok(());
        }
        let samples = parse_transcript(BufReader::new(file), provider)?;
        self.files.insert(
            path.to_owned(),
            CachedFile {
                modified,
                len,
                provider,
                samples,
            },
        );
        Ok(())
    }
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
struct RawTokens {
    #[serde(deserialize_with = "token_count_or_zero")]
    input_tokens: u64,
    #[serde(deserialize_with = "token_count_or_zero")]
    output_tokens: u64,
    #[serde(alias = "cache_read_input_tokens")]
    #[serde(deserialize_with = "token_count_or_zero")]
    cached_input_tokens: u64,
    #[serde(alias = "cache_creation_input_tokens")]
    #[serde(deserialize_with = "token_count_or_zero")]
    cache_write_input_tokens: u64,
    #[serde(deserialize_with = "token_count_or_zero")]
    reasoning_output_tokens: u64,
}

fn token_count_or_zero<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(Option::<u64>::deserialize(deserializer)?.unwrap_or_default())
}

impl RawTokens {
    fn delta(self, before: Self) -> Self {
        if self.input_tokens < before.input_tokens || self.output_tokens < before.output_tokens {
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

struct Sample {
    at: DateTime<Utc>,
    model: String,
    tokens: Tokens,
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

fn parse_transcript(mut input: impl BufRead, provider: ProviderId) -> std::io::Result<Vec<Sample>> {
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

struct LocalRun {
    provider: ProviderId,
    session: String,
    sample: Sample,
}

fn read_local_runs(path: &Path, since: DateTime<Utc>) -> CoreResult<Vec<LocalRun>> {
    let result = (|| -> rusqlite::Result<Vec<LocalRun>> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_timeout(StdDuration::from_secs(5))?;
        let mut query = db.prepare(
            "SELECT provider, thread_id, model, input_tokens, output_tokens, coalesce(completed_at, started_at)
             FROM runs WHERE provider IN (?1, ?2, ?3)
             AND julianday(coalesce(completed_at, started_at)) >= julianday(?4)",
        )?;
        query
            .query_map(
                params![
                    r#""antigravity""#,
                    r#""open-code""#,
                    r#""mock""#,
                    since.to_rfc3339()
                ],
                |row| {
                    let provider = match row.get_ref(0)?.as_str()? {
                        r#""antigravity""# => ProviderId::Antigravity,
                        r#""open-code""# => ProviderId::OpenCode,
                        _ => ProviderId::Mock,
                    };
                    let at =
                        row.get_ref(5)?
                            .as_str()?
                            .parse::<DateTime<Utc>>()
                            .map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    5,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                    Ok(LocalRun {
                        provider,
                        session: row.get(1)?,
                        sample: Sample {
                            at,
                            model: row.get(2)?,
                            tokens: Tokens {
                                input_tokens: row.get::<_, Option<u64>>(3)?.unwrap_or_default(),
                                output_tokens: row.get::<_, Option<u64>>(4)?.unwrap_or_default(),
                                ..Tokens::default()
                            },
                        },
                    })
                },
            )?
            .collect()
    })();
    result.map_err(|error| CoreError::Storage(error.to_string()))
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
struct Tokens {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
}

impl Tokens {
    fn total(self) -> u64 {
        // Codex reasoning is a subset of output tokens.
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }

    fn estimated_cost(self, provider: ProviderId, model: &str) -> f64 {
        let Some([input, output, read, write]) = model_rates_per_million(provider, model) else {
            return 0.0;
        };
        (self.input_tokens as f64 * input
            + self.output_tokens as f64 * output
            + self.cache_read_tokens as f64 * read
            + self.cache_write_tokens as f64 * write)
            / 1_000_000.0
    }
}

/// Per-million-token USD rates `[input, output, cache_read, cache_write]`, or `None` for free/unknown models.
pub(crate) fn model_rates_per_million(provider: ProviderId, model: &str) -> Option<[f64; 4]> {
    let model = model.to_ascii_lowercase();
    match provider {
        ProviderId::Claude if model.contains("opus") => Some([15.0, 75.0, 1.5, 18.75]),
        ProviderId::Claude if model.contains("sonnet") => Some([3.0, 15.0, 0.30, 3.75]),
        ProviderId::Claude if model.contains("haiku") => Some([0.80, 4.0, 0.08, 1.0]),
        ProviderId::Codex if model.contains("astra") => Some([10.0, 40.0, 1.0, 10.0]),
        ProviderId::Codex if model.contains("sol") => Some([2.50, 15.0, 0.25, 2.50]),
        ProviderId::Codex if model.contains("terra") => Some([1.75, 14.0, 0.175, 1.75]),
        ProviderId::Codex if model.contains("luna") => Some([0.60, 2.40, 0.06, 0.60]),
        ProviderId::Antigravity if model.contains("neon") => Some([2.50, 15.0, 0.25, 2.50]),
        ProviderId::Antigravity if model.contains("argon") => Some([1.25, 10.0, 0.125, 1.25]),
        ProviderId::Antigravity if model.contains("hydrogen") || model.contains("flash") => {
            Some([0.30, 2.50, 0.03, 0.30])
        }
        _ => None,
    }
}

/// Estimated USD cost of one token snapshot, using the same price table as the usage dashboard.
pub(crate) fn estimate_tokens_cost(
    provider: ProviderId,
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    Tokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        reasoning_tokens: 0,
    }
    .estimated_cost(provider, model)
}

#[derive(Clone, Serialize)]
pub(crate) struct UsageResponse {
    window: UsageWindow,
    totals: Totals,
    providers: Vec<UsageProviderRow>,
    models: Vec<UsageModelRow>,
    buckets: Vec<UsageBucket>,
    scanned_at: String,
}

#[derive(Clone, Serialize)]
struct UsageWindow {
    days: u32,
    resolution: &'static str,
    since: String,
    until: String,
}

#[derive(Clone, Copy, Serialize)]
struct Totals {
    cost_usd: f64,
    total_tokens: u64,
    #[serde(flatten)]
    tokens: Tokens,
    sessions: usize,
    requests: u64,
}

#[derive(Clone, Serialize)]
struct UsageProviderRow {
    provider: &'static str,
    label: &'static str,
    color: &'static str,
    #[serde(flatten)]
    totals: Totals,
    cost_share: f64,
    token_share: f64,
}

#[derive(Clone, Serialize)]
struct UsageModelRow {
    provider: &'static str,
    model: String,
    #[serde(flatten)]
    totals: Totals,
    cost_share: f64,
    token_share: f64,
}

#[derive(Clone, Serialize)]
struct UsageBucket {
    key: String,
    label: String,
    total_cost_usd: f64,
    total_tokens: u64,
    by_provider: BTreeMap<&'static str, BucketProvider>,
}

#[derive(Clone, Default, Serialize)]
struct BucketProvider {
    cost_usd: f64,
    total_tokens: u64,
}

#[derive(Default)]
struct Rollup {
    cost: f64,
    tokens: Tokens,
    sessions: HashSet<usize>,
    requests: u64,
}

impl Rollup {
    fn record(&mut self, sample: &Sample, session: usize, cost: f64) {
        self.cost += cost;
        self.tokens.add(sample.tokens);
        self.sessions.insert(session);
        self.requests += 1;
    }

    fn totals(&self) -> Totals {
        Totals {
            cost_usd: self.cost,
            total_tokens: self.tokens.total(),
            tokens: self.tokens,
            sessions: self.sessions.len(),
            requests: self.requests,
        }
    }
}

fn provider_info(provider: ProviderId) -> (&'static str, &'static str, &'static str) {
    match provider {
        ProviderId::Claude => ("claude", "Claude Code", "#d97757"),
        ProviderId::Codex => ("codex", "Codex", "var(--text)"),
        ProviderId::Antigravity => ("antigravity", "Antigravity", "#8c7bd1"),
        ProviderId::OpenCode => ("open-code", "OpenCode", "#5b9bbd"),
        ProviderId::Mock => ("mock", "Mock", "#808080"),
    }
}

struct UsageAggregation<'a> {
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    step: Duration,
    totals: Rollup,
    providers: HashMap<ProviderId, Rollup>,
    models: HashMap<(ProviderId, &'a str), Rollup>,
    buckets: Vec<UsageBucket>,
}

impl<'a> UsageAggregation<'a> {
    fn record(&mut self, provider: ProviderId, session: usize, sample: &'a Sample) {
        if sample.at < self.since || sample.at > self.until {
            return;
        }
        let index = (sample.at - self.since).num_seconds() / self.step.num_seconds();
        let index = (index as usize).min(self.buckets.len() - 1);
        let bucket = &mut self.buckets[index];
        let cost = sample.tokens.estimated_cost(provider, &sample.model);
        self.totals.record(sample, session, cost);
        self.providers
            .entry(provider)
            .or_default()
            .record(sample, session, cost);
        self.models
            .entry((provider, &sample.model))
            .or_default()
            .record(sample, session, cost);
        let provider = bucket
            .by_provider
            .entry(provider_info(provider).0)
            .or_default();
        let tokens = sample.tokens.total();
        provider.cost_usd += cost;
        provider.total_tokens = provider.total_tokens.saturating_add(tokens);
        bucket.total_cost_usd += cost;
        bucket.total_tokens = bucket.total_tokens.saturating_add(tokens);
    }
}

fn aggregate<'a>(
    days: u32,
    offset: i32,
    until: DateTime<Utc>,
    files: impl Iterator<Item = &'a CachedFile>,
    runs: &'a [LocalRun],
) -> UsageResponse {
    let hourly = days == 1;
    let offset = Duration::minutes(i64::from(offset));
    let local_now = (until + offset).naive_utc();
    let (since, step, count, resolution) = if hourly {
        let hour = local_now
            .date()
            .and_hms_opt(local_now.hour(), 0, 0)
            .expect("hour is valid")
            .and_utc()
            - offset;
        (hour - Duration::hours(23), Duration::hours(1), 24, "hour")
    } else {
        let midnight = local_now
            .date()
            .and_hms_opt(0, 0, 0)
            .expect("midnight is valid")
            .and_utc()
            - offset;
        (
            midnight - Duration::days(i64::from(days - 1)),
            Duration::days(1),
            days,
            "day",
        )
    };
    let buckets = (0..count)
        .map(|index| {
            let start = since + step * index as i32;
            let (key, label) = if hourly {
                (rfc3339(start), (start + offset).format("%H:%M").to_string())
            } else {
                let day = (start + offset).format("%Y-%m-%d").to_string();
                (day.clone(), day)
            };
            UsageBucket {
                key,
                label,
                total_cost_usd: 0.0,
                total_tokens: 0,
                by_provider: BTreeMap::new(),
            }
        })
        .collect();
    let mut result = UsageAggregation {
        since,
        until,
        step,
        totals: Rollup::default(),
        providers: HashMap::new(),
        models: HashMap::new(),
        buckets,
    };
    let mut next_session = 0;
    for file in files {
        for sample in &file.samples {
            result.record(file.provider, next_session, sample);
        }
        next_session += 1;
    }
    let mut run_sessions = HashMap::new();
    for run in runs {
        let session = run_sessions.entry(&run.session).or_insert_with(|| {
            let id = next_session;
            next_session += 1;
            id
        });
        result.record(run.provider, *session, &run.sample);
    }
    let totals = result.totals.totals();
    let shares = |row: &Totals| {
        let cost = if totals.cost_usd > 0.0 {
            row.cost_usd / totals.cost_usd
        } else {
            0.0
        };
        let tokens = if totals.total_tokens > 0 {
            row.total_tokens as f64 / totals.total_tokens as f64
        } else {
            0.0
        };
        (cost, tokens)
    };
    let mut providers: Vec<_> = result
        .providers
        .into_iter()
        .map(|(id, usage)| {
            let (provider, label, color) = provider_info(id);
            let totals = usage.totals();
            let (cost_share, token_share) = shares(&totals);
            UsageProviderRow {
                provider,
                label,
                color,
                totals,
                cost_share,
                token_share,
            }
        })
        .collect();
    providers.sort_by(|a, b| {
        b.totals
            .cost_usd
            .total_cmp(&a.totals.cost_usd)
            .then(a.provider.cmp(b.provider))
    });
    let mut models: Vec<_> = result
        .models
        .into_iter()
        .filter_map(|((id, model), usage)| {
            let totals = usage.totals();
            if model == "<synthetic>" || (totals.total_tokens == 0 && totals.cost_usd == 0.0) {
                return None;
            }
            let (cost_share, token_share) = shares(&totals);
            Some(UsageModelRow {
                provider: provider_info(id).0,
                model: model.to_owned(),
                totals,
                cost_share,
                token_share,
            })
        })
        .collect();
    models.sort_by(|a, b| {
        b.totals
            .cost_usd
            .total_cmp(&a.totals.cost_usd)
            .then(a.provider.cmp(b.provider))
            .then(a.model.cmp(&b.model))
    });
    UsageResponse {
        window: UsageWindow {
            days,
            resolution,
            since: rfc3339(since),
            until: rfc3339(until),
        },
        totals,
        providers,
        models,
        buckets: result.buckets,
        scanned_at: rfc3339(until),
    }
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use t3_core::{AuthConfig, Run, RunStatus, ServerConfig, Store};
    use tower::ServiceExt;
    use uuid::Uuid;

    const CLAUDE: &str = r#"
{"timestamp":"2026-01-02T11:30:00Z","message":{"id":"a","model":"claude-sonnet","usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":4,"cache_creation_input_tokens":6}}}
{"timestamp":"2026-01-02T11:30:01Z","message":{"id":"a","model":"claude-sonnet","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":4,"cache_creation_input_tokens":6}}}
{"timestamp":"2026-01-02T11:30:02Z","message":{"id":"a","model":"claude-sonnet","usage":{"output_tokens":9}}}
{"timestamp":"2026-01-02T11:31:00Z","message":{"id":"b","model":"claude-haiku","usage":{"input_tokens":25,"cache_creation_input_tokens":null}}}
{"timestamp":"2026-01-02T11:32:00Z","message":{"id":"c","content":"no usage"}}
"#;

    const CODEX: &str = r#"
{"type":"session_meta","model":"gpt-6.2-sol"}
{"timestamp":"2026-01-02T11:30:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":3},"last_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":3}}}}
{"timestamp":"2026-01-02T11:30:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":3},"last_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":10,"reasoning_output_tokens":3}}}}
{"type":"turn_context","payload":{"model":"gpt-6.2-astra"}}
{"timestamp":"2026-01-02T12:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":180,"cached_input_tokens":120,"cache_write_input_tokens":8,"output_tokens":22,"reasoning_output_tokens":8}}}}
{"type":"event_msg","payload":{"type":"token_count","info":null}}
invalid json
"#;

    #[test]
    fn claude_streaming_snapshots_and_pricing() {
        let samples = parse_transcript(CLAUDE.as_bytes(), ProviderId::Claude).unwrap();
        assert_eq!(samples.len(), 2);
        let sample = &samples[0];
        assert_eq!(sample.model, "claude-sonnet");
        assert_eq!(sample.tokens.output_tokens, 20);
        assert_eq!(sample.tokens.total(), 130);
        assert!(
            (sample
                .tokens
                .estimated_cost(ProviderId::Claude, &sample.model)
                - 0.000_623_7)
                .abs()
                < 1e-12
        );
        assert!(
            (samples[1]
                .tokens
                .estimated_cost(ProviderId::Claude, "claude-haiku")
                - 0.000_02)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn codex_usage_uses_last_or_forward_delta_and_tracks_model() {
        let samples = parse_transcript(CODEX.as_bytes(), ProviderId::Codex).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].model, "gpt-6.2-sol");
        assert_eq!(samples[0].tokens.input_tokens, 20);
        assert_eq!(samples[0].tokens.total(), 110);
        let sample = &samples[1];
        assert_eq!(sample.model, "gpt-6.2-astra");
        assert_eq!(sample.tokens.input_tokens, 40);
        assert_eq!(sample.tokens.cache_read_tokens, 40);
        assert_eq!(sample.tokens.cache_write_tokens, 8);
        assert_eq!(sample.tokens.output_tokens, 12);
        assert_eq!(sample.tokens.reasoning_tokens, 5);
        assert_eq!(sample.tokens.total(), 100);
        assert!(
            (sample
                .tokens
                .estimated_cost(ProviderId::Codex, &sample.model)
                - 0.001)
                .abs()
                < 1e-12
        );
        assert_eq!(
            sample
                .tokens
                .estimated_cost(ProviderId::OpenCode, "local/model"),
            0.0
        );
    }

    #[test]
    fn hourly_and_daily_windows_include_empty_buckets() {
        let file = CachedFile {
            modified: None,
            len: 0,
            provider: ProviderId::Claude,
            samples: parse_transcript(CLAUDE.as_bytes(), ProviderId::Claude).unwrap(),
        };
        let now = "2026-01-02T12:30:00Z".parse().unwrap();
        let hourly = aggregate(1, 0, now, std::iter::once(&file), &[]);
        assert_eq!(hourly.window.resolution, "hour");
        assert_eq!(hourly.window.since, "2026-01-01T13:00:00Z");
        assert_eq!(hourly.buckets.len(), 24);
        assert_eq!(hourly.buckets[22].by_provider["claude"].total_tokens, 155);
        assert_eq!(hourly.buckets[23].label, "12:00");
        assert_eq!(hourly.buckets[23].total_tokens, 0);
        assert_eq!(hourly.buckets[0].total_tokens, 0);
        let daily = aggregate(7, 0, now, std::iter::once(&file), &[]);
        assert_eq!(daily.window.resolution, "day");
        assert_eq!(daily.buckets.len(), 7);
        assert_eq!(daily.buckets[0].key, "2025-12-27");
        assert_eq!(daily.buckets[6].key, "2026-01-02");
        assert_eq!(daily.totals.total_tokens, 155);
        assert_eq!(daily.totals.sessions, 1);
        assert_eq!(daily.totals.requests, 2);
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("t3-usage-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn usage_route_aggregates_local_sources_and_revalidates_on_refresh() {
        let home = Fixture::new();
        let projects = home.0.join(".claude/projects/fixture");
        fs::create_dir_all(&projects).unwrap();
        let now = Utc::now() - Duration::minutes(15);
        let line = |id: &str, output| {
            json!({
            "timestamp": now,
            "message": {"id": id, "model": "claude-opus", "usage": {"input_tokens": 10, "output_tokens": output}}
        }).to_string()
        };
        let log = projects.join("session.jsonl");
        fs::write(&log, line("first", 5)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&log, projects.join("symlink.jsonl")).unwrap();
        fs::write(projects.join("ignore.json"), line("ignored", 9_000)).unwrap();

        let mut state = ServerState::new(
            ServerConfig {
                data_dir: home.0.join("server"),
                auth: AuthConfig::Bearer {
                    token: "fixture-usage-token".into(),
                },
                ..Default::default()
            },
            "http://127.0.0.1:3774".into(),
        )
        .unwrap();
        state.usage_home = Some(home.0.clone());
        let project = state
            .store
            .create_project("fixture".into(), home.0.clone())
            .await
            .unwrap();
        let thread_id = Uuid::new_v4();
        state
            .store
            .create_thread(t3_core::Thread {
                id: thread_id,
                project_id: project.id,
                parent_thread_id: None,
                relationship: None,
                title: "fixture".into(),
                model_selection: Default::default(),
                runtime_mode: Default::default(),
                interaction_mode: Default::default(),
                status: Default::default(),
                worktree_path: None,
                branch: None,
                provider_session_id: None,
                archived: false,
                pinned: false,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
        for (ordinal, provider, model) in [
            (1, ProviderId::Antigravity, "gemini-neon"),
            (2, ProviderId::OpenCode, "local/example"),
            (3, ProviderId::Claude, "claude-opus"),
        ] {
            state
                .store
                .create_run(Run {
                    id: Uuid::new_v4(),
                    thread_id,
                    ordinal,
                    provider,
                    model: model.into(),
                    effort: None,
                    status: RunStatus::Completed,
                    prompt: String::new(),
                    provider_session_id: None,
                    input_tokens: Some(30),
                    output_tokens: Some(10),
                    duration_ms: Some(1),
                    error_message: None,
                    started_at: now,
                    completed_at: Some(now),
                })
                .await
                .unwrap();
        }
        let app = crate::router(state);
        let get = |query: &'static str| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(
                        Request::get(query)
                            .header("authorization", "Bearer fixture-usage-token")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
                serde_json::from_slice::<Value>(&body).unwrap()
            }
        };
        let first = get("/api/usage?days=7").await;
        assert_eq!(first["window"]["days"], 7);
        assert_eq!(first["totals"]["total_tokens"], 95);
        assert_eq!(first["totals"]["requests"], 3);
        assert_eq!(first["totals"]["sessions"], 2);
        assert_eq!(first["providers"][0]["provider"], "claude");
        assert_eq!(first["providers"][2]["provider"], "open-code");
        assert_eq!(first["models"][0]["model"], "claude-opus");
        assert_eq!(first["buckets"].as_array().unwrap().len(), 7);
        assert!(
            first["scanned_at"]
                .as_str()
                .unwrap()
                .parse::<DateTime<Utc>>()
                .is_ok()
        );
        let warm = get("/api/usage").await;
        assert_eq!(warm, first);

        fs::write(
            &log,
            format!("{}\n{}", line("first", 5), line("second", 30)),
        )
        .unwrap();
        let refreshed = get("/api/usage?days=7&refresh=1").await;
        assert_eq!(refreshed["totals"]["total_tokens"], 135);
        fs::remove_file(&log).unwrap();
        let removed = get("/api/usage?days=7&refresh=true").await;
        assert_eq!(removed["totals"]["total_tokens"], 80);
    }
}
