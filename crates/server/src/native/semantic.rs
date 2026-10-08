//! Native semantic introspection (audit 83): provider registry status and
//! capability coverage.
//!
//! Two read-only endpoints:
//!
//! - `GET /native/semantic/status` — which semantic providers are
//!   registered (id, version, capabilities) plus the always-available
//!   fallback. The status reports EXACTLY the registry wired into
//!   `deps.semantic` — the SAME `Arc` the daemon graph hands the agent. No
//!   parallel registry is ever constructed; when no registry is wired the
//!   fallback-only shape is reported from the fallback's constant identity
//!   (`configured: false`) — never a 500.
//! - `GET /native/semantic/capabilities` — the capability matrix plus the
//!   union coverage; selection on this surface is capability-driven only.
//!
//! Only provider identity, version and capability booleans are exposed:
//! no endpoints, credentials, model names or other implementation data.

use faktor_semantic::fallback::{GenericSemanticFallback, GENERIC_FALLBACK_ID};
use faktor_semantic::registry::{fallback_operation_status, SemanticProviderRegistry};
use faktor_semantic::types::{
    SemanticCapabilities, SemanticCompleteness, SemanticFidelity, SemanticOp, SemanticProvider,
};

use super::*;

/// One capability set's public introspection shape.
fn capabilities_json(caps: SemanticCapabilities) -> serde_json::Value {
    let mut operations = serde_json::Map::new();
    for op in SemanticOp::ALL {
        operations.insert(
            op.as_str().to_string(),
            serde_json::Value::Bool(caps.supports(op)),
        );
    }
    serde_json::json!({
        "operations": operations,
        "composeDelta": caps.compose_delta,
        "constrainedEdit": caps.constrained_edit,
    })
}

/// One provider's public introspection shape. Deliberately narrow: id +
/// version + declared truth, nothing else. A provider with no VALIDATED
/// descriptor reports NO capabilities (audit P1-OBS): selection never trusts
/// an unvalidated claim, and neither may this surface — the claim would be
/// indistinguishable from readiness.
fn provider_json(provider: &dyn SemanticProvider) -> serde_json::Value {
    match provider.validated_descriptor() {
        Some(descriptor) => serde_json::json!({
            "id": descriptor.id.as_str(),
            "version": descriptor.version,
            "validated": true,
            "fidelity": descriptor.fidelity.unwrap_or(SemanticFidelity::None),
            "completeness": descriptor
                .completeness
                .unwrap_or(SemanticCompleteness::Unknown),
            "capabilities": capabilities_json(descriptor.capabilities),
        }),
        None => serde_json::json!({
            "id": provider.id().as_str(),
            "version": provider.version(),
            "validated": false,
            "fidelity": SemanticFidelity::None,
            "completeness": SemanticCompleteness::Unknown,
            "capabilities": capabilities_json(SemanticCapabilities::NONE),
        }),
    }
}

/// The fallback shape WITHOUT constructing a registry: the generic
/// fallback's identity and capability set are process constants, and
/// reporting them must never create a parallel authority.
fn fallback_json() -> serde_json::Value {
    serde_json::json!({
        "id": GENERIC_FALLBACK_ID,
        "version": 1,
        "capabilities": capabilities_json(GenericSemanticFallback::default().capabilities()),
    })
}

