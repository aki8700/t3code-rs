use super::*;
use t3_core::{InteractionMode, RuntimeMode, SwarmBoardSnapshot, SwarmMember, SwarmMessage};

const SNAPSHOT_MESSAGES: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmWakeDeliveryRow {
    pub thread_id: Uuid,
    pub run_id: Uuid,
    pub from_seq: i64,
    pub to_seq: i64,
    pub redeliveries: u8,
    pub wake_depth: u8,
    pub mode_ceiling: Option<(RuntimeMode, InteractionMode)>,
}

impl SqliteStore {
    pub async fn ensure_swarm_board(&self, coordinator: Thread) -> CoreResult<SwarmBoardSnapshot> {
        self.with_writer(move |connection| {
            let tx = connection.transaction().map_err(storage_error)?;
            let thread = coordinator.id.to_string();
            let now = timestamp(Utc::now());
            tx.execute(
                "INSERT INTO swarm_boards (id, coordinator_thread_id, title, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4) ON CONFLICT(coordinator_thread_id) DO NOTHING",
                params![Uuid::new_v4().to_string(), thread, coordinator.title, now],
            ).map_err(storage_error)?;
            tx.execute(
                "INSERT INTO swarm_members (board_id, name, thread_id, provider, model, effort, state, role, created_at)
                 SELECT id, 'coordinator', ?1, ?2, ?3, ?4, 'waiting', 'coordinator', ?5
                 FROM swarm_boards WHERE coordinator_thread_id = ?1
                 ON CONFLICT(board_id, name) DO NOTHING",
                params![thread, encode(&coordinator.model_selection.provider)?,
                    coordinator.model_selection.model, coordinator.model_selection.effort, now],
            ).map_err(storage_error)?;
            let board = board_by_thread(&tx, coordinator.id)?
                .ok_or_else(|| storage_error("swarm board disappeared"))?;
            tx.commit().map_err(storage_error)?;
            Ok(board)
        }).await
    }

    pub async fn get_swarm_board_by_thread(
        &self,
        thread_id: Uuid,
    ) -> CoreResult<Option<SwarmBoardSnapshot>> {
        self.with_reader(move |connection| {
            let snapshot = connection.unchecked_transaction().map_err(storage_error)?;
            let board = board_by_thread(&snapshot, thread_id)?;
            snapshot.commit().map_err(storage_error)?;
            Ok(board)
        })
        .await
    }

