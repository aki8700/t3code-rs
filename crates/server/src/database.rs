use rusqlite::{Connection, OpenFlags, params};
use std::{path::PathBuf, time::Duration};
use t3_core::{CoreError, CoreResult, OrchModelStat, ProviderId, RunStatus, ServerConfig};
use uuid::Uuid;

pub(crate) async fn with_db<T: Send + 'static>(
    path: PathBuf,
    query: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
) -> CoreResult<T> {
    tokio::task::spawn_blocking(move || {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.busy_timeout(Duration::from_secs(5))?;
        query(&db)
    })
    .await
    .map_err(|error| CoreError::Internal(error.to_string()))?
    .map_err(|error| CoreError::Storage(error.to_string()))
}

fn encode<T: serde::Serialize>(value: T) -> CoreResult<String> {
    serde_json::to_string(&value).map_err(|error| CoreError::Internal(error.to_string()))
}

/// Provider statistics omit the ordinal-zero rows that anchor thread metadata items and the
/// runs imported from T3 Code, whose ids are UUIDv5. The success rate counts finished runs only.
pub async fn orchestration_stats(config: &ServerConfig) -> CoreResult<Vec<OrchModelStat>> {
    let completed = encode(RunStatus::Completed)?;
    let failed = encode(RunStatus::Failed)?;
    let rate_limited = encode(RunStatus::RateLimited)?;
    with_db(config.db_path(), move |db| {
        let mut query = db.prepare(
            "SELECT provider, model, count(*), sum(status = ?1), sum(status = ?2),
                coalesce(1.0 * sum(status = ?1) / nullif(sum(status IN (?1, ?2, ?3)), 0), 0.0),
                coalesce(avg(duration_ms), 0.0),
                coalesce(sum(input_tokens), 0), coalesce(sum(output_tokens), 0),
                coalesce(sum(cache_read_tokens), 0), coalesce(sum(cache_creation_tokens), 0),
                coalesce(sum(reasoning_tokens), 0)
             FROM runs WHERE ordinal <> 0 AND substr(id, 15, 1) <> '5'
             GROUP BY provider, model ORDER BY count(*) DESC, provider, model",
        )?;
        query
            .query_map(params![completed, failed, rate_limited], |row| {
                let provider =
                    serde_json::from_str(&row.get::<_, String>(0)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                Ok(OrchModelStat {
                    provider,
                    model: row.get(1)?,
                    total_runs: row.get(2)?,
                    completed_runs: row.get(3)?,
                    failed_runs: row.get(4)?,
                    success_rate: row.get(5)?,
                    avg_duration_ms: row.get(6)?,
                    total_input_tokens: row.get(7)?,
                    total_output_tokens: row.get(8)?,
                    total_cache_read_tokens: row.get(9)?,
                    total_cache_creation_tokens: row.get(10)?,
                    total_reasoning_tokens: row.get(11)?,
                })
            })?
            .collect()
    })
    .await
}

/// The token columns of one `runs` row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RunTokens {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_creation: Option<u64>,
    pub reasoning: Option<u64>,
}

impl RunTokens {
    /// This run's share of a session-cumulative report, given the shares stored for the
    /// session's earlier runs. A report below that sum means the provider restarted its count.
    pub(crate) fn since(self, stored: RunTokens) -> RunTokens {
        let share = |now: Option<u64>, before: Option<u64>| match now {
            Some(now) => now.checked_sub(before.unwrap_or(0)).map(Some),
            None => Some(None),
        };
        let (Some(input), Some(output), Some(cache_read), Some(cache_creation), Some(reasoning)) = (
            share(self.input, stored.input),
            share(self.output, stored.output),
            share(self.cache_read, stored.cache_read),
            share(self.cache_creation, stored.cache_creation),
            share(self.reasoning, stored.reasoning),
        ) else {
            return self;
        };
        RunTokens {
            input,
            output,
            cache_read,
            cache_creation,
            reasoning,
        }
    }
}

