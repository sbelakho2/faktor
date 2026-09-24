//! Permission requester for the HTTP world: the agent waits (bounded by the
//! DURABLE permission deadline) until the frozen UI resolves the request
//! through `POST /native/permission/reply`.
//!
//! ONE authority map: a permission id is resolvable only while a live
//! `request()` future owns an entry here. There is no decision store — a
//! resolution REMOVES the entry and hands the decision to the exact waiter it
//! owned, so:
//!
//! - a reply for an unknown/future id can never plant a decision a later
//!   request would consume (the pre-authorization defect);
//! - decisions cannot accumulate for the daemon's lifetime, and a
//!   post-timeout resolution is simply an unknown id (never stale state);
//! - two concurrent requests for one id are a typed conflict.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use faktor_agent::PermissionRequester;
use faktor_core::capability::PermissionDecision;
use faktor_core::id::SessionId;
use faktor_session::ops::PermissionRequest;
use tokio::sync::oneshot;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The typed refusal of a resolution attempt against a live permission owned
/// by a DIFFERENT session (a wrong-window UI race): nothing is consumed, so
/// the legitimate waiter stays armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMismatch {
    pub permission_id: i64,
    pub owner: SessionId,
    pub responder: SessionId,
}

impl std::fmt::Display for SessionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "permission {} is owned by session {}, not session {}",
            self.permission_id, self.owner, self.responder
        )
    }
}

impl std::error::Error for SessionMismatch {}

#[derive(Clone, Debug)]
pub struct PendingPermission {
    pub id: i64,
    pub session_id: SessionId,
    pub capability: String,
    pub detail: serde_json::Value,
}

/// One live waiter: the UI-visible view plus the ONLY channel through which
/// its decision can be delivered. Removing the entry from the map is the
/// resolution authority; the sender is useless once removed.
struct PendingEntry {
    view: PendingPermission,
    sender: oneshot::Sender<PermissionDecision>,
}

/// RAII ownership of one authority-map entry: the request future owns its
/// entry, so dropping the future (timeout, cancellation, panic) removes it.
/// A successful delivery disarms the guard — the resolver already removed the
/// entry, and a stale guard must never touch a later request with the same id.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<i64, PendingEntry>>>,
    id: i64,
    armed: bool,
}

impl PendingGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|poisoned| {
            self.pending.clear_poison();
            poisoned.into_inner()
        });
        pending.remove(&self.id);
    }
}

#[derive(Clone)]
pub struct ChannelPermissionRequester {
    /// THE authority map (view + delivery channel per live request).
    pending: Arc<Mutex<HashMap<i64, PendingEntry>>>,
    /// Upper bound of one live wait. The durable `expires_ms` can only cut it
    /// shorter, never extend it — the durable clock is the outer authority.
    timeout: Duration,
}

