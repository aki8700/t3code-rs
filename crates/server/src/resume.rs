//! Resuming provider sessions: the pre-resume session check and the restart recovery of runs
//! that a stopped server left in flight.

use std::path::{Path, PathBuf};

use t3_core::ProviderId;
use uuid::Uuid;

/// Sent in place of the original prompt when a run resumes after a server restart. The session
/// already holds the original request, so resending it would repeat the work.
pub(crate) const RESTART_PROMPT: &str = "The harness restarted and interrupted your previous turn. Check the current state of the files and git first, then continue the interrupted work.";

/// Whether `session_id` can be resumed from `workspace`; see
/// `usage::provider_session_resumable`.
pub(crate) async fn session_resumable(
    provider: ProviderId,
    home: Option<PathBuf>,
    workspace: &Path,
    session_id: &str,
) -> bool {
    let workspace = workspace.to_path_buf();
    let session_id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::usage::provider_session_resumable(provider, home.as_deref(), &workspace, &session_id)
    })
    .await
    .unwrap_or(false)
}

/// Stops provider processes that a previous server left running for `thread_id`.
///
/// Runs do not record their pid, but every provider process with the t3 MCP server carries
/// `--thread-id <thread>` in its own or its MCP child's command line. The thread id is a random
/// UUID, so a match cannot be a recycled pid. The process group is signalled, as at spawn
/// each provider becomes the leader of its own group.
#[cfg(unix)]
pub(crate) async fn stop_orphaned_processes(thread_id: Uuid) {
    let groups = tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("ps")
            .args(["-A", "-ww", "-o", "pid=,pgid=,uid=,command="])
            .output()?;
        // SAFETY: getuid and getpgrp have no preconditions.
        let (uid, own_group) = unsafe { (libc::getuid(), libc::getpgrp()) };
        Ok::<_, std::io::Error>(orphan_groups(
            &String::from_utf8_lossy(&output.stdout),
            thread_id,
            uid,
            own_group,
        ))
    })
    .await
    .unwrap_or_else(|error| Err(std::io::Error::other(error)));
    let groups = match groups {
        Ok(groups) => groups,
        Err(error) => {
            tracing::warn!(%error, %thread_id, "Could not list processes to find orphaned runs");
            return;
        }
    };
    for group in groups {
        tracing::warn!(%thread_id, pgid = group, "Stopping a provider process left by the previous server");
        stop_group(group).await;
    }
}

#[cfg(not(unix))]
pub(crate) async fn stop_orphaned_processes(_thread_id: Uuid) {}

/// Process groups in `ps -o pid=,pgid=,uid=,command=` output that belong to `thread_id`.
#[cfg(unix)]
fn orphan_groups(
    ps: &str,
    thread_id: Uuid,
    uid: libc::uid_t,
    own_group: libc::pid_t,
) -> Vec<libc::pid_t> {
    let spaced = format!("--thread-id {thread_id}");
    let quoted = format!("\"--thread-id\",\"{thread_id}\"");
    let mut groups = Vec::new();
    for line in ps.lines() {
        let mut rest = line.trim_start();
        let mut field = || {
            let (value, tail) = rest.split_once(char::is_whitespace)?;
            rest = tail.trim_start();
            Some(value)
        };
        let (Some(_pid), Some(group), Some(owner)) = (field(), field(), field()) else {
            continue;
        };
        let (Ok(group), Ok(owner)) = (group.parse::<libc::pid_t>(), owner.parse::<libc::uid_t>())
        else {
            continue;
        };
        if owner == uid
            && group > 1
            && group != own_group
            && (rest.contains(&spaced) || rest.contains(&quoted))
            && !groups.contains(&group)
        {
            groups.push(group);
        }
    }
    groups
}

/// SIGTERM, then SIGKILL when the group outlives the grace period, as `process::interrupt` does.
#[cfg(unix)]
async fn stop_group(group: libc::pid_t) {
    use std::time::Duration;

    let alive = || {
        // SAFETY: signal 0 only checks that the group exists.
        unsafe { libc::killpg(group, 0) == 0 }
    };
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        // SAFETY: killpg has no memory preconditions; group > 1 excludes init and broadcast.
        if unsafe { libc::killpg(group, signal) } != 0 {
            return;
        }
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if !alive() {
                return;
            }
        }
    }
    tracing::warn!(
        pgid = group,
        "A provider process left by the previous server is still alive"
    );
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn orphan_groups_match_the_thread_and_skip_others() {
        let thread = Uuid::new_v4();
        let other = Uuid::new_v4();
        let ps = format!(
            "  101   101   501 claude -p --mcp-config {{\"mcpServers\":{{\"t3rs\":{{\"args\":[\"mcp\",\"--url\",\"http://x\",\"--thread-id\",\"{thread}\"]}}}}}}
  102   101   501 /bin/t3d mcp --url http://x --thread-id {thread}
  103   103   501 /bin/t3d mcp --url http://x --thread-id {other}
  104   104     0 /bin/t3d mcp --url http://x --thread-id {thread}
  105    77   501 /bin/t3d mcp --url http://x --thread-id {thread}
  106   106   501 codex exec -c mcp_servers.t3rs.args=[\"mcp\",\"--thread-id\",\"{thread}\"] -
garbage line
"
        );
        assert_eq!(orphan_groups(&ps, thread, 501, 77), vec![101, 106]);
    }

    #[tokio::test]
    async fn stops_a_live_process_group() {
        let thread = Uuid::new_v4();
        let mut child = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "sleep 30; :",
                "sh",
                "--thread-id",
                &thread.to_string(),
            ])
            .process_group(0)
            .spawn()
            .unwrap();
        stop_orphaned_processes(thread).await;
        let status = child.wait().unwrap();
        assert!(!status.success());
    }
}
