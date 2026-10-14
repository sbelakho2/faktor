//! The FEDERATED index-query seam: organization-shared and cross-repository
//! reads (the `shared_intelligence` / `cross_repo_intelligence` paid value
//! entitlements).
//!
//! The LOCAL single-workspace query path (`IndexService::view` plus
//! `WorkspaceIndex::{files_for_token, symbol_lookup}`) is the community
//! capability and is never gated. [`federated_query`] is the ONE entry that
//! answers a query spanning the organization's repositories:
//!
//! - ONE workspace is an organization-shared read
//!   ([`ValueEntitlement::SharedIntelligence`]);
//! - SEVERAL workspaces are a cross-repository read
//!   ([`ValueEntitlement::CrossRepoIntelligence`]).
//!
//! Both consult the wired [`ValueGate`] BEFORE any index is touched. A
//! denial is the typed [`FederatedQueryRefusal::Entitlement`] naming the
//! entitlement, its hook and (when resolved) the plan — never an empty
//! result. With no cloud authority the gate is absent and the read is
//! refused `cloud_absent` typed while the local per-workspace query stays
//! fully functional.
//!
//! Org binding: the caller's organization is DERIVED through
//! [`WorkspaceOwnershipBinding`] (the principal plus durable workspace
//! ownership state), never supplied as request data, and every requested
//! workspace's owner is resolved from durable state BEFORE any index is
//! read. A workspace owned by another organization refuses the WHOLE query
//! ([`FederatedQueryRefusal::ForeignWorkspace`]); an unresolvable caller or
//! workspace fails closed. A caller can therefore never widen its
//! organization by passing workspace ids.
//!
//! Bounded by construction: at most [`MAX_FEDERATED_WORKSPACES`] workspaces
//! are read, at most [`MAX_FEDERATED_QUERY_TOKENS`] distinct query tokens
//! are searched, and at most [`MAX_FEDERATED_HITS`] hits are returned; the
//! outcome says `truncated` instead of pretending to be exact.

use std::fmt;

use faktor_core::entitlement::{ValueEntitlement, ValueGate, ValueGateRefusal};
use faktor_core::id::WorkspaceId;

use crate::service::IndexService;
use crate::tokenize;

/// Hard bound on workspaces one federated query reads.
pub const MAX_FEDERATED_WORKSPACES: usize = 64;
/// Hard bound on distinct query tokens searched per request.
pub const MAX_FEDERATED_QUERY_TOKENS: usize = 8;
/// Hard bound on hits one federated query returns.
pub const MAX_FEDERATED_HITS: usize = 200;
/// Bound on one organization id (mirrors the durable enterprise text bound).
pub const MAX_ORGANIZATION_ID_BYTES: usize = 256;
/// Per-token per-workspace candidate bound (before the global merge).
const PER_TOKEN_HITS: usize = 256;

/// Bounded organization identity of the federated seam. This crate is
/// cloud-free by dependency direction, so the type mirrors the durable
/// organization-id shape (non-empty printable ASCII, at most
/// [`MAX_ORGANIZATION_ID_BYTES`]) instead of importing the commercial type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrganizationId(String);

impl OrganizationId {
    /// The bounded, printable organization id (an empty/oversized/control
    /// value is refused: it could never name a durable organization).
    pub fn try_new(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("organization id is empty".into());
        }
        if trimmed.len() > MAX_ORGANIZATION_ID_BYTES {
            return Err(format!(
                "organization id exceeds the {MAX_ORGANIZATION_ID_BYTES}-byte bound"
            ));
        }
        if !trimmed.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
            return Err("organization id must be printable ASCII".into());
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OrganizationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The DURABLE organization binding of the federated index seam. Callers
/// NEVER supply the organization as request data: the seam resolves the
/// caller's organization from the authenticated principal and each
/// workspace's owner from durable workspace ownership state through this
/// authority, then refuses any workspace that belongs to another
/// organization. An unresolvable caller fails the read closed.
pub trait WorkspaceOwnershipBinding: Send + Sync {
    /// The organization of the authenticated caller (principal / durable
    /// session state), never request data.
    fn caller_organization(&self) -> Result<OrganizationId, String>;
    /// The organization owning one workspace, from durable state.
    fn workspace_organization(&self, workspace: WorkspaceId) -> Result<OrganizationId, String>;
}

/// Deterministic single-organization binding: every workspace resolves to
/// the same organization. This is the shape of a LOCAL single-tenant
/// deployment; a multi-tenant embedder must implement the trait over its
/// durable workspace registry instead.
#[derive(Debug, Clone)]
pub struct SingleOrganizationBinding {
    organization: OrganizationId,
}

impl SingleOrganizationBinding {
    pub fn new(organization: OrganizationId) -> Self {
        Self { organization }
    }
}

impl WorkspaceOwnershipBinding for SingleOrganizationBinding {
    fn caller_organization(&self) -> Result<OrganizationId, String> {
        Ok(self.organization.clone())
    }

