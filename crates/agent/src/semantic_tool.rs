//! The real `semantic_query` tool (audit Tangerine-10): the model's explicit
//! escape hatch into the semantic provider. It is NOT synthesized from model
//! capabilities (embedding support is irrelevant): the runtime injects the
//! spec into the bundle only while an EXTERNAL semantic provider is
//! registered, and the execution is implemented here — a call can therefore
//! never reach "unknown tool". Fallback-only environments answer a typed
//! `semantic_unavailable` refusal.

use std::sync::Arc;

use faktor_core::capability::Capability;
use faktor_core::error::{Error, ErrorKind};
use faktor_core::resource::ResourceClass;
use faktor_core::time::{Clock, SystemClock};
use faktor_semantic::{
    SemanticCall, SemanticCapabilities, SemanticContextRequest, SemanticEnvelope, SemanticError,
    SemanticOp, SemanticProviderRegistry, SemanticSelection, SemanticSnapshotId,
    SEMANTIC_SCHEMA_VERSION,
};

use crate::tool::{RecoveryHint, Tool, ToolOutcome, ToolRunCtx, SEMANTIC_QUERY_TOOL};
use faktor_provider::ToolSpec;

/// Byte bound of one rendered semantic-query result.
pub const SEMANTIC_QUERY_RENDER_MAX_BYTES: usize = 8 * 1024;
/// Byte bound of one rendered excerpt.
pub const SEMANTIC_QUERY_EXCERPT_MAX_BYTES: usize = 512;
/// Default item count when the model passes no `limit`.
pub const SEMANTIC_QUERY_DEFAULT_LIMIT: usize = 8;
/// Hard item bound.
pub const SEMANTIC_QUERY_MAX_LIMIT: usize = 32;

/// The model-visible spec (the runtime decides WHEN to inject it).
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: SEMANTIC_QUERY_TOOL.into(),
        description: "Query the semantic (Tangerine) provider for bounded context around a \
                      question: returns entity ids, workspace paths, relevance and excerpts, \
                      stamped with the provider and snapshot identity. Available only while a \
                      semantic provider is registered."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["query"]
        }),
    }
}

fn unavailable(detail: &str) -> Error {
    Error::new(
        ErrorKind::Provider {
            code: "semantic_unavailable".into(),
            retryable: false,
        },
        format!("semantic provider unavailable: {detail}"),
    )
}

fn semantic_failure(err: SemanticError) -> Error {
    Error::new(
        ErrorKind::Provider {
            code: "semantic_error".into(),
            retryable: false,
        },
        format!("semantic query failed: {err}"),
    )
}

/// Render one validated context envelope as bounded DATA. Pure, so the
/// render contract is unit-testable without a provider.
pub fn render(envelope: &SemanticEnvelope<faktor_semantic::SemanticContextPack>) -> String {
    let mut out = format!(
        "semantic context: provider={} provider_version={} snapshot={} items={} degraded={}\n",
        envelope.provider_id,
        envelope.provider_version,
        envelope.snapshot_id.to_hex(),
        envelope.payload.items.len(),
        envelope.payload.degraded
    );
    for item in &envelope.payload.items {
        let excerpt = truncate_bytes(item.excerpt.trim(), SEMANTIC_QUERY_EXCERPT_MAX_BYTES);
        out.push_str(&format!(
            "- {} [{}] relevance={}\n  {}\n",
            item.entity.path.as_str(),
            item.entity.entity_id.as_str(),
            item.relevance_bps,
            excerpt
        ));
        if out.len() >= SEMANTIC_QUERY_RENDER_MAX_BYTES {
            out.push_str("(truncated at the render bound)\n");
            break;
        }
    }
    truncate_bytes(&out, SEMANTIC_QUERY_RENDER_MAX_BYTES).to_string()
}

