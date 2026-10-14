#[cfg(test)]
mod model_call_intent_tests {
    use crate::*;

    #[test]
    fn quality_floors_are_hard_and_never_lowered() {
        // Implement/Review/Debug route on a HARD 60 floor; the compaction
        // summarizer is ADAPTIVE (60 target, 50 minimum) because its output
        // cannot certify anything; route_request applies the minimum
        // verbatim (the policy decides nothing above it).
        assert_eq!(ModelCallIntent::implement_main().quality_floor(), 60);
        assert_eq!(ModelCallIntent::debug().quality_floor(), 60);
        assert_eq!(ModelCallIntent::review().quality_floor(), 60);
        let compact = ModelCallIntent::compact();
        assert_eq!(compact.quality_floor(), 50, "adaptive minimum");
        assert_eq!(compact.quality_target(), 60, "adaptive target");
        assert!(matches!(
            compact.quality,
            QualityRequirement::Adaptive {
                minimum: 50,
                target: 60
            }
        ));
        let adaptive = ModelCallIntent {
            phase: RouterPhase::Implement,
            required_capabilities: vec![],
            quality: QualityRequirement::Adaptive {
                target: 85,
                minimum: 70,
            },
            expected_output_tokens: 2048,
            semantic_risk: 0,
            output_trust: OutputTrust::Implementation,
        };
        assert_eq!(adaptive.quality.minimum(), 70);
        assert_eq!(adaptive.quality.target(), 85);
        assert_eq!(adaptive.quality_floor(), 70);
        assert_eq!(adaptive.quality_target(), 85);
    }

    #[test]
    fn output_trust_is_explicit_and_compaction_can_never_certify() {
        // Item 26: every intent carries what its output may be trusted to
        // BE. Compaction is a context-compression summary: it may stand in
        // for transcript context but it certifies no completion and
        // replaces no immutable task fact/ledger row; a title is ephemeral;
        // implement/review keep their completion-capable classes.
        assert_eq!(
            ModelCallIntent::implement_main().output_trust(),
            OutputTrust::Implementation
        );
        assert_eq!(
            ModelCallIntent::debug().output_trust(),
            OutputTrust::Implementation
        );
        assert_eq!(
            ModelCallIntent::review().output_trust(),
            OutputTrust::VerificationOpinion
        );
        assert_eq!(
            ModelCallIntent::compact().output_trust(),
            OutputTrust::ContextCompression
        );
        assert_eq!(
            ModelCallIntent::title().output_trust(),
            OutputTrust::Ephemeral
        );
        assert!(!ModelCallIntent::compact().certifies_completion());
        assert!(!ModelCallIntent::title().certifies_completion());
        assert!(ModelCallIntent::implement_main().certifies_completion());
        assert!(ModelCallIntent::review().certifies_completion());
        assert!(!OutputTrust::ContextCompression.certifies_completion());
        assert!(!OutputTrust::Ephemeral.certifies_completion());
        assert!(!OutputTrust::ContextCompression.may_replace_durable_facts());
        assert!(!OutputTrust::Ephemeral.may_replace_durable_facts());
        assert!(OutputTrust::VerificationOpinion.may_replace_durable_facts());
        assert_eq!(
            OutputTrust::ContextCompression.provenance_tag(),
            "context_compression"
        );
    }

