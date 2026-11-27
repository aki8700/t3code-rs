use super::*;

#[tokio::test]
async fn startup_gate_registers_the_pid_before_cli_exec() {
    let fixture = Fixture::new(r#"test "$(cat registered)" = "$$" && touch ran"#);
    let mut req = test_request(ProviderId::OpenCode);
    req.workspace_dir = fixture.0.clone();
    let registered = fixture.0.join("registered");
    let driver = OpenCodeDriver::new(fixture.binary());
    let (tx, _rx) = mpsc::unbounded_channel();
    startup::with_spawn_hook(driver.run_turn(req, tx), move |pid, start| {
        assert_eq!(
            process_start_time(pid as u32).as_deref(),
            Some(start.as_str())
        );
        fs::write(&registered, pid.to_string()).unwrap();
        std::future::ready(Ok(()))
    })
    .await
    .unwrap();
    assert!(
        fixture.0.join("ran").exists(),
        "CLI must see its registered PID"
    );
}

#[tokio::test]
async fn codex_receives_plain_text_and_eof() {
    let fixture = Fixture::new(
        r#"
[ "$1" = exec ]
prompt=$(/bin/cat)
[ "$prompt" = 'line one
line two' ]
printf '%s\n' '{"type":"thread.started","thread_id":"fake-codex"}' '{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":2}}'
"#,
    );
    let driver = CodexDriver::new(fixture.binary());
    let mut req = test_request(ProviderId::Codex);
    req.prompt = "line one\nline two".into();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(Duration::from_secs(5), driver.run_turn(req, tx))
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(receive(&mut rx).await, Some(StreamEvent::SessionStarted { provider_session_id }) if provider_session_id == "fake-codex")
    );
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Usage {
            input_tokens: Some(1),
            output_tokens: Some(2),
            ..
        })
    ));
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Completed { .. })
    ));
    assert!(receive(&mut rx).await.is_none());
}

#[tokio::test]
async fn nonzero_exit_returns_actionable_stderr_and_redacts_mcp_token() {
    let fixture = Fixture::new(
        r#"
printf '%s\n' '{"type":"turn.failed","error":{"message":"tool failed: fixture-token"}}'
printf '%s\n' 'fixture-token: stderr diagnostic' >&2
exit 7
"#,
    );
    let driver = CodexDriver::new(fixture.binary());
    let mut req = test_request(ProviderId::Codex);
    req.mcp_injection = Some(McpInjectionSpec {
        binary_path: "/fixture/t3d".into(),
        server_url: "http://127.0.0.1:3780".into(),
        internal_token: "fixture-token".into(),
        thread_id: req.thread_id,
        cua_binary_path: None,
        tool_profile: t3_core::McpToolProfile::Full,
    });
    let (tx, mut rx) = mpsc::unbounded_channel();
    driver.run_turn(req, tx).await.unwrap();
    let Some(StreamEvent::Failed { message, .. }) = receive(&mut rx).await else {
        panic!("expected one terminal failure");
    };
    assert!(message.contains("exit status: 7"));
    assert!(message.contains("tool failed: [redacted]"));
    assert!(message.contains("[redacted]: stderr diagnostic"));
    assert!(!message.contains("fixture-token"));
    assert!(receive(&mut rx).await.is_none());
}

#[tokio::test]
async fn repetitive_stream_terminates_the_provider_group() {
    let line = "The model repeats the same line indefinitely.\n";
    let phrase = "We still need to inspect the exact same file again. ";
    for event in [
        json!({
            "type": "stream_event", "event": {"type": "content_block_delta",
                "delta": {"type": "text_delta", "text": line.repeat(8)}}
        }),
        json!({
            "type": "stream_event", "event": {"type": "content_block_delta",
                "delta": {"type": "thinking_delta", "thinking": phrase.repeat(6)}}
        }),
    ] {
        let fixture = Fixture::new(&format!(
            r#"
/bin/sleep 60 &
printf '{{"type":"system","session_id":"%s:%s"}}\n' "$$" "$!"
printf '%s\n' '{event}'
wait
"#
        ));
        let driver = ClaudeDriver::new(fixture.binary());
        let req = test_request(ProviderId::Claude);
        let id = req.run_id;
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(Duration::from_secs(2), driver.run_turn(req, tx))
            .await
            .expect("repetition should stop the provider before it exits")
            .unwrap();
        let Some(StreamEvent::SessionStarted {
            provider_session_id,
        }) = receive(&mut rx).await
        else {
            panic!("fixture did not start");
        };
        let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
        assert_processes_exited(pid.parse().unwrap(), grandchild.parse().unwrap()).await;
        assert!(matches!(
            receive(&mut rx).await,
            Some(StreamEvent::Failed { message, .. }) if message == "repetition loop detected — terminated runaway provider process to save tokens/battery"
        ));
        assert!(receive(&mut rx).await.is_none());
        assert!(!driver.send_steer(id, "late".into()).await.unwrap());
    }
}

