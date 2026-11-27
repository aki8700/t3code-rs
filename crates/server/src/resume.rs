//! Resuming provider sessions: the pre-resume session check and the restart recovery of runs
//! that a stopped server left in flight.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use t3_core::ProviderId;
use t3_store::OrphanProcess;

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
pub(crate) async fn stop_orphaned_processes(
    orphan_processes: Vec<OrphanProcess>,
) -> Vec<OrphanProcess> {
    let ps = tokio::task::spawn_blocking(move || {
        let output = Command::new("ps")
            .args(["-A", "-ww", "-o", "pid=,pgid=,uid=,lstart="])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
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
    stop_orphaned_processes_from_snapshot_with_probe(orphan_processes, &ps, process_identity_alive)
        .await
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityProbe {
    /// Recorded leader (pid == pgid, uid, start_time) is still present.
    LeaderMatch,
    /// Process group has no leader matching the recorded start_time, and no new leader recycled the pgid.
    LeaderGone,
    /// A different leader now owns this pgid (PGID recycled); never signal it.
    Recycled,
    /// Could not inspect the process table; never signal or clear pending rows.
    Failed,
}

#[cfg(all(unix, test))]
pub(crate) async fn stop_orphaned_processes_from_snapshot(
    orphan_processes: Vec<OrphanProcess>,
    ps: &str,
) -> Vec<OrphanProcess> {
    stop_orphaned_processes_from_snapshot_with_probe(orphan_processes, ps, |target| {
        if !group_alive(target.process_group_id) {
            return IdentityProbe::LeaderGone;
        }
        probe_target_in_ps(ps, target, unsafe { libc::getuid() }, unsafe {
            libc::getpgrp()
        })
    })
    .await
}

#[cfg(unix)]
async fn stop_orphaned_processes_from_snapshot_with_probe<F>(
    orphan_processes: Vec<OrphanProcess>,
    ps: &str,
    probe: F,
) -> Vec<OrphanProcess>
where
    F: Fn(&OrphanProcess) -> IdentityProbe,
{
    let groups = {
        // SAFETY: getuid and getpgrp have no preconditions.
        let (uid, own_group) = unsafe { (libc::getuid(), libc::getpgrp()) };
        matching_groups(ps, &orphan_processes, uid, own_group)
    };
    stop_verified_groups(&orphan_processes, &groups, probe).await
}

#[cfg(unix)]
const SIGTERM_GRACE_MAX: std::time::Duration = std::time::Duration::from_millis(1500);
#[cfg(unix)]
const SIGTERM_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(unix)]
async fn wait_for_groups_exit(groups: &[OrphanProcess]) {
    wait_for_exit_with(
        || {
            groups
                .iter()
                .any(|target| group_alive(target.process_group_id))
        },
        SIGTERM_GRACE_MAX,
        SIGTERM_POLL_INTERVAL,
    )
    .await;
}

#[cfg(unix)]
async fn wait_for_exit_with<F>(
    mut any_alive: F,
    max_wait: std::time::Duration,
    poll_interval: std::time::Duration,
) -> bool
where
    F: FnMut() -> bool,
{
    if !any_alive() {
        return true;
    }
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return !any_alive();
        }
        let sleep_for = poll_interval.min(deadline.saturating_duration_since(now));
        tokio::time::sleep(sleep_for).await;
        if !any_alive() {
            return true;
        }
    }
}

#[cfg(unix)]
async fn stop_verified_groups(
    targets: &[OrphanProcess],
    groups: &[OrphanProcess],
    probe: impl Fn(&OrphanProcess) -> IdentityProbe,
) -> Vec<OrphanProcess> {
    for target in groups {
        tracing::warn!(%target.run_id, pgid = target.process_group_id, "Stopping a provider process left by the previous server");
        // SAFETY: group IDs were matched to the persisted provider leader identity above.
        unsafe {
            libc::killpg(target.process_group_id, libc::SIGTERM);
        }
    }
    if !groups.is_empty() {
        wait_for_groups_exit(groups).await;
    }
    let survivors: Vec<_> = groups
        .iter()
        .filter(|target| {
            if !group_alive(target.process_group_id) {
                return false;
            }
            // Send SIGKILL if either the original leader is still alive OR the leader
            // exited on SIGTERM while un-recycled child processes remain in the group.
            // Never SIGKILL on PGID recycle or ps probe failure.
            matches!(
                probe(target),
                IdentityProbe::LeaderMatch | IdentityProbe::LeaderGone
            )
        })
        .cloned()
        .collect();
    for target in &survivors {
        // SAFETY: the group is verified not recycled and probe succeeded above.
        unsafe {
            libc::killpg(target.process_group_id, libc::SIGKILL);
        }
    }
    if !survivors.is_empty() {
        wait_for_groups_exit(&survivors).await;
    }
    targets
        .iter()
        .filter(|target| match probe(target) {
            IdentityProbe::Recycled => true,
            IdentityProbe::LeaderGone => !group_alive(target.process_group_id),
            IdentityProbe::LeaderMatch | IdentityProbe::Failed => false,
        })
        .cloned()
        .collect()
}

