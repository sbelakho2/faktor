//! Permission requester for the HTTP world: the agent waits (with a timeout)
//! until the frozen UI resolves the permission through
//! `POST /api/perm/{id}/resolve`.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use faktor_agent::PermissionRequester;
use faktor_core::capability::PermissionDecision;
use faktor_core::id::SessionId;
use faktor_session::ops::PermissionRequest;
use tokio::sync::Notify;

/// The typed refusal of a POISONED permission-decision authority: a writer
/// panicked while holding the decision map, so a resolution can no longer be
/// trusted. Every resolve refuses — it is never silently treated as an
/// unknown/already-resolved id (which would mask the authority failure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityPoisoned {
    /// Which authority is poisoned.
    pub authority: &'static str,
    /// Why it is poisoned (the panic poison's message).
    pub detail: String,
}

impl AuthorityPoisoned {
    fn decisions(detail: impl std::fmt::Display) -> Self {
        Self {
            authority: "permission-decision authority",
            detail: format!(
                "a writer panicked while holding it ({detail}); refusing the resolution rather \
                 than guessing a permission outcome"
            ),
        }
    }
}

impl std::fmt::Display for AuthorityPoisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "poisoned authority {}: {}", self.authority, self.detail)
    }
}

impl std::error::Error for AuthorityPoisoned {}

#[derive(Clone, Debug)]
pub struct PendingPermission {
    pub id: i64,
    pub session_id: SessionId,
    pub capability: String,
    pub detail: serde_json::Value,
}

#[derive(Clone)]
pub struct ChannelPermissionRequester {
    waiters: Arc<Mutex<HashMap<i64, Arc<Notify>>>>,
    decisions: Arc<Mutex<HashMap<i64, PermissionDecision>>>,
    pending: Arc<Mutex<HashMap<i64, PendingPermission>>>,
    timeout: Duration,
}