    #[test]
    fn model_output_trust_gates_the_correctness_critical_writers() {
        // Audit item 3 (negative coverage): a ContextCompression summary and
        // an Ephemeral title can take NEITHER the completion-fact NOR the
        // verification-evidence admission; an Implementation output may
        // author completion facts (existing behavior) but never verification
        // evidence, and a VerificationOpinion may author both.
        let compaction = ModelOutput::new("summary", OutputTrust::ContextCompression, 7);
        let ephemeral = ModelOutput::new("title", OutputTrust::Ephemeral, 8);
        let implementation = ModelOutput::new("patch", OutputTrust::Implementation, 9);
        let opinion = ModelOutput::new("verdict", OutputTrust::VerificationOpinion, 10);

        for refused in [&compaction, &ephemeral] {
            let err = refused.completion_fact().unwrap_err();
            assert_eq!(err.trust, refused.trust);
            assert!(err.to_string().contains(refused.trust.provenance_tag()));
            assert!(refused.verification_evidence().is_err());
        }
        assert!(
            implementation.verification_evidence().is_err(),
            "an implementation output is verified, never authored as evidence"
        );
        assert!(
            implementation.completion_fact().is_ok(),
            "existing Implementation behavior is unchanged"
        );
        assert!(opinion.completion_fact().is_ok());
        assert!(opinion.verification_evidence().is_ok());
        // The capability gate used by every durable fact writer: durable
        // projections pass, model text is checked before anything moves.
        assert!(FactSource::Durable.check_completion_fact().is_ok());
        assert!(FactSource::Model(&implementation)
            .check_completion_fact()
            .is_ok());
        assert!(FactSource::Model(&compaction)
            .check_completion_fact()
            .is_err());
        assert!(FactSource::Model(&ephemeral)
            .check_completion_fact()
            .is_err());
        // The refusal maps to a typed permission error, never a panic.
        let err = FactSource::refusal_error(&compaction.completion_fact().unwrap_err());
        assert_eq!(err.kind, faktor_core::error::ErrorKind::Permission);
        // Provenance tags round-trip; unknown tags refuse.
        for trust in [
            OutputTrust::Ephemeral,
            OutputTrust::ContextCompression,
            OutputTrust::Implementation,
            OutputTrust::VerificationOpinion,
        ] {
            assert_eq!(
                OutputTrust::from_provenance_tag(trust.provenance_tag()),
                Some(trust)
            );
        }
        assert_eq!(OutputTrust::from_provenance_tag("made_up"), None);
    }

    #[test]
    fn route_request_carries_the_real_planned_dimensions_not_a_guess() {
        let intent = ModelCallIntent::implement_main();
        let req = intent.route_request(12_345, 7_000, 0);
        assert_eq!(req.context_tokens, 12_345, "the plan's real input estimate");
        assert_eq!(req.estimated_output_tokens, 7_000, "the real output cap");
        assert_eq!(req.quality_floor, 60);
        assert_eq!(
            req.quality_target, None,
            "a HARD requirement sends no preference tier"
        );
        assert_ne!(req.context_tokens, 16_384, "no hard-coded pre-plan guess");
        assert_ne!(req.estimated_output_tokens, 2048, "no hard-coded 2048");
        assert_eq!(intent.phase, RouterPhase::Implement);
        // The adaptive distinction really rides the request (audit item 2):
        // compaction sends its 60 target above its 50 minimum.
        let compact = ModelCallIntent::compact().route_request(1_000, 100, 0);
        assert_eq!(compact.quality_floor, 50);
        assert_eq!(compact.quality_target, Some(60));
        // Review stays hard: no target.
        assert_eq!(
            ModelCallIntent::review()
                .route_request(1_000, 100, 0)
                .quality_target,
            None
        );
    }
}

#[cfg(test)]
mod hard_quality_floor_tests {
    use crate::*;
    use faktor_core::model::ModelDescriptor;
    use faktor_router::RouterService;

    fn candidate(quality: u8) -> ModelDescriptor {
        ModelDescriptor {
            provider: "p".into(),
            model: "m".into(),
            context: 128_000,
            max_output: 16_000,
            tools: true,
            parallel_tools: false,
            reasoning: false,
            thinking: false,
            vision: false,
            structured_output: false,
            embeddings: false,
            streaming: true,
            economics: faktor_core::model::ModelEconomics {
                coding_reliability: quality,
                tool_reliability: quality,
                reasoning_reliability: quality,
                context_reliability: quality,
                ..Default::default()
            },
            source: faktor_core::model::ModelSource::ProviderCatalog,
        }
    }