impl ChannelPermissionRequester {
    pub fn new(timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout,
        })
    }

    /// The authority map with classified recovery: every entry is owned by a
    /// live `request()` future whose guard removes it, and a resolution can
    /// only ever deliver to a real waiter (there is no decision store to
    /// poison into a wrong grant), so a poisoned guard is recovered with the
    /// poison flag cleared instead of wedging the permission surface.
    fn lock_pending(&self) -> MutexGuard<'_, HashMap<i64, PendingEntry>> {
        self.pending.lock().unwrap_or_else(|poisoned| {
            self.pending.clear_poison();
            poisoned.into_inner()
        })
    }

    /// Resolve ONE live pending request. Removes-to-own:
    ///
    /// - `Ok(true)` iff a live waiter owned by `session` was removed and its
    ///   exact receiver got `decision`;
    /// - `Ok(false)` for an unknown/already-resolved/timed-out/cancelled id
    ///   (never stored, so no pre-authorization and no retained state);
    /// - `Err(SessionMismatch)` when a live waiter exists but belongs to
    ///   another session — refused typed WITHOUT consuming it.
    pub fn resolve(
        &self,
        session: SessionId,
        permission_id: i64,
        decision: PermissionDecision,
    ) -> Result<bool, SessionMismatch> {
        let owned = {
            let mut pending = self.lock_pending();
            match pending.get(&permission_id) {
                None => return Ok(false),
                Some(entry) if entry.view.session_id != session => {
                    return Err(SessionMismatch {
                        permission_id,
                        owner: entry.view.session_id,
                        responder: session,
                    });
                }
                Some(_) => pending
                    .remove(&permission_id)
                    .expect("entry presence checked under the same guard"),
            }
        };
        // A send failure means the receiver was already gone (the future was
        // dropped concurrently): nothing was resolved, and nothing is kept.
        Ok(owned.sender.send(decision).is_ok())
    }

    pub fn pending_count(&self) -> usize {
        self.lock_pending().len()
    }

    /// The ids currently waiting for resolution (sorted, stable).
    pub fn pending_ids(&self) -> Vec<i64> {
        let mut v: Vec<i64> = self.lock_pending().keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Snapshot of pending permission requests (id, session, capability,
    /// detail) for `GET /native/permissions`.
    pub fn pending_views(&self) -> Vec<PendingPermission> {
        let mut v: Vec<PendingPermission> = self
            .lock_pending()
            .values()
            .map(|entry| entry.view.clone())
            .collect();
        v.sort_by_key(|p| p.id);
        v
    }
}

impl PermissionRequester for ChannelPermissionRequester {
    fn request(
        &self,
        session: SessionId,
        permission: &PermissionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>>
    {
        let id = permission.id;
        let remaining_ms = permission.expires_ms.saturating_sub(now_ms());
        // The durable deadline is the authority: once it passed, no waiter is
        // registered and no window is granted — a daemon restart can never
        // mint a fresh one.
        if remaining_ms <= 0 {
            return Box::pin(async move {
                Err(faktor_core::error::Error::timeout(format!(
                    "permission {id} expired (durable deadline passed)"
                )))
            });
        }
        let detail =
            serde_json::to_value(&permission.capability).unwrap_or(serde_json::Value::Null);
        let view = PendingPermission {
            id,
            session_id: session,
            capability: detail
                .get("capability")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            detail,
        };
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.lock_pending();
            if pending.contains_key(&id) {
                return Box::pin(async move {
                    Err(faktor_core::error::Error::conflict(format!(
                        "permission {id} already has a live waiter"
                    )))
                });
            }
            pending.insert(id, PendingEntry { view, sender });
        }
        let guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id,
            armed: true,
        };
        let wait = self.timeout.min(Duration::from_millis(remaining_ms as u64));
        Box::pin(async move {
            let mut guard = guard;
            match tokio::time::timeout(wait, receiver).await {
                Ok(Ok(decision)) => {
                    // The resolver removed the entry before sending; disarm so
                    // this stale guard can never remove a later entry.
                    guard.disarm();
                    Ok(decision)
                }
                Ok(Err(_)) => {
                    // The sender was dropped without a decision: typed
                    // refusal, and the guard removes any residue.
                    Err(faktor_core::error::Error::permission(format!(
                        "permission {id} resolved without a decision"
                    )))
                }
                Err(_) => Err(faktor_core::error::Error::timeout(format!(
                    "permission {id} not resolved within {}ms",
                    wait.as_millis()
                ))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::capability::Capability;
    use faktor_core::id::OpId;

    fn permission_with_deadline(id: i64, expires_ms: i64) -> PermissionRequest {
        PermissionRequest {
            id,
            op_id: OpId::new(1),
            capability: Capability::ExecuteShell {
                command: "ls".into(),
            },
            event_seq: faktor_core::id::EventSeq::new(1),
            expires_ms,
        }
    }

    fn permission(id: i64) -> PermissionRequest {
        permission_with_deadline(id, now_ms() + 60_000)
    }

    #[test]
    fn resolve_unknown_id_is_false_and_retains_nothing() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        assert!(!r.resolve(sid, 42, PermissionDecision::Allow).unwrap());
        assert_eq!(r.pending_count(), 0);
        assert!(r.pending_ids().is_empty());
        assert!(r.pending_views().is_empty());
    }

    #[test]
    fn hundred_thousand_synthetic_resolve_cycles_retain_nothing() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        for id in 0..100_000i64 {
            assert!(!r.resolve(sid, id, PermissionDecision::Allow).unwrap());
        }
        assert_eq!(r.pending_count(), 0, "no decision/state accumulation");
        assert!(r.pending_ids().is_empty());
        assert!(r.pending_views().is_empty());
    }

    #[tokio::test]
    async fn resolve_before_request_plants_nothing() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        // A reply for a future id is refused and stores NOTHING.
        assert!(!r.resolve(sid, 7, PermissionDecision::Allow).unwrap());
        let r2 = r.clone();
        let waiter = tokio::spawn(async move { r2.request(sid, &permission(7)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(r.pending_ids(), vec![7], "the later request still waits");
        assert!(
            !waiter.is_finished(),
            "no planted Allow may be consumed by a later request"
        );
        // The waiter is only resolved by a REAL resolution after it exists.
        assert!(r.resolve(sid, 7, PermissionDecision::Deny).unwrap());
        assert_eq!(waiter.await.unwrap().unwrap(), PermissionDecision::Deny);
        assert!(r.pending_ids().is_empty());
    }

    #[tokio::test]
    async fn valid_pending_resolves_once_and_empties_the_authority() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        let r2 = r.clone();
        let waiter = tokio::spawn(async move { r2.request(sid, &permission(7)).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(r.pending_ids(), vec![7]);
        assert!(r.resolve(sid, 7, PermissionDecision::Allow).unwrap());
        assert_eq!(waiter.await.unwrap(), PermissionDecision::Allow);
        assert_eq!(r.pending_count(), 0, "authority map back to zero");
        assert!(r.pending_views().is_empty());
        // A second resolution has nothing to own: false, no state.
        assert!(!r.resolve(sid, 7, PermissionDecision::Deny).unwrap());
        assert_eq!(r.pending_count(), 0);
    }

    #[tokio::test]
    async fn timeout_then_late_resolution_is_false_and_state_free() {
        let r = ChannelPermissionRequester::new(Duration::from_millis(30));
        let sid = SessionId::new(1);
        let err = r.request(sid, &permission(5)).await.unwrap_err();
        assert_eq!(err.kind, faktor_core::error::ErrorKind::Timeout);
        assert!(
            r.pending_ids().is_empty(),
            "waiter cleaned up after timeout"
        );
        // A late reply for the timed-out id is refused and kept nowhere.
        assert!(!r.resolve(sid, 5, PermissionDecision::Allow).unwrap());
        assert!(r.pending_ids().is_empty());
        assert!(r.pending_views().is_empty());
    }

    #[tokio::test]
    async fn dropping_the_request_future_removes_the_pending_entry() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        let r2 = r.clone();
        let waiter = tokio::spawn(async move { r2.request(sid, &permission(8)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(r.pending_ids(), vec![8]);
        waiter.abort();
        let _ = waiter.await;
        assert!(
            r.pending_ids().is_empty(),
            "the RAII guard must remove the entry when the future dies"
        );
        assert!(!r.resolve(sid, 8, PermissionDecision::Allow).unwrap());
        assert!(r.pending_views().is_empty());
    }

    #[tokio::test]
    async fn cancelled_session_with_a_pending_permission_leaves_no_retention() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        let mut waiters = Vec::new();
        for id in 10..14 {
            let r = r.clone();
            waiters.push(tokio::spawn(async move {
                r.request(sid, &permission(id)).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(r.pending_ids(), vec![10, 11, 12, 13]);
        // Session cancelled: every live request future is aborted.
        for w in &waiters {
            w.abort();
        }
        for w in waiters {
            let _ = w.await;
        }
        assert_eq!(r.pending_count(), 0, "no retention after cancellation");
        for id in 10..14 {
            assert!(!r.resolve(sid, id, PermissionDecision::Allow).unwrap());
        }
    }

    #[tokio::test]
    async fn expired_durable_deadline_is_never_extended() {
        // Huge configured cap: only the DURABLE deadline may end the wait.
        let r = ChannelPermissionRequester::new(Duration::from_secs(3600));
        let sid = SessionId::new(1);
        let began = std::time::Instant::now();
        let err = r
            .request(sid, &permission_with_deadline(21, now_ms() - 1))
            .await
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::error::ErrorKind::Timeout);
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "an already-expired durable deadline grants no fresh window"
        );
        assert!(r.pending_ids().is_empty());
        assert!(!r.resolve(sid, 21, PermissionDecision::Allow).unwrap());

        // A durable deadline 40ms out ends the wait even though the cap is an
        // hour: remaining = expires_ms - now.
        let err = r
            .request(sid, &permission_with_deadline(22, now_ms() + 40))
            .await
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::error::ErrorKind::Timeout);
        assert!(r.pending_ids().is_empty());
    }

    #[tokio::test]
    async fn wrong_session_resolution_is_refused_without_consuming_the_waiter() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let owner = SessionId::new(1);
        let other = SessionId::new(2);
        let r2 = r.clone();
        let waiter = tokio::spawn(async move { r2.request(owner, &permission(31)).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let err = r.resolve(other, 31, PermissionDecision::Allow).unwrap_err();
        assert_eq!(err.permission_id, 31);
        assert_eq!(err.owner, owner);
        assert_eq!(err.responder, other);
        assert!(err.to_string().contains("owned by session 1"), "{err}");
        assert_eq!(
            r.pending_ids(),
            vec![31],
            "the wrong-session attempt must not consume the live waiter"
        );
        assert!(r.resolve(owner, 31, PermissionDecision::Allow).unwrap());
        assert_eq!(waiter.await.unwrap(), PermissionDecision::Allow);
        assert_eq!(r.pending_count(), 0);
    }

    #[tokio::test]
    async fn duplicate_live_waiter_is_a_typed_conflict() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        let r2 = r.clone();
        let waiter = tokio::spawn(async move { r2.request(sid, &permission(41)).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let err = r.request(sid, &permission(41)).await.unwrap_err();
        assert_eq!(err.kind, faktor_core::error::ErrorKind::Conflict);
        assert_eq!(r.pending_ids(), vec![41], "the first waiter is untouched");
        assert!(r.resolve(sid, 41, PermissionDecision::Allow).unwrap());
        assert_eq!(waiter.await.unwrap(), PermissionDecision::Allow);
    }

    #[tokio::test]
    async fn resolver_wakes_waiter() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        let r2 = r.clone();
        let handle = tokio::spawn(async move { r2.request(sid, &permission(7)).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(r.resolve(sid, 7, PermissionDecision::Allow).unwrap());
        let decision = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decision, PermissionDecision::Allow);
        assert_eq!(r.pending_count(), 0);
    }

    #[tokio::test]
    async fn many_concurrent_waiters_resolve_independently() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let sid = SessionId::new(1);
        let mut handles = Vec::new();
        for id in 100..110 {
            let r = r.clone();
            handles.push(tokio::spawn(async move {
                r.request(sid, &permission(id)).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        for id in 100..110 {
            assert!(r.resolve(sid, id, PermissionDecision::Allow).unwrap());
        }
        for h in handles {
            assert_eq!(h.await.unwrap().unwrap(), PermissionDecision::Allow);
        }
    }

    #[tokio::test]
    async fn pending_views_reflect_live_requests_and_cleanup() {
        let r = ChannelPermissionRequester::new(Duration::from_millis(30));
        let s1 = SessionId::new(10);
        let s2 = SessionId::new(20);
        let mut handles = Vec::new();
        for (id, session) in [(1, s1), (2, s2)] {
            let r = r.clone();
            handles.push(tokio::spawn(async move {
                r.request(session, &permission(id)).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let views = r.pending_views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].id, 1);
        assert_eq!(views[0].session_id, s1);
        assert_eq!(views[0].capability, "execute_shell");
        assert_eq!(views[0].detail["detail"]["command"], "ls");
        // Resolving removes the view.
        assert!(r.resolve(s1, 1, PermissionDecision::Allow).unwrap());
        assert_eq!(r.pending_views().len(), 1);
        assert_eq!(r.pending_views()[0].id, 2);
        // Timeout cleans the remaining view.
        for h in handles {
            let _ = h.await;
        }
        assert!(
            r.pending_views().is_empty(),
            "timed-out request must not linger"
        );
        // Double resolve: no second view, decision wins.
        assert!(!r.resolve(s1, 1, PermissionDecision::Deny).unwrap());
        assert!(r.pending_views().is_empty());
    }
}
