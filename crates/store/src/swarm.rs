use super::*;
use t3_core::{SwarmBoardSnapshot, SwarmMember, SwarmMessage};

const SNAPSHOT_MESSAGES: usize = 50;

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
        self.with_writer(move |connection| {
            let tx = connection.transaction().map_err(storage_error)?;
            let created_at = Utc::now();
            tx.execute(
                "INSERT INTO swarm_messages (board_id, sender, text, mentions_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    board_id.to_string(),
                    sender,
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