    #[test]
    fn hard_60_with_best_available_50_is_a_no_capable_model_refusal() {
        // Audit (e): quality floors are HARD. The old logic lowered the
        // requested floor toward the best available candidate; the fix
        // refuses typed: requested hard 60 with best available 50 =>
        // NoCapableModel — and a 90-quality candidate serves the SAME
        // request.
        let policy = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(50)])),
            RoutingMode::Economy,
        );
        let req = ModelCallIntent::implement_main().route_request(4_000, 2_048, 0);
        assert!(
            matches!(policy.route(&req), Err(RouteFailure::NoCapableModel)),
            "hard 60 with a best available 50 must be a typed NoCapableModel, never a lowered floor"
        );
        let policy2 = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(90)])),
            RoutingMode::Economy,
        );
        assert!(
            policy2.route(&req).is_ok(),
            "an above-floor candidate serves"
        );
        // Balanced raises to its band; MaximumQuality never probes below.
        let bal = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(70)])),
            RoutingMode::Balanced,
        );
        assert!(matches!(bal.route(&req), Err(RouteFailure::NoCapableModel)));
        let maxq = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(55)])),
            RoutingMode::MaximumQuality,
        );
        assert!(matches!(
            maxq.route(&req),
            Err(RouteFailure::NoCapableModel)
        ));
    }
}

#[cfg(test)]
mod efficiency_flags_tests {
    use crate::*;
    use faktor_context::information::FailurePrior;
    use faktor_context::selection::ContextCandidate;

    /// A hostile-value prior standing in for the learning crate's handle:
    /// the value is irrelevant to the gate test.
    struct AlwaysDouble;

    impl FailurePrior for AlwaysDouble {
        fn omission_risk(&self, _candidate: &ContextCandidate) -> f64 {
            2.0
        }
    }

    #[test]
    fn efficiency_flags_default_all_off() {
        let flags = EfficiencyFlags::default();
        assert!(!flags.failure_learning);
        assert!(!flags.ccr);
        assert!(!flags.typed_handoff);
        assert!(!flags.semantic_context);
        assert!(!flags.rework_routing);
    }

    /// The gate: the prior is handed to the planner ONLY when the parsed
    /// `failure_learning` flag is on AND a handle exists. Every other
    /// combination yields `None` (baseline/parity path).
    #[test]
    fn prior_is_applied_only_when_the_flag_is_on_and_a_handle_exists() {
        let prior = AlwaysDouble;
        let handle: Option<&(dyn FailurePrior + Send + Sync)> = Some(&prior);
        let off = EfficiencyFlags::default();
        assert!(off.context_prior(handle).is_none(), "off => no prior");
        let on = EfficiencyFlags {
            failure_learning: true,
            ..Default::default()
        };
        assert!(on.context_prior(handle).is_some(), "on + handle => prior");
        assert!(
            on.context_prior(None).is_none(),
            "on + no handle => no prior"
        );
        // Running through the concrete trait object never invokes the
        // prior while the flag is off (the handle is not even observed).
        let gated = off.context_prior(handle);
        assert!(gated.is_none());
    }
}

#[cfg(test)]
mod compact_adaptive_quality_tests {
    use crate::*;
    use faktor_core::model::{
        MicroUsdPerMillionTokens, ModelCapabilities, ModelEconomics, ModelSource, PriceQuote,
        PricingSnapshot, PricingState, QualityAuthority, QualityMetric, QualityStatement,
    };
    use faktor_router::{EmptyOutcomeStore, RouteCandidate, RouterService};

