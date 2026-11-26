//! Keeps macOS from idle-sleeping while a run is active. Display sleep and power settings are
//! left alone.

use crate::RunCoordinator;
use std::{io, process::Stdio, time::Duration};
use tokio::{
    process::{Child, Command},
    task::JoinSet,
};

/// Far below the shortest idle-sleep timer macOS offers (one minute).
const POLL: Duration = Duration::from_secs(5);

/// A held assertion. Dropping it releases the assertion.
trait Assertion {
    fn alive(&mut self) -> bool;
}

impl Assertion for Child {
    fn alive(&mut self) -> bool {
        matches!(self.try_wait(), Ok(None))
    }
}

struct SleepGuard<A, F> {
    acquire: F,
    held: Option<A>,
    failing: bool,
}

impl<A: Assertion, F: FnMut() -> io::Result<A>> SleepGuard<A, F> {
    fn new(acquire: F) -> Self {
        Self {
            acquire,
            held: None,
            failing: false,
        }
    }

    fn update(&mut self, active_runs: usize) {
        if active_runs == 0 {
            self.held = None;
        } else if !self.held.as_mut().is_some_and(A::alive) {
            match (self.acquire)() {
                Ok(assertion) => {
                    self.held = Some(assertion);
                    self.failing = false;
                }
                Err(error) => {
                    self.held = None;
                    if !std::mem::replace(&mut self.failing, true) {
                        tracing::warn!(%error, "could not prevent idle sleep");
                    }
                }
            }
        }
    }
}

/// `-w` makes caffeinate exit with this process even if it is killed without cleanup.
fn caffeinate() -> io::Result<Child> {
    Command::new("/usr/bin/caffeinate")
        .args(["-i", "-w", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}

/// Watches the active runs until the returned set is dropped. Does nothing outside macOS.
pub(crate) fn spawn(coordinator: RunCoordinator) -> JoinSet<()> {
    let mut tasks = JoinSet::new();
    if cfg!(target_os = "macos") {
        tasks.spawn(async move {
            let mut guard = SleepGuard::new(caffeinate);
            loop {
                let active_runs = coordinator.0.active.lock().await.len();
                guard.update(active_runs);
                tokio::time::sleep(POLL).await;
            }
        });
    }
    tasks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    struct Fake {
        alive: Rc<Cell<bool>>,
        released: Rc<Cell<usize>>,
    }

    impl Assertion for Fake {
        fn alive(&mut self) -> bool {
            self.alive.get()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            self.released.set(self.released.get() + 1);
        }
    }

    #[test]
    fn holds_while_runs_are_active_and_releases_when_idle() {
        let alive = Rc::new(Cell::new(true));
        let released = Rc::new(Cell::new(0));
        let acquired = Cell::new(0);
        let fail = Cell::new(false);
        let mut guard = SleepGuard::new(|| {
            if fail.get() {
                return Err(io::Error::other("spawn failed"));
            }
            acquired.set(acquired.get() + 1);
            alive.set(true);
            Ok(Fake {
                alive: alive.clone(),
                released: released.clone(),
            })
        });
        let mut step = |active_runs| {
            guard.update(active_runs);
            (acquired.get(), released.get(), guard.held.is_some())
        };

        assert_eq!(step(0), (0, 0, false));
        assert_eq!(step(1), (1, 0, true));
        assert_eq!(step(2), (1, 0, true));
        assert_eq!(step(1), (1, 0, true));
        assert_eq!(step(0), (1, 1, false));
        assert_eq!(step(0), (1, 1, false));
        assert_eq!(step(1), (2, 1, true));

        // An assertion that died on its own is replaced; a failed attempt is retried.
        alive.set(false);
        assert_eq!(step(1), (3, 2, true));
        alive.set(false);
        fail.set(true);
        assert_eq!(step(1), (3, 3, false));
        fail.set(false);
        assert_eq!(step(1), (4, 3, true));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "creates a real power assertion"]
    async fn real_caffeinate_asserts_and_releases() {
        let holds = |pid: u32| {
            let output = std::process::Command::new("/usr/bin/pmset")
                .args(["-g", "assertions"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| line.contains(&format!("pid {pid}(caffeinate)")))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let mut guard = SleepGuard::new(caffeinate);
        guard.update(1);
        let pid = guard.held.as_ref().unwrap().id().unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let held = holds(pid);
        println!("{held:#?}");
        assert!(guard.held.as_mut().unwrap().alive());
        assert_eq!(held.len(), 1);
        assert!(held[0].contains("PreventUserIdleSystemSleep"));
        guard.update(0);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(holds(pid).is_empty());
    }
}