impl ChannelPermissionRequester {
    pub fn new(timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            waiters: Arc::new(Mutex::new(HashMap::new())),
            decisions: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout,
        })
    }

    /// The permission-decision map with the typed AUTHORITY/POLICY refusal:
    /// a poisoned guard refuses the resolution instead of guessing an
    /// outcome that a panicking writer may have left half-applied.
    fn lock_decisions(
        &self,
    ) -> Result<MutexGuard<'_, HashMap<i64, PermissionDecision>>, AuthorityPoisoned> {
        self.decisions.lock().map_err(AuthorityPoisoned::decisions)
    }

    /// The waiter registry with classified recovery: it is an OWNERSHIP
    /// projection of the live `request()` futures (the bounded timeout is
    /// the ultimate authority), so a poisoned guard is recovered with the
    /// poison flag cleared rather than stranding every later resolution.
    fn lock_waiters(&self) -> MutexGuard<'_, HashMap<i64, Arc<Notify>>> {
        self.waiters.lock().unwrap_or_else(|poisoned| {
            self.waiters.clear_poison();
            poisoned.into_inner()
        })
    }

    /// Read-only projection of the decision map for the waiter path: recovers
    /// a poisoned guard with the poison flag cleared (the mutation path,
    /// [`Self::resolve`], carries the typed refusal).
    fn project_decisions(&self) -> MutexGuard<'_, HashMap<i64, PermissionDecision>> {
        self.decisions.lock().unwrap_or_else(|poisoned| {
            self.decisions.clear_poison();
            poisoned.into_inner()
        })
    }

    /// The pending-request view map with classified recovery: it is a
    /// DERIVED projection served to the UI and re-derived by later
    /// `request()`/timeout cleanup, so a poisoned guard is recovered with
    /// the poison flag cleared (never wedging the permission surface).
    fn lock_pending(&self) -> MutexGuard<'_, HashMap<i64, PendingPermission>> {
        self.pending.lock().unwrap_or_else(|poisoned| {
            self.pending.clear_poison();
            poisoned.into_inner()
        })
    }

    /// Called by the HTTP resolver. Returns false when the permission is
    /// unknown or already resolved (never double-resolves); a poisoned
    /// decision authority is the typed [`AuthorityPoisoned`] refusal.
    pub fn resolve(
        &self,
        permission_id: i64,
        decision: PermissionDecision,
    ) -> Result<bool, AuthorityPoisoned> {
        {
            let mut decisions = self.lock_decisions()?;
            if decisions.contains_key(&permission_id) {
                return Ok(false);
            }
            decisions.insert(permission_id, decision);
        }
        self.lock_pending().remove(&permission_id);
        if let Some(notify) = self.lock_waiters().remove(&permission_id) {
            notify.notify_one();
        }
        Ok(true)
    }

    pub fn pending_count(&self) -> usize {
        self.lock_waiters().len()
    }

    /// The ids currently waiting for resolution (sorted, stable).
    pub fn pending_ids(&self) -> Vec<i64> {
        let mut v: Vec<i64> = self.lock_waiters().keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// Snapshot of pending permission requests (id, session, capability,
    /// detail) for `GET /permission/list`.
    pub fn pending_views(&self) -> Vec<PendingPermission> {
        let mut v: Vec<PendingPermission> = self.lock_pending().values().cloned().collect();
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
        let notify = Arc::new(Notify::new());
        let detail =
            serde_json::to_value(&permission.capability).unwrap_or(serde_json::Value::Null);
        self.lock_waiters().insert(id, notify.clone());
        self.lock_pending().insert(
            id,
            PendingPermission {
                id,
                session_id: session,
                capability: detail
                    .get("capability")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                detail,
            },
        );
        // A decision may already exist (the resolver raced ahead of us).
        // Read-only projection: recover a poisoned guard (the mutation path,
        // `resolve`, is the one that carries the typed refusal).
        if let Some(d) = self.project_decisions().get(&id).copied() {
            self.lock_waiters().remove(&id);
            self.lock_pending().remove(&id);
            return Box::pin(async move { Ok(d) });
        }
        let me = self.clone();
        Box::pin(async move {
            tokio::select! {
                _ = notify.notified() => {
                    match me.project_decisions().get(&id).copied() {
                        Some(decision) => Ok(decision),
                        None => Err(faktor_core::error::Error::permission(
                            format!("permission {id} resolved without a decision"),
                        )),
                    }
                }
                _ = tokio::time::sleep(me.timeout) => {
                    me.lock_waiters().remove(&id);
                    me.lock_pending().remove(&id);
                    Err(faktor_core::error::Error::timeout(format!(
                        "permission {id} not resolved within {}ms",
                        me.timeout.as_millis()
                    )))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::capability::Capability;
    use faktor_core::id::OpId;

    fn permission(id: i64) -> PermissionRequest {
        PermissionRequest {
            id,
            op_id: OpId::new(1),
            capability: Capability::ExecuteShell {
                command: "ls".into(),
            },
            event_seq: faktor_core::id::EventSeq::new(1),
        }
    }

    #[tokio::test]
    async fn resolver_wakes_waiter() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let r2 = r.clone();
        let handle =
            tokio::spawn(
                async move { r2.request(SessionId::new(1), &permission(7)).await.unwrap() },
            );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(r.resolve(7, PermissionDecision::Allow).unwrap());
        let decision = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decision, PermissionDecision::Allow);
        assert_eq!(r.pending_count(), 0);
    }

    #[tokio::test]
    async fn double_resolve_loses_second() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        assert!(r.resolve(1, PermissionDecision::Allow).unwrap());
        assert!(
            !r.resolve(1, PermissionDecision::Deny).unwrap(),
            "first decision wins"
        );
    }

    #[tokio::test]
    async fn timeout_returns_permission_error() {
        let r = ChannelPermissionRequester::new(Duration::from_millis(30));
        let result = r.request(SessionId::new(1), &permission(2)).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind,
            faktor_core::error::ErrorKind::Timeout
        );
        assert_eq!(r.pending_count(), 0, "waiter cleaned up after timeout");
    }

    #[tokio::test]
    async fn pre_resolved_decision_is_returned_immediately() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        assert!(r.resolve(3, PermissionDecision::Deny).unwrap());
        let d = r.request(SessionId::new(1), &permission(3)).await.unwrap();
        assert_eq!(d, PermissionDecision::Deny);
    }

    #[tokio::test]
    async fn many_concurrent_waiters_resolve_independently() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        let mut handles = Vec::new();
        for id in 100..110 {
            let r = r.clone();
            handles.push(tokio::spawn(async move {
                r.request(SessionId::new(1), &permission(id)).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        for id in 100..110 {
            assert!(r.resolve(id, PermissionDecision::Allow).unwrap());
        }
        for h in handles {
            assert_eq!(h.await.unwrap().unwrap(), PermissionDecision::Allow);
        }
    }

    #[tokio::test]
    async fn pending_views_reflect_live_requests_and_cleanup() {
        let r = ChannelPermissionRequester::new(Duration::from_millis(30));
        let mut handles = Vec::new();
        for (id, session) in [(1, SessionId::new(10)), (2, SessionId::new(20))] {
            let r = r.clone();
            handles.push(tokio::spawn(async move {
                r.request(session, &permission(id)).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let views = r.pending_views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].id, 1);
        assert_eq!(views[0].session_id, SessionId::new(10));
        assert_eq!(views[0].capability, "execute_shell");
        assert_eq!(views[0].detail["detail"]["command"], "ls");
        // Resolving removes the view.
        assert!(r.resolve(1, PermissionDecision::Allow).unwrap());
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
        assert!(!r.resolve(1, PermissionDecision::Deny).unwrap());
        assert!(r.pending_views().is_empty());
    }

    /// Poison one lock exactly like a panicking writer: panic while holding
    /// the guard (never a poisoned into_inner recovery).
    fn poison<T: Send + 'static>(lock: Arc<Mutex<T>>) {
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = lock.lock().unwrap();
            panic!("poison the permission lock (test seam)");
        }));
        assert!(poisoned.is_err(), "the poisoner must unwind");
    }

    #[tokio::test]
    async fn poisoned_decision_authority_refuses_resolution_typed() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        poison(Arc::clone(&r.decisions));
        assert!(r.decisions.is_poisoned());
        // AUTHORITY/POLICY state => typed refusal: never a silent false
        // ("unknown/already resolved") and never a double-resolve.
        let err = r
            .resolve(1, PermissionDecision::Allow)
            .expect_err("a poisoned decision authority must refuse typed");
        assert_eq!(err.authority, "permission-decision authority");
        assert!(err.to_string().contains("poisoned authority"), "{err}");
        // Nothing was half-applied: no decision and no stranded waiter.
        assert_eq!(r.pending_count(), 0);
        assert!(r.pending_views().is_empty());
    }

    #[tokio::test]
    async fn poisoned_waiter_and_pending_registries_recover_and_keep_serving() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        poison(Arc::clone(&r.waiters));
        poison(Arc::clone(&r.pending));
        assert!(r.waiters.is_poisoned() && r.pending.is_poisoned());
        // OWNERSHIP projections => reconciled recovery (poison cleared)
        // instead of wedging the permission surface.
        assert_eq!(r.pending_count(), 0);
        assert!(r.pending_ids().is_empty());
        assert!(r.pending_views().is_empty());
        assert!(!r.waiters.is_poisoned() && !r.pending.is_poisoned());

        // A full request/resolve round-trip still works after the recovery.
        let r2 = r.clone();
        let waiter =
            tokio::spawn(async move { r2.request(SessionId::new(1), &permission(9)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(r.pending_ids(), vec![9]);
        assert_eq!(r.pending_views()[0].id, 9);
        assert!(r.resolve(9, PermissionDecision::Allow).unwrap());
        assert_eq!(waiter.await.unwrap().unwrap(), PermissionDecision::Allow);
        assert_eq!(r.pending_views().len(), 0);
    }

    #[tokio::test]
    async fn poisoned_decision_read_projection_recovers_and_serves_waiter() {
        let r = ChannelPermissionRequester::new(Duration::from_secs(5));
        // A decision lands, then the map is poisoned: a waiter's read-only
        // projection recovers it and serves the decision (never a spurious
        // timeout), while the poison flag is cleared for the mutation path.
        assert!(r.resolve(11, PermissionDecision::Deny).unwrap());
        poison(Arc::clone(&r.decisions));
        let d = r
            .request(SessionId::new(1), &permission(11))
            .await
            .expect("the pre-resolved decision is served despite the poison");
        assert_eq!(d, PermissionDecision::Deny);
        assert!(!r.decisions.is_poisoned());
    }
}
