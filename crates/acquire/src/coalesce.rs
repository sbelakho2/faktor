//! Request coalescing by normalized acquisition identity.
//!
//! `docs/acquire.md` §11: one external request, N awaiters. Callers that
//! arrive while an identical acquisition (same normalized identity, account
//! scope, requested fields and freshness policy) is in flight wait for the
//! same result instead of issuing a second request. The leader owns the
//! external request; followers never touch the transport.
//!
//! Cancellation is honest: if the leader is cancelled or its future is
//! dropped, followers resolve with `Cancelled` rather than waiting forever,
//! and the in-flight slot is removed so the next caller can retry.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ctx::lock;
use crate::error::AcquisitionError;
use crate::request::AcquisitionKey;

type SharedResult<T> = Result<Arc<T>, AcquisitionError>;

struct Slot<T> {
    tx: tokio::sync::watch::Sender<Option<SharedResult<T>>>,
}

/// Counters for one coalescer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoalescerStats {
    /// Requests that actually executed the operation.
    pub leaders: u64,
    /// Callers that attached to an in-flight request.
    pub waiters: u64,
}

/// Coalesces identical in-flight acquisitions.
pub struct Coalescer<T> {
    inflight: Mutex<BTreeMap<AcquisitionKey, Arc<Slot<T>>>>,
    stats: Mutex<CoalescerStats>,
}

impl<T> Default for Coalescer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Coalescer<T> {
    /// An empty coalescer.
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(BTreeMap::new()),
            stats: Mutex::new(CoalescerStats::default()),
        }
    }

    /// How many acquisitions are in flight.
    pub fn in_flight(&self) -> usize {
        lock(&self.inflight).len()
    }

    /// The counters.
    pub fn stats(&self) -> CoalescerStats {
        *lock(&self.stats)
    }
}

impl<T: Send + Sync + 'static> Coalescer<T> {
    /// Run `op` for `key`, or wait for the identical in-flight run.
    ///
    /// The leader executes `op` exactly once; every follower that arrives
    /// before completion receives a clone of the same shared result.
    pub async fn run<F, Fut>(&self, key: AcquisitionKey, op: F) -> SharedResult<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, AcquisitionError>>,
    {
        let (slot, is_leader) = {
            let mut inflight = lock(&self.inflight);
            match inflight.get(&key) {
                Some(slot) => (slot.clone(), false),
                None => {
                    let (tx, _rx) = tokio::sync::watch::channel::<Option<SharedResult<T>>>(None);
                    let slot = Arc::new(Slot { tx });
                    inflight.insert(key.clone(), slot.clone());
                    (slot, true)
                }
            }
        };

        if !is_leader {
            {
                let mut stats = lock(&self.stats);
                stats.waiters = stats.waiters.saturating_add(1);
            }
            let mut rx = slot.tx.subscribe();
            loop {
                if let Some(result) = rx.borrow().clone() {
                    return result;
                }
                if rx.changed().await.is_err() {
                    // The leader vanished without a result: fail closed
                    // instead of hanging.
                    return Err(AcquisitionError::Cancelled);
                }
            }
        }

        {
            let mut stats = lock(&self.stats);
            stats.leaders = stats.leaders.saturating_add(1);
        }
        let mut guard = LeaderGuard {
            key,
            slot: slot.clone(),
            coalescer: self,
            completed: false,
        };
        let result = op().await.map(Arc::new);
        guard.complete(result.clone());
        result
    }
}

struct LeaderGuard<'a, T> {
    key: AcquisitionKey,
    slot: Arc<Slot<T>>,
    coalescer: &'a Coalescer<T>,
    completed: bool,
}

impl<T> LeaderGuard<'_, T> {
    fn complete(&mut self, result: SharedResult<T>) {
        self.completed = true;
        self.slot.tx.send_replace(Some(result));
        lock(&self.coalescer.inflight).remove(&self.key);
    }
}

impl<T> Drop for LeaderGuard<'_, T> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        // The leader future was dropped mid-flight (caller cancelled or the
        // task was aborted): release the followers with a typed refusal and
        // free the slot.
        self.slot
            .tx
            .send_replace(Some(Err(AcquisitionError::Cancelled)));
        lock(&self.coalescer.inflight).remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{AcquisitionRequest, RequestedFields, RequestedFreshness};

    fn key(identity: &str) -> AcquisitionKey {
        AcquisitionRequest::new(identity, RequestedFields::all(), RequestedFreshness::Live)
            .unwrap()
            .coalescing_key()
            .unwrap()
    }

    #[tokio::test]
    async fn identical_callers_share_one_run() {
        let coalescer: Coalescer<u32> = Coalescer::new();
        let first = coalescer.run(key("https://h/p"), || async { Ok(1) }).await;
        let second = coalescer.run(key("https://h/p"), || async { Ok(2) }).await;
        assert_eq!(*first.unwrap(), 1);
        assert_eq!(*second.unwrap(), 2, "after completion a new run executes");
        assert_eq!(coalescer.stats().leaders, 2);
        assert_eq!(coalescer.in_flight(), 0);
    }

    #[tokio::test]
    async fn dropped_leader_releases_followers_with_cancelled() {
        let coalescer = Arc::new(Coalescer::<u32>::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let leader_started = started.clone();
        let leader_coalescer = coalescer.clone();
        let leader = tokio::spawn(async move {
            leader_coalescer
                .run(key("https://h/p"), || async move {
                    leader_started.notify_one();
                    std::future::pending::<()>().await;
                    Ok(1)
                })
                .await
        });
        started.notified().await;
        assert_eq!(coalescer.in_flight(), 1);

        let follower_coalescer = coalescer.clone();
        let follower = tokio::spawn(async move {
            follower_coalescer
                .run(key("https://h/p"), || async { Ok(2) })
                .await
        });
        // Wait until the follower has attached as a waiter, then abort the
        // leader: the follower must observe a typed Cancelled, not hang.
        let attached = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while coalescer.stats().waiters == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(attached.is_ok(), "follower never attached");
        leader.abort();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), follower)
            .await
            .expect("followers must not hang when the leader is dropped")
            .unwrap();
        assert_eq!(result.unwrap_err(), AcquisitionError::Cancelled);
        assert_eq!(coalescer.in_flight(), 0);
    }

    #[tokio::test]
    async fn different_keys_never_coalesce() {
        let coalescer: Coalescer<u32> = Coalescer::new();
        let a = coalescer.run(key("https://h/a"), || async { Ok(1) }).await;
        let b = coalescer.run(key("https://h/b"), || async { Ok(2) }).await;
        assert_eq!(*a.unwrap(), 1);
        assert_eq!(*b.unwrap(), 2);
    }
}
