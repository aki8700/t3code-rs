use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};
use uuid::Uuid;

/// The registry is locked only to find a key. Holders and waiters keep the
/// individual lock alive even when an operation is cancelled.
#[derive(Default)]
pub(crate) struct KeyedLocks(Mutex<HashMap<Uuid, Weak<RwLock<()>>>>);

impl KeyedLocks {
    pub(crate) fn get(&self, id: Uuid) -> Arc<RwLock<()>> {
        let mut locks = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lock) = locks.get(&id).and_then(Weak::upgrade) {
            return lock;
        }
        if locks.len() == locks.capacity() {
            locks.retain(|_, lock| lock.strong_count() > 0);
        }
        let lock = Arc::new(RwLock::new(()));
        locks.insert(id, Arc::downgrade(&lock));
        lock
    }

    pub(super) fn live_keys(&self) -> Vec<Uuid> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(&id, lock)| (lock.strong_count() > 0).then_some(id))
            .collect()
    }
}

pub(crate) struct ThreadGuard {
    pub(crate) id: Uuid,
    _guard: OwnedRwLockWriteGuard<()>,
}

impl super::RunCoordinator {
    pub(crate) async fn lock_thread(&self, id: Uuid) -> ThreadGuard {
        ThreadGuard {
            id,
            _guard: self.0.thread_locks.get(id).write_owned().await,
        }
    }
}