/// Sums the tokens stored for the other runs of one provider session.
pub(crate) async fn session_tokens_except(
    config: &ServerConfig,
    provider: ProviderId,
    session_id: String,
    run_id: Uuid,
) -> CoreResult<RunTokens> {
    let provider = encode(provider)?;
    with_db(config.db_path(), move |db| {
        db.query_row(
            "SELECT sum(input_tokens), sum(output_tokens), sum(cache_read_tokens),
                sum(cache_creation_tokens), sum(reasoning_tokens)
             FROM runs WHERE provider = ?1 AND provider_session_id = ?2 AND id <> ?3",
            params![provider, session_id, run_id.to_string()],
            |row| {
                Ok(RunTokens {
                    input: row.get(0)?,
                    output: row.get(1)?,
                    cache_read: row.get(2)?,
                    cache_creation: row.get(3)?,
                    reasoning: row.get(4)?,
                })
            },
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (Fixture, ServerConfig) {
        let dir = std::env::temp_dir().join(format!("t3-database-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = ServerConfig {
            data_dir: dir.clone(),
            ..Default::default()
        };
        Connection::open(config.db_path())
            .unwrap()
            .execute_batch(
                "CREATE TABLE runs (
                    id TEXT PRIMARY KEY, ordinal INTEGER, provider TEXT, model TEXT, status TEXT,
                    provider_session_id TEXT, input_tokens INTEGER, output_tokens INTEGER,
                    cache_read_tokens INTEGER, cache_creation_tokens INTEGER,
                    reasoning_tokens INTEGER, duration_ms INTEGER)",
            )
            .unwrap();
        (Fixture(dir), config)
    }

    fn insert(config: &ServerConfig, id: Uuid, status: RunStatus, session: &str, input: u64) {
        Connection::open(config.db_path())
            .unwrap()
            .execute(
                "INSERT INTO runs VALUES (?1, 1, ?2, 'm', ?3, ?4, ?5, ?6, NULL, NULL, NULL, 10)",
                params![
                    id.to_string(),
                    encode(ProviderId::Antigravity).unwrap(),
                    encode(status).unwrap(),
                    session,
                    input,
                    input / 10,
                ],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn success_rate_counts_finished_native_runs_only() {
        let (_dir, config) = fixture();
        for status in [
            RunStatus::Completed,
            RunStatus::Completed,
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::RateLimited,
            RunStatus::Cancelled,
            RunStatus::Running,
        ] {
            insert(&config, Uuid::new_v4(), status, "s", 0);
        }
        let imported = Uuid::parse_str("6ba7b810-9dad-51d1-80b4-00c04fd430c8").unwrap();
        assert_eq!(imported.get_version_num(), 5);
        insert(&config, imported, RunStatus::Failed, "legacy", 0);
        let stats = orchestration_stats(&config).await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(
            (
                stats[0].total_runs,
                stats[0].completed_runs,
                stats[0].failed_runs
            ),
            (7, 3, 1)
        );
        assert!((stats[0].success_rate - 3.0 / 5.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn session_tokens_sum_the_other_runs_of_that_session() {
        let (_dir, config) = fixture();
        let current = Uuid::new_v4();
        insert(&config, Uuid::new_v4(), RunStatus::Completed, "agy-1", 100);
        insert(&config, Uuid::new_v4(), RunStatus::Failed, "agy-1", 50);
        insert(&config, Uuid::new_v4(), RunStatus::Completed, "agy-2", 7000);
        insert(&config, current, RunStatus::Running, "agy-1", 9000);
        let stored =
            session_tokens_except(&config, ProviderId::Antigravity, "agy-1".into(), current)
                .await
                .unwrap();
        assert_eq!((stored.input, stored.output), (Some(150), Some(15)));
        let none =
            session_tokens_except(&config, ProviderId::Antigravity, "unseen".into(), current)
                .await
                .unwrap();
        assert_eq!(none, RunTokens::default());
    }

    #[test]
    fn cumulative_report_minus_stored_shares_is_the_run_share() {
        let report = RunTokens {
            input: Some(980_175),
            output: Some(152_244),
            cache_read: None,
            ..Default::default()
        };
        let stored = RunTokens {
            input: Some(976_582),
            output: Some(151_683),
            cache_read: Some(5),
            ..Default::default()
        };
        assert_eq!(
            report.since(stored),
            RunTokens {
                input: Some(3_593),
                output: Some(561),
                ..Default::default()
            }
        );
        assert_eq!(report.since(RunTokens::default()), report);
        let restarted = RunTokens {
            input: Some(1_200),
            output: Some(900),
            ..Default::default()
        };
        assert_eq!(restarted.since(stored), restarted);
    }
}