    pub async fn upsert_swarm_member(&self, member: SwarmMember) -> CoreResult<()> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "INSERT INTO swarm_members (board_id, name, thread_id, provider, model, effort,
                    state, role, last_read_seq, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(board_id, name) DO UPDATE SET provider = excluded.provider,
                    model = excluded.model, effort = excluded.effort
                 WHERE swarm_members.thread_id = excluded.thread_id",
                    params![
                        member.board_id.to_string(),
                        member.name,
                        member.thread_id.to_string(),
                        encode(&member.provider)?,
                        member.model,
                        member.effort,
                        member.state,
                        member.role,
                        member.last_read_seq,
                        timestamp(member.created_at)
                    ],
                )
                .map_err(storage_error)
                .and_then(|changed| {
                    if changed == 0 {
                        Err(CoreError::InvalidArgument(
                            "swarm name is already in use".into(),
                        ))
                    } else {
                        Ok(())
                    }
                })
        })
        .await
    }

    pub async fn update_swarm_member_state(
        &self,
        thread_id: Uuid,
        state: &str,
    ) -> CoreResult<bool> {
        let state = state.to_owned();
        self.with_writer(move |connection| {
            connection
                .execute(
                    "UPDATE swarm_members SET state = ?2
                 WHERE thread_id = ?1 AND state != ?2 AND state != 'killed'",
                    params![thread_id.to_string(), state],
                )
                .map(|changed| changed != 0)
                .map_err(storage_error)
        })
        .await
    }

    /// Marks posts `(from, to]` read. The cursor moves only when it already covers `from`,
    /// so an earlier range that was never acknowledged stays unread.
    pub async fn ack_swarm_member_read_seq(
        &self,
        thread_id: Uuid,
        from: i64,
        to: i64,
    ) -> CoreResult<bool> {
        self.with_writer(move |connection| {
            connection
                .execute(
                    "UPDATE swarm_members SET last_read_seq = ?3
                     WHERE thread_id = ?1 AND last_read_seq >= ?2 AND last_read_seq < ?3",
                    params![thread_id.to_string(), from, to],
                )
                .map(|changed| changed != 0)
                .map_err(storage_error)
        })
        .await
    }

    pub async fn append_swarm_message(
        &self,
        board_id: Uuid,
        sender: String,
        text: String,
        mentions: Vec<String>,
    ) -> CoreResult<SwarmMessage> {
        self.append_swarm_message_with_run(board_id, sender, text, mentions, None)
            .await
    }

    pub async fn append_swarm_message_with_run(
        &self,
        board_id: Uuid,
        sender: String,
        text: String,
        mentions: Vec<String>,
        sender_run_id: Option<Uuid>,
    ) -> CoreResult<SwarmMessage> {
        self.with_writer(move |connection| {
            let tx = connection.transaction().map_err(storage_error)?;
            let created_at = Utc::now();
            tx.execute(
                "INSERT INTO swarm_messages (board_id, sender, sender_run_id, text, mentions_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    board_id.to_string(),
                    sender,
                    sender_run_id.map(|id| id.to_string()),
                    text,
                    encode(&mentions)?,
                    timestamp(created_at)
                ],
            )
            .map_err(storage_error)?;
            let seq = tx.last_insert_rowid();
            tx.execute(
                "UPDATE swarm_boards SET updated_at = ?2 WHERE id = ?1",
                params![board_id.to_string(), timestamp(created_at)],
            )
            .map_err(storage_error)?;
            tx.commit().map_err(storage_error)?;
            Ok(SwarmMessage {
                seq,
                board_id,
                sender,
                text,
                mentions,
                created_at,
            })
        })
        .await
    }

    pub async fn worker_posted_for_run(
        &self,
        board_id: Uuid,
        sender: &str,
        run_id: Uuid,
        since: DateTime<Utc>,
    ) -> CoreResult<bool> {
        let sender = sender.to_owned();
        self.with_reader(move |connection| {
            let found: Option<i64> = connection
                .query_row(
                    "SELECT 1 FROM swarm_messages
                     WHERE board_id = ?1 AND sender = ?2
                       AND (sender_run_id = ?3 OR created_at >= ?4)
                     LIMIT 1",
                    params![
                        board_id.to_string(),
                        sender,
                        run_id.to_string(),
                        timestamp(since)
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage_error)?;
            Ok(found.is_some())
        })
        .await
    }

    pub async fn upsert_swarm_wake_delivery(&self, row: SwarmWakeDeliveryRow) -> CoreResult<()> {
        self.with_writer(move |connection| {
            let mode_ceiling_json = row
                .mode_ceiling
                .as_ref()
                .map(encode)
                .transpose()?;
            connection
                .execute(
                    "INSERT INTO swarm_wake_deliveries (
                        thread_id, run_id, from_seq, to_seq, redeliveries, wake_depth, mode_ceiling_json, created_at
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                    ON CONFLICT(thread_id, run_id, from_seq, to_seq) DO UPDATE SET
                        redeliveries = excluded.redeliveries,
                        wake_depth = excluded.wake_depth,
                        mode_ceiling_json = excluded.mode_ceiling_json",
                    params![
                        row.thread_id.to_string(),
                        row.run_id.to_string(),
                        row.from_seq,
                        row.to_seq,
                        i64::from(row.redeliveries),
                        i64::from(row.wake_depth),
                        mode_ceiling_json,
                        timestamp(Utc::now()),
                    ],
                )
                .map_err(storage_error)?;
            Ok(())
        })
        .await
    }

    pub async fn list_swarm_wake_deliveries(
        &self,
        thread_id: Uuid,
    ) -> CoreResult<Vec<SwarmWakeDeliveryRow>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT thread_id, run_id, from_seq, to_seq, redeliveries, wake_depth, mode_ceiling_json
                 FROM swarm_wake_deliveries
                 WHERE thread_id = ?1
                 ORDER BY rowid ASC",
                [thread_id.to_string()],
                |row| {
                    let redeliveries: i64 = row.get(4)?;
                    let wake_depth: i64 = row.get(5)?;
                    let mode_ceiling_json: Option<String> = row.get(6)?;
                    let mode_ceiling = mode_ceiling_json
                        .as_deref()
                        .map(|raw| {
                            serde_json::from_str(raw).map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    6,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })
                        })
                        .transpose()?;
                    Ok(SwarmWakeDeliveryRow {
                        thread_id: parse(row, 0)?,
                        run_id: parse(row, 1)?,
                        from_seq: row.get(2)?,
                        to_seq: row.get(3)?,
                        redeliveries: u8::try_from(redeliveries).unwrap_or(u8::MAX),
                        wake_depth: u8::try_from(wake_depth).unwrap_or(u8::MAX),
                        mode_ceiling,
                    })
                },
            )
        })
        .await
    }

    pub async fn replace_swarm_wake_deliveries(
        &self,
        thread_id: Uuid,
        rows: Vec<SwarmWakeDeliveryRow>,
    ) -> CoreResult<()> {
        self.with_writer(move |connection| {
            let tx = connection.transaction().map_err(storage_error)?;
            tx.execute(
                "DELETE FROM swarm_wake_deliveries WHERE thread_id = ?1",
                [thread_id.to_string()],
            )
            .map_err(storage_error)?;
            let now = timestamp(Utc::now());
            for row in rows {
                let mode_ceiling_json = row
                    .mode_ceiling
                    .as_ref()
                    .map(encode)
                    .transpose()?;
                tx.execute(
                    "INSERT INTO swarm_wake_deliveries (
                        thread_id, run_id, from_seq, to_seq, redeliveries, wake_depth, mode_ceiling_json, created_at
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        row.thread_id.to_string(),
                        row.run_id.to_string(),
                        row.from_seq,
                        row.to_seq,
                        i64::from(row.redeliveries),
                        i64::from(row.wake_depth),
                        mode_ceiling_json,
                        &now,
                    ],
                )
                .map_err(storage_error)?;
            }
            tx.commit().map_err(storage_error)?;
            Ok(())
        })
        .await
    }

    pub async fn list_swarm_messages(
        &self,
        board_id: Uuid,
        since: i64,
        limit: usize,
    ) -> CoreResult<Vec<SwarmMessage>> {
        self.with_reader(move |connection| {
            query_all(
                connection,
                "SELECT seq, board_id, sender, text, mentions_json, created_at
             FROM swarm_messages WHERE board_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                params![board_id.to_string(), since, sql_limit(limit)],
                message_row,
            )
        })
        .await
    }

    pub async fn count_swarm_messages(&self, board_id: Uuid) -> CoreResult<usize> {
        self.with_reader(move |connection| {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM swarm_messages WHERE board_id = ?1",
                    [board_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            usize::try_from(count).map_err(storage_error)
        })
        .await
    }
}