fn truncate_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// The real tool: execute consults the registry with the SAME view-snapshot
/// derivation the automatic consult uses, so both paths share cache identity.
pub fn tool(registry: Arc<SemanticProviderRegistry>) -> Arc<Tool> {
    Arc::new(Tool {
        name: SEMANTIC_QUERY_TOOL.into(),
        description: spec().description,
        input_schema: spec().input_schema,
        resource_class: ResourceClass::DiskRead,
        capability: Some(Capability::ReadWorkspace { path: ".".into() }),
        recovery_hint: RecoveryHint::Idempotent,
        path_args: Vec::new(),
        execute: Arc::new(move |ctx: ToolRunCtx, args: serde_json::Value| {
            let registry = registry.clone();
            Box::pin(async move {
                let query = args
                    .get("query")
                    .and_then(|q| q.as_str())
                    .ok_or_else(|| Error::malformed("semantic_query requires query"))?;
                if query.is_empty() || query.len() > 4096 {
                    return Err(Error::malformed(
                        "semantic_query query must be 1..=4096 bytes",
                    ));
                }
                let limit = args
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(SEMANTIC_QUERY_DEFAULT_LIMIT as u64)
                    .clamp(1, SEMANTIC_QUERY_MAX_LIMIT as u64) as usize;
                let selection = registry
                    .select_validated(
                        &SemanticCapabilities::CONTEXT,
                        SemanticOp::Context,
                        &ctx.cancellation,
                    )
                    .await;
                let provider = match selection {
                    SemanticSelection::Provider(provider) => provider,
                    SemanticSelection::Fallback(_) => {
                        return Err(unavailable("no external semantic provider is registered"));
                    }
                };
                let provider_id = provider.id().clone();
                let provider_version = provider.version();
                let workspace = ctx.identity.workspace_id;
                let revision = format!("session:{}", ctx.session_id.raw());
                let snapshot_id = SemanticSnapshotId::derive(
                    workspace,
                    &revision,
                    &provider_id,
                    provider_version,
                    SEMANTIC_SCHEMA_VERSION,
                );
                let request = SemanticContextRequest {
                    call: SemanticCall::new(
                        ctx.op_id,
                        ctx.session_id,
                        workspace,
                        SystemClock.now_ms(),
                        ctx.cancellation.child(),
                    ),
                    workspace,
                    source_revision: revision,
                    snapshot_id,
                    query: query.to_string(),
                    max_items: limit,
                    max_bytes: SEMANTIC_QUERY_RENDER_MAX_BYTES,
                };
                let envelope = registry.context(request).await.map_err(semantic_failure)?;
                if envelope.provider_id.as_str() == faktor_semantic::GENERIC_FALLBACK_ID {
                    // The registered provider failed and the registry degraded:
                    // never present fallback data as provider evidence.
                    return Err(unavailable(
                        "the registered provider degraded to the generic fallback",
                    ));
                }
                Ok(ToolOutcome {
                    text: render(&envelope),
                    ..ToolOutcome::default()
                })
            })
        }),
    })
}