    fn candidate(provider: &str, model: &str, quality: u8, price: u64) -> RouteCandidate {
        let caps = ModelCapabilities {
            context: 128_000,
            max_output: 16_000,
            tools: true,
            streaming: true,
            ..Default::default()
        };
        let economics = ModelEconomics {
            tool_reliability: quality,
            reasoning_reliability: quality,
            coding_reliability: quality,
            context_reliability: quality,
            estimated_latency_ms: 300,
            ..Default::default()
        };
        let descriptor = faktor_core::model::ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            context: caps.context as u64,
            max_output: caps.max_output as u64,
            tools: caps.tools,
            parallel_tools: caps.parallel_tools,
            reasoning: caps.reasoning,
            thinking: caps.thinking,
            vision: caps.vision,
            structured_output: caps.json_schema,
            embeddings: caps.embeddings,
            streaming: caps.streaming,
            economics,
            source: ModelSource::UserOverride,
        };
        let pricing = PricingState::Known(PricingSnapshot::exact(
            PriceQuote {
                input: MicroUsdPerMillionTokens(price),
                output: MicroUsdPerMillionTokens(price),
                cache_read: MicroUsdPerMillionTokens(0),
                cache_write: MicroUsdPerMillionTokens(0),
            },
            1,
            "test".into(),
        ));
        // A DECLARED prior (UserConfigured authority) on every dimension:
        // the row is authorized by its owner, not by a ConservativeUnknown
        // placeholder.
        let authority = QualityAuthority::UserConfigured {
            source: format!("providers.{provider}.quality"),
            version: "faktor-user-quality-v1".into(),
        };
        let statement = QualityStatement {
            tool: QualityMetric::new(quality, authority.clone()),
            reasoning: QualityMetric::new(quality, authority.clone()),
            coding: QualityMetric::new(quality, authority.clone()),
            context: QualityMetric::new(quality, authority),
        };
        RouteCandidate::with_quality(descriptor, pricing, statement)
    }

    #[test]
    fn compact_adaptive_band_prefers_the_target_then_relaxes_and_cannot_certify() {
        // A strong-but-expensive 90 model and a cheap declared-50 model.
        let strong = candidate("strong", "big", 90, 30_000_000);
        let cheap = candidate("weak", "small", 50, 100_000);
        let both = Arc::new(RouterService::with_route_candidates(
            vec![strong, cheap.clone()],
            Arc::new(EmptyOutcomeStore),
        ));
        let policy = EconomicRoutingPolicy::new(both, RoutingMode::Economy);
        // Hard Implement floor 60: the 50-quality model is refused, so the
        // expensive 90 model serves (the hard floor is unchanged).
        let implement = ModelCallIntent::implement_main().route_request(1_000, 100, 0);
        assert_eq!(policy.route(&implement).unwrap().model, "big");
        let compact = ModelCallIntent::compact();
        assert_eq!(compact.quality_floor(), 50);
        assert_eq!(compact.quality_target(), 60);
        let req = compact.route_request(1_000, 100, 0);
        // Audit item 2: the adaptive request PREFERS its 60 target over the
        // cheaper declared-50 model — the target candidate wins outright.
        assert_eq!(
            policy.route(&req).unwrap().model,
            "big",
            "the target tier must win while a target candidate survives"
        );
        // With ONLY the declared-50 model registered, the adaptive request
        // relaxes into [50, 60): a lower-quality model may summarize.
        let relaxed = Arc::new(RouterService::with_route_candidates(
            vec![cheap],
            Arc::new(EmptyOutcomeStore),
        ));
        let relaxed_policy = EconomicRoutingPolicy::new(relaxed, RoutingMode::Economy);
        assert_eq!(relaxed_policy.route(&req).unwrap().model, "small");
        // ... and the compaction output can never certify completion nor
        // replace immutable task facts/ledger rows.
        assert!(!compact.certifies_completion());
        assert!(!compact.output_trust().may_replace_durable_facts());
        assert_eq!(
            compact.output_trust().provenance_tag(),
            "context_compression"
        );
    }
}

#[cfg(test)]
mod maximum_quality_authority_tests {
    //! P1 (one quality authority): MaximumQuality tier formation must read
    //! the SAME authority-aware effective quality the router's priced
    //! qualification applies. Durable measured outcomes supersede the
    //! descriptor, and a `ConservativeUnknown` placeholder authorizes
    //! nothing above floor 0.
    use crate::*;
    use faktor_core::model::{
        MicroUsdPerMillionTokens, ModelDescriptor, ModelEconomics, ModelSource, PriceQuote,
        PricingSnapshot, PricingState, QualityAuthority, QualityMetric, QualityStatement,
    };
    use faktor_router::{
        MemoryOutcomeStore, OutcomeKey, OutcomeSample, OutcomeStore, RouteCandidate, RouteRequest,
        RouterService,
    };
    use std::sync::Arc;

