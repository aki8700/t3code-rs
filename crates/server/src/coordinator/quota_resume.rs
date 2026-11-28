//! Resume a terminal quota-limited swarm turn, including after a daemon restart.
use super::*;
use chrono::{DateTime, Duration, Local, NaiveTime, TimeZone};
use rusqlite::OptionalExtension;
use tokio::sync::oneshot;

pub(super) const CONTEXT_NOTICE: &str = "利用枠のリセット予定を記録しました";
const RESUME: &str = "[system] 利用枠が戻ったので再開します。";
pub(super) type Timers = StdMutex<HashMap<Uuid, (Uuid, oneshot::Sender<()>)>>;

pub(super) fn deadline(message: &str, ended: DateTime<Utc>) -> DateTime<Utc> {
    message_deadline(message, ended.with_timezone(&Local)).unwrap_or(ended + Duration::minutes(30))
}

fn message_deadline<T: TimeZone>(message: &str, ended: DateTime<T>) -> Option<DateTime<Utc>> {
    // Prefer the absolute timestamp appended by the driver or persisted at completion.
    for (offset, key) in message.rmatch_indices("reset_at") {
        let tail = &message[offset + key.len()..];
        let raw = tail
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '=' | '"'))
            .split(|c: char| c.is_whitespace() || matches!(c, ')' | '}' | ']' | ',' | ';' | '"'))
            .next()?;
        if let Some(reset) = crate::provider_hold::parse_reset_at(raw) {
            return Some(reset);
        }
    }
    let lower = message.to_ascii_lowercase();
    if let Some((_, mut rest)) = lower.split_once("resets in ") {
        let mut seconds = 0i64;
        let mut found = false;
        loop {
            rest = rest.trim_start();
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let Some(unit) = rest.as_bytes().get(digits) else {
                break;
            };
            let scale = match unit {
                b'h' => 3600,
                b'm' => 60,
                b's' => 1,
                _ => break,
            };
            let count = rest[..digits].parse::<i64>().ok()?;
            seconds = seconds.checked_add(count.checked_mul(scale)?)?;
            found = true;
            rest = &rest[digits + 1..];
        }
        if found {
            return ended
                .to_utc()
                .checked_add_signed(Duration::try_seconds(seconds)?);
        }
    }
    let (_, clock) = lower.split_once("try again at ")?;
    let mut words = clock.split_whitespace();
    let time = words.next()?;
    let period = words
        .next()?
        .trim_end_matches(|c: char| !c.is_ascii_alphabetic());
    let time = NaiveTime::parse_from_str(&format!("{time} {period}"), "%I:%M %p").ok()?;
    let mut day = ended.date_naive();
    if time < ended.time() {
        day = day.succ_opt()?;
    }
    ended
        .timezone()
        .from_local_datetime(&day.and_time(time))
        .earliest()
        .map(|t| t.to_utc())
}

impl RunCoordinator {
    pub(super) fn quota_reset_time(
        &self,
        selection: &ModelSelection,
        message: &str,
        outcome: &Outcome,
        now: DateTime<Utc>,
    ) -> DateTime<Utc> {
        let reset = outcome
            .reset_at
            .as_deref()
            .and_then(crate::provider_hold::parse_reset_at)
            .or_else(|| message_deadline(message, now.with_timezone(&Local)))
            .or(outcome.limited_until)
            .filter(|reset| *reset > now);
        self.0
            .holds
            .blocking(selection, None)
            .map(|(hold, _)| hold.until)
            .into_iter()
            .chain(reset)
            .max()
            .unwrap_or(now + Duration::minutes(30))
    }