fn board_by_thread(
    connection: &Connection,
    thread_id: Uuid,
) -> CoreResult<Option<SwarmBoardSnapshot>> {
    let board = connection.query_row(
        "SELECT b.id, b.coordinator_thread_id, b.title
         FROM swarm_members AS m JOIN swarm_boards AS b ON b.id = m.board_id WHERE m.thread_id = ?1",
        [thread_id.to_string()],
        |row| Ok((parse::<Uuid>(row, 0)?, parse(row, 1)?, row.get(2)?)),
    ).optional().map_err(storage_error)?;
    let Some((board_id, coordinator_thread_id, title)) = board else {
        return Ok(None);
    };
    let members = query_all(
        connection,
        "SELECT board_id, name, thread_id, provider, model, effort, state, role, last_read_seq, created_at
         FROM swarm_members WHERE board_id = ?1 ORDER BY role, created_at, name",
        [board_id.to_string()],
        |row| Ok(SwarmMember {
            board_id: parse(row, 0)?, name: row.get(1)?, thread_id: parse(row, 2)?,
            provider: decode(row, 3)?, model: row.get(4)?, effort: row.get(5)?,
            state: row.get(6)?, role: row.get(7)?, last_read_seq: row.get(8)?, created_at: parse(row, 9)?,
        }),
    )?;
    let mut messages = query_all(
        connection,
        "SELECT seq, board_id, sender, text, mentions_json, created_at
         FROM swarm_messages WHERE board_id = ?1 ORDER BY seq DESC LIMIT ?2",
        params![board_id.to_string(), sql_limit(SNAPSHOT_MESSAGES)],
        message_row,
    )?;
    let head_seq = messages.first().map_or(0, |message| message.seq);
    messages.reverse();
    Ok(Some(SwarmBoardSnapshot {
        board_id,
        coordinator_thread_id,
        title,
        members,
        messages,
        head_seq,
    }))
}