#[cfg(not(unix))]
pub(crate) async fn stop_orphaned_processes(_targets: Vec<OrphanProcess>) -> Vec<OrphanProcess> {
    Vec::new()
}

/// Recorded process group leaders in `ps -o pid=,pgid=,uid=,lstart=` output.
#[cfg(unix)]
fn matching_groups(
    ps: &str,
    targets: &[OrphanProcess],
    uid: libc::uid_t,
    own_group: libc::pid_t,
) -> Vec<OrphanProcess> {
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
            && !groups
                .iter()
                .any(|existing: &OrphanProcess| existing.process_group_id == group)
        {
            groups.push(target.clone());
        }
    }
    groups
}

#[cfg(unix)]
fn probe_target_in_ps(
    ps: &str,
    target: &OrphanProcess,
    uid: libc::uid_t,
    own_group: libc::pid_t,
) -> IdentityProbe {
    if target.process_group_id <= 1 || target.process_group_id == own_group {
        return IdentityProbe::Recycled;
    }
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
        if pid == target.process_group_id && group == target.process_group_id {
            let start_time = fields[3..8].join(" ");
            return if owner == uid && start_time == target.start_time {
                IdentityProbe::LeaderMatch
            } else {
                IdentityProbe::Recycled
            };
        }
    }
    IdentityProbe::LeaderGone
}

#[cfg(unix)]
fn process_identity_alive(target: &OrphanProcess) -> IdentityProbe {
    let Ok(output) = Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,pgid=,uid=,lstart="])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .output()
    else {
        return IdentityProbe::Failed;
    };
    if !output.status.success() {
        return IdentityProbe::Failed;
    }
    probe_target_in_ps(
        &String::from_utf8_lossy(&output.stdout),
        target,
        unsafe { libc::getuid() },
        unsafe { libc::getpgrp() },
    )
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
    use uuid::Uuid;

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
        let groups = matching_groups(&ps, std::slice::from_ref(&target), 501, 77);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], target);
        let changed_start = OrphanProcess {
            start_time: "Fri Nov 27 13:35:00 2026".into(),
            ..target
        };
        assert!(matching_groups(&ps, &[changed_start], 501, 77).is_empty());
    }

    #[test]
    fn single_digit_day_lstart_spacing_matches_saved_normalized_time() {
        let target = OrphanProcess {
            run_id: Uuid::new_v4(),
            process_group_id: 101,
            start_time: "Sat Nov 7 09:05:01 2026".into(),
        };
        let ps = "101 101 501 Sat Nov  7 09:05:01 2026 provider\n";
        let groups = matching_groups(ps, std::slice::from_ref(&target), 501, 77);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], target);
    }

    #[tokio::test]
    async fn stops_mcp_disabled_provider_process_by_recorded_identity() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30; :"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        let waiter = std::thread::spawn(move || {
            let mut child = child;
            child.wait().unwrap()
        });
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
        let status = waiter.join().unwrap();
        assert_eq!(stopped.len(), 1);
        assert!(!status.success());
    }

    #[tokio::test]
    async fn wait_for_exit_returns_quickly_once_process_exits() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let checks = AtomicUsize::new(0);
        let start = std::time::Instant::now();
        let exited = wait_for_exit_with(
            || checks.fetch_add(1, Ordering::SeqCst) < 2,
            std::time::Duration::from_millis(1500),
            std::time::Duration::from_millis(20),
        )
        .await;
        assert!(exited);
        assert!(start.elapsed() < std::time::Duration::from_millis(300));
        assert!(
            !wait_for_exit_with(
                || true,
                std::time::Duration::from_millis(60),
                std::time::Duration::from_millis(20),
            )
            .await
        );
    }
}
