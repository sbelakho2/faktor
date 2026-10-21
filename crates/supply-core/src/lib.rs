//! faktor-supply-core — pure, deterministic supply-domain foundations.
//!
//! This crate holds the vocabulary every Faktor Supply crate shares and
//! nothing else: exact integer money/quantities, evidence-bound terms with
//! typed unknowns, revisions, assumptions, and the typed revisioned supply
//! graph. It performs no I/O, has no model, no network and no clock.
//!
//! Invariants:
//!
//! - no floating point in money or quantity paths;
//! - no known value without evidence or an explicit assumption
//!   ([`basis::Term`] refuses it);
//! - unknown is [`basis::Availability::Unavailable`] with a typed reason, never
//!   zero;
//! - supplier roles (manufacturer, authorized distributor, independent
//!   distributor, broker, marketplace seller) are distinct and never merged;
//! - every graph, list and text field has a hard bound.

pub mod basis;
pub mod decision;
pub mod distribution;
pub mod graph;
pub mod money;

pub use basis::{
    validate_text, Assumption, Availability, Basis, Conflict, Day, EvidenceRef, Provenance,
    Revision, SourceKind, Term, Unavailable, UnavailableReason, UncertaintyClass, UncertaintyNote,
    MAX_ASSUMPTIONS, MAX_CONDITIONS, MAX_ID_BYTES, MAX_TEXT_BYTES,
};
pub use decision::{
    unavailable, Candidate, CandidateId, CapabilitySummary, CommercialTermsSummary, Counterfactual,
    FitAssessment, InventorySummary, LandedCostSummary, LogisticsSummary, RecommendedAction,
    RiskSummary, SupplyDecision, SupplyRequirement, MAX_CANDIDATES, MAX_CONFLICTS,
    MAX_COUNTERFACTUALS, MAX_DECISION_ASSUMPTIONS, MAX_DECISION_EVIDENCE, MAX_MISSING_ITEMS,
};
pub use distribution::{Distribution, Quantile, MAX_SAMPLE_SIZE};
pub use graph::{
    AuthorizationStatus, ContractId, ContractTerms, Edge, EdgeKind, GraphError, Incoterm,
    LifecycleStatus, Manufacturer, ManufacturerId, Node, NodeRef, Part, PartId, Pcn, PcnId,
    PcnKind, Process, ProcessId, ProcessKind, Qualification, QualificationId, QualificationKind,
    QualificationStatus, Risk, RiskId, RiskKind, RiskLevel, Shipment, ShipmentId, Site, SiteId,
    Supplier, SupplierId, SupplierRole, SupplyGraph, TransportMode, MAX_GRAPH_EDGES,
    MAX_GRAPH_NODES, MAX_NODE_EVIDENCE, MAX_TRAVERSAL_DEPTH,
};
pub use money::{
    div_round_half_away, div_round_up, isqrt, isqrt_scaled_ppm, mul_div_round_half_up,
    mul_div_round_up, Currency, ExactError, Money, Rate, ScaledQuantity, Units, MAX_MONEY_MICROS,
    MAX_PPM, MAX_SCALED_QUANTITY, MAX_UNITS, MICROS_PER_UNIT,
};