    pub(super) fn cancel_quota_timer(&self, thread_id: Uuid) {
        self.0
            .quota_timers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&thread_id);
    }

    pub(super) fn schedule_quota_resume(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
        until: DateTime<Utc>,
    ) {
        if !self.0.settings.get().auto_resume_limited_threads {
            return;
        }
        let (cancel, cancelled) = oneshot::channel();
        self.0
            .quota_timers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(thread_id, (run_id, cancel));
        let weak = Arc::downgrade(&self.0);
        let delay = (until - Utc::now()).to_std().unwrap_or_default();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = cancelled => return,
                _ = tokio::time::sleep(delay) => {}
            }
            if let Some(inner) = weak.upgrade()
                && let Err(error) = RunCoordinator(inner)
                    .resume_limited_member(thread_id, run_id)
                    .await
            {
                tracing::warn!(%error,%thread_id,%run_id,"Could not resume quota-limited swarm member");
            }
        });
    }

    async fn resume_limited_member(&self, thread_id: Uuid, run_id: Uuid) -> CoreResult<()> {
        let guard = self.lock_thread(thread_id).await;
        {
            let mut timers = self
                .0
                .quota_timers
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !timers
                .get(&thread_id)
                .is_some_and(|(id, timer)| *id == run_id && timer.is_closed())
            {
                return Ok(());
            }
            timers.remove(&thread_id);
        }
        if self.ensure_accepting_runs().is_err()
            || !self.0.settings.get().auto_resume_limited_threads
            || self.active().contains_key(&thread_id)
        {
            return Ok(());
        }
        let Some(run) = self.limited_turns(Some(thread_id)).await?.pop() else {
            return Ok(());
        };
        if run.id != run_id {
            return Ok(());
        }
        let thread = self.0.store.get_thread(thread_id).await?;
        let context = self.quota_prompt_context(thread_id, run_id).await?;
        let selection = context.model_selection.unwrap_or(run.model_selection);
        if let Some((hold, _)) = self.0.holds.blocking(&selection, None) {
            self.schedule_quota_resume(thread_id, run_id, hold.until);
            return Ok(());
        }
        let instruction = if run.role == "coordinator" {
            "swarm_read_board で未読の掲示板を読み、中断前の依頼を続けてください。"
        } else {
            "中断前の依頼を続けてください。"
        };
        let prompt = if run.prompt.starts_with(RESUME) {
            run.prompt
        } else {
            format!("{RESUME} {instruction}\n\n{}", run.prompt)
        };
        self.send_locked(
            &guard,
            thread,
            Message {
                prompt,
                behavior: Some(FollowUpBehavior::Queue),
                auto_wake: true,
                mode_ceiling: context.mode_ceiling,
                attachment_ids: context.attachment_ids,
                model_selection: Some(selection),
            },
            context.wake_depth,
            false,
        )
        .await?;
        Ok(())
    }

    async fn quota_prompt_context(
        &self,
        thread_id: Uuid,
        run_id: Uuid,
    ) -> CoreResult<ResumeContext> {
        let ordinal: Option<i64> = self
            .sql(move |db| {
                db.query_row(
                    "SELECT ordinal FROM turn_items WHERE run_id=?1 AND (kind=?2 OR (kind=?3 AND summary=?4))
                     ORDER BY (kind=?3) DESC, ordinal LIMIT 1",
                    [
                        run_id.to_string(),
                        json!(TurnItemKind::UserMessage).to_string(),
                        json!(TurnItemKind::SystemNotice).to_string(), CONTEXT_NOTICE.into(),
                    ],
                    |row| row.get(0),
                ).optional()
            })
            .await?;
        let Some(ordinal) = ordinal else {
            return Ok(ResumeContext::default());
        };
        let (items, _, _) = self
            .0
            .store
            .list_turn_items_page(thread_id, 1, ordinal.checked_add(1))
            .await?;
        match items.into_iter().find(|item| item.run_id == run_id) {
            Some(item) => {
                serde_json::from_value(item.payload).map_err(|e| CoreError::Storage(e.to_string()))
            }
            None => Ok(ResumeContext::default()),
        }
    }

    // Only the latest run may resume. Also repair missing notices for superseded turns
    // on startup if shutdown interrupted their finalization after the durable quota notice.
    async fn limited_turns(&self, thread_id: Option<Uuid>) -> CoreResult<Vec<LimitedTurn>> {
        let encoded: Vec<String> = self.sql(move |db| {
            let mut query = db.prepare(
                "SELECT json_object('thread_id', m.thread_id, 'role', m.role, 'name', m.name,
                        'board_id', m.board_id, 'id', r.id, 'status', json(r.status),
                        'ended', r.completed_at, 'error', coalesce(r.error_message, ''), 'prompt', r.prompt,
                        'model_selection', json_object('provider', json(r.provider), 'model', r.model, 'effort', r.effort),
                        'latest', r.id=latest.id)
                 FROM swarm_members m JOIN threads t ON t.id=m.thread_id
                 JOIN runs latest ON latest.id=(
                     SELECT id FROM runs WHERE thread_id=m.thread_id ORDER BY ordinal DESC LIMIT 1)
                 JOIN runs r ON r.thread_id=m.thread_id
                 WHERE (?3 IS NULL OR m.thread_id=?3) AND t.archived=0 AND m.state<>'killed'
                   AND r.completed_at IS NOT NULL
                   AND (r.status=?1 OR (r.status=?2 AND r.error_message GLOB 'usage_limit:*'))
                   AND (r.id=latest.id OR (?3 IS NULL AND NOT EXISTS (
                       SELECT 1 FROM swarm_messages WHERE board_id=m.board_id AND sender='system' AND sender_run_id=r.id)
                       AND EXISTS (SELECT 1 FROM turn_items WHERE run_id=r.id AND kind=?4 AND summary=?5)))"
            )?;
            query.query_map(rusqlite::params![
                json!(RunStatus::RateLimited).to_string(), json!(RunStatus::Failed).to_string(),
                thread_id.map(|id| id.to_string()), json!(TurnItemKind::SystemNotice).to_string(), CONTEXT_NOTICE
            ], |row| row.get(0))?.collect()
        }).await?;
        encoded
            .iter()
            .map(|text| {
                serde_json::from_str(text).map_err(|error| CoreError::Storage(error.to_string()))
            })
            .collect()
    }

    pub(crate) async fn restore_quota_resumes(&self, excluded: &HashSet<Uuid>) -> CoreResult<()> {
        // Startup calls this before accepting requests. Every timer also rechecks the
        // persisted run identity under the thread lock at dispatch.
        for run in self.limited_turns(None).await? {
            let _guard = self.lock_thread(run.thread_id).await;
            let until = deadline(&run.error, run.ended);
            self.ensure_quota_notice(
                run.board_id,
                &run.name,
                run.id,
                run.status,
                run.ended,
                until,
            )
            .await?;
            if run.latest != 0 && !excluded.contains(&run.thread_id) {
                self.schedule_quota_resume(run.thread_id, run.id, until);
            }
        }
        Ok(())
    }

    pub(super) async fn ensure_quota_notice(
        &self,
        board_id: Uuid,
        name: &str,
        run_id: Uuid,
        status: RunStatus,
        ended: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> CoreResult<()> {
        let signature = format!("worker {name} ended ({status:?}) without posting");
        // Older t3d versions wrote the silent-worker notice without a sender_run_id.
        let present = self
            .sql(move |db| {
                db.query_row(
                    "SELECT 1 FROM swarm_messages WHERE board_id=?1 AND sender='system'
             AND (sender_run_id=?2 OR (sender_run_id IS NULL AND created_at>=?3 AND instr(text,?4)>0)) LIMIT 1",
                    rusqlite::params![
                        board_id.to_string(),
                        run_id.to_string(),
                        ended.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                        signature
                    ],
                    |_| Ok(()),
                )
                .optional()
            })
            .await?
            .is_some();
        if !present {
            self.0
                .store
                .append_swarm_message_with_run(
                    board_id,
                    "system".into(),
                    format!("[system] {name} ended ({status:?}). 利用枠のリセット予定 {until}"),
                    vec![],
                    Some(run_id),
                )
                .await?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct LimitedTurn {
    id: Uuid,
    thread_id: Uuid,
    board_id: Uuid,
    role: String,
    latest: u8,
    name: String,
    ended: DateTime<Utc>,
    status: RunStatus,
    prompt: String,
    error: String,
    model_selection: ModelSelection,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ResumeContext {
    wake_depth: u8,
    mode_ceiling: Option<(RuntimeMode, InteractionMode)>,
    attachment_ids: Vec<Uuid>,
    model_selection: Option<ModelSelection>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_resets_relative_to_the_original_local_failure_time() {
        let ended = DateTime::parse_from_rfc3339("2026-11-28T13:01:00+09:00").unwrap();
        for (message, expected) in [
            ("try again at 1:35 PM.", "2026-11-28T04:35:00Z"),
            ("try again at 12:15 AM.", "2026-11-28T15:15:00Z"),
            ("Resets in 4h23m", "2026-11-28T08:24:00Z"),
            (
                "usage limit (reset_at: 2026-11-29T06:00:00Z)",
                "2026-11-29T06:00:00Z",
            ),
            (
                r#"{"type":"usage_limit","reset_at":1795932000}"#,
                "2026-11-29T06:00:00Z",
            ),
        ] {
            assert_eq!(
                message_deadline(message, ended),
                Some(expected.parse().unwrap())
            );
        }
        assert_eq!(
            deadline("unknown", ended.to_utc()),
            "2026-11-28T04:31:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }
}
