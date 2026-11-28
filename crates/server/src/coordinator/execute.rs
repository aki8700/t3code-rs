use super::*;

impl RunCoordinator {
    pub(super) async fn execute(&self, turn: RunningTurn) -> Option<RunningTurn> {
        let RunningTurn {
            thread,
            request,
            original_prompt,
            attachment_ids,
            mut cancellation,
            done,
            chained_dones,
            driver,
            notify_parent,
            wake_depth,
            mode_ceiling,
            verify_fix_depth,
            last_verify_signature,
            verify_dead_ends,
            fallback_attempts,
            mcp_token,
            swarm,
            started_at: run_started_at,
            workspace_dir,
            verify_command,
        } = turn;
        let started = Instant::now();
        let thread_id = thread.id;
        let run_id = request.run_id;
        let current_selection = request.model_selection.clone();
        let codex_usage_before = if request.model_selection.provider == ProviderId::Codex {
            if let Some(session_id) = request.resume_session_id.clone() {
                let home = self
                    .0
                    .session_home
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                tokio::task::spawn_blocking(move || {
                    crate::usage::codex_session_usage_snapshot(home.as_deref(), &session_id)
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            }
        } else {
            None
        };

        // Capture pre-run Git tree snapshot outside the global `active` lock (Astra B1)
        // so slow git subprocesses never block unrelated threads.
        let writes_workspace = thread.interaction_mode != InteractionMode::Plan
            && current_selection.provider != ProviderId::GeminiApp;
        let pre_tree = if writes_workspace {
            match GitManager::snapshot_tree(&workspace_dir).await {
                Ok(Some(tree)) => match self
                    .0
                    .store
                    .save_run_checkpoint(run_id, thread_id, tree.clone())
                    .await
                {
                    Ok(baseline) => Some(baseline),
                    Err(error) => {
                        tracing::debug!(%error, %run_id, "Could not save pre-run git checkpoint");
                        Some(tree)
                    }
                },
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!(%error, %run_id, "Could not snapshot workspace before run");
                    None
                }
            }
        } else {
            None
        };

        let mut outcome = Outcome {
            provider: request.model_selection.provider,
            model: request.model_selection.model.clone(),
            session: request.resume_session_id.clone(),
            input: None,
            output: None,
            cache_read: None,
            cache_creation: None,
            reasoning_tokens: None,
            has_context_usage: false,
            had_file_changes: false,
            had_side_effects: false,
            hard_rate_limited: false,
            reset_at: None,
            limited_until: None,
            failure_class: None,
            assistant: String::new(),
            reasoning: String::new(),
            terminal: None,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = {
            let live = self.0.settings.get();
            let worker = run_turn_with_retry(
                driver.clone(),
                request,
                tx,
                live.auto_resume_limited_threads,
                live.rate_limit_max_retries,
            );
            #[cfg(unix)]
            let worker = {
                let store = self.0.store.clone();
                t3_providers::with_spawn_hook(worker, move |pid, start| {
                    let store = store.clone();
                    async move { store.record_orphan_process(run_id, pid, start).await }
                })
            };
            tokio::pin!(worker);
            let mut result = loop {
                tokio::select! {
                    biased;
                    _ = async { let _ = cancellation.wait_for(|cancelled| *cancelled).await; } => {
                        let interrupt = driver.interrupt(run_id);
                        tokio::pin!(interrupt);
                        let result = tokio::select! {
                            result = &mut interrupt => result,
                            _ = &mut worker => interrupt.await,
                        };
                        if let Err(error) = result {
                            tracing::warn!(%error, %run_id, "Provider interrupt failed");
                        }
                        break Ok(());
                    }
                    Some(event) = rx.recv() => {
                        if let Err(error) = self.event(thread_id, run_id, event, &mut outcome).await {
                            let _ = driver.interrupt(run_id).await;
                            break Err(error);
                        }
                    }
                    result = &mut worker => break result,
                }
            };
            // A terminal event may already be queued when the shutdown signal wins.
            while let Ok(event) = rx.try_recv() {
                if let Err(error) = self.event(thread_id, run_id, event, &mut outcome).await {
                    result = Err(error);
                    break;
                }
            }
            result
        };
        drop(mcp_token);
        // The provider worker has returned, so its attempt groups have gone through the
        // driver's process cleanup. Rows from retries and terminal failures are no longer
        // restart cleanup targets either.
        if let Err(error) = self.0.store.clear_run_orphan_processes(run_id).await {
            tracing::warn!(%error, %run_id, "Could not clear finished provider process tracking");
        }

        struct VerifyFailure {
            command: String,
            exit_code: Option<i32>,
            timed_out: bool,
            tail: String,
            summary: String,
            signature: String,
        }
        let mut verify_failure: Option<VerifyFailure> = None;
        let mut verify_cancelled = false;
        let mut verify_failed_error: Option<String> = None;
        if !*cancellation.borrow()
            && result.is_ok()
            && matches!(outcome.terminal, Some((RunStatus::Completed, _)))
            && writes_workspace
            && let Some(cmd) = verify_command
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            let post_tree = GitManager::snapshot_tree(&workspace_dir)
                .await
                .ok()
                .flatten();
            let tree_changed = pre_tree.is_some() && post_tree != pre_tree;
            if outcome.had_file_changes || verify_fix_depth > 0 || tree_changed {
                match crate::shell::run_shell_cancellable(
                    &workspace_dir,
                    crate::shell::ShellExecRequest {
                        command: cmd.to_owned(),
                        timeout_secs: Some(60),
                    },
                    cancellation.clone(),
                )
                .await
                {
                    Ok(out) => {
                        let mut combined = String::new();
                        if !out.stdout.is_empty() {
                            combined.push_str(&out.stdout);
                        }
                        if !out.stderr.is_empty() {
                            if !combined.is_empty() && !combined.ends_with('\n') {
                                combined.push('\n');
                            }
                            combined.push_str(&out.stderr);
                        }
                        if out.cancelled {
                            if !combined.is_empty() && !combined.ends_with('\n') {
                                combined.push('\n');
                            }
                            combined.push_str("[キャンセルされました]");
                        } else if out.timed_out {
                            if !combined.is_empty() && !combined.ends_with('\n') {
                                combined.push('\n');
                            }
                            combined.push_str("[タイムアウトにより強制終了しました]");
                        }
                        let summary = out.command.chars().take(240).collect::<String>();
                        let _ = self
                            .append(NewTurnItem {
                                thread_id,
                                run_id,
                                kind: TurnItemKind::CommandExecution,
                                summary: Some(summary),
                                payload: json!({
                                    "command": out.command,
                                    "cwd": out.cwd,
                                    "exit_code": out.exit_code,
                                    "stdout": out.stdout,
                                    "stderr": out.stderr,
                                    "output": combined,
                                    "duration_ms": out.duration_ms,
                                    "timed_out": out.timed_out,
                                    "cancelled": out.cancelled,
                                    "truncated": out.truncated,
                                    "source": "verify_hook",
                                }),
                                searchable_text: Some(format!("{}\n{}", out.command, combined)),
                            })
                            .await;
                        if out.cancelled {
                            verify_cancelled = true;
                        } else if out.timed_out || out.exit_code != Some(0) {
                            let tail = redact_and_tail(&combined, 4_000);
                            let signature = crate::verify::outcome_signature(
                                out.exit_code,
                                out.timed_out,
                                &tail,
                            );
                            let summary = crate::verify::summarize_failure(
                                &out.command,
                                out.exit_code,
                                out.timed_out,
                                &tail,
                            );
                            verify_failure = Some(VerifyFailure {
                                command: out.command.clone(),
                                exit_code: out.exit_code,
                                timed_out: out.timed_out,
                                tail,
                                summary,
                                signature,
                            });
                            verify_failed_error = Some(if out.timed_out {
                                format!("verify_command timed out: {}", out.command)
                            } else {
                                format!(
                                    "verify_command failed (exit {:?}): {}",
                                    out.exit_code, out.command
                                )
                            });
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%err, %run_id, "Post-turn verify_command failed to launch");
                    }
                }
            }
        }

        let actions = swarm
            .as_ref()
            .map(|turn| turn.actions(&outcome.assistant))
            .unwrap_or_default();
        // Only multi-member actions and silent-worker notifications need board
        // serialization. Take the board before a member so reciprocal wakes
        // cannot deadlock. An existing post for this run cannot disappear.
        let mut may_notify = false;
        let board_id = if let Some(turn) = &swarm {
            if let Some(name) = &turn.worker_name {
                may_notify = !self
                    .0
                    .store
                    .worker_posted_for_run(turn.board_id, name, run_id, run_started_at)
                    .await
                    .unwrap_or(false);
            }
            (may_notify || !actions.is_empty()).then_some(turn.board_id)
        } else if thread.relationship.as_deref() == Some("swarm-worker") {
            // Membership can become visible after this turn was prepared.
            match self.get_board(thread_id).await {
                Ok(board) => {
                    may_notify = board.is_some();
                    board.map(|board| board.board_id)
                }
                Err(error) => {
                    tracing::error!(%error, %run_id, "Could not load worker board for silent-exit notification");
                    None
                }
            }
        } else {
            None
        };
        let mut board_guard = match board_id {
            Some(id) => Some(self.0.board_locks.get(id).write_owned().await),
            None => None,
        };
        let guard = self.lock_thread(thread_id).await;
        let cancelled = *cancellation.borrow() || verify_cancelled;
        let shutting_down_run = cancelled
            && self
                .active()
                .get(&thread_id)
                .is_some_and(|r| !r.stop_requested);
        // Persist a provider-reported terminal limit even if shutdown races finalization.
        // Other in-flight turns keep their session and inputs for crash recovery.
        let terminal_limit = matches!(outcome.terminal, Some((RunStatus::RateLimited, _)))
            || (matches!(outcome.terminal, Some((RunStatus::Failed, _)))
                && outcome.failure_class == Some(FailureClass::UsageLimit));
        if shutting_down_run && !terminal_limit {
            if let Err(error) = self.flush_output(thread_id, run_id, &mut outcome).await {
                tracing::error!(%error, %run_id, "Could not keep the interrupted output");
            }
            self.settle_live_state(thread_id, run_id, outcome.session)
                .await;
            self.active().remove(&thread_id);
            done.send_replace(true);
            for waiter in chained_dones {
                waiter.send_replace(true);
            }
            return None;
        }
        let mut error_class = None;
        let (mut status, mut error) = if cancelled && !shutting_down_run {
            (RunStatus::Cancelled, Some("Interrupted".into()))
        } else if let Err(error) = result
            && !shutting_down_run
        {
            let message = error.to_string();
            error_class = Some(classify_harness_failure(
                current_selection.provider,
                &message,
                None,
            ));
            (RunStatus::Failed, Some(message))
        } else {
            match outcome.terminal.take() {
                Some(terminal) => {
                    error_class = outcome.failure_class;
                    terminal
                }
                None => {
                    error_class = Some(FailureClass::HarnessError);
                    (
                        RunStatus::Failed,
                        Some("provider ended without a completion event".into()),
                    )
                }
            }
        };

        let mut scheduled_auto_followup: Option<(Message, QueuedMeta)> = None;
        let mut verify_passed = true;
        if status == RunStatus::Completed
            && let Some(failure) = verify_failure
        {
            verify_passed = false;
            let max_fixes = self.0.settings.get().verify_max_auto_fixes.min(10) as u8;
            let repeated = crate::verify::is_repeated_outcome(
                &failure.signature,
                last_verify_signature.as_deref(),
                &verify_dead_ends,
            );
            if verify_fix_depth < max_fixes && !repeated {
                let next_depth = verify_fix_depth + 1;
                let fix_prompt = crate::verify::build_fix_prompt(
                    &failure.command,
                    failure.exit_code,
                    failure.timed_out,
                    &failure.summary,
                    &failure.tail,
                    next_depth,
                    max_fixes,
                    &verify_dead_ends,
                );
                let next_dead_ends = crate::verify::record_dead_end(
                    verify_dead_ends,
                    next_depth,
                    failure.signature.clone(),
                    failure.exit_code,
                    failure.timed_out,
                    failure.summary.clone(),
                );
                let notice = format!(
                    "verify_command が失敗したため自動修正を試行します ({next_depth}/{max_fixes})"
                );
                let _ = self
                    .append(NewTurnItem {
                        thread_id,
                        run_id,
                        kind: TurnItemKind::SystemNotice,
                        summary: Some(notice.clone()),
                        payload: json!({
                            "message": notice,
                            "verify_fix_depth": next_depth,
                            "verify_max_auto_fixes": max_fixes,
                            "saw": failure.summary,
                            "dead_ends_count": next_dead_ends.len(),
                        }),
                        searchable_text: None,
                    })
                    .await;
                scheduled_auto_followup = Some((
                    Message {
                        prompt: fix_prompt,
                        behavior: None,
                        attachment_ids: Vec::new(),
                        model_selection: Some(current_selection.clone()),
                        auto_wake: false,
                        mode_ceiling: None,
                    },
                    QueuedMeta {
                        notify_parent,
                        wake_depth,
                        mode_ceiling: Some(
                            mode_ceiling.unwrap_or((thread.runtime_mode, thread.interaction_mode)),
                        ),
                        verify_fix_depth: next_depth,
                        last_verify_signature: Some(failure.signature),
                        verify_dead_ends: next_dead_ends,
                        fallback_attempts,
                    },
                ));
            } else {
                let (reason, notice, err_msg) = if repeated {
                    (
                        "same_result_twice",
                        format!(
                            "verify_command が同じ失敗を繰り返したため自動修正を打ち切りエスカレーションします（{}）",
                            failure.summary
                        ),
                        format!(
                            "verify_command repeated the same failure (escalated): {}",
                            failure.summary
                        ),
                    )
                } else {
                    (
                        "max_attempts_reached",
                        format!(
                            "verify_command が自動修正上限（{max_fixes} 回）に達したためエスカレーションします（{}）",
                            failure.summary
                        ),
                        verify_failed_error.unwrap_or_else(|| "verify_command failed".into()),
                    )
                };
                if max_fixes > 0 {
                    let _ = self
                        .append(NewTurnItem {
                            thread_id,
                            run_id,
                            kind: TurnItemKind::SystemNotice,
                            summary: Some(notice.clone()),
                            payload: json!({
                                "message": notice,
                                "reason": reason,
                                "verify_fix_depth": verify_fix_depth,
                                "verify_max_auto_fixes": max_fixes,
                                "saw": failure.summary,
                                "dead_ends": verify_dead_ends,
                            }),
                            searchable_text: None,
                        })
                        .await;
                }
                status = RunStatus::Failed;
                error = Some(err_msg);
                error_class = Some(FailureClass::HarnessError);
            }
        } else if status == RunStatus::RateLimited
            && outcome.hard_rate_limited
            && !outcome.had_side_effects
            && let Some((fallback_model, next_fallback_idx)) =
                self.pick_fallback_model(&current_selection, fallback_attempts)
        {
            let notice = format!(
                "クォータ上限を検知したためフォールバックモデル {}/{} へ自動切替します",
                fallback_model.provider.as_str(),
                fallback_model.model
            );
            let _ = self
                .append(NewTurnItem {
                    thread_id,
                    run_id,
                    kind: TurnItemKind::SystemNotice,
                    summary: Some(notice.clone()),
                    payload: json!({
                        "message": notice,
                        "fallback_provider": fallback_model.provider,
                        "fallback_model": fallback_model.model,
                    }),
                    searchable_text: None,
                })
                .await;
            scheduled_auto_followup = Some((
                Message {
                    prompt: original_prompt,
                    behavior: None,
                    attachment_ids: attachment_ids.clone(),
                    model_selection: Some(fallback_model),
                    auto_wake: false,
                    mode_ceiling: None,
                },
                QueuedMeta {
                    notify_parent,
                    wake_depth,
                    mode_ceiling: Some(
                        mode_ceiling.unwrap_or((thread.runtime_mode, thread.interaction_mode)),
                    ),
                    verify_fix_depth,
                    last_verify_signature,
                    verify_dead_ends,
                    fallback_attempts: next_fallback_idx,
                },
            ));
        }

        if let Err(failure) = self.flush_output(thread_id, run_id, &mut outcome).await {
            status = RunStatus::Failed;
            error = Some(failure.to_string());
            error_class = Some(FailureClass::HarnessError);
        }
        if status == RunStatus::Completed
            && verify_passed
            && !actions.is_empty()
            && let Err(failure) = self.apply_swarm_actions(&guard, thread_id, actions).await
        {
            status = RunStatus::Failed;
            error = Some(failure.to_string());
            error_class = Some(FailureClass::HarnessError);
        }
        if !may_notify {
            drop(board_guard.take());
        }
        let mut swarm_reset = None;
        if matches!(status, RunStatus::Failed | RunStatus::RateLimited)
            && let Some(message) = error.as_deref()
        {
            let class = error_class.unwrap_or_else(|| {
                if status == RunStatus::RateLimited {
                    classify_rate_limit(outcome.provider, message, None)
                } else {
                    classify_provider_failure(outcome.provider, message, None)
                }
            });
            if (status == RunStatus::RateLimited || class == FailureClass::UsageLimit)
                && (swarm.is_some() || thread.relationship.as_deref() == Some("swarm-worker"))
            {
                let now = Utc::now();
                let until = self.quota_reset_time(&current_selection, message, &outcome, now);
                if outcome
                    .reset_at
                    .as_deref()
                    .and_then(crate::provider_hold::parse_reset_at)
                    != Some(until)
                {
                    outcome.reset_at = Some(until.to_rfc3339());
                }
                // Preserve the effective modes and delivery depth even when coalescing
                // rewrote the original queued prompt or the daemon restarts before reset.
                if let Err(failure) = self
                    .append(NewTurnItem {
                        thread_id,
                        run_id,
                        kind: TurnItemKind::SystemNotice,
                        summary: Some(quota_resume::CONTEXT_NOTICE.into()),
                        payload: json!({
                            "message": quota_resume::CONTEXT_NOTICE,
                            "reset_at": until,
                            "attachment_ids": attachment_ids,
                            "wake_depth": wake_depth,
                            "mode_ceiling": (thread.runtime_mode, thread.interaction_mode),
                            "model_selection": current_selection,
                        }),
                        searchable_text: None,
                    })
                    .await
                {
                    tracing::error!(%failure,%run_id,"Could not persist quota resume context");
                }
                swarm_reset = Some((now, until));
            }
            error = Some(format_run_error(
                class,
                message,
                outcome.reset_at.as_deref(),
            ));
        }
        // Codex and Antigravity report the resumed session's running total, not this run's share.
        let reported = crate::database::RunTokens {
            input: outcome.input,
            output: outcome.output,
            cache_read: outcome.cache_read,
            cache_creation: outcome.cache_creation,
            reasoning: outcome.reasoning_tokens,
        };
        let tokens = match (outcome.provider, outcome.session.clone()) {
            (ProviderId::Codex, Some(session_id)) => {
                let home = self
                    .0
                    .session_home
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let before = codex_usage_before;
                tokio::task::spawn_blocking(move || {
                    let after =
                        crate::usage::codex_session_usage_snapshot(home.as_deref(), &session_id);
                    crate::usage::codex_run_usage_since(before, after)
                })
                .await
                .ok()
                .flatten()
                .map_or(reported, |usage| crate::database::RunTokens {
                    input: Some(usage.input_tokens),
                    output: Some(usage.output_tokens),
                    cache_read: Some(usage.cache_read_tokens),
                    cache_creation: Some(usage.cache_creation_tokens),
                    reasoning: Some(usage.reasoning_tokens),
                })
            }
            (ProviderId::Antigravity, Some(session_id))
                if reported != crate::database::RunTokens::default() =>
            {
                match crate::database::session_tokens_except(
                    &self.0.cfg,
                    outcome.provider,
                    session_id,
                    run_id,
                )
                .await
                {
                    Ok(stored) => reported.since(stored),
                    Err(error) => {
                        tracing::warn!(%run_id, %error, "Keeping the session total as run usage");
                        reported
                    }
                }
            }
            _ => reported,
        };
        outcome.input = tokens.input;
        outcome.output = tokens.output;
        outcome.cache_read = tokens.cache_read;
        outcome.cache_creation = tokens.cache_creation;
        outcome.reasoning_tokens = tokens.reasoning;
        let thread_status = match status {
            RunStatus::Completed => ThreadStatus::Completed,
            RunStatus::Cancelled => ThreadStatus::Interrupted,
            RunStatus::RateLimited => ThreadStatus::RateLimited,
            _ => ThreadStatus::Failed,
        };
        let store = &self.0.store;
        if let Err(error) = store
            .finish_run(
                run_id,
                status,
                outcome.session.clone(),
                outcome.input,
                outcome.output,
                outcome.cache_read,
                outcome.cache_creation,
                outcome.reasoning_tokens,
                started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                error.clone(),
            )
            .await
        {
            tracing::error!(%error, %run_id, "Could not persist run completion");
        }
        self.settle_live_state(thread_id, run_id, outcome.session)
            .await;
        // Before any wake below (parent, coordinator) so a woken run on this provider is held.
        if let Some(until) = outcome.limited_until.filter(|until| *until > Utc::now())
            && (status == RunStatus::RateLimited
                || (status == RunStatus::Failed
                    && outcome.failure_class == Some(FailureClass::UsageLimit)))
        {
            let reason = error.clone().unwrap_or_default();
            self.hold_provider(ProviderHold {
                provider: outcome.provider,
                model: model_scoped(&outcome.model, &reason).then(|| outcome.model.clone()),
                until,
                reason,
            });
        } else if status == RunStatus::Completed {
            self.0.holds.record_success(&thread.model_selection);
        } else if status == RunStatus::Failed && outcome.provider != ProviderId::Mock {
            let reason = error.as_deref().unwrap_or("run failed");
            if let Some(hold) =
                self.0
                    .holds
                    .record_failure(&thread.model_selection, reason, Utc::now())
            {
                self.hold_provider(hold);
            }
        }
        let mut enqueued_followup = false;
        let mut followup_run_id = None;
        if let Some((followup_msg, followup_meta)) = scheduled_auto_followup {
            match self
                .enqueue_with_meta(&thread, followup_msg, followup_meta, true)
                .await
            {
                Ok(id) => {
                    enqueued_followup = true;
                    followup_run_id = Some(id);
                }
                Err(error) => {
                    tracing::error!(%error, %thread_id, "Could not enqueue automatic follow-up run");
                }
            }
        }
        let accepting_runs = !self.0.shutting_down.load(Ordering::SeqCst);
        let defer_completion = enqueued_followup && accepting_runs;
        if defer_completion && let Some(followup_id) = followup_run_id {
            let mut dones = Vec::with_capacity(chained_dones.len() + 1);
            dones.push(done.clone());
            dones.extend(chained_dones.iter().cloned());
            self.defer_dones_for_queued_run(followup_id, dones);
        }
        let followup = match store.list_queued_runs(thread_id).await {
            Ok(queued) => defer_completion || !queued.is_empty(),
            Err(error) => {
                tracing::warn!(%error, %thread_id, "Could not check for queued follow-up runs");
                defer_completion
            }
        };
        if notify_parent && !defer_completion {
            let child_error = error
                .as_deref()
                .map(|error| format_delegated_error(outcome.provider, &outcome.model, error));
            if let Err(error) = self
                .record_child_completion(&thread, thread_status, child_error.as_deref())
                .await
            {
                tracing::error!(%error, %run_id, "Could not record delegated task completion");
            }
        }
        if let Err(error) = store.update_thread_status(thread_id, thread_status).await {
            tracing::error!(%error, %run_id, "Could not persist thread status");
        }
        if let Err(error) = self.settle_wake(&guard, thread_id, run_id, status).await {
            tracing::error!(%error, %run_id, "Could not acknowledge swarm wake posts");
        }
        if !defer_completion
            && (swarm.is_some() || thread.relationship.as_deref() == Some("swarm-worker"))
            && let Err(error) = self.swarm_state(thread_id, "waiting").await
        {
            tracing::error!(%error, %run_id, "Could not persist swarm member state");
        }
        if !defer_completion
            && may_notify
            && !(thread.model_selection.provider == ProviderId::Mock
                && status == RunStatus::Completed)
            && let Err(error) = self
                .notify_worker_ended_silently(
                    &guard,
                    thread_id,
                    run_id,
                    run_started_at,
                    status,
                    error.as_deref(),
                )
                .await
        {
            tracing::error!(%error, %run_id, "Could not notify coordinator of silent worker exit");
        }
        if let Some((ended, until)) = swarm_reset {
            match self.get_board(thread_id).await {
                Ok(Some(board)) => {
                    if let Some(member) = board
                        .members
                        .iter()
                        .find(|m| m.thread_id == thread_id && m.state != "killed")
                    {
                        if let Err(error) = self
                            .ensure_quota_notice(
                                member.board_id,
                                &member.name,
                                run_id,
                                status,
                                ended,
                                until,
                            )
                            .await
                        {
                            tracing::warn!(%error, %run_id, "Could not record swarm quota notice");
                        }
                        self.emit(WsServerMessage::Resync {
                            thread_id: Some(board.coordinator_thread_id),
                        });
                        if !followup {
                            self.schedule_quota_resume(thread_id, run_id, until);
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, %run_id, "Could not check quota-limited swarm membership")
                }
            }
        }
        drop(board_guard);
        self.emit(WsServerMessage::RunStatusChanged {
            thread_id,
            run_id,
            status,
            followup,
            error,
        });
        self.emit(WsServerMessage::ThreadStatusChanged {
            thread_id,
            status: thread_status,
        });
        if notify_parent
            && !defer_completion
            && let Some(parent_thread_id) = thread.parent_thread_id
        {
            self.emit(WsServerMessage::SubagentUpdated {
                parent_thread_id,
                child_thread_id: thread_id,
                status: thread_status,
            });
        }
        // Keep the old entry until the successor is installed, so a project
        // deletion cannot slip between the two runs.
        let next = if accepting_runs {
            self.start_next(&guard, thread.project_id, None).await
        } else {
            Ok(None)
        };
        if !matches!(&next, Ok(Some(_))) {
            self.active().remove(&thread_id);
        }
        match next {
            Ok(Some((_, next_turn))) => {
                if !defer_completion {
                    done.send_replace(true);
                    for waiter in chained_dones {
                        waiter.send_replace(true);
                    }
                }
                Some(next_turn)
            }
            Ok(None) => {
                if !defer_completion {
                    done.send_replace(true);
                    for waiter in chained_dones {
                        waiter.send_replace(true);
                    }
                }
                None
            }
            Err(error) => {
                if !defer_completion {
                    done.send_replace(true);
                    for waiter in chained_dones {
                        waiter.send_replace(true);
                    }
                }
                tracing::error!(%error, %thread_id, "Could not start next queued run");
                self.emit(WsServerMessage::Error {
                    message: error.to_string(),
                });
                None
            }
        }
    }
}
