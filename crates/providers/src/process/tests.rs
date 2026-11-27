use super::*;
use crate::{AntigravityDriver, ClaudeDriver, CodexDriver, OpenCodeDriver, test_request};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use t3_core::{
    traits::ProviderDriver,
    types::{McpInjectionSpec, PendingResponse},
};

pub(super) struct Fixture(pub(super) PathBuf);

impl Fixture {
    pub(super) fn new(script: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("t3-provider-fixture-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let file = dir.join("cli");
        fs::write(&file, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
        Self(dir)
    }

    pub(super) fn binary(&self) -> PathBuf {
        self.0.join("cli")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) async fn receive(rx: &mut mpsc::UnboundedReceiver<StreamEvent>) -> Option<StreamEvent> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        if matches!(event, Some(StreamEvent::ProcessStarted { .. })) {
            continue;
        }
        return event;
    }
}

pub(super) async fn assert_processes_exited(pid: u32, grandchild: i32) {
    let pids = format!("{pid},{grandchild}");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let output = command(Path::new("ps"))
                .args(["-o", "stat=", "-p", &pids])
                .output()
                .await
                .unwrap();
            // A killed descendant can briefly remain as a zombie until its new parent reaps it.
            if output.stderr.is_empty()
                && String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .all(|status| status.trim_start().starts_with('Z'))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("a process is still running after group cleanup");
}

pub(super) async fn check_group_cleanup(interrupt_turn: bool) {
    let fixture = Fixture::new(
        r#"
trap '' TERM
/bin/sleep 60 &
printf '{"type":"system","session_id":"%s:%s"}\n' "$$" "$!"
wait
"#,
    );
    let driver = ClaudeDriver::new(fixture.binary());
    let req = test_request(ProviderId::Claude);
    let id = req.run_id;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let running = driver.clone();
    let task = tokio::spawn(async move { running.run_turn(req, tx).await });
    let Some(StreamEvent::SessionStarted {
        provider_session_id,
    }) = receive(&mut rx).await
    else {
        panic!("fixture did not start");
    };
    let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
    let pid = pid.parse().unwrap();
    let grandchild = grandchild.parse().unwrap();
    // SAFETY: getpgid only queries process metadata.
    assert_eq!(unsafe { libc::getpgid(grandchild) }, pid as i32);
    if interrupt_turn {
        driver.interrupt(id).await.unwrap();
        let event = receive(&mut rx).await;
        assert!(
            matches!(event, Some(StreamEvent::Failed { message, .. }) if message == "interrupted")
        );
        task.await.unwrap().unwrap();
    } else {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    assert!(!driver.send_steer(id, "late".into()).await.unwrap());
    assert_processes_exited(pid, grandchild).await;
}

pub(super) async fn assert_idle_group_cleanup(wait_script: &str, expected_failure: &str) {
    let fixture = Fixture::new(&format!(
        r#"
/bin/sleep 60 >/dev/null &
printf '{{"type":"system","session_id":"%s:%s"}}\n' "$$" "$!"
{wait_script}
"#,
    ));
    let active = ActiveRunMap::default();
    let run_id = Uuid::new_v4();
    let mut cmd = command(&fixture.binary());
    cmd.arg("--print").arg("hello");
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(3),
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
    .expect("idle timeout must terminate hung CLI")
    .unwrap();

    let first = receive(&mut rx).await;
    let Some(StreamEvent::SessionStarted {
        provider_session_id,
    }) = first
    else {
        panic!("expected fixture session, received {first:?}");
    };
    let (pid, grandchild) = provider_session_id.split_once(':').unwrap();
    assert_processes_exited(pid.parse().unwrap(), grandchild.parse().unwrap()).await;
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Failed { message, .. }) if message.contains(expected_failure)
    ));
    assert!(receive(&mut rx).await.is_none());
    assert!(active.lock().unwrap().is_empty());
}