/// The authenticated registry view as JSON. `registry` is exactly
/// `deps.semantic` (the graph's ONE Arc); `None` reports the fallback-only
/// shape with an empty provider list. The fallback is stateless, so its
/// shape never depends on whether a registry is wired.
fn status_json(registry: Option<&SemanticProviderRegistry>) -> serde_json::Value {
    let providers = registry
        .map(|registry| {
            registry
                .providers()
                .iter()
                .map(|p| provider_json(p.as_ref()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // VALIDATED descriptors only (audit P1-OBS): an unvalidated claim never
    // appears as snapshot readiness.
    let snapshot_providers = registry
        .map(|registry| {
            registry
                .providers()
                .iter()
                .filter(|p| {
                    p.validated_descriptor()
                        .is_some_and(|d| d.capabilities.supports(SemanticOp::Snapshot))
                })
                .map(|p| p.id().as_str().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let fallback = registry
        .map(|registry| provider_json(registry.fallback()))
        .unwrap_or_else(fallback_json);
    let fallback_snapshot = registry
        .map(|registry| {
            registry
                .fallback()
                .capabilities()
                .supports(SemanticOp::Snapshot)
        })
        .unwrap_or_else(|| {
            GenericSemanticFallback::default()
                .capabilities()
                .supports(SemanticOp::Snapshot)
        });
    // Per-operation fidelity/completeness/source (audit Tangerine-4): a bare
    // boolean could not distinguish heuristic fallback from compiler-exact
    // Tangerine; consumers branch on these fields instead.
    let mut operations = serde_json::Map::new();
    for op in SemanticOp::ALL {
        let status = registry
            .map(|registry| registry.operation_status(op))
            .unwrap_or_else(|| fallback_operation_status(op));
        operations.insert(
            op_slug(op).to_string(),
            serde_json::json!({
                "available": status.available,
                "fidelity": status.fidelity,
                "completeness": status.completeness,
                "source": status.source,
            }),
        );
    }
    serde_json::json!({
        "configured": registry.is_some(),
        "providerCount": providers.len(),
        "providers": providers,
        "fallback": fallback,
        "snapshotState": {
            "providers": snapshot_providers,
            "fallback": fallback_snapshot,
        },
        "operations": operations,
    })
}

/// Stable wire slug for one semantic operation.
fn op_slug(op: SemanticOp) -> &'static str {
    match op {
        SemanticOp::Snapshot => "snapshot",
        SemanticOp::Context => "context",
        SemanticOp::Delta => "delta",
        SemanticOp::Affected => "affected",
        SemanticOp::Verify => "verify",
        SemanticOp::Explain => "explain",
    }
}

/// The capability matrix JSON over the same registry view.
fn capabilities_report_json(registry: Option<&SemanticProviderRegistry>) -> serde_json::Value {
    let providers = registry
        .map(|registry| {
            registry
                .providers()
                .iter()
                .map(|p| provider_json(p.as_ref()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let fallback_caps = registry
        .map(|registry| registry.fallback().capabilities())
        .unwrap_or_else(|| GenericSemanticFallback::default().capabilities());
    // VALIDATED descriptors only (audit P1-OBS): the union reports what the
    // registry can actually select, never a provider's unvalidated claim.
    let validated_caps = |registry: &SemanticProviderRegistry| {
        registry
            .providers()
            .iter()
            .filter_map(|p| p.validated_descriptor())
            .map(|d| d.capabilities)
            .collect::<Vec<_>>()
    };
    let mut union = serde_json::Map::new();
    for op in SemanticOp::ALL {
        let supported = registry
            .map(|registry| {
                validated_caps(registry)
                    .iter()
                    .any(|caps| caps.supports(op))
            })
            .unwrap_or(false)
            || fallback_caps.supports(op);
        union.insert(op.as_str().to_string(), serde_json::Value::Bool(supported));
    }
    let compose_delta = registry
        .map(|registry| {
            validated_caps(registry)
                .iter()
                .any(|caps| caps.compose_delta)
        })
        .unwrap_or(false)
        || fallback_caps.compose_delta;
    let constrained_edit = registry
        .map(|registry| {
            validated_caps(registry)
                .iter()
                .any(|caps| caps.constrained_edit)
        })
        .unwrap_or(false)
        || fallback_caps.constrained_edit;
    serde_json::json!({
        "configured": registry.is_some(),
        "providers": providers,
        "fallback": registry
            .map(|registry| provider_json(registry.fallback()))
            .unwrap_or_else(fallback_json),
        "union": {
            "operations": union,
            "composeDelta": compose_delta,
            "constrainedEdit": constrained_edit,
        },
    })
}

/// `GET /native/semantic/status` — registry status without leaking
/// implementation data. Inspects ONLY `deps.semantic` (the graph's ONE
/// registry; no parallel registry exists). `configured: false` + fallback
/// is a 200, not an error: an unconfigured daemon still has working
/// fallback semantics.
pub(crate) async fn native_semantic_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    Json(status_json(state.deps.semantic.as_deref())).into_response()
}

/// `GET /native/semantic/capabilities` — per-provider capability matrix plus
/// the union across providers and the fallback, over the SAME registry.
pub(crate) async fn native_semantic_capabilities(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    Json(capabilities_report_json(state.deps.semantic.as_deref())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::{CancellationToken, OpId, SessionId, WorkspaceId};
    use faktor_semantic::types::{
        SemanticContextPack, SemanticContextRequest, SemanticError, SemanticProviderId,
    };
    use std::sync::Arc;

    struct ProbeProvider {
        id: SemanticProviderId,
    }

    impl SemanticProvider for ProbeProvider {
        fn id(&self) -> SemanticProviderId {
            self.id.clone()
        }

        fn version(&self) -> u32 {
            7
        }

        fn capabilities(&self) -> SemanticCapabilities {
            SemanticCapabilities::CONTEXT
        }

        fn context(
            &self,
            _request: SemanticContextRequest,
        ) -> faktor_semantic::BoxFuture<
            '_,
            Result<faktor_semantic::SemanticEnvelope<SemanticContextPack>, SemanticError>,
        > {
            Box::pin(async { Err(SemanticError::Refused("probe is introspection-only".into())) })
        }
    }

    fn probe_registry() -> SemanticProviderRegistry {
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        registry.register(Arc::new(ProbeProvider {
            id: SemanticProviderId::parse("probe.provider-1").unwrap(),
        }));
        registry
    }

    /// A provider that CLAIMS every capability but has no validated
    /// descriptor (audit P1-OBS): the introspection surfaces must report the
    /// claim as untrusted, never repeat it.
    struct UnvalidatedClaimProvider;

    impl SemanticProvider for UnvalidatedClaimProvider {
        fn id(&self) -> SemanticProviderId {
            SemanticProviderId::parse("lying.provider").unwrap()
        }

        fn version(&self) -> u32 {
            9
        }

        fn capabilities(&self) -> SemanticCapabilities {
            SemanticCapabilities::ALL
        }

        fn validated_descriptor(
            &self,
        ) -> Option<faktor_semantic::types::SemanticProviderDescriptor> {
            None
        }
    }

    #[test]
    fn status_serves_exactly_the_wired_registry() {
        // A registry view passed in IS the served one: every provider of the
        // wired registry (and only those) appears, with `configured: true`.
        let registry = probe_registry();
        let body = status_json(Some(&registry));
        assert_eq!(body["configured"], true);
        assert_eq!(body["providerCount"], 1);
        assert_eq!(body["providers"][0]["id"], "probe.provider-1");
        assert_eq!(body["providers"][0]["version"], 7);
        assert_eq!(
            body["providers"][0]["capabilities"]["operations"]["context"],
            true
        );
        assert_eq!(
            body["providers"][0]["capabilities"]["operations"]["snapshot"],
            false
        );
        assert_eq!(
            body["snapshotState"]["providers"].as_array().unwrap().len(),
            0
        );
        let caps = capabilities_report_json(Some(&registry));
        assert_eq!(caps["configured"], true);
        assert_eq!(caps["union"]["operations"]["context"], true);
    }

    #[test]
    fn absent_registry_is_a_static_fallback_shape_without_a_parallel_registry() {
        let body = status_json(None);
        assert_eq!(body["configured"], false);
        assert_eq!(body["providerCount"], 0);
        assert!(body["providers"].as_array().unwrap().is_empty());
        assert_eq!(body["fallback"]["id"], GENERIC_FALLBACK_ID);
        // TRUTHFUL fallback (audit Tangerine-4): no snapshot/verify claims.
        assert!(!body["snapshotState"]["fallback"].as_bool().unwrap());
        assert_eq!(body["operations"]["context"]["available"], true);
        assert_eq!(body["operations"]["context"]["fidelity"], "heuristic");
        assert_eq!(body["operations"]["context"]["completeness"], "partial");
        assert_eq!(body["operations"]["verify"]["available"], false);
        assert_eq!(body["operations"]["snapshot"]["available"], false);
        let caps = capabilities_report_json(None);
        assert_eq!(caps["configured"], false);
        assert_eq!(caps["union"]["operations"]["explain"], true);
    }

    /// P1-OBS: an unvalidated provider's capability CLAIM never reaches the
    /// status or capability surfaces; only validated truth drives them.
    #[test]
    fn unvalidated_provider_claims_never_reach_the_status_surfaces() {
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        registry.register(Arc::new(UnvalidatedClaimProvider));
        let body = status_json(Some(&registry));
        assert_eq!(body["providerCount"], 1);
        assert_eq!(body["providers"][0]["validated"], false);
        assert_eq!(
            body["providers"][0]["capabilities"]["operations"]["context"],
            false
        );
        assert_eq!(
            body["providers"][0]["capabilities"]["operations"]["snapshot"],
            false
        );
        assert!(
            body["snapshotState"]["providers"]
                .as_array()
                .unwrap()
                .is_empty(),
            "an unvalidated snapshot claim is not readiness"
        );
        assert_eq!(body["operations"]["snapshot"]["available"], false);
        assert_eq!(
            body["operations"]["snapshot"]["source"],
            GENERIC_FALLBACK_ID
        );
        let caps = capabilities_report_json(Some(&registry));
        assert_eq!(caps["union"]["operations"]["snapshot"], false);
        assert_eq!(caps["union"]["composeDelta"], false);
        assert_eq!(caps["union"]["constrainedEdit"], false);
        assert_eq!(
            caps["union"]["operations"]["context"], true,
            "the fallback's truthful context service remains in the union"
        );
    }

    #[test]
    fn probe_provider_never_serves_data_from_the_introspection_surface() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        // Introspection must not become a data path: the probe's own
        // operation refuses typed, and the status shape remains metadata.
        let registry = probe_registry();
        let provider = registry.providers()[0].clone();
        let request = SemanticContextRequest {
            call: faktor_semantic::SemanticCall::new(
                OpId::new(1),
                SessionId::new(1),
                WorkspaceId::new(1),
                0,
                CancellationToken::new(),
            ),
            workspace: WorkspaceId::new(1),
            source_revision: "rev".into(),
            snapshot_id: faktor_semantic::SemanticSnapshotId::default(),
            query: "q".into(),
            max_items: 1,
            max_bytes: 1,
        };
        let mut future = Box::pin(provider.context(request));
        let mut cx = Context::from_waker(Waker::noop());
        let result = match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("probe must resolve immediately"),
        };
        assert!(matches!(result, Err(SemanticError::Refused(_))));
    }
}