    fn workspace_organization(&self, _workspace: WorkspaceId) -> Result<OrganizationId, String> {
        Ok(self.organization.clone())
    }
}

/// One bounded federated query. The organization is NOT request data: it is
/// derived from the caller's principal through [`WorkspaceOwnershipBinding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedQueryRequest {
    /// The workspaces whose published generations are queried.
    pub workspaces: Vec<WorkspaceId>,
    /// The query text (free form; tokenized by the ONE index tokenizer).
    pub query: String,
    /// Requested hit bound; clamped to [`MAX_FEDERATED_HITS`].
    pub limit: usize,
}

/// One federated hit: which workspace, which repository-relative path, and
/// the summed token frequency that ranked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedHit {
    pub workspace: WorkspaceId,
    pub path: String,
    pub freq: u64,
}

/// The outcome of one gated federated query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederatedQueryOutcome {
    /// The entitlement this read required (and that was granted).
    pub entitlement: ValueEntitlement,
    pub hits: Vec<FederatedHit>,
    /// Workspaces whose published generation was actually read.
    pub workspaces_read: usize,
    /// A workspace had no published generation yet, or a bound cut the read.
    pub truncated: bool,
}

/// The typed refusal of one federated query: the cloud-owned paid decision
/// first, then the durable organization binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FederatedQueryRefusal {
    /// The paid `shared_intelligence` / `cross_repo_intelligence`
    /// entitlement was not granted.
    Entitlement(ValueGateRefusal),
    /// The caller organization could not be resolved from durable state:
    /// fail closed, never fall through to a local read.
    UnboundCaller { reason: String },
    /// A requested workspace's owner could not be resolved from durable
    /// state.
    UnboundWorkspace {
        workspace: WorkspaceId,
        reason: String,
    },
    /// A requested workspace is owned by ANOTHER organization: the whole
    /// query is refused before any index is read (never a partial
    /// cross-tenant fold).
    ForeignWorkspace {
        caller: OrganizationId,
        owner: OrganizationId,
        workspace: WorkspaceId,
    },
}

impl fmt::Display for FederatedQueryRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FederatedQueryRefusal::Entitlement(refusal) => write!(f, "{refusal}"),
            FederatedQueryRefusal::UnboundCaller { reason } => write!(
                f,
                "federated index binding: caller organization unresolved: {reason}"
            ),
            FederatedQueryRefusal::UnboundWorkspace { workspace, reason } => write!(
                f,
                "federated index binding: workspace {} owner unresolved: {reason}",
                workspace.raw()
            ),
            FederatedQueryRefusal::ForeignWorkspace {
                caller,
                owner,
                workspace,
            } => write!(
                f,
                "federated index binding: workspace {} belongs to organization {owner}, not the caller organization {caller}",
                workspace.raw()
            ),
        }
    }
}

impl std::error::Error for FederatedQueryRefusal {}

/// The entitlement one federated read requires: several workspaces are the
/// cross-repository augmentation; a single organization-scoped workspace is
/// the shared-intelligence augmentation.
pub fn required_entitlement(workspaces: usize) -> ValueEntitlement {
    if workspaces > 1 {
        ValueEntitlement::CrossRepoIntelligence
    } else {
        ValueEntitlement::SharedIntelligence
    }
}

