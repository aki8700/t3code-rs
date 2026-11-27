use super::*;

#[test]
fn failure_notice_uses_steering_or_a_system_notice_fallback() {
    let (steer_tx, mut steer_rx) = mpsc::unbounded_channel();
    let (tx, mut rx) = mpsc::unbounded_channel();
    warn_agent_of_tool_failure(Some(&steer_tx), &tx);
    assert!(matches!(
        steer_rx.try_recv(),
        Ok(StdinMsg::Steer(message)) if message == TOOL_FAILURE_LOOP_NOTICE
    ));
    assert!(rx.try_recv().is_err());

    warn_agent_of_tool_failure(None, &tx);
    assert!(matches!(
        rx.try_recv(),
        Ok(StreamEvent::Item { kind: TurnItemKind::SystemNotice, summary: Some(message), .. })
            if message == TOOL_FAILURE_LOOP_NOTICE
    ));
}

#[tokio::test]
async fn antigravity_passes_prompt_via_print_and_parses_event_stream() {
    let fixture = Fixture::new(
        r#"
last=""
prev=""
for arg in "$@"; do
    prev="$last"
    last="$arg"
done
[ "$prev" = --print ]
[ "$last" = hello ]
printf '%s\n' \
  '{"event":"init","conversation_id":"fake-agy"}' \
  '{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"OK"}}' \
  '{"event":"result","result":{"conversation_id":"fake-agy","status":"SUCCESS","response":"OK\n","usage":{"input_tokens":5,"output_tokens":2}}}'
"#,
    );
    let driver = AntigravityDriver::new(fixture.binary());
    let req = test_request(ProviderId::Antigravity);
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(Duration::from_secs(5), driver.run_turn(req, tx))
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(receive(&mut rx).await, Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "fake-agy")
    );
    assert!(
        matches!(receive(&mut rx).await, Some(StreamEvent::AssistantDelta { text }) if text == "OK")
    );
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Usage {
            input_tokens: Some(5),
            output_tokens: Some(2),
            ..
        })
    ));
    assert!(
        matches!(receive(&mut rx).await, Some(StreamEvent::Completed { final_text: Some(t) }) if t == "OK\n")
    );
}

#[tokio::test]
async fn interrupt_escalates_to_kill_for_the_entire_group() {
    check_group_cleanup(true).await;
}

#[tokio::test]
async fn dropping_the_turn_kills_the_entire_group() {
    check_group_cleanup(false).await;
}

#[test]
fn group_signals_reject_invalid_pids() {
    for pid in [0, 1, i32::MAX as u32 + 1, u32::MAX] {
        assert_eq!(
            signal_group(pid, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[tokio::test]
async fn provider_exit_releases_descendants_that_keep_pipes_open() {
    let fixture = Fixture::new(
        r#"
/bin/sleep 60 &
printf '{"type":"system","session_id":"%s:%s"}\n' "$$" "$!"
printf '{"type":"result","subtype":"success","result":"finished"}\n'
exit 0
"#,
    );
    let driver = ClaudeDriver::new(fixture.binary());
    let req = test_request(ProviderId::Claude);
    let run_id = req.run_id;
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(Duration::from_secs(5), driver.run_turn(req, tx))
        .await
        .expect("a descendant held the provider pipes open")
        .unwrap();
    let Some(StreamEvent::SessionStarted {
        provider_session_id,
    }) = receive(&mut rx).await
    else {
        panic!("provider failed to start");
    };
    let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
    assert_processes_exited(pid.parse().unwrap(), grandchild.parse().unwrap()).await;
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Completed { .. })
    ));
    driver.interrupt(run_id).await.unwrap();
}

#[tokio::test]
async fn completed_response_is_not_tested_for_repetition() {
    let answer = "The final answer intentionally repeats this same example.\n".repeat(10);
    let event = json!({"type": "result", "subtype": "success", "result": answer});
    let fixture = Fixture::new(&format!("printf '%s\\n' '{event}'"));
    let driver = ClaudeDriver::new(fixture.binary());
    let (tx, mut rx) = mpsc::unbounded_channel();
    driver
        .run_turn(test_request(ProviderId::Claude), tx)
        .await
        .unwrap();
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Completed { final_text: Some(text) }) if text == answer
    ));
}

#[tokio::test]
async fn oversized_unterminated_stdout_line_fails_the_turn() {
    let fixture = Fixture::new(
        r#"
/usr/bin/head -c 4194305 /dev/zero
/bin/sleep 60
"#,
    );
    let driver = ClaudeDriver::new(fixture.binary());
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(2),
        driver.run_turn(test_request(ProviderId::Claude), tx),
    )
    .await
    .expect("stdout must be capped before the line ends")
    .unwrap();
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Failed { message, .. }) if message.contains("4 MiB")
    ));
}