/// A minimal in-process semantic provider for runtime tests (registered in
/// the registry exactly like Tangerine would be). Only context is answered.
#[cfg(test)]
pub(crate) struct FakeContextProvider {
    calls: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl FakeContextProvider {
    pub(crate) fn new() -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn context_calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
impl faktor_semantic::SemanticProvider for FakeContextProvider {
    fn id(&self) -> faktor_semantic::SemanticProviderId {
        faktor_semantic::SemanticProviderId::parse("tangerine-test").expect("valid id")
    }

    fn version(&self) -> u32 {
        7
    }

    fn capabilities(&self) -> faktor_semantic::SemanticCapabilities {
        faktor_semantic::SemanticCapabilities::CONTEXT
    }

    fn context(
        &self,
        request: SemanticContextRequest,
    ) -> faktor_semantic::BoxFuture<
        '_,
        Result<
            SemanticEnvelope<faktor_semantic::SemanticContextPack>,
            faktor_semantic::SemanticError,
        >,
    > {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let provider_id = self.id();
        let provider_version = self.version();
        Box::pin(async move {
            let entity = faktor_semantic::SemanticEntityRef::new(
                request.workspace,
                faktor_semantic::WorkspacePath::parse("src/auth/service.rs").expect("path"),
                faktor_semantic::SemanticEntityId::parse("crate::auth::TokenValidator::validate")
                    .expect("entity id"),
            );
            let payload = faktor_semantic::SemanticContextPack {
                items: vec![faktor_semantic::SemanticContextItem {
                    entity,
                    relevance_bps: 9_500,
                    excerpt: "fn validate(token: &Token) -> Result<(), AuthError>".into(),
                }],
                truncated: false,
                total_bytes: 64,
                degraded: false,
            };
            Ok(SemanticEnvelope::new(
                provider_id,
                provider_version,
                request.workspace,
                request.snapshot_id,
                1,
                payload,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_semantic::{
        SemanticContextItem, SemanticContextPack, SemanticEnvelope, SemanticProviderId,
    };

    fn envelope(
        items: Vec<SemanticContextItem>,
        degraded: bool,
    ) -> SemanticEnvelope<SemanticContextPack> {
        SemanticEnvelope {
            schema_version: SEMANTIC_SCHEMA_VERSION,
            provider_id: SemanticProviderId::parse("tangerine").unwrap(),
            provider_version: 7,
            workspace: faktor_core::WorkspaceId::new(3),
            snapshot_id: SemanticSnapshotId::from_file_hash(faktor_core::hash::FileHash::from(
                [9u8; 32],
            )),
            generated_ms: 1,
            payload: SemanticContextPack {
                total_bytes: 1,
                truncated: false,
                items,
                degraded,
            },
            provenance: Default::default(),
        }
    }

    #[test]
    fn render_is_bounded_and_carries_provider_and_snapshot_identity() {
        let item = SemanticContextItem {
            entity: faktor_semantic::SemanticEntityRef::new(
                faktor_core::WorkspaceId::new(3),
                faktor_semantic::WorkspacePath::parse("src/lib.rs").unwrap(),
                faktor_semantic::SemanticEntityId::parse("path:src/lib.rs").unwrap(),
            ),
            relevance_bps: 9_000,
            excerpt: "x".repeat(SEMANTIC_QUERY_EXCERPT_MAX_BYTES * 2),
        };
        let rendered = render(&envelope(vec![item], false));
        assert!(rendered.contains("provider=tangerine"));
        assert!(rendered.contains("snapshot="));
        assert!(rendered.len() <= SEMANTIC_QUERY_RENDER_MAX_BYTES);
        assert!(!rendered.contains(&"x".repeat(SEMANTIC_QUERY_EXCERPT_MAX_BYTES + 1)));
    }

    fn ctx() -> ToolRunCtx {
        use faktor_core::id::{OpId, SessionId};
        use faktor_core::WorkspaceIdentity;
        ToolRunCtx {
            session_id: SessionId::new(7),
            op_id: OpId::new(8),
            identity: WorkspaceIdentity::new(
                faktor_core::WorkspaceId::new(1),
                faktor_core::WorktreeId::new(2),
                faktor_core::TaskId::new(3),
            ),
            cancellation: faktor_core::cancellation::CancellationToken::new(),
            artifacts: Arc::new(crate::ToolArtifactSink::Null),
            tool_call_mode: crate::ToolCallMode::Native,
            workspace: None,
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            deadline_ms: 0,
            permission_granted: true,
        }
    }

    #[tokio::test]
    async fn a_registered_provider_answers_and_fallback_only_refuses_typed() {
        use faktor_semantic::{GenericSemanticFallback, SemanticProviderRegistry};

        // Fallback only: a typed semantic_unavailable refusal, never a
        // pretend answer.
        let empty = Arc::new(SemanticProviderRegistry::new(
            GenericSemanticFallback::default(),
        ));
        let err = (tool(empty).execute)(ctx(), serde_json::json!({ "query": "x" }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("semantic provider unavailable"),
            "{err}"
        );

        // A registered provider answers with its provider/snapshot identity.
        let fake = Arc::new(FakeContextProvider::new());
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        registry.register(fake.clone());
        let out = (tool(Arc::new(registry)).execute)(
            ctx(),
            serde_json::json!({ "query": "token validation", "limit": 4 }),
        )
        .await
        .unwrap();
        assert!(out.text.contains("provider=tangerine-test"), "{}", out.text);
        assert!(out.text.contains("crate::auth::TokenValidator::validate"));
        assert!(out.text.contains("src/auth/service.rs"));
        assert_eq!(fake.context_calls(), 1);
    }

    #[test]
    fn the_bundle_spec_is_the_registered_shape() {
        let spec = spec();
        assert_eq!(spec.name, SEMANTIC_QUERY_TOOL);
        assert_eq!(spec.input_schema["required"], serde_json::json!(["query"]));
    }
}
