//! Public-seam tests for lazy tool exposure (`docs/acquire.md` §2/§4): an
//! ordinary prompt must produce byte-identical bundles whether or not a lazy
//! tool is registered, a Rust `supplier` trait discussion must not activate
//! anything, and an activated prompt exposes the lazy tool only in the
//! masked phases.

use std::sync::Arc;

use faktor_agent::{
    acquire_source_phases, acquire_source_triggers, evaluate_triggers, PhaseMask, RecoveryHint,
    RouterPhase, Tool, ToolActivationSet, ToolExposure, ToolOutcome, ToolRegistry, ToolTrigger,
    TriggerVerdict,
};
use faktor_core::model::ModelCapabilities;
use faktor_core::resource::ResourceClass;

fn tool(name: &str, class: ResourceClass, schema: serde_json::Value) -> Tool {
    Tool {
        name: name.into(),
        description: format!("{name} description"),
        input_schema: schema,
        resource_class: class,
        capability: None,
        recovery_hint: RecoveryHint::Idempotent,
        path_args: vec![],
        execute: Arc::new(|_ctx, _args| Box::pin(async move { Ok(ToolOutcome::default()) })),
    }
}

fn source_market() -> Tool {
    tool(
        "source_market",
        ResourceClass::Network,
        serde_json::json!({
            "type": "object",
            "properties": {"op": {"enum": ["search", "product", "quote", "bom", "job"]}},
            "required": ["op"],
            "additionalProperties": false
        }),
    )
}

fn registry(with_lazy: bool) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(tool(
        "read_file",
        ResourceClass::DiskRead,
        serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    ));
    registry.register(tool(
        "run_command",
        ResourceClass::Terminal,
        serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
    ));
    if with_lazy {
        registry.register_lazy(
            source_market(),
            ToolExposure::lazy(acquire_source_phases(), acquire_source_triggers()),
        );
    }
    registry
}

fn phases_exposing(registry: &ToolRegistry, activation: &ToolActivationSet) -> Vec<RouterPhase> {
    RouterPhase::ALL
        .into_iter()
        .filter(|phase| {
            registry
                .bundle_for_phase_with_activation(*phase, &ModelCapabilities::default(), activation)
                .tool_names()
                .contains(&"source_market")
        })
        .collect()
}

#[test]
fn ordinary_prompt_bundles_are_byte_identical_with_a_lazy_registration() {
    let caps = ModelCapabilities::default();
    let baseline = registry(false);
    let with_lazy = registry(true);
    for phase in RouterPhase::ALL {
        let a = baseline.bundle_for_phase(phase, &caps);
        let b = with_lazy.bundle_for_phase(phase, &caps);
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap(),
            "{phase:?}: an inactive lazy tool must not change one byte"
        );
        assert_eq!(a.bundle_hash(), b.bundle_hash());
    }
    // The ordinary prompt does not activate anything.
    let activation = with_lazy.activation_for_text(
        &ToolActivationSet::new(),
        "fix the failing parser test in src/parser.rs",
    );
    assert!(activation.is_empty());
    assert!(phases_exposing(&with_lazy, &activation).is_empty());
}

#[test]
fn supplier_trait_discussion_does_not_activate() {
    let with_lazy = registry(true);
    let activation = with_lazy.activation_for_text(
        &ToolActivationSet::new(),
        "the supplier trait in Rust should not be object safe here; see supplier.rs",
    );
    assert!(
        activation.is_empty(),
        "a weak signal alone must never activate"
    );
    assert_eq!(
        evaluate_triggers(&acquire_source_triggers(), "supplier"),
        TriggerVerdict::None
    );
}

#[test]
fn activated_prompt_exposes_the_tool_only_in_the_masked_phases() {
    let with_lazy = registry(true);
    let activation = with_lazy.activation_for_text(
        &ToolActivationSet::new(),
        "please quote this BOM: 5,000 connectors",
    );
    assert!(activation.is_active("source_market"));
    assert_eq!(
        phases_exposing(&with_lazy, &activation),
        vec![
            RouterPhase::Plan,
            RouterPhase::Explore,
            RouterPhase::Retrieve,
            RouterPhase::Implement,
            RouterPhase::Debug,
        ]
    );

    // The explicit flag deactivates, and a product URL activates.
    let off = with_lazy.activation_for_text(&activation, "/source off");
    assert!(!off.is_active("source_market"));
    let url = with_lazy.activation_for_text(
        &ToolActivationSet::new(),
        "https://detail.1688.com/offer/123.html",
    );
    assert!(url.is_active("source_market"));
}

#[test]
fn a_lazy_mask_cannot_breach_model_only_phases_through_the_public_api() {
    let mut registry = ToolRegistry::new();
    registry.register_lazy(
        source_market(),
        ToolExposure::lazy(PhaseMask::ALL, vec![ToolTrigger::signal("always")]),
    );
    let mut activation = ToolActivationSet::new();
    registry.observe_activation("always", &mut activation);
    assert!(activation.is_active("source_market"));
    for phase in [RouterPhase::Compact, RouterPhase::Title, RouterPhase::Embed] {
        let bundle = registry.bundle_for_phase_with_activation(
            phase,
            &ModelCapabilities::default(),
            &activation,
        );
        assert!(!bundle.tool_names().contains(&"source_market"), "{phase:?}");
    }
}
