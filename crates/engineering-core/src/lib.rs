//! Faktor Engineering core substrate.
//!
//! One revision-bound Engineering Design Graph, conditional
//! [`EngineeringValue`] facts with explicit provenance/applicability/
//! uncertainty, executable [`AnalysisReceipt`]s, and the 11-gate response
//! contract. Unknowns are typed [`ValueState::Unavailable`] values; gates 5
//! and 6 can only be established by machine-checked dimensions and executed
//! receipts, never by prose.

pub mod digest;
pub mod error;
pub mod gate;
pub mod graph;
pub mod receipt;
pub mod requirements;
pub mod value;

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
pub use value::{
    quantity_in, Applicability, EngineeringValue, MaterialRef, Orientation, Provenance,
    ProvenanceKind, RevisionId, ThicknessRange, UnavailableValue, ValueState,
};