fn message_row(row: &Row<'_>) -> rusqlite::Result<SwarmMessage> {
    Ok(SwarmMessage {
        seq: row.get(0)?,
        board_id: parse(row, 1)?,
        sender: row.get(2)?,
        text: row.get(3)?,
        mentions: decode(row, 4)?,
        created_at: parse(row, 5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::path::PathBuf;

    fn test_thread(project_id: Uuid, title: &str) -> Thread {
        let now = Utc::now();
        Thread {
            id: Uuid::new_v4(),
            project_id,
            title: title.into(),
            status: ThreadStatus::Idle,
            model_selection: ModelSelection {
                provider: ProviderId::Codex,
                model: "test".into(),
                effort: None,
                agent: None,
            },
            runtime_mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Default,
            worktree_path: None,
            branch: None,
            pinned: false,
            provider_session_id: None,
            parent_thread_id: None,
            relationship: None,
            archived: false,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn wake_delivery_persistence_roundtrip_and_replace() {
        let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
        let thread_id = Uuid::new_v4();
        let run1 = Uuid::new_v4();
        let run2 = Uuid::new_v4();

        let row1 = SwarmWakeDeliveryRow {
            thread_id,
            run_id: run1,
            from_seq: 1,
            to_seq: 3,
            redeliveries: 0,
            wake_depth: 1,
            mode_ceiling: Some((RuntimeMode::FullAccess, InteractionMode::Default)),
        };
        let row2 = SwarmWakeDeliveryRow {
            thread_id,
            run_id: run2,
            from_seq: 4,
            to_seq: 5,
            redeliveries: 1,
            wake_depth: 2,
            mode_ceiling: None,
        };

        store
            .upsert_swarm_wake_delivery(row1.clone())
            .await
            .unwrap();
        store
            .upsert_swarm_wake_delivery(row2.clone())
            .await
            .unwrap();
        let listed = store.list_swarm_wake_deliveries(thread_id).await.unwrap();
        assert_eq!(listed, vec![row1, row2.clone()]);

        let mut updated = row2;
        updated.redeliveries = 2;
        store
            .replace_swarm_wake_deliveries(thread_id, vec![updated.clone()])
            .await
            .unwrap();
        let after_replace = store.list_swarm_wake_deliveries(thread_id).await.unwrap();
        assert_eq!(after_replace, vec![updated]);
    }

    #[tokio::test]
    async fn worker_posted_for_run_matches_run_id_even_with_clock_skew() {
        let store = SqliteStore::open_in_memory(StorageConfig::default()).unwrap();
        let project = store
            .create_project("p".into(), PathBuf::from("/tmp/p"))
            .await
            .unwrap();
        let coord = test_thread(project.id, "coordinator");
        store.create_thread(coord.clone()).await.unwrap();
        let board = store.ensure_swarm_board(coord).await.unwrap();

        let run_a = Uuid::new_v4();
        let run_b = Uuid::new_v4();
        let future_since = Utc::now() + Duration::hours(1);

        assert!(
            !store
                .worker_posted_for_run(board.board_id, "worker-1", run_a, future_since)
                .await
                .unwrap()
        );
        store
            .append_swarm_message_with_run(
                board.board_id,
                "worker-1".into(),
                "@coordinator [done] result: ok".into(),
                vec!["coordinator".into()],
                Some(run_a),
            )
            .await
            .unwrap();
        assert!(
            store
                .worker_posted_for_run(board.board_id, "worker-1", run_a, future_since)
                .await
                .unwrap()
        );
        assert!(
            !store
                .worker_posted_for_run(board.board_id, "worker-1", run_b, future_since)
                .await
                .unwrap()
        );
    }
}