pub(super) async fn run_agy_fixture(script: &str, idle: Duration) -> Vec<StreamEvent> {
    let fixture = Fixture::new(script);
    let (tx, mut rx) = mpsc::unbounded_channel();
    run_turn_with_idle_timeout(
        &ActiveRunMap::default(),
        Uuid::new_v4(),
        ProviderId::Antigravity,
        command(&fixture.binary()),
        Input::None,
        None,
        tx,
        idle,
    )
    .await
    .unwrap();
    let mut events = Vec::new();
    while let Some(event) = receive(&mut rx).await {
        events.push(event);
    }
    events
}

// agy holds every later step on stdout until an ACTIVE tool step finishes,
// so a long background command looks silent even though the agent is working.

pub(super) async fn run_claude_fixture(script: &str, idle: Duration) -> Vec<StreamEvent> {
    let fixture = Fixture::new(script);
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        Duration::from_secs(5),
        run_turn_with_idle_timeout(
            &ActiveRunMap::default(),
            Uuid::new_v4(),
            ProviderId::Claude,
            command(&fixture.binary()),
            Input::None,
            None,
            tx,
            idle,
        ),
    )
    .await
    .expect("idle timeout must end the run")
    .unwrap();
    let mut events = Vec::new();
    while let Some(event) = receive(&mut rx).await {
        events.push(event);
    }
    events
}

pub(super) fn assert_completed_after_t3d_stop(events: &[StreamEvent]) {
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { final_text: Some(text) }) if text == "ok"),
        "{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::Item { kind: TurnItemKind::SystemNotice, summary: Some(summary), .. }
                if summary.contains("t3d stopped")
        )),
        "{events:?}"
    );
}

// A CLI can emit its turn result and then stay alive (e.g. waiting on a
// background task). Killing it is t3d's cleanup, not a provider failure.

/// Runs a Claude turn whose CLI asks permission to run `ls -la`, answers it through
/// `respond_request`, and returns the single line the CLI read back from stdin.
pub(super) async fn answer_control_request(approved: bool, payload: Value) -> Value {
    let fixture = Fixture::new(
        r#"
case " $* " in
    *" --permission-mode default --permission-prompt-tool stdio "*) ;;
    *) exit 9 ;;
esac
IFS= read -r first
printf '%s\n' '{"type":"control_request","request_id":"cli-req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls -la"}}}'
IFS= read -r answer
printf '%s\n' "$answer" > "$(dirname "$0")/answer.json"
printf '%s\n' '{"type":"result","result":"ok"}'
if IFS= read -r unexpected; then
    exit 17
fi
"#,
    );
    let driver = ClaudeDriver::new(fixture.binary());
    let req = test_request(ProviderId::Claude);
    let run_id = req.run_id;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let running = driver.clone();
    let task = tokio::spawn(async move { running.run_turn(req, tx).await });

    let Some(StreamEvent::PendingApproval { request }) = receive(&mut rx).await else {
        panic!("expected a pending approval");
    };
    assert_eq!(request.title, "Bash");
    assert_eq!(request.payload["command"], "ls -la");
    let resp = PendingResponse {
        request_id: request.id,
        approved,
        payload,
    };
    assert!(driver.respond_request(run_id, resp.clone()).await.unwrap());
    // The request is consumed once answered, so a repeated answer is refused.
    assert!(!driver.respond_request(run_id, resp).await.unwrap());
    assert!(matches!(
        receive(&mut rx).await,
        Some(StreamEvent::Completed { .. })
    ));
    task.await.unwrap().unwrap();

    let answer = fs::read_to_string(fixture.0.join("answer.json")).unwrap();
    serde_json::from_str(&answer).unwrap()
}

#[path = "idle_timeout_tests.rs"]
mod idle_timeout_tests;
#[path = "tests_a.rs"]
mod tests_a;
#[path = "tests_b.rs"]
mod tests_b;
#[path = "tests_c.rs"]
mod tests_c;
