use super::*;

#[test]
fn normalizes_single_digit_day_lstart_spacing() {
    assert_eq!(
        normalize_process_start_time("Sat Nov  7 09:05:01 2026\n"),
        "Sat Nov 7 09:05:01 2026"
    );
}

#[tokio::test]
async fn streaming_providers_keep_stdin_open_for_steering_then_close_it() {
    let fixture = Fixture::new(
        r#"
check_privacy() {
    [ "$DO_NOT_TRACK:$AGY_DISABLE_TELEMETRY:$DISABLE_TELEMETRY:$CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC" = '1:1:1:1' ]
    [ "$T3CODE_TELEMETRY_ENABLED" = false ]
    [ "$T3CODE_OTEL_SDK_DISABLED" = true ]
}
check_privacy
if [ "$1" = --version ]; then
    printf 'fixture 1.0\n'
    exit
fi
IFS= read -r first
[ "$first" = '{"message":{"content":"hello","role":"user"},"type":"user"}' ]
printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-stream","conversation_id":"fake-stream"}'
IFS= read -r next
[ "$next" = '{"message":{"content":"follow up","role":"user"},"type":"user"}' ]
printf '%s\n' '{"type":"result","result":"ok"}'
if IFS= read -r unexpected; then
    exit 17
fi
"#,
    );
    for driver in [Arc::new(ClaudeDriver::new(fixture.binary())) as Arc<dyn ProviderDriver>] {
        let health = driver.health_check().await;
        assert!(health.available, "{:?}", health.error);
        assert_eq!(health.version.as_deref(), Some("fixture 1.0"));
        let req = test_request(driver.provider_id());
        let id = req.run_id;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let running = Arc::clone(&driver);
        let task = tokio::spawn(async move { running.run_turn(req, tx).await });
        assert!(
            matches!(receive(&mut rx).await, Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "fake-stream")
        );
        assert!(driver.send_steer(id, "follow up".into()).await.unwrap());
        assert!(
            matches!(receive(&mut rx).await, Some(StreamEvent::Completed { final_text: Some(text) }) if text == "ok")
        );
        task.await.unwrap().unwrap();
        assert!(!driver.send_steer(id, "late".into()).await.unwrap());
    }
}

#[tokio::test]
async fn opencode_reads_no_stdin_and_finishes_on_exit() {
    let fixture = Fixture::new(
        r#"
[ "$1" = run ]
if IFS= read -r unexpected; then exit 17; fi
printf '%s\n' '{"type":"step_start","sessionID":"fake-opencode"}' '{"type":"step_finish","part":{"reason":"stop","tokens":{"input":3,"output":4}}}'
"#,
    );
    let driver = OpenCodeDriver::new(fixture.binary());
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(5),
        driver.run_turn(test_request(ProviderId::OpenCode), tx),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(receive(&mut rx).await, Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "fake-opencode")
    );
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Usage {
            input_tokens: Some(3),
            output_tokens: Some(4),
            ..
        })
    ));
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Completed { .. })
    ));
}

#[tokio::test]
async fn repeated_failed_tool_result_survives_interleaved_read_success() {
    let fixture = Fixture::new(
        r#"
IFS= read -r prompt
printf '%s\n' '{"type":"system","subtype":"init","session_id":"failure-loop"}'
printf '%s\n' \
  '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"make build"}}]}}' \
  '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b1","is_error":true,"content":"failed in 10ms"}]}}' \
  '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"src/main.rs"}}]}}' \
  '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"r1","content":"source"}]}}' \
  '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b2","name":"Bash","input":{"command":"make build"}}]}}' \
  '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b2","is_error":true,"content":"failed in 30ms"}]}}' \
  '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b3","name":"Bash","input":{"command":"make build"}}]}}' \
  '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b3","is_error":true,"content":"failed in 50ms"}]}}' \
  '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b4","name":"Bash","input":{"command":"make build"}}]}}' \
  '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b4","is_error":true,"content":"failed in 80ms"}]}}' \
  '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b5","name":"Bash","input":{"command":"make build"}}]}}' \
  '{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b5","is_error":true,"content":"failed in 100ms"}]}}' \
  '{"type":"result","subtype":"success","result":"unexpected completion"}'
"#,
    );
    let driver = ClaudeDriver::new(fixture.binary());
    let (tx, mut rx) = mpsc::unbounded_channel();
    driver
        .run_turn(test_request(ProviderId::Claude), tx)
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = receive(&mut rx).await {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::Failed { error_code: Some(code), .. }
                if code == "tool_failure_loop"
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn stdout_accepts_a_line_at_the_limit_and_an_unterminated_final_line() {
    let mut input = br#"{"type":"system","session_id":"boundary"}"#.to_vec();
    input.resize(4 * 1024 * 1024, b' ');
    input.push(b'\n');
    input.extend_from_slice(br#"{"type":"result","subtype":"success","result":"ok"}"#);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (close, _) = oneshot::channel();
    let (stop, _) = oneshot::channel();
    let outcome = read_events(
        input.as_slice(),
        ProviderId::Claude,
        None,
        &tx,
        None,
        &ControlRequests::default(),
        close,
        stop,
        DEFAULT_IDLE_STDOUT_TIMEOUT,
        t3_core::McpToolProfile::Full,
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        ReadOutcome::Terminal(Some(StreamEvent::Completed { .. }))
    ));
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "boundary"
    ));
}

