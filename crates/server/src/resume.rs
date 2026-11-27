//! Resuming provider sessions: the pre-resume session check and the restart recovery of runs
//! that a stopped server left in flight.

use std::path::{Path, PathBuf};

use t3_core::ProviderId;
use t3_store::OrphanProcess;
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

/// Stops recorded provider process groups and returns runs confirmed stopped.
#[cfg(unix)]
pub(crate) async fn stop_orphaned_processes(orphan_processes: Vec<OrphanProcess>) -> Vec<Uuid> {
    let ps = tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("ps")
            .args(["-A", "-ww", "-o", "pid=,pgid=,uid=,lstart="])
            .output()?;
        if !output.status.success() {
            return Err(std::io::Error::other("ps returned a failure status"));
        }
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&output.stdout).into_owned())
    })
    .await
    .unwrap_or_else(|error| Err(std::io::Error::other(error)));
    let ps = match ps {
        Ok(ps) => ps,
        Err(error) => {
            tracing::warn!(%error, "Could not list processes to find orphaned runs");
            return Vec::new();
        }
    };
    stop_orphaned_processes_from_snapshot(orphan_processes, &ps).await
}

#[cfg(unix)]
pub(crate) async fn stop_orphaned_processes_from_snapshot(
    orphan_processes: Vec<OrphanProcess>,
    ps: &str,
) -> Vec<Uuid> {
    let groups = {
        // SAFETY: getuid and getpgrp have no preconditions.
        let (uid, own_group) = unsafe { (libc::getuid(), libc::getpgrp()) };
        matching_groups(ps, &orphan_processes, uid, own_group)
    };
    stop_verified_groups(&orphan_processes, &groups).await
}

#[cfg(unix)]
async fn stop_verified_groups(
    targets: &[OrphanProcess],
    groups: &[(Uuid, libc::pid_t)],
) -> Vec<Uuid> {
    for (run_id, group) in groups {
        tracing::warn!(%run_id, pgid = group, "Stopping a provider process left by the previous server");
        // SAFETY: group IDs were matched to the persisted provider leader identity above.
        unsafe {
            libc::killpg(*group, libc::SIGTERM);
        }
    }
    if !groups.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }
    let survivors: Vec<_> = groups
        .iter()
        .filter(|(_, group)| group_alive(*group))
        .copied()
        .collect();
    for (_, group) in &survivors {
        // SAFETY: group IDs were verified against the recorded process identity.
        unsafe {
            libc::killpg(*group, libc::SIGKILL);
        }
    }
    if !survivors.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    }
    let stopped: Vec<_> = targets
        .iter()
        .filter(|target| !group_alive(target.process_group_id))
        .map(|target| target.run_id)
        .collect();
    stopped
}

#[cfg(not(unix))]
pub(crate) async fn stop_orphaned_processes(_targets: Vec<OrphanProcess>) -> Vec<Uuid> {
    Vec::new()
}

/// Recorded process group leaders in `ps -o pid=,pgid=,uid=,lstart=` output.
#[cfg(unix)]
fn matching_groups(
    ps: &str,
    targets: &[OrphanProcess],
    uid: libc::uid_t,
    own_group: libc::pid_t,
) -> Vec<(Uuid, libc::pid_t)> {
    let mut groups = Vec::new();
    for line in ps.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let (Ok(pid), Ok(group), Ok(owner)) = (
            fields[0].parse::<libc::pid_t>(),
            fields[1].parse::<libc::pid_t>(),
            fields[2].parse::<libc::uid_t>(),
        ) else {
            continue;
        };
        let start_time = fields[3..8].join(" ");
        if pid != group || owner != uid || group <= 1 || group == own_group {
            continue;
        }
        if let Some(target) = targets
            .iter()
            .find(|target| target.process_group_id == group && target.start_time == start_time)
            && !groups.iter().any(|(_, existing)| *existing == group)
        {
            groups.push((target.run_id, group));
        }
    }
    groups
}

#[cfg(unix)]
fn group_alive(group: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the group exists.
    unsafe { libc::killpg(group, 0) == 0 }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn recorded_group_identity_matches_only_the_same_leader_and_start_time() {
        let thread = Uuid::new_v4();
        let ps = format!(
            "  101   101   501 Fri Nov 27 13:34:00 2026 prompt-{thread}
  102   101   501 Fri Nov 27 13:34:01 2026 provider child
  103   103   501 Fri Nov 27 13:34:00 2026 different process
  104   104     0 Fri Nov 27 13:34:00 2026 other user
  105    77   501 Fri Nov 27 13:34:00 2026 current server
garbage line
"
        );
        let target = OrphanProcess {
            run_id: Uuid::new_v4(),
            process_group_id: 101,
            start_time: "Fri Nov 27 13:34:00 2026".into(),
        };
        assert_eq!(
            matching_groups(&ps, std::slice::from_ref(&target), 501, 77),
            vec![(target.run_id, 101)]
        );
        let changed_start = OrphanProcess {
            start_time: "Fri Nov 27 13:35:00 2026".into(),
            ..target
        };
        assert!(matching_groups(&ps, &[changed_start], 501, 77).is_empty());
    }

    #[tokio::test]
    async fn stops_mcp_disabled_provider_process_by_recorded_identity() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30; :"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let target = OrphanProcess {
            run_id: Uuid::new_v4(),
            process_group_id: pid as i32,
            start_time: "Fri Nov 27 13:34:00 2026".into(),
        };
        // This provider command intentionally has no MCP/thread-id arguments. The process table
        // snapshot proves the persisted PGID and leader start time are sufficient to select it.
        let snapshot = format!(
            "{pid} {pid} {} {}",
            unsafe { libc::getuid() },
            target.start_time
        );
        let stopped = stop_orphaned_processes_from_snapshot(vec![target], &snapshot).await;
        let status = child.wait().unwrap();
        assert_eq!(stopped.len(), 1);
        assert!(!status.success());
    }
}
