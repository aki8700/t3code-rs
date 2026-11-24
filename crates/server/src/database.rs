use rusqlite::{Connection, OpenFlags, params};
use std::{path::PathBuf, time::Duration};
use t3_core::{CoreError, CoreResult, OrchModelStat, RunStatus, ServerConfig};

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

/// Provider statistics omit the ordinal-zero rows that anchor thread metadata items.
pub async fn orchestration_stats(config: &ServerConfig) -> CoreResult<Vec<OrchModelStat>> {
    let encode = |status| {
        serde_json::to_string(&status).map_err(|error| CoreError::Internal(error.to_string()))
    };
    let completed = encode(RunStatus::Completed)?;
    let failed = encode(RunStatus::Failed)?;
    with_db(config.db_path(), move |db| {
        let mut query = db.prepare(
            "SELECT provider, model, count(*), sum(status = ?1), sum(status = ?2),
                avg(status = ?1), coalesce(avg(duration_ms), 0.0),
                coalesce(sum(input_tokens), 0), coalesce(sum(output_tokens), 0)
             FROM runs WHERE ordinal <> 0
             GROUP BY provider, model ORDER BY count(*) DESC, provider, model",
        )?;
        query
            .query_map(params![completed, failed], |row| {
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
                })
            })?
            .collect()
    })
    .await
}