#[test]
fn group_signal_treats_teardown_eperm_and_esrch_as_benign() {
    assert!(
        classify_group_signal_error(
            12345,
            libc::SIGKILL,
            io::Error::from_raw_os_error(libc::EPERM),
        )
        .is_ok()
    );
    assert!(
        classify_group_signal_error(
            12345,
            libc::SIGTERM,
            io::Error::from_raw_os_error(libc::EPERM),
        )
        .is_ok()
    );
    assert!(
        classify_group_signal_error(
            12345,
            libc::SIGKILL,
            io::Error::from_raw_os_error(libc::ESRCH),
        )
        .is_ok()
    );
    assert_eq!(
        classify_group_signal_error(12345, 0, io::Error::from_raw_os_error(libc::EPERM))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
    assert_eq!(
        classify_group_signal_error(
            12345,
            libc::SIGKILL,
            io::Error::from_raw_os_error(libc::EINVAL),
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::EINVAL)
    );
}

#[tokio::test]
async fn idle_stdout_timeout_terminates_hung_cli_and_descendants() {
    assert_idle_group_cleanup("wait", "idle timeout").await;
}

#[tokio::test]
async fn idle_timeout_terminates_provider_after_stdout_closes() {
    assert_idle_group_cleanup("exec 1>&-\nwait", "stdout closed").await;
}

#[tokio::test]
async fn stdout_eof_closes_streaming_stdin_and_preserves_exit_diagnostics() {
    let fixture = Fixture::new(
        r#"
IFS= read -r prompt
exec 1>&-
if IFS= read -r extra; then exit 17; fi
echo 'fixture shutdown error' >&2
exit 37
"#,
    );
    let driver = ClaudeDriver::new(fixture.binary());
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(3),
        driver.run_turn(test_request(ProviderId::Claude), tx),
    )
    .await
    .expect("CLI must receive stdin EOF after closing stdout")
    .unwrap();
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Failed { message, .. }) if message.contains("37") && message.contains("fixture shutdown error")
    ));
}
#[tokio::test]
async fn idle_timeout_waits_for_in_flight_agy_tool() {
    let events = run_agy_fixture(
            r#"
printf '%s\n' '{"event":"init","conversation_id":"c"}' \
  '{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"ACTIVE","tool_name":"run_command"}}'
/bin/sleep 2.5
printf '%s\n' '{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"DONE","tool_name":"run_command"}}' \
  '{"event":"result","result":{"status":"SUCCESS","response":"done"}}'
"#,
            Duration::from_secs(1),
        )
        .await;
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { final_text: Some(text) }) if text == "done"),
        "{events:?}"
    );
}

#[tokio::test]
async fn provider_exit_failure_after_result_still_fails() {
    let events = run_claude_fixture(
        r#"
printf '%s\n' '{"type":"result","subtype":"success","result":"ok"}'
exit 3
"#,
        Duration::from_secs(1),
    )
    .await;
    assert!(
        matches!(events.last(), Some(StreamEvent::Failed { message, .. }) if message.contains("exit status: 3")),
        "{events:?}"
    );
}

#[tokio::test]
async fn approved_control_request_writes_allow_response_to_stdin() {
    let answer = answer_control_request(true, json!({})).await;
    assert_eq!(
        answer,
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "cli-req-1",
                "response": {"behavior": "allow", "updatedInput": {"command": "ls -la"}}
            }
        })
    );
}

#[tokio::test]
async fn denied_control_request_writes_deny_response_to_stdin() {
    let answer = answer_control_request(false, json!({})).await;
    assert_eq!(
        answer,
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "cli-req-1",
                "response": {"behavior": "deny", "message": "User denied tool execution"}
            }
        })
    );
}

#[tokio::test]
async fn cancelled_approval_resolves_and_restores_the_idle_timeout() {
    let fixture = Fixture::new(
        r#"
IFS= read -r first
printf '%s\n' '{"type":"control_request","request_id":"cli-req-1","request":{"subtype":"can_use_tool","tool_name":"Write","input":{"file_path":"/w/a"}}}'
printf '%s\n' '{"type":"control_cancel_request","request_id":"cli-req-1"}'
/bin/sleep 30
"#,
    );
    let active = ActiveRunMap::default();
    let run_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(10),
        run_turn_with_idle_timeout(
            &active,
            run_id,
            ProviderId::Claude,
            command(&fixture.binary()),
            Input::StreamJson(json!("write a")),
            None,
            tx,
            Duration::from_millis(800),
        ),
    )
    .await
    .expect("a cancelled approval must not suspend the idle timeout")
    .unwrap();
    let Some(StreamEvent::PendingApproval { request }) = receive(&mut rx).await else {
        panic!("expected a pending approval");
    };
    let resolved = receive(&mut rx).await;
    assert!(
        matches!(resolved, Some(StreamEvent::PendingResolved { request_id }) if request_id == request.id),
        "{resolved:?}"
    );
    let failed = receive(&mut rx).await;
    assert!(
        matches!(&failed, Some(StreamEvent::Failed { message, .. }) if message.contains("idle timeout")),
        "{failed:?}"
    );
}