/// The ONE gated federated query: entitlement first, then the durable
/// organization binding for every requested workspace, then a bounded read
/// over the published generations. The gate decides; denied reads return
/// the typed refusal and touch no index. A workspace owned by another
/// organization refuses the whole query before any index handle is opened.
pub fn federated_query(
    service: &IndexService,
    request: &FederatedQueryRequest,
    gate: &ValueGate,
    binding: &dyn WorkspaceOwnershipBinding,
) -> Result<FederatedQueryOutcome, FederatedQueryRefusal> {
    let entitlement = required_entitlement(request.workspaces.len());
    gate.check(entitlement)
        .map_err(FederatedQueryRefusal::Entitlement)?;
    let caller = binding
        .caller_organization()
        .map_err(|reason| FederatedQueryRefusal::UnboundCaller { reason })?;
    // Resolve EVERY workspace owner BEFORE opening any index handle: a
    // foreign workspace refuses the whole request, so a partial cross-tenant
    // fold can never be assembled.
    for workspace in &request.workspaces {
        let owner = binding
            .workspace_organization(*workspace)
            .map_err(|reason| FederatedQueryRefusal::UnboundWorkspace {
                workspace: *workspace,
                reason,
            })?;
        if owner != caller {
            return Err(FederatedQueryRefusal::ForeignWorkspace {
                caller,
                owner,
                workspace: *workspace,
            });
        }
    }

    let limited: Vec<WorkspaceId> = request
        .workspaces
        .iter()
        .take(MAX_FEDERATED_WORKSPACES)
        .copied()
        .collect();
    let mut truncated = limited.len() < request.workspaces.len();

    let mut tokens: Vec<String> = tokenize(&request.query);
    tokens.sort();
    tokens.dedup();
    if tokens.len() > MAX_FEDERATED_QUERY_TOKENS {
        tokens.truncate(MAX_FEDERATED_QUERY_TOKENS);
        truncated = true;
    }
    let limit = request.limit.min(MAX_FEDERATED_HITS);

    // BTreeMap keeps the merge deterministic regardless of workspace order
    // or index iteration order; the final sort is (freq DESC, path ASC).
    let mut merged: std::collections::BTreeMap<(WorkspaceId, String), u64> =
        std::collections::BTreeMap::new();
    let mut workspaces_read = 0usize;
    for workspace in &limited {
        let Some(view) = service.view(*workspace) else {
            // No published generation yet: an honest truncation, never a
            // fabricated miss.
            truncated = true;
            continue;
        };
        workspaces_read += 1;
        let index = view.index();
        let index = index.lock().unwrap_or_else(|poison| poison.into_inner());
        for token in &tokens {
            for hit in index.files_for_token(*workspace, token, PER_TOKEN_HITS) {
                *merged.entry((*workspace, hit.path.clone())).or_insert(0) += u64::from(hit.freq);
            }
        }
    }

    let mut hits: Vec<FederatedHit> = merged
        .into_iter()
        .map(|((workspace, path), freq)| FederatedHit {
            workspace,
            path,
            freq,
        })
        .collect();
    hits.sort_by(|a, b| {
        b.freq
            .cmp(&a.freq)
            .then_with(|| a.workspace.to_string().cmp(&b.workspace.to_string()))
            .then_with(|| a.path.cmp(&b.path))
    });
    if hits.len() > limit {
        hits.truncate(limit);
        truncated = true;
    }

    Ok(FederatedQueryOutcome {
        entitlement,
        hits,
        workspaces_read,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::entitlement::{ValueEntitlementGate, ValueGateRefusal, ValueRefusalCause};
    use std::collections::HashMap;

    #[derive(Debug)]
    struct GateOver {
        granted: ValueEntitlement,
        plan_id: &'static str,
    }

    impl ValueEntitlementGate for GateOver {
        fn check_value_entitlement(
            &self,
            entitlement: ValueEntitlement,
        ) -> Result<(), ValueGateRefusal> {
            if entitlement == self.granted {
                Ok(())
            } else {
                Err(ValueGateRefusal {
                    entitlement,
                    hook: entitlement.enforcement_hook(),
                    cause: ValueRefusalCause::NotInPlan,
                    plan_id: Some(self.plan_id.into()),
                    organization_id: Some("org_acme".into()),
                    message: format!(
                        "value entitlement {} not granted for organization org_acme: not_in_plan (plan Some(\"{}\"))",
                        entitlement.as_str(),
                        self.plan_id
                    ),
                })
            }
        }
    }

    fn entitled(granted: ValueEntitlement) -> ValueGate {
        ValueGate::wired(std::sync::Arc::new(GateOver {
            granted,
            plan_id: "team",
        }))
    }

    fn org(id: &str) -> OrganizationId {
        OrganizationId::try_new(id).unwrap()
    }

    struct MapBinding {
        caller: OrganizationId,
        owners: HashMap<u64, OrganizationId>,
    }

    impl WorkspaceOwnershipBinding for MapBinding {
        fn caller_organization(&self) -> Result<OrganizationId, String> {
            Ok(self.caller.clone())
        }

        fn workspace_organization(&self, workspace: WorkspaceId) -> Result<OrganizationId, String> {
            self.owners
                .get(&workspace.raw())
                .cloned()
                .ok_or_else(|| format!("workspace {} has no durable owner", workspace.raw()))
        }
    }

    fn service(dir: &std::path::Path) -> std::sync::Arc<IndexService> {
        let store = std::sync::Arc::new(
            faktor_store::Store::open(dir.join("store.sqlite"), false).unwrap(),
        );
        IndexService::open(
            store,
            dir.join("index_data"),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap()
    }

    #[test]
    fn entitlement_selection_is_single_vs_cross_repository() {
        assert_eq!(
            required_entitlement(0),
            ValueEntitlement::SharedIntelligence
        );
        assert_eq!(
            required_entitlement(1),
            ValueEntitlement::SharedIntelligence
        );
        assert_eq!(
            required_entitlement(2),
            ValueEntitlement::CrossRepoIntelligence
        );
    }

    #[test]
    fn organization_id_is_bounded_printable_and_non_empty() {
        assert!(OrganizationId::try_new("").is_err());
        assert!(OrganizationId::try_new("   ").is_err());
        assert!(OrganizationId::try_new("\u{0}null").is_err());
        assert!(OrganizationId::try_new("\u{2603}").is_err());
        assert!(OrganizationId::try_new(&"a".repeat(MAX_ORGANIZATION_ID_BYTES)).is_ok());
        assert!(OrganizationId::try_new(&"a".repeat(MAX_ORGANIZATION_ID_BYTES + 1)).is_err());
        assert_eq!(org("org_acme").as_str(), "org_acme");
    }

    /// The gate is consulted BEFORE any index handle: with a service whose
    /// data root is unusable, a denied read still returns the TYPED refusal
    /// (never an empty outcome, never an error from the store), and an
    /// entitled read over an unattached workspace answers an honest empty
    /// hit set with `truncated`.
    #[test]
    fn denied_reads_are_refused_typed_and_entitled_reads_never_touch_a_denied_seam() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let workspace = WorkspaceId::new(7);
        let request = FederatedQueryRequest {
            workspaces: vec![workspace],
            query: "refresh token".into(),
            limit: 10,
        };
        let local = SingleOrganizationBinding::new(org("org_acme"));

        // Single workspace requires shared_intelligence: a cross-repo-only
        // plan is refused typed at the seam.
        let refusal = federated_query(
            &service,
            &request,
            &entitled(ValueEntitlement::CrossRepoIntelligence),
            &local,
        )
        .unwrap_err();
        let FederatedQueryRefusal::Entitlement(refusal) = refusal else {
            panic!("expected the entitlement refusal: {refusal}");
        };
        assert_eq!(refusal.entitlement, ValueEntitlement::SharedIntelligence);
        assert_eq!(refusal.hook.as_str(), "intelligence_federation");
        assert_eq!(refusal.cause, ValueRefusalCause::NotInPlan);
        assert_eq!(refusal.plan_id.as_deref(), Some("team"));

        // Offline: typed cloud_absent, naming the seam.
        let refusal =
            federated_query(&service, &request, &ValueGate::absent(), &local).unwrap_err();
        let FederatedQueryRefusal::Entitlement(refusal) = refusal else {
            panic!("expected the entitlement refusal: {refusal}");
        };
        assert_eq!(refusal.cause, ValueRefusalCause::CloudAbsent);
        assert_eq!(refusal.entitlement, ValueEntitlement::SharedIntelligence);

        // Entitled: the read runs (unattached workspace => honest empty with
        // truncation), and the required entitlement is reported.
        let outcome = federated_query(
            &service,
            &request,
            &entitled(ValueEntitlement::SharedIntelligence),
            &local,
        )
        .unwrap();
        assert_eq!(outcome.entitlement, ValueEntitlement::SharedIntelligence);
        assert!(outcome.hits.is_empty());
        assert_eq!(outcome.workspaces_read, 0);
        assert!(outcome.truncated);

        // Two workspaces need cross_repo_intelligence: a shared-only plan is
        // refused typed naming the cross-repo entitlement.
        let cross = FederatedQueryRequest {
            workspaces: vec![workspace, WorkspaceId::new(8)],
            ..request
        };
        let refusal = federated_query(
            &service,
            &cross,
            &entitled(ValueEntitlement::SharedIntelligence),
            &local,
        )
        .unwrap_err();
        let FederatedQueryRefusal::Entitlement(refusal) = refusal else {
            panic!("expected the entitlement refusal: {refusal}");
        };
        assert_eq!(refusal.entitlement, ValueEntitlement::CrossRepoIntelligence);
        assert_eq!(refusal.hook.as_str(), "cross_repo_index");
    }

    /// The federated read is organization-bound: a workspace owned by
    /// another organization refuses the whole query before any index handle
    /// is opened (gate or no gate), an unresolvable workspace fails closed,
    /// and a same-organization read proceeds.
    #[test]
    fn federated_reads_refuse_foreign_workspaces_typed_and_serve_same_org() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let org_a = org("org_a");
        let org_b = org("org_b");
        let binding = MapBinding {
            caller: org_a.clone(),
            owners: HashMap::from([(7u64, org_a.clone()), (8u64, org_b.clone())]),
        };
        // One workspace requires shared_intelligence; several require
        // cross_repo_intelligence: each shape consults its own gate.
        let shared_gate = entitled(ValueEntitlement::SharedIntelligence);
        let cross_gate = entitled(ValueEntitlement::CrossRepoIntelligence);

        // Same organization: the query runs (unattached workspaces => honest
        // empty outcome with truncation, never an error).
        let outcome = federated_query(
            &service,
            &FederatedQueryRequest {
                workspaces: vec![WorkspaceId::new(7)],
                query: "refresh token".into(),
                limit: 10,
            },
            &shared_gate,
            &binding,
        )
        .unwrap();
        assert_eq!(outcome.workspaces_read, 0);
        assert!(outcome.truncated);

        // Cross-organization workspace: refused typed, naming both
        // organizations and the workspace, before any read (zero hits
        // returned, never a partial fold).
        let refusal = federated_query(
            &service,
            &FederatedQueryRequest {
                workspaces: vec![WorkspaceId::new(7), WorkspaceId::new(8)],
                query: "refresh token".into(),
                limit: 10,
            },
            &cross_gate,
            &binding,
        )
        .unwrap_err();
        let message = refusal.to_string();
        let FederatedQueryRefusal::ForeignWorkspace {
            caller,
            owner,
            workspace,
        } = refusal
        else {
            panic!("expected the typed foreign-workspace refusal: {message}");
        };
        assert_eq!(caller, org_a);
        assert_eq!(owner, org_b);
        assert_eq!(workspace, WorkspaceId::new(8));
        assert!(message.contains("org_b"), "{message}");
        assert!(message.contains("org_a"), "{message}");

        // The gate is still consulted first: a denied/absent plan refuses the
        // entitlement even when the workspace set is foreign.
        let refusal = federated_query(
            &service,
            &FederatedQueryRequest {
                workspaces: vec![WorkspaceId::new(8)],
                query: "refresh token".into(),
                limit: 10,
            },
            &ValueGate::absent(),
            &binding,
        )
        .unwrap_err();
        assert!(matches!(refusal, FederatedQueryRefusal::Entitlement(_)));

        // An unresolvable owner fails closed: no index is touched.
        let refusal = federated_query(
            &service,
            &FederatedQueryRequest {
                workspaces: vec![WorkspaceId::new(9)],
                query: "refresh token".into(),
                limit: 10,
            },
            &shared_gate,
            &binding,
        )
        .unwrap_err();
        let FederatedQueryRefusal::UnboundWorkspace { workspace, reason } = refusal else {
            panic!("expected the typed unbound-workspace refusal: {refusal}");
        };
        assert_eq!(workspace, WorkspaceId::new(9));
        assert!(reason.contains("no durable owner"), "{reason}");

        // The other organization sees its own workspace and refuses ours.
        let caller_b = MapBinding {
            caller: org_b,
            owners: HashMap::from([(7u64, org_a), (8u64, org("org_b"))]),
        };
        assert!(federated_query(
            &service,
            &FederatedQueryRequest {
                workspaces: vec![WorkspaceId::new(8)],
                query: "refresh token".into(),
                limit: 10,
            },
            &shared_gate,
            &caller_b,
        )
        .is_ok());
        assert!(matches!(
            federated_query(
                &service,
                &FederatedQueryRequest {
                    workspaces: vec![WorkspaceId::new(7)],
                    query: "refresh token".into(),
                    limit: 10,
                },
                &shared_gate,
                &caller_b,
            )
            .unwrap_err(),
            FederatedQueryRefusal::ForeignWorkspace { .. }
        ));
    }
}
