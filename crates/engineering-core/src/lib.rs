//! Faktor Engineering core substrate.
//!
//! One revision-bound Engineering Design Graph, conditional
//! [`EngineeringValue`] facts with explicit provenance/applicability/
//! uncertainty, executable [`AnalysisReceipt`]s, and the 11-gate response
//! contract. Unknowns are typed [`ValueState::Unavailable`] values; gates 5
//! and 6 can only be established by machine-checked dimensions and executed
//! receipts, never by prose.

pub mod attestation;
pub mod digest;
pub mod error;
pub mod gate;
pub mod graph;
pub mod pipeline;
pub mod receipt;
pub mod requirements;
pub mod selector;
pub mod value;

pub use attestation::{
    key_history_path, load_key_history, load_or_create_signing_key, load_signing_key,
    load_trusted_verifier, rotate_signing_key, signing_key_path, KeyHistory, KeyHistoryEntry,
    KeyValidity, ReceiptAttestation, ReceiptSigner, ReceiptVerifier, ATTESTATION_ALGORITHM,
    ENGINEERING_KEY_HISTORY_FILE, ENGINEERING_SIGNING_KEY_FILE, KEY_HISTORY_VERSION,
    MAX_KEY_HISTORY_BYTES, MAX_TRUSTED_KEYS,
};
pub use digest::Digest;
pub use error::{CoreError, CoreResult};
pub use gate::{
    research_response, Agreement, AssumptionRecord, AuthorityRef, CounterexampleProbe,
    CounterexampleSearch, CrossCheck, DimensionCheck, DimensionChecks, EngineeringResponse,
    ExecutedAnalysis, FalsifiableTest, FalsifiableVerification, Formulation, Gate, GateGap, GateId,
    GateState, GateViolation, KnowledgeAuthority, ModelSelection, ProbeOutcome,
    RealizationCategory, RealizationConsequence, RealizationConsequences, ResearchRequest,
    ResponseAssessment, TheorySelection, UncertaintyContribution, UncertaintyStatement,
    ValidityEnvelope, ValidityEnvelopes,
};
pub use graph::{
    DesignEdge, DesignGraph, DesignNode, EdgeId, EdgeKind, EdgeType, EngineeringDesignGraph,
    NodeId, NodeKind, NodeType, DEFAULT_MAX_EDGES, DEFAULT_MAX_NODES,
};
pub use pipeline::{
    run as run_engineering_pipeline, KernelCrossCheck, KernelProbe, KernelRefusal, KernelRejection,
    KernelReport, KernelStatus, PipelineBlocker, PipelineOutcome, PipelineRequest, PipelineRun,
    MAX_PIPELINE_PROBES,
};
pub use receipt::{
    AnalysisIdentity, AnalysisKind, AnalysisReceipt, AnalysisWarning, ArtifactRef, Convergence,
    ExecutionEvidence, PropertyRevision, ReceiptContent, ReceiptResult, ReceiptValue,
    Repeatability, SupervisedProcessEvidence, WarningSeverity,
};
pub use requirements::{
    evaluate as evaluate_requirements, si_crosscheck_verdict, CandidateEvaluation, CandidateFacts,
    CategoricalFact, DerivedTest, Discipline, EngineeringConstraint, EngineeringRequirement,
    EngineeringRequirementSet, EngineeringVerdict, ModelBinding, NumericLimit, OutcomeState,
    RequirementEvidence, RequirementOutcome, RequirementTarget, TestKind, MAX_CANDIDATE_CATEGORIES,
    MAX_CANDIDATE_FACTS, MAX_DERIVED_TESTS, MAX_REQUIREMENTS, MAX_REQUIREMENT_CONSTRAINTS,
    MAX_REQUIREMENT_EVIDENCE, MAX_REQUIREMENT_MODELS,
};
pub use selector::{
    ApplicabilitySelection, CatalogModel, ModelCatalog, ModelSelector, ModelValidityOverride,
    RequirementSelection, SelectedModel, SelectionProvenance, SelectionRequest, ValidityDomain,
    MAX_CATALOG_MODELS, MAX_SELECTION_REQUESTS, MAX_VALIDITY_DOMAINS,
};
pub use value::{
    quantity_in, Applicability, EngineeringValue, MaterialRef, Orientation, Provenance,
    ProvenanceKind, RevisionId, ThicknessRange, UnavailableValue, ValueState,
};