#[tokio::test]
async fn run_guard_finish_false_skips_second_group_kill() {
    let mut cmd = command(Path::new("/bin/sleep"));
    cmd.arg("60")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let pid = child.id().unwrap();
    let active = ActiveRunMap::default();
    let run_id = Uuid::new_v4();
    let interrupted = Arc::new(AtomicBool::new(false));
    let (released, release_rx) = watch::channel(false);
    active.lock().unwrap().insert(
        run_id,
        ActiveProcess {
            pid,
            released: release_rx,
            stdin_tx: None,
            control_requests: ControlRequests::default(),
            interrupted: Arc::clone(&interrupted),
        },
    );
    let guard = RunGuard {
        active: &active,
        run_id,
        interrupted,
        released,
    };

    // Calling finish(false) removes the active entry and marks released without killing.
    guard.finish(false);
    assert!(active.lock().unwrap().is_empty());
    assert!(*guard.released.borrow());
    // Child should still be alive because finish(false) did not call GroupCleanup::kill().
    assert!(child.try_wait().unwrap().is_none());

    // Dropping guard after finish(false) must also be a no-op; child stays alive until killed.
    drop(guard);
    assert!(child.try_wait().unwrap().is_none());

    GroupCleanup(Some(pid)).kill();
    wait_for_exit(pid).await.unwrap();
    let _ = child.wait().await;
}

#[tokio::test]
async fn idle_timeout_resets_while_a_single_event_is_arriving() {
    let fixture = Fixture::new(
        r#"
printf '{"type":"result","result":"'
for i in 1 2 3 4 5; do
    /bin/sleep 0.15
    printf x
done
printf '"}\n'
"#,
    );
    let (tx, mut rx) = mpsc::unbounded_channel();
    run_turn_with_idle_timeout(
        &ActiveRunMap::default(),
        Uuid::new_v4(),
        ProviderId::Claude,
        command(&fixture.binary()),
        Input::None,
        None,
        tx,
        Duration::from_millis(600),
    )
    .await
    .unwrap();
    let event = receive(&mut rx).await;
    assert!(
        matches!(&event, Some(StreamEvent::Completed { final_text: Some(text) }) if text == "xxxxx"),
        "{event:?}"
    );
}
#[tokio::test]
async fn completed_result_survives_idle_kill() {
    let events = run_claude_fixture(
        r#"
printf '%s\n' '{"type":"result","subtype":"success","result":"ok"}'
exec /bin/sleep 60
"#,
        Duration::from_secs(1),
    )
    .await;
    assert_completed_after_t3d_stop(&events);
}

#[tokio::test]
async fn idle_timeout_still_stops_a_tool_that_never_finishes() {
    let events = run_agy_fixture(
            r#"
printf '%s\n' '{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"ACTIVE","tool_name":"run_command"}}'
exec /bin/sleep 60
"#,
            Duration::from_secs(1),
        )
        .await;
    assert!(
        matches!(events.last(), Some(StreamEvent::Failed { message, .. }) if message.contains("run_command")),
        "{events:?}"
    );
}

#[tokio::test]
async fn approval_with_updated_input_allows_the_edited_call() {
    let answer = answer_control_request(true, json!({"updatedInput": {"command": "ls"}})).await;
    assert_eq!(
        answer["response"]["response"],
        json!({"behavior": "allow", "updatedInput": {"command": "ls"}})
    );
}

#[tokio::test]
async fn pending_approval_outlasts_the_idle_timeout() {
    let fixture = Fixture::new(
        r#"
IFS= read -r first
printf '%s\n' '{"type":"control_request","request_id":"cli-req-1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/w/a"}}}'
IFS= read -r answer
printf '%s\n' '{"type":"result","result":"ok"}'
"#,
    );
    let active = ActiveRunMap::default();
    let run_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let running = Arc::clone(&active);
    let binary = fixture.binary();
    let task = tokio::spawn(async move {
        run_turn_with_idle_timeout(
            &running,
            run_id,
            ProviderId::Claude,
            command(&binary),
            Input::StreamJson(json!("write a")),
            None,
            tx,
            Duration::from_millis(800),
        )
        .await
    });
    let Some(StreamEvent::PendingApproval { request }) = receive(&mut rx).await else {
        panic!("expected a pending approval");
    };
    // The user takes several idle periods to answer.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let resp = PendingResponse {
        request_id: request.id,
        approved: true,
        payload: json!({}),
    };
    assert!(send_control_response(&active, run_id, &resp));
    let event = receive(&mut rx).await;
    assert!(
        matches!(&event, Some(StreamEvent::Completed { final_text: Some(text) }) if text == "ok"),
        "{event:?}"
    );
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn opencode_stderr_free_usage_limit_triggers_immediate_stop() {
    use tokio::io::AsyncWriteExt;

    let (mut stderr_writer, stderr_reader) = tokio::io::duplex(1024);
    let fatal_limit = Arc::new(Mutex::new(None));
    let stop_notify = Arc::new(tokio::sync::Notify::new());

    let tail_limit = Arc::clone(&fatal_limit);
    let tail_notify = Arc::clone(&stop_notify);
    let tail_task = tokio::spawn(async move {
        read_tail_watching_limit(stderr_reader, ProviderId::OpenCode, tail_limit, tail_notify).await
    });

    let notified = stop_notify.notified();
    stderr_writer
        .write_all(b"INFO starting opencode\nERROR: Free usage limit reached\n")
        .await
        .unwrap();
    stderr_writer.flush().await.unwrap();

    tokio::time::timeout(Duration::from_secs(1), notified)
        .await
        .expect("stop_notify must fire immediately on OpenCode free usage limit");

    drop(stderr_writer);
    let tail = String::from_utf8_lossy(&tail_task.await.unwrap().unwrap()).into_owned();
    assert!(tail.contains("Free usage limit reached"));
    let recorded = fatal_limit.lock().unwrap().clone().unwrap();
    assert!(recorded.contains("Free usage limit reached"));
}