    fn descriptor(provider: &str, model: &str, coding: u8, context: u8) -> ModelDescriptor {
        ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            context: 128_000,
            max_output: 16_000,
            tools: true,
            parallel_tools: true,
            reasoning: false,
            thinking: false,
            vision: false,
            structured_output: false,
            embeddings: false,
            streaming: true,
            economics: ModelEconomics {
                tool_reliability: coding,
                reasoning_reliability: coding,
                coding_reliability: coding,
                context_reliability: context,
                estimated_latency_ms: 300,
                ..Default::default()
            },
            source: ModelSource::UserOverride,
        }
    }

    fn pricing(price: u64) -> PricingState {
        PricingState::Known(PricingSnapshot::exact(
            PriceQuote {
                input: MicroUsdPerMillionTokens(price),
                output: MicroUsdPerMillionTokens(price),
                cache_read: MicroUsdPerMillionTokens(0),
                cache_write: MicroUsdPerMillionTokens(0),
            },
            1,
            "authority-test".into(),
        ))
    }

    /// A descriptor-derived candidate: its declared quality is the
    /// descriptor's built-in prior (never measured).
    fn builtin_candidate(provider: &str, model: &str, quality: u8, price: u64) -> RouteCandidate {
        RouteCandidate::new(
            descriptor(provider, model, quality, quality),
            pricing(price),
        )
    }

    fn user_metric(value: u8) -> QualityMetric {
        QualityMetric::new(
            value,
            QualityAuthority::UserConfigured {
                source: "providers.declared.quality".into(),
                version: "faktor-user-quality-v1".into(),
            },
        )
    }

    fn all_user(value: u8) -> QualityStatement {
        QualityStatement {
            tool: user_metric(value),
            reasoning: user_metric(value),
            coding: user_metric(value),
            context: user_metric(value),
        }
    }

    fn declared_candidate(
        provider: &str,
        model: &str,
        descriptor_quality: u8,
        declared: QualityStatement,
        price: u64,
    ) -> RouteCandidate {
        RouteCandidate::with_quality(
            descriptor(provider, model, descriptor_quality, descriptor_quality),
            pricing(price),
            declared,
        )
    }

    fn conservative_candidate(
        provider: &str,
        model: &str,
        descriptor_quality: u8,
        price: u64,
    ) -> RouteCandidate {
        let d = descriptor(provider, model, descriptor_quality, descriptor_quality);
        let quality = QualityStatement::conservative_unknown(&d.performance());
        RouteCandidate::with_quality(d, pricing(price), quality)
    }

    fn implement_req(floor: u8) -> RouteRequest {
        RouteRequest {
            phase: RouterPhase::Implement,
            required_capabilities: vec![],
            context_tokens: 1_000,
            estimated_output_tokens: 100,
            quality_floor: floor,
            quality_target: None,
            task_budget_remaining_micro: 0,
            latency_preference_ms: None,
            task_class: TaskClass::Medium,
            risk_bucket: RiskBucket::Low,
        }
    }

    /// Seed verified Implement/Medium/Low outcomes for one (provider, model).
    fn seed(
        store: &MemoryOutcomeStore,
        provider: &str,
        model: &str,
        successes: u64,
        failures: u64,
    ) {
        let key = OutcomeKey {
            provider: provider.into(),
            model: model.into(),
            phase: RouterPhase::Implement,
            task_class: TaskClass::Medium,
            risk_bucket: RiskBucket::Low,
        };
        for _ in 0..successes {
            store.append_sample(
                &key,
                OutcomeSample {
                    verified_success: true,
                    rework_cost_micro: 0,
                    rework_turns: 0,
                },
            );
        }
        for _ in 0..failures {
            store.append_sample(
                &key,
                OutcomeSample {
                    verified_success: false,
                    rework_cost_micro: 0,
                    rework_turns: 0,
                },
            );
        }
    }

    fn service(rows: Vec<RouteCandidate>, store: Arc<MemoryOutcomeStore>) -> Arc<RouterService> {
        Arc::new(RouterService::with_route_candidates(rows, store))
    }

    fn maxq(svc: &Arc<RouterService>) -> Arc<dyn RoutingPolicy> {
        EconomicRoutingPolicy::new(svc.clone(), RoutingMode::MaximumQuality)
    }

    #[test]
    fn maximum_quality_prefers_higher_measured_quality_over_equal_descriptor_priors() {
        // Both rows carry the SAME descriptor prior (80); durable outcomes
        // measure them differently. A descriptor-only tier would put both in
        // one tier and let the cheaper row win; the authority-aware tier puts
        // the higher-measured row alone at the top.
        let store = Arc::new(MemoryOutcomeStore::new());
        seed(&store, "measured-high", "m", 200, 0);
        seed(&store, "measured-low", "m", 100, 0);
        let high = builtin_candidate("measured-high", "m", 80, 5_000_000);
        let low = builtin_candidate("measured-low", "m", 80, 1);
        let svc = service(vec![high, low], store);
        let chosen = maxq(&svc).route(&implement_req(0)).unwrap();
        assert_eq!(
            chosen.provider, "measured-high",
            "MaximumQuality must choose the higher MEASURED quality: {}",
            chosen.reasoning
        );
    }

    #[test]
    fn maximum_quality_reverses_descriptor_order_by_measured_outcomes() {
        // Descriptor-high (95) measured-low (~60) is the CHEAPER row; a
        // descriptor tier would probe both at the low row's tier and let the
        // cheap descriptor-high row win. Authority-aware tiers put the
        // descriptor-low/high-measured row alone at the top.
        let store = Arc::new(MemoryOutcomeStore::new());
        seed(&store, "descriptor-high", "m", 70, 30);
        seed(&store, "descriptor-low", "m", 90, 15);
        let descriptor_high = builtin_candidate("descriptor-high", "m", 95, 1);
        let descriptor_low = builtin_candidate("descriptor-low", "m", 50, 5_000_000);
        let svc = service(vec![descriptor_high, descriptor_low], store);
        let chosen = maxq(&svc).route(&implement_req(0)).unwrap();
        assert_eq!(
            chosen.provider, "descriptor-low",
            "measured outcomes must reverse the descriptor ordering: {}",
            chosen.reasoning
        );
    }

    #[test]
    fn maximum_quality_upgrades_conservative_unknown_with_measured_outcomes() {
        let store = Arc::new(MemoryOutcomeStore::new());
        seed(&store, "unknown", "m", 200, 0);
        let req = implement_req(50);
        let upgraded = service(
            vec![conservative_candidate("unknown", "m", 50, 1_000_000)],
            store,
        );
        let chosen = maxq(&upgraded).route(&req).unwrap();
        assert_eq!(
            chosen.provider, "unknown",
            "strong measured outcomes must authorize a ConservativeUnknown row: {}",
            chosen.reasoning
        );
        // Without evidence the placeholder authorizes nothing above floor 0,
        // so the identical single-candidate set is refused — proving the
        // selection above rode the measured upgrade, not the placeholder.
        let empty = service(
            vec![conservative_candidate("unknown", "m", 50, 1_000_000)],
            Arc::new(MemoryOutcomeStore::new()),
        );
        assert_eq!(maxq(&empty).route(&req), Err(RouteFailure::NoCapableModel));
    }

    #[test]
    fn maximum_quality_ignores_declared_high_downgraded_by_measured_failures() {
        // The loud row DECLARES 95 but durable verified failures measure it
        // at 0; the quiet row declares 70 but measures ~64. A declared tier
        // (95, then 70) would deny both and wrongly surface NoCapableModel;
        // authority-aware tiers serve the quiet row's measured tier directly.
        let store = Arc::new(MemoryOutcomeStore::new());
        seed(&store, "loud", "m", 0, 100);
        seed(&store, "quiet", "m", 60, 20);
        let loud = declared_candidate("loud", "m", 95, all_user(95), 1);
        let quiet = declared_candidate("quiet", "m", 70, all_user(70), 5_000_000);
        let svc = service(vec![loud, quiet], store);
        let chosen = maxq(&svc).route(&implement_req(0)).unwrap();
        assert_eq!(
            chosen.provider, "quiet",
            "a declared-high but measured-failed row must not force a pointless high tier: {}",
            chosen.reasoning
        );
    }

    #[test]
    fn maximum_quality_exact_quality_ties_resolve_economically() {
        let store = Arc::new(MemoryOutcomeStore::new());
        seed(&store, "cheap", "m", 200, 0);
        seed(&store, "pricey", "m", 200, 0);
        let cheap = builtin_candidate("cheap", "m", 80, 1_000);
        let pricey = builtin_candidate("pricey", "m", 80, 50_000_000);
        let svc = service(vec![cheap.clone(), pricey.clone()], store);
        let req = implement_req(0);
        let now = faktor_core::model::unix_now_ms();
        let q_cheap = faktor_router::phase_quality_value(
            &svc.effective_quality_for(&cheap, &req, now),
            req.phase,
        );
        let q_pricey = faktor_router::phase_quality_value(
            &svc.effective_quality_for(&pricey, &req, now),
            req.phase,
        );
        assert_eq!(
            q_cheap, q_pricey,
            "the authority-aware phase qualities must tie exactly"
        );
        let chosen = maxq(&svc).route(&req).unwrap();
        assert_eq!(
            chosen.provider, "cheap",
            "exact authoritative-quality ties must be resolved economically: {}",
            chosen.reasoning
        );
    }
}

