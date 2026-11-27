use std::{path::Path, time::Duration as StdDuration};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, params};
use t3_core::{CoreError, CoreResult, ProviderId};

use super::{Sample, pricing::Tokens};

pub(super) struct LocalRun {
    pub(super) provider: ProviderId,
    pub(super) session: String,
    pub(super) sample: Sample,
}

pub(super) fn read_local_runs(path: &Path, since: DateTime<Utc>) -> CoreResult<Vec<LocalRun>> {
    let result = (|| -> rusqlite::Result<Vec<LocalRun>> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_timeout(StdDuration::from_secs(5))?;
        let mut query = db.prepare(
            "SELECT provider, thread_id, model, input_tokens, output_tokens, cache_read_tokens,
                cache_creation_tokens, reasoning_tokens, coalesce(completed_at, started_at)
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
                    let input_tokens = row.get::<_, Option<u64>>(3)?.unwrap_or_default();
                    let cache_read_tokens = row.get::<_, Option<u64>>(5)?.unwrap_or_default();
                    let cache_creation_tokens = row.get::<_, Option<u64>>(6)?.unwrap_or_default();
                    let at =
                        row.get_ref(8)?
                            .as_str()?
                            .parse::<DateTime<Utc>>()
                            .map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    8,
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
                                input_tokens: input_tokens
                                    .saturating_sub(cache_read_tokens)
                                    .saturating_sub(cache_creation_tokens),
                                output_tokens: row.get::<_, Option<u64>>(4)?.unwrap_or_default(),
                                cache_read_tokens,
                                cache_write_tokens: cache_creation_tokens,
                                reasoning_tokens: row.get::<_, Option<u64>>(7)?.unwrap_or_default(),
                            },
                        },
                    })
                },
            )?
            .collect()
    })();
    result.map_err(|error| CoreError::Storage(error.to_string()))
}
