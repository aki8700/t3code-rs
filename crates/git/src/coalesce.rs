use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use t3_core::{CoreError, CoreResult};
use tokio::sync::{Mutex, Notify};

struct SlotState<T> {
    in_flight: bool,
    due_after_in_flight: bool,
    completed_cycle: u64,
    last_result: Option<Result<T, String>>,
}

impl<T> Default for SlotState<T> {
    fn default() -> Self {
        Self {
            in_flight: false,
            due_after_in_flight: false,
            completed_cycle: 0,
            last_result: None,
        }
    }
}

struct Slot<T> {
    state: Mutex<SlotState<T>>,
    notify: Notify,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(SlotState::default()),
            notify: Notify::new(),
        }
    }
}

/// Coalesces concurrent repository operations by repo path into at most one in-flight run
/// plus one follow-up rerun (`in_flight` + `due_after_in_flight`).
pub struct RepoCoalescer<T> {
    slots: Mutex<HashMap<PathBuf, Arc<Slot<T>>>>,
}

impl<T> Default for RepoCoalescer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> RepoCoalescer<T> {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl<T: Clone + Send + 'static> RepoCoalescer<T> {
    pub async fn run<F, Fut>(&self, repo: &Path, work: F) -> CoreResult<T>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = CoreResult<T>>,
    {
        let repo_key = repo.to_path_buf();
        let slot = {
            let mut slots = self.slots.lock().await;
            Arc::clone(slots.entry(repo_key.clone()).or_default())
        };

        let target_cycle = {
            let mut state = slot.state.lock().await;
            if state.in_flight {
                state.due_after_in_flight = true;
                Some(state.completed_cycle + 2)
            } else {
                state.in_flight = true;
                state.due_after_in_flight = false;
                None
            }
        };

        if let Some(target_cycle) = target_cycle {
            loop {
                let notified = {
                    let state = slot.state.lock().await;
                    if state.completed_cycle >= target_cycle || !state.in_flight {
                        return match state.last_result.clone() {
                            Some(Ok(value)) => Ok(value),
                            Some(Err(err)) => Err(CoreError::Git(err)),
                            None => Err(CoreError::Git(
                                "coalesced git operation finished without a result".into(),
                            )),
                        };
                    }
                    slot.notify.notified()
                };
                notified.await;
            }
        }

        loop {
            let res = work().await;
            let cached = match &res {
                Ok(value) => Ok(value.clone()),
                Err(err) => Err(err.to_string()),
            };

            let mut state = slot.state.lock().await;
            state.completed_cycle += 1;
            state.last_result = Some(cached);
            if state.due_after_in_flight {
                state.due_after_in_flight = false;
                slot.notify.notify_waiters();
                continue;
            }

            state.in_flight = false;
            slot.notify.notify_waiters();
            drop(state);

            let mut slots = self.slots.lock().await;
            if let Some(current) = slots.get(&repo_key)
                && Arc::ptr_eq(current, &slot)
            {
                let st = slot.state.lock().await;
                if !st.in_flight {
                    slots.remove(&repo_key);
                }
            }
            return res;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Barrier;

    #[tokio::test]
    async fn coalesces_five_concurrent_calls_for_same_repo_into_at_most_two_runs() {
        let coalescer = Arc::new(RepoCoalescer::<usize>::new());
        let repo = PathBuf::from("/tmp/repo-coalesce-same");
        let calls = Arc::new(AtomicUsize::new(0));
        let first_started = Arc::new(Barrier::new(2));

        let c0 = Arc::clone(&coalescer);
        let r0 = repo.clone();
        let calls0 = Arc::clone(&calls);
        let started0 = Arc::clone(&first_started);
        let leader = tokio::spawn(async move {
            c0.run(&r0, || {
                let calls = Arc::clone(&calls0);
                let started = Arc::clone(&started0);
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        started.wait().await;
                        tokio::time::sleep(Duration::from_millis(40)).await;
                    }
                    Ok(n)
                }
            })
            .await
        });

        first_started.wait().await;

        let mut followers = Vec::new();
        for _ in 0..4 {
            let c = Arc::clone(&coalescer);
            let r = repo.clone();
            let calls_clone = Arc::clone(&calls);
            followers.push(tokio::spawn(async move {
                c.run(&r, || {
                    let calls = Arc::clone(&calls_clone);
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                        Ok(n)
                    }
                })
                .await
            }));
        }

        let leader_val = leader.await.unwrap().unwrap();
        assert_eq!(leader_val, 2);
        for handle in followers {
            let val = handle.await.unwrap().unwrap();
            assert_eq!(val, 2);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn distinct_repo_paths_execute_in_parallel_without_blocking() {
        let coalescer = Arc::new(RepoCoalescer::<&'static str>::new());
        let barrier = Arc::new(Barrier::new(2));

        let c_a = Arc::clone(&coalescer);
        let b_a = Arc::clone(&barrier);
        let task_a = tokio::spawn(async move {
            c_a.run(Path::new("/tmp/repo-a"), || {
                let b = Arc::clone(&b_a);
                async move {
                    b.wait().await;
                    Ok("a")
                }
            })
            .await
        });

        let c_b = Arc::clone(&coalescer);
        let b_b = Arc::clone(&barrier);
        let task_b = tokio::spawn(async move {
            c_b.run(Path::new("/tmp/repo-b"), || {
                let b = Arc::clone(&b_b);
                async move {
                    b.wait().await;
                    Ok("b")
                }
            })
            .await
        });

        let (res_a, res_b) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(task_a, task_b)
        })
        .await
        .expect("distinct repositories must not block each other");

        assert_eq!(res_a.unwrap().unwrap(), "a");
        assert_eq!(res_b.unwrap().unwrap(), "b");
    }
}