/// Route-tier authority (economic audit item 7 P1): every PRODUCTION decision
/// leaves the routing policy with a durable execution class derived at
/// decision time — `Local` from an authoritative local-zero route, the
/// policy tier's hosted class (`hosted_economy` / `hosted_balanced` /
/// `frontier`) from a genuinely priced hosted call, and `Unknown` wherever no
/// authority establishes one. This is the writer that makes hosted tiers
/// non-zero in production; before it, only `LocalZero` ever classified.
#[cfg(test)]
mod route_tier_authority_tests {
    use crate::*;
    use faktor_core::model::{
        ExecutionClass, MicroUsdPerMillionTokens, ModelDescriptor, ModelSource, PriceQuote,
        PricingSnapshot, PricingState, QualityAuthority, QualityMetric, QualityStatement,
    };
    use faktor_router::{EmptyOutcomeStore, RouteCandidate, RouterService};

    fn candidate(
        provider: &str,
        model: &str,
        quality: u8,
        pricing: PricingState,
    ) -> RouteCandidate {
        let descriptor = ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            context: 128_000,
            max_output: 16_000,
            tools: true,
            parallel_tools: false,
            reasoning: false,
            thinking: false,
            vision: false,
            structured_output: false,
            embeddings: false,
            streaming: true,
            economics: faktor_core::model::ModelEconomics {
                tool_reliability: quality,
                reasoning_reliability: quality,
                coding_reliability: quality,
                context_reliability: quality,
                estimated_latency_ms: 300,
                ..Default::default()
            },
            source: ModelSource::UserOverride,
        };
        let authority = QualityAuthority::UserConfigured {
            source: format!("providers.{provider}.quality"),
            version: "faktor-user-quality-v1".into(),
        };
        let statement = QualityStatement {
            tool: QualityMetric::new(quality, authority.clone()),
            reasoning: QualityMetric::new(quality, authority.clone()),
            coding: QualityMetric::new(quality, authority.clone()),
            context: QualityMetric::new(quality, authority),
        };
        RouteCandidate::with_quality(descriptor, pricing, statement)
    }

    fn exact(price: u64) -> PricingState {
        PricingState::Known(PricingSnapshot::exact(
            PriceQuote {
                input: MicroUsdPerMillionTokens(price),
                output: MicroUsdPerMillionTokens(price),
                cache_read: MicroUsdPerMillionTokens(0),
                cache_write: MicroUsdPerMillionTokens(0),
            },
            1,
            "test".into(),
        ))
    }

    fn priced_policy(mode: RoutingMode) -> Arc<EconomicRoutingPolicy> {
        EconomicRoutingPolicy::new(
            Arc::new(RouterService::with_route_candidates(
                vec![candidate("hosted", "m", 90, exact(1_000_000))],
                Arc::new(EmptyOutcomeStore),
            )),
            mode,
        )
    }

    fn routed_class(policy: &EconomicRoutingPolicy) -> ExecutionClass {
        let req = ModelCallIntent::implement_main().route_request(1_000, 100, 0);
        policy
            .route(&req)
            .expect("a priced hosted route serves")
            .execution_class
    }

    #[test]
    fn economy_balanced_and_frontier_policies_classify_hosted_calls_at_route_time() {
        assert_eq!(
            routed_class(&priced_policy(RoutingMode::Economy)),
            ExecutionClass::HostedEconomy
        );
        assert_eq!(
            routed_class(&priced_policy(RoutingMode::Balanced)),
            ExecutionClass::HostedBalanced
        );
        assert_eq!(
            routed_class(&priced_policy(RoutingMode::MaximumQuality)),
            ExecutionClass::Frontier,
            "MaximumQuality IS the frontier policy tier"
        );
    }

    #[test]
    fn a_local_zero_route_is_local_under_every_policy_and_pins_establish_no_tier() {
        for mode in [
            RoutingMode::Economy,
            RoutingMode::Balanced,
            RoutingMode::MaximumQuality,
        ] {
            let policy = EconomicRoutingPolicy::new(
                Arc::new(RouterService::with_route_candidates(
                    vec![candidate("ollama", "local", 90, PricingState::LocalZero)],
                    Arc::new(EmptyOutcomeStore),
                )),
                mode.clone(),
            );
            assert_eq!(
                routed_class(&policy),
                ExecutionClass::Local,
                "{mode:?} must never override LocalZero locality"
            );
        }
        // A pin validates the explicit model and establishes NO tier: a
        // genuinely priced hosted pin stays `Unknown` (never a guessed
        // tier), while a local-zero pin is still classed local.
        let pinned_hosted = EconomicRoutingPolicy::new(
            Arc::new(RouterService::with_pinned_route_candidates(
                vec![candidate("hosted", "m", 90, exact(1_000_000))],
                "hosted",
                "m",
                Arc::new(EmptyOutcomeStore),
            )),
            RoutingMode::Pinned {
                provider: "hosted".into(),
                model: "m".into(),
            },
        );
        assert_eq!(routed_class(&pinned_hosted), ExecutionClass::Unknown);
        let pinned_local = EconomicRoutingPolicy::new(
            Arc::new(RouterService::with_pinned_route_candidates(
                vec![candidate("ollama", "local", 90, PricingState::LocalZero)],
                "ollama",
                "local",
                Arc::new(EmptyOutcomeStore),
            )),
            RoutingMode::Pinned {
                provider: "ollama".into(),
                model: "local".into(),
            },
        );
        assert_eq!(routed_class(&pinned_local), ExecutionClass::Local);
    }

    #[test]
    fn an_unpriced_descriptor_only_route_never_fabricates_a_tier() {
        // Descriptor-only services consulted no pricing authority: even under
        // the frontier policy the class must stay Unknown.
        let descriptor = ModelDescriptor {
            provider: "p".into(),
            model: "m".into(),
            context: 128_000,
            max_output: 16_000,
            tools: true,
            parallel_tools: false,
            reasoning: false,
            thinking: false,
            vision: false,
            structured_output: false,
            embeddings: false,
            streaming: true,
            economics: faktor_core::model::ModelEconomics {
                tool_reliability: 90,
                reasoning_reliability: 90,
                coding_reliability: 90,
                context_reliability: 90,
                estimated_latency_ms: 300,
                ..Default::default()
            },
            source: ModelSource::ConservativeDefault,
        };
        let policy = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![descriptor])),
            RoutingMode::MaximumQuality,
        );
        assert_eq!(
            routed_class(&policy),
            ExecutionClass::Unknown,
            "no pricing authority -> no fabricated frontier class"
        );
    }
}