#[tokio::test]
async fn repeated_tool_calls_terminate_the_provider_group() {
    let mut script = String::from(
        "/bin/sleep 60 &\nprintf '{\"type\":\"system\",\"session_id\":\"%s:%s\"}\\n' \"$$\" \"$!\"\n",
    );
    for idx in 0..8 {
        let call = json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "id": format!("tool-{idx}"),
                    "name": "Bash",
                    "input": {"command": "cargo test"}
                }]
            }
        });
        let result = json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("tool-{idx}"),
                    "content": "failed"
                }]
            }
        });
        script.push_str(&format!("printf '%s\\n' '{call}'\n"));
        script.push_str(&format!("printf '%s\\n' '{result}'\n"));
    }
    script.push_str("wait\n");

    let fixture = Fixture::new(&script);
    let driver = ClaudeDriver::new(fixture.binary());
    let req = test_request(ProviderId::Claude);
    let id = req.run_id;
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(Duration::from_secs(2), driver.run_turn(req, tx))
        .await
        .expect("tool loop should stop the provider before it exits")
        .unwrap();
    let Some(StreamEvent::SessionStarted {
        provider_session_id,
    }) = receive(&mut rx).await
    else {
        panic!("fixture did not start");
    };
    let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
    assert_processes_exited(pid.parse().unwrap(), grandchild.parse().unwrap()).await;

    let mut failed_message = None;
    while let Some(event) = receive(&mut rx).await {
        if let StreamEvent::Failed { message, .. } = event {
            failed_message = Some(message);
        }
    }
    assert_eq!(
        failed_message.as_deref(),
        Some(
            "tool call repetition loop detected — terminated runaway provider process to save tokens/battery"
        )
    );
    assert!(!driver.send_steer(id, "late".into()).await.unwrap());
}

#[tokio::test]
async fn completed_result_survives_kill_after_stdout_closes() {
    let events = run_claude_fixture(
        r#"
printf '%s\n' '{"type":"result","subtype":"success","result":"ok"}'
exec 1>&-
exec /bin/sleep 60
"#,
        Duration::from_secs(1),
    )
    .await;
    assert_completed_after_t3d_stop(&events);
}

#[tokio::test]
async fn idle_stdout_timeout_resets_when_events_stream() {
    let fixture = Fixture::new(
        r#"
printf '{"type":"system","session_id":"active-stream"}\n'
for i in 1 2 3 4 5 6 7 8 9 10; do
    /bin/sleep 0.15
    printf '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"tick "}}}\n'
done
printf '{"type":"result","subtype":"success","result":"done"}\n'
"#,
    );
    let active = ActiveRunMap::default();
    let run_id = Uuid::new_v4();
    let cmd = command(&fixture.binary());
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(5),
        run_turn_with_idle_timeout(
            &active,
            run_id,
            ProviderId::Claude,
            cmd,
            Input::None,
            None,
            tx,
            Duration::from_secs(1),
        ),
    )
    .await
    .expect("active stream should complete")
    .unwrap();

    let mut completed = false;
    while let Some(event) = receive(&mut rx).await {
        if matches!(event, StreamEvent::Completed { final_text: Some(ref text) } if text == "done")
        {
            completed = true;
        }
    }
    assert!(completed);
}

#[test]
fn control_response_for_unknown_run_is_refused() {
    let active = ActiveRunMap::default();
    let resp = PendingResponse {
        request_id: Uuid::new_v4(),
        approved: true,
        payload: json!({}),
    };
    assert!(!send_control_response(&active, Uuid::new_v4(), &resp));
}

#[test]
fn detect_fatal_stderr_limit_matches_opencode_case_insensitively() {
    assert_eq!(
        detect_fatal_stderr_limit(
            ProviderId::OpenCode,
            "ERROR 2026-11-27T14:00:00Z Free Usage Limit Reached for model"
        ),
        Some("Free usage limit reached")
    );
    assert_eq!(
        detect_fatal_stderr_limit(ProviderId::OpenCode, "WARN normal diagnostic line"),
        None
    );
    assert_eq!(
        detect_fatal_stderr_limit(ProviderId::Codex, "Free usage limit reached"),
        None
    );
}
