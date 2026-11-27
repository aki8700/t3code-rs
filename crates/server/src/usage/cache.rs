use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader},
    path::{Path, PathBuf},
    time::{Duration as StdDuration, Instant, SystemTime},
};

use chrono::{DateTime, Duration, Utc};
use t3_core::{CoreResult, ProviderId};

#[cfg(unix)]
use super::transcript_open_flags;
use super::{
    Sample, SessionContextUsage, UsageResponse,
    aggregate::aggregate,
    codex::find_codex_session_file,
    local_runs::{LocalRun, read_local_runs},
    session::{find_claude_session_file, read_session_transcript},
    transcript::parse_transcript,
};

const SNAPSHOT_TTL: StdDuration = StdDuration::from_secs(2);
const SESSION_CTX_CACHE_MAX: usize = 256;
const SESSION_CTX_MISS_TTL: StdDuration = StdDuration::from_secs(10);

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

pub(super) struct CachedFile {
    pub(super) modified: Option<SystemTime>,
    pub(super) len: u64,
    pub(super) provider: ProviderId,
    pub(super) samples: Vec<Sample>,
}

struct UsageSnapshot {
    scanned_at: DateTime<Utc>,
    stamp: Instant,
    runs: Vec<LocalRun>,
    reports: HashMap<(u32, i32), UsageResponse>,
}

impl UsageScanCache {
    pub(super) fn report(
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
