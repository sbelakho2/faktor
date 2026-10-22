//! The typed, revisioned supply graph.
//!
//! Nodes are strongly typed (a [`PartId`] can never be passed where a
//! [`SupplierId`] is required) and every node kind is distinct. Suppliers
//! carry their commercial role explicitly — manufacturer, authorized
//! distributor, independent distributor, broker and marketplace seller are
//! never merged. The graph is bounded, deterministic and adjacency is indexed
//! for `O(V + E)` traversal.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::basis::{validate_text, EvidenceRef, Revision, MAX_ID_BYTES, MAX_TEXT_BYTES};
use crate::money::{Currency, Units};
use crate::ExactError;

/// Maximum nodes in one supply graph.
pub const MAX_GRAPH_NODES: usize = 100_000;
/// Maximum edges in one supply graph.
pub const MAX_GRAPH_EDGES: usize = 500_000;
/// Maximum evidence references carried by one node.
pub const MAX_NODE_EVIDENCE: usize = 32;
/// Maximum bounded facts carried by one node.
pub const MAX_NODE_FACTS: usize = 32;
/// Maximum traversal depth.
pub const MAX_TRAVERSAL_DEPTH: usize = 64;

macro_rules! node_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(JsonSchema,
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            Serialize, Deserialize, Default,
        )]
        #[serde(transparent)]
        pub struct $name(pub u32);

        impl $name {
            /// Construct an identifier.
            pub const fn new(value: u32) -> Self {
                Self(value)
            }
            /// The raw identifier.
            pub const fn get(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}:{}", stringify!($name), self.0)
            }
        }
    };
}

node_id!(
    /// A purchase part / item.
    PartId
);
node_id!(
    /// A device manufacturer (the party that makes the part).
    ManufacturerId
);
node_id!(
    /// A commercial supplier of any role.
    SupplierId
);
node_id!(
    /// A physical site.
    SiteId
);
node_id!(
    /// A shipment or logistics movement.
    ShipmentId
);
node_id!(
    /// A manufacturing or inspection process.
    ProcessId
);
node_id!(
    /// A qualification record.
    QualificationId
);
node_id!(
    /// A product change notification or end-of-life notice.
    PcnId
);
node_id!(
    /// A contract-terms record.
    ContractId
);
node_id!(
    /// A supply risk record.
    RiskId
);

/// The role a supplier plays. These are never merged: a broker is not an
/// authorized distributor and a marketplace listing is not a manufacturer.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupplierRole {
    /// The party that manufactures the part.
    Manufacturer,
    /// A distributor explicitly authorized by the manufacturer.
    AuthorizedDistributor,
    /// An independent distributor with no manufacturer authorization.
    IndependentDistributor,
    /// A broker arranging transactions without taking title.
    Broker,
    /// A marketplace seller listing on a third-party platform.
    MarketplaceSeller,
}

impl SupplierRole {
    /// True only for the actual manufacturer.
    pub const fn is_manufacturer(self) -> bool {
        matches!(self, Self::Manufacturer)
    }

    /// True for roles whose chain of custody is manufacturer-authorized.
    pub const fn authorization_expected(self) -> bool {
        matches!(self, Self::AuthorizedDistributor)
    }

    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manufacturer => "manufacturer",
            Self::AuthorizedDistributor => "authorized_distributor",
            Self::IndependentDistributor => "independent_distributor",
            Self::Broker => "broker",
            Self::MarketplaceSeller => "marketplace_seller",
        }
    }

    /// Relative traceability risk of the role, never a substitute for
    /// evidence. `0` means authorization is expected and can be verified.
    pub const fn traceability_rank(self) -> u8 {
        match self {
            Self::Manufacturer | Self::AuthorizedDistributor => 0,
            Self::IndependentDistributor => 1,
            Self::MarketplaceSeller => 2,
            Self::Broker => 3,
        }
    }
}

/// Explicit authorization status. `Unknown` is not "authorized".
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationStatus {
    /// Authorization is evidenced.
    Authorized,
    /// The supplier is explicitly not authorized.
    NotAuthorized,
    /// No authorization evidence exists.
    Unknown,
}

/// Incoterms 2020 vocabulary. Operational semantics (who pays which leg and
/// where risk transfers) live in `faktor-supply-logistics`.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Incoterm {
    /// Ex Works.
    Exw,
    /// Free Carrier.
    Fca,
    /// Free Alongside Ship.
    Fas,
    /// Free On Board.
    Fob,
    /// Cost and Freight.
    Cfr,
    /// Cost, Insurance and Freight.
    Cif,
    /// Carriage Paid To.
    Cpt,
    /// Carriage and Insurance Paid To.
    Cip,
    /// Delivered at Place.
    Dap,
    /// Delivered at Place Unloaded.
    Dpu,
    /// Delivered Duty Paid.
    Ddp,
}

impl Incoterm {
    /// The four-letter code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Exw => "EXW",
            Self::Fca => "FCA",
            Self::Fas => "FAS",
            Self::Fob => "FOB",
            Self::Cfr => "CFR",
            Self::Cif => "CIF",
            Self::Cpt => "CPT",
            Self::Cip => "CIP",
            Self::Dap => "DAP",
            Self::Dpu => "DPU",
            Self::Ddp => "DDP",
        }
    }

    /// True for sea/inland-water terms where risk transfers alongside the ship.
    pub const fn is_maritime(self) -> bool {
        matches!(self, Self::Fas | Self::Fob | Self::Cfr | Self::Cif)
    }
}

/// Transportation mode.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TransportMode {
    /// Air freight.
    Air,
    /// Full container ocean freight.
    OceanFcl,
    /// Less-than-container ocean freight.
    OceanLcl,
    /// Rail freight.
    Rail,
    /// Road freight.
    Road,
    /// Express courier.
    Courier,
}

/// Part lifecycle status.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    /// Actively produced.
    Active,
    /// Production ending, last-time-buy window may exist.
    LastTimeBuy,
    /// Not recommended for new designs.
    NotRecommendedForNewDesign,
    /// End of life.
    EndOfLife,
    /// Obsolete.
    Obsolete,
}

impl LifecycleStatus {
    /// True when new designs should not use the part.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::EndOfLife | Self::Obsolete)
    }
}

/// Process class.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessKind {
    /// Machining (milling, turning, grinding).
    Machining,
    /// Surface-mount assembly.
    Smt,
    /// Soldering (reflow, wave, hand).
    Soldering,
    /// Electrical or functional test.
    Test,
    /// Inspection / metrology.
    Inspection,
    /// Conformal coating or potting.
    Coating,
    /// Mechanical assembly.
    Assembly,
    /// Additive manufacturing.
    Additive,
    /// Injection molding.
    InjectionMolding,
    /// Other declared process.
    Other,
}

/// Qualification vocabulary attached to a graph node.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationKind {
    /// Production Part Approval Process.
    Ppap,
    /// Advanced Product Quality Planning.
    Apqp,
    /// First Article Inspection.
    Fai,
    /// Supplier quality audit.
    SupplierAudit,
    /// Measurement system analysis.
    Msa,
    /// Process capability submission.
    ProcessCapability,
    /// Traceability / chain of custody.
    Traceability,
    /// Counterfeit-avoidance / authenticity.
    Authenticity,
    /// Material or substance declaration.
    MaterialDeclaration,
}

/// The status of a qualification record.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationStatus {
    /// Not started.
    NotStarted,
    /// Evidence is being collected.
    EvidencePending,
    /// Approved by an evidenced authority.
    Approved,
    /// Approval expired.
    Expired,
    /// Suspended pending corrective action.
    Suspended,
    /// Revoked.
    Revoked,
}

/// PCN / lifecycle notice class.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PcnKind {
    /// A process/material/design change notification.
    ProductChange,
    /// End-of-life notice.
    EndOfLife,
    /// Last-time-buy notice.
    LastTimeBuy,
    /// Discontinuation without replacement.
    Discontinuation,
}

/// Risk class in the supply graph.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskKind {
    /// Single-source / continuity exposure.
    SingleSource,
    /// Financial exposure.
    Financial,
    /// Quality escape exposure.
    Quality,
    /// Lead-time / delivery exposure.
    LeadTime,
    /// Geopolitical / trade exposure.
    Trade,
    /// Counterfeit exposure.
    Counterfeit,
    /// Obsolescence exposure.
    Obsolescence,
    /// Capacity exposure.
    Capacity,
}

/// Ordered qualitative level.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Critical.
    Critical,
}

/// A part node.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    /// Identifier.
    pub id: PartId,
    /// Manufacturer part number.
    pub mpn: String,
    /// Bounded description.
    pub description: String,
    /// Lifecycle status.
    pub lifecycle: LifecycleStatus,
    /// Revision of this record.
    pub revision: Revision,
}

/// A manufacturer node.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manufacturer {
    /// Identifier.
    pub id: ManufacturerId,
    /// Name.
    pub name: String,
    /// ISO country code, when known.
    pub country: Option<String>,
    /// Revision of this record.
    pub revision: Revision,
}

/// A supplier node. The role is mandatory and cannot be inferred.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Supplier {
    /// Identifier.
    pub id: SupplierId,
    /// Name.
    pub name: String,
    /// Commercial role.
    pub role: SupplierRole,
    /// Explicit authorization status.
    pub authorization: AuthorizationStatus,
    /// Evidence supporting the profile.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Revision of this record.
    pub revision: Revision,
}

/// A physical site.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    /// Identifier.
    pub id: SiteId,
    /// Operating supplier.
    pub supplier: SupplierId,
    /// ISO country code.
    pub country: String,
    /// Region / city.
    pub region: String,
    /// Revision of this record.
    pub revision: Revision,
}

/// A shipment record.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shipment {
    /// Identifier.
    pub id: ShipmentId,
    /// Transport mode.
    pub mode: TransportMode,
    /// Origin description.
    pub origin: String,
    /// Destination description.
    pub destination: String,
    /// The governing contract, when known.
    pub contract: Option<ContractId>,
    /// Revision of this record.
    pub revision: Revision,
}

/// A process node.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    /// Identifier.
    pub id: ProcessId,
    /// Process name.
    pub name: String,
    /// Process class.
    pub kind: ProcessKind,
    /// Bounded capability statement (equipment class, envelope).
    pub capability: String,
    /// Revision of this record.
    pub revision: Revision,
}

/// A qualification record node.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    /// Identifier.
    pub id: QualificationId,
    /// The qualified subject.
    pub subject: NodeRef,
    /// Qualification class.
    pub kind: QualificationKind,
    /// Current status.
    pub status: QualificationStatus,
    /// Validity end day.
    pub valid_until: Option<crate::basis::Day>,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Revision of this record.
    pub revision: Revision,
}

/// A PCN / EOL record node.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pcn {
    /// Identifier.
    pub id: PcnId,
    /// Affected part.
    pub part: PartId,
    /// Notice class.
    pub kind: PcnKind,
    /// Effective day.
    pub effective_on: crate::basis::Day,
    /// Last order day, when stated.
    pub last_order_on: Option<crate::basis::Day>,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Revision of this record.
    pub revision: Revision,
}

/// Contract terms node (commercial vocabulary; logistics semantics live in
/// the logistics crate).
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractTerms {
    /// Identifier.
    pub id: ContractId,
    /// Incoterm.
    pub incoterm: Incoterm,
    /// Payment days (net).
    pub payment_days: u32,
    /// Warranty days.
    pub warranty_days: u32,
    /// Non-cancelable / non-returnable.
    pub ncnr: bool,
    /// Currency.
    pub currency: Currency,
    /// Minimum order quantity, when contracted.
    pub moq: Option<Units>,
    /// Price validity end day.
    pub price_valid_until: Option<crate::basis::Day>,
    /// Revision of this record.
    pub revision: Revision,
}

/// A risk node.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Risk {
    /// Identifier.
    pub id: RiskId,
    /// Risk class.
    pub kind: RiskKind,
    /// Severity.
    pub severity: RiskLevel,
    /// Probability per delivery/period, when evidenced.
    pub probability: crate::basis::Availability<crate::money::Rate>,
    /// Mitigation statement.
    pub mitigation: String,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Revision of this record.
    pub revision: Revision,
}

/// A typed graph node.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    /// Part.
    Part(Part),
    /// Manufacturer.
    Manufacturer(Manufacturer),
    /// Supplier.
    Supplier(Supplier),
    /// Site.
    Site(Site),
    /// Shipment.
    Shipment(Shipment),
    /// Process.
    Process(Process),
    /// Qualification.
    Qualification(Qualification),
    /// PCN / EOL.
    Pcn(Pcn),
    /// Contract terms.
    Contract(ContractTerms),
    /// Risk.
    Risk(Risk),
}

impl Node {
    /// The typed key of this node.
    pub fn key(&self) -> NodeRef {
        match self {
            Self::Part(node) => NodeRef::Part(node.id),
            Self::Manufacturer(node) => NodeRef::Manufacturer(node.id),
            Self::Supplier(node) => NodeRef::Supplier(node.id),
            Self::Site(node) => NodeRef::Site(node.id),
            Self::Shipment(node) => NodeRef::Shipment(node.id),
            Self::Process(node) => NodeRef::Process(node.id),
            Self::Qualification(node) => NodeRef::Qualification(node.id),
            Self::Pcn(node) => NodeRef::Pcn(node.id),
            Self::Contract(node) => NodeRef::Contract(node.id),
            Self::Risk(node) => NodeRef::Risk(node.id),
        }
    }

    /// Validate bounded text and evidence lists.
    pub fn validate(&self) -> Result<(), ExactError> {
        match self {
            Self::Part(node) => {
                validate_text(&node.mpn, MAX_ID_BYTES, "part mpn")?;
                validate_text(&node.description, MAX_TEXT_BYTES, "part description")
            }
            Self::Manufacturer(node) => {
                validate_text(&node.name, MAX_TEXT_BYTES, "manufacturer name")?;
                if let Some(country) = &node.country {
                    validate_text(country, MAX_ID_BYTES, "manufacturer country")?;
                }
                Ok(())
            }
            Self::Supplier(node) => {
                validate_text(&node.name, MAX_TEXT_BYTES, "supplier name")?;
                validate_evidence(&node.evidence)
            }
            Self::Site(node) => {
                validate_text(&node.country, MAX_ID_BYTES, "site country")?;
                validate_text(&node.region, MAX_TEXT_BYTES, "site region")
            }
            Self::Shipment(node) => {
                validate_text(&node.origin, MAX_TEXT_BYTES, "shipment origin")?;
                validate_text(&node.destination, MAX_TEXT_BYTES, "shipment destination")
            }
            Self::Process(node) => {
                validate_text(&node.name, MAX_TEXT_BYTES, "process name")?;
                validate_text(&node.capability, MAX_TEXT_BYTES, "process capability")
            }
            Self::Qualification(node) => validate_evidence(&node.evidence),
            Self::Pcn(node) => validate_evidence(&node.evidence),
            Self::Contract(_) => Ok(()),
            Self::Risk(node) => {
                validate_text(&node.mitigation, MAX_TEXT_BYTES, "risk mitigation")?;
                validate_evidence(&node.evidence)
            }
        }
    }
}

fn validate_evidence(evidence: &[EvidenceRef]) -> Result<(), ExactError> {
    if evidence.len() > MAX_NODE_EVIDENCE {
        return Err(ExactError::OutOfBounds {
            what: "node evidence",
            detail: format!("{} entries", evidence.len()),
        });
    }
    for reference in evidence {
        reference.validate()?;
    }
    Ok(())
}

/// A typed node reference. Variants cannot be confused.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum NodeRef {
    /// Part.
    Part(PartId),
    /// Manufacturer.
    Manufacturer(ManufacturerId),
    /// Supplier.
    Supplier(SupplierId),
    /// Site.
    Site(SiteId),
    /// Shipment.
    Shipment(ShipmentId),
    /// Process.
    Process(ProcessId),
    /// Qualification.
    Qualification(QualificationId),
    /// PCN.
    Pcn(PcnId),
    /// Contract.
    Contract(ContractId),
    /// Risk.
    Risk(RiskId),
}

impl NodeRef {
    /// The node-class label.
    pub const fn kind_label(self) -> &'static str {
        match self {
            Self::Part(_) => "part",
            Self::Manufacturer(_) => "manufacturer",
            Self::Supplier(_) => "supplier",
            Self::Site(_) => "site",
            Self::Shipment(_) => "shipment",
            Self::Process(_) => "process",
            Self::Qualification(_) => "qualification",
            Self::Pcn(_) => "pcn",
            Self::Contract(_) => "contract",
            Self::Risk(_) => "risk",
        }
    }
}

impl fmt::Display for NodeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Part(id) => write!(f, "{id}"),
            Self::Manufacturer(id) => write!(f, "{id}"),
            Self::Supplier(id) => write!(f, "{id}"),
            Self::Site(id) => write!(f, "{id}"),
            Self::Shipment(id) => write!(f, "{id}"),
            Self::Process(id) => write!(f, "{id}"),
            Self::Qualification(id) => write!(f, "{id}"),
            Self::Pcn(id) => write!(f, "{id}"),
            Self::Contract(id) => write!(f, "{id}"),
            Self::Risk(id) => write!(f, "{id}"),
        }
    }
}

/// Typed edge class.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Supplier can supply the part.
    Supplies,
    /// Manufacturer makes the part.
    Manufactures,
    /// Supplier operates the site.
    Operates,
    /// Shipment moves goods under a contract.
    GovernedBy,
    /// Part requires the process.
    RequiresProcess,
    /// Qualification applies to the subject.
    Qualifies,
    /// PCN affects the part.
    AffectsPart,
    /// Risk exposes the subject (supplier, site, part or shipment).
    Exposes,
    /// Supplier is associated with the site.
    LocatedAt,
}

/// A typed edge.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    /// Source node.
    pub from: NodeRef,
    /// Target node.
    pub to: NodeRef,
    /// Edge class.
    pub kind: EdgeKind,
}

/// A rejected graph operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// The graph node bound was reached.
    #[error("graph node bound {max} reached")]
    NodeBound {
        /// The bound.
        max: usize,
    },
    /// The graph edge bound was reached.
    #[error("graph edge bound {max} reached")]
    EdgeBound {
        /// The bound.
        max: usize,
    },
    /// A duplicate node key.
    #[error("duplicate node {0}")]
    DuplicateNode(NodeRef),
    /// A duplicate edge.
    #[error("duplicate edge")]
    DuplicateEdge,
    /// An edge endpoint does not exist.
    #[error("edge endpoint {0} does not exist")]
    MissingEndpoint(NodeRef),
    /// A self edge is not meaningful in this graph.
    #[error("self edges are refused")]
    SelfEdge,
    /// The edge's node kinds are not legal for its class.
    #[error("edge class {kind:?} is not legal from {from} to {to}")]
    IllegalEdge {
        /// Source.
        from: NodeRef,
        /// Target.
        to: NodeRef,
        /// Class.
        kind: EdgeKind,
    },
    /// A node failed validation.
    #[error("invalid node: {0}")]
    InvalidNode(String),
    /// A reference to a missing node.
    #[error("missing node {0}")]
    MissingNode(NodeRef),
    /// A human approval is required for this consequential change.
    #[error(transparent)]
    Approval(#[from] faktor_approval::ApprovalError),
}

/// A bounded, deterministic supply graph.
#[derive(Debug, Clone, Serialize)]
pub struct SupplyGraph {
    /// Revision of the whole graph.
    pub revision: Revision,
    /// Nodes in insertion order (a deterministic serialization).
    pub nodes: Vec<Node>,
    /// Edges in insertion order.
    pub edges: Vec<Edge>,
    #[serde(skip)]
    index: HashMap<NodeRef, usize>,
    #[serde(skip)]
    adjacency: HashMap<NodeRef, Vec<usize>>,
    #[serde(skip)]
    reverse: HashMap<NodeRef, Vec<usize>>,
    #[serde(skip)]
    edge_set: HashSet<Edge>,
}

/// Wire shape for [`SupplyGraph`]. Indexes are derived, never trusted from
/// JSON: a plain `deserialize` must produce a graph whose lookups/traversal
/// work immediately instead of silently returning empty adjacency.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SupplyGraphDef {
    revision: Revision,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl<'de> Deserialize<'de> for SupplyGraph {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let def = SupplyGraphDef::deserialize(deserializer)?;
        let mut graph = Self::new(def.revision);
        graph.nodes = def.nodes;
        graph.edges = def.edges;
        graph.reindex_all();
        Ok(graph)
    }
}

impl SupplyGraph {
    /// An empty graph at a revision.
    pub fn new(revision: Revision) -> Self {
        Self {
            revision,
            nodes: Vec::new(),
            edges: Vec::new(),
            index: HashMap::new(),
            adjacency: HashMap::new(),
            reverse: HashMap::new(),
            edge_set: HashSet::new(),
        }
    }

    /// Number of nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Add a typed node.
    pub fn add_node(&mut self, node: Node) -> Result<NodeRef, GraphError> {
        if self.nodes.len() >= MAX_GRAPH_NODES {
            return Err(GraphError::NodeBound {
                max: MAX_GRAPH_NODES,
            });
        }
        let key = node.key();
        if self.index.contains_key(&key) {
            return Err(GraphError::DuplicateNode(key));
        }
        node.validate()
            .map_err(|error| GraphError::InvalidNode(error.to_string()))?;
        self.index.insert(key, self.nodes.len());
        self.adjacency.insert(key, Vec::new());
        self.nodes.push(node);
        Ok(key)
    }

    /// Add a typed edge, validating endpoint existence and edge legality.
    pub fn add_edge(&mut self, edge: Edge) -> Result<(), GraphError> {
        if self.edges.len() >= MAX_GRAPH_EDGES {
            return Err(GraphError::EdgeBound {
                max: MAX_GRAPH_EDGES,
            });
        }
        if edge.from == edge.to {
            return Err(GraphError::SelfEdge);
        }
        if !self.index.contains_key(&edge.from) {
            return Err(GraphError::MissingEndpoint(edge.from));
        }
        if !self.index.contains_key(&edge.to) {
            return Err(GraphError::MissingEndpoint(edge.to));
        }
        if !edge_kind_allowed(edge.kind, edge.from, edge.to) {
            return Err(GraphError::IllegalEdge {
                from: edge.from,
                to: edge.to,
                kind: edge.kind,
            });
        }
        if !self.edge_set.insert(edge) {
            return Err(GraphError::DuplicateEdge);
        }
        let index = self.edges.len();
        self.edges.push(edge);
        self.adjacency.entry(edge.from).or_default().push(index);
        self.reverse.entry(edge.to).or_default().push(index);
        Ok(())
    }

    /// Look up a node.
    pub fn node(&self, key: NodeRef) -> Option<&Node> {
        self.index.get(&key).map(|index| &self.nodes[*index])
    }

    /// True when the node exists.
    pub fn contains(&self, key: NodeRef) -> bool {
        self.index.contains_key(&key)
    }

    /// Outgoing edges in deterministic target order.
    pub fn outgoing(&self, key: NodeRef) -> Vec<Edge> {
        self.neighbors(key, true)
    }

    /// Incoming edges in deterministic source order.
    pub fn incoming(&self, key: NodeRef) -> Vec<Edge> {
        self.neighbors(key, false)
    }

    fn neighbors(&self, key: NodeRef, outgoing: bool) -> Vec<Edge> {
        let mut edges = if outgoing {
            self.adjacency
                .get(&key)
                .map(|indices| {
                    indices
                        .iter()
                        .map(|index| self.edges[*index])
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            self.reverse
                .get(&key)
                .map(|indices| {
                    indices
                        .iter()
                        .map(|index| self.edges[*index])
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        edges.sort_by(|left, right| {
            let left_key = if outgoing { left.to } else { left.from };
            let right_key = if outgoing { right.to } else { right.from };
            left_key.cmp(&right_key).then(left.kind.cmp(&right.kind))
        });
        edges
    }

    /// Breadth-first traversal, bounded by depth and result size. Neighbours
    /// are visited in deterministic key order. Complexity `O(V + E)`.
    pub fn traverse(&self, start: NodeRef, max_depth: usize) -> Vec<(NodeRef, usize)> {
        let max_depth = max_depth.min(MAX_TRAVERSAL_DEPTH);
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut result = Vec::new();
        if !self.contains(start) {
            return result;
        }
        visited.insert(start);
        queue.push_back((start, 0_usize));
        while let Some((key, depth)) = queue.pop_front() {
            result.push((key, depth));
            if depth >= max_depth {
                continue;
            }
            for edge in self.outgoing(key) {
                if visited.insert(edge.to) {
                    queue.push_back((edge.to, depth + 1));
                }
            }
        }
        result
    }

    /// All suppliers that can supply a part (incoming `Supplies` edges).
    pub fn suppliers_for_part(&self, part: PartId) -> Vec<SupplierId> {
        let mut suppliers = self
            .incoming(NodeRef::Part(part))
            .into_iter()
            .filter(|edge| edge.kind == EdgeKind::Supplies)
            .filter_map(|edge| match edge.from {
                NodeRef::Supplier(id) => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>();
        suppliers.sort_unstable();
        suppliers.dedup();
        suppliers
    }

    /// All parts a supplier can supply (outgoing `Supplies` edges).
    pub fn parts_for_supplier(&self, supplier: SupplierId) -> Vec<PartId> {
        let mut parts = self
            .outgoing(NodeRef::Supplier(supplier))
            .into_iter()
            .filter(|edge| edge.kind == EdgeKind::Supplies)
            .filter_map(|edge| match edge.to {
                NodeRef::Part(id) => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>();
        parts.sort_unstable();
        parts.dedup();
        parts
    }

    /// Validate a deserialized graph structurally: bounds, unique nodes,
    /// endpoint existence, edge legality and duplicate edges. This is
    /// `O(V + E)` and does not require the derived indexes.
    pub fn validate(&self) -> Result<(), GraphError> {
        if self.nodes.len() > MAX_GRAPH_NODES {
            return Err(GraphError::NodeBound {
                max: MAX_GRAPH_NODES,
            });
        }
        if self.edges.len() > MAX_GRAPH_EDGES {
            return Err(GraphError::EdgeBound {
                max: MAX_GRAPH_EDGES,
            });
        }
        let mut nodes = HashSet::new();
        for node in &self.nodes {
            let key = node.key();
            if !nodes.insert(key) {
                return Err(GraphError::DuplicateNode(key));
            }
            node.validate()
                .map_err(|error| GraphError::InvalidNode(error.to_string()))?;
        }
        let mut edges = HashSet::new();
        for edge in &self.edges {
            if !nodes.contains(&edge.from) {
                return Err(GraphError::MissingEndpoint(edge.from));
            }
            if !nodes.contains(&edge.to) {
                return Err(GraphError::MissingEndpoint(edge.to));
            }
            if edge.from == edge.to {
                return Err(GraphError::SelfEdge);
            }
            if !edge_kind_allowed(edge.kind, edge.from, edge.to) {
                return Err(GraphError::IllegalEdge {
                    from: edge.from,
                    to: edge.to,
                    kind: edge.kind,
                });
            }
            if !edges.insert(*edge) {
                return Err(GraphError::DuplicateEdge);
            }
        }
        Ok(())
    }

    /// Rebuild every derived index WITHOUT deduplicating stored edges, so
    /// deserialization is lazy but `validate` can still surface duplicates.
    fn reindex_all(&mut self) {
        self.index.clear();
        self.adjacency.clear();
        self.reverse.clear();
        self.edge_set.clear();
        for (position, node) in self.nodes.iter().enumerate() {
            self.index.insert(node.key(), position);
            self.adjacency.entry(node.key()).or_default();
        }
        for (position, edge) in self.edges.iter().enumerate() {
            self.edge_set.insert(*edge);
            self.adjacency.entry(edge.from).or_default().push(position);
            self.reverse.entry(edge.to).or_default().push(position);
        }
    }

    /// Approve/qualify a supplier qualification record. This is the only
    /// mutator for `QualificationStatus`; the human token must bind to the
    /// current record revision and the exact status change.
    pub fn set_qualification_status(
        &mut self,
        id: QualificationId,
        status: QualificationStatus,
        approval: Option<&faktor_approval::ApprovalToken>,
    ) -> Result<(), GraphError> {
        let position = self
            .index
            .get(&NodeRef::Qualification(id))
            .copied()
            .ok_or(GraphError::MissingNode(NodeRef::Qualification(id)))?;
        let mut node = self.nodes[position].clone();
        let Node::Qualification(qualification) = &mut node else {
            return Err(GraphError::InvalidNode(
                "node is not a qualification".to_string(),
            ));
        };
        let kind = if status == QualificationStatus::Revoked {
            faktor_approval::ApprovalKind::SupplyAvlChange
        } else {
            faktor_approval::ApprovalKind::SupplySupplierQualification
        };
        let digest = faktor_approval::content_digest(
            b"faktor.supply.qualification-change/v1",
            &(
                &self.revision,
                &qualification.id,
                &qualification.status,
                &status,
            ),
        )?;
        let check = faktor_approval::ApprovalCheck::new(
            kind,
            format!("qualification:{}", qualification.id.0),
            qualification.revision.get().to_string(),
            digest,
        );
        let token = approval.ok_or_else(|| faktor_approval::ApprovalError::ApprovalRequired {
            kind: check.kind,
            subject: check.subject.clone(),
            bound_revision: check.bound_revision.clone(),
            bound_digest: check.bound_digest.clone(),
        })?;
        token.require(&check)?;
        qualification.status = status;
        qualification.revision = qualification
            .revision
            .next()
            .map_err(|error| GraphError::InvalidNode(error.to_string()))?;
        self.revision = self
            .revision
            .next()
            .map_err(|error| GraphError::InvalidNode(error.to_string()))?;
        self.nodes[position] = node;
        Ok(())
    }

    /// Change a contractual/payment term. Contract and payment changes are
    /// always consequential and always require a human approval bound to the
    /// exact new terms.
    pub fn set_contract_terms(
        &mut self,
        id: ContractId,
        terms: ContractTerms,
        approval: Option<&faktor_approval::ApprovalToken>,
    ) -> Result<(), GraphError> {
        if terms.id != id {
            return Err(GraphError::InvalidNode(
                "contract terms id does not match the target".to_string(),
            ));
        }
        let position = self
            .index
            .get(&NodeRef::Contract(id))
            .copied()
            .ok_or(GraphError::MissingNode(NodeRef::Contract(id)))?;
        let current = self.nodes[position].clone();
        let Node::Contract(existing) = &current else {
            return Err(GraphError::InvalidNode(
                "node is not a contract".to_string(),
            ));
        };
        let digest = faktor_approval::content_digest(
            b"faktor.supply.contract-change/v1",
            &(&self.revision, existing, &terms),
        )?;
        let check = faktor_approval::ApprovalCheck::new(
            faktor_approval::ApprovalKind::SupplyContractTerms,
            format!("contract:{}", id.0),
            existing.revision.get().to_string(),
            digest,
        );
        let token = approval.ok_or_else(|| faktor_approval::ApprovalError::ApprovalRequired {
            kind: check.kind,
            subject: check.subject.clone(),
            bound_revision: check.bound_revision.clone(),
            bound_digest: check.bound_digest.clone(),
        })?;
        token.require(&check)?;
        let mut terms = terms;
        terms.revision = existing
            .revision
            .next()
            .map_err(|error| GraphError::InvalidNode(error.to_string()))?;
        self.revision = self
            .revision
            .next()
            .map_err(|error| GraphError::InvalidNode(error.to_string()))?;
        self.nodes[position] = Node::Contract(terms);
        Ok(())
    }

    /// Change a supplier's AVL authorization state.
    pub fn set_supplier_authorization(
        &mut self,
        id: SupplierId,
        authorization: AuthorizationStatus,
        approval: Option<&faktor_approval::ApprovalToken>,
    ) -> Result<(), GraphError> {
        let position = self
            .index
            .get(&NodeRef::Supplier(id))
            .copied()
            .ok_or(GraphError::MissingNode(NodeRef::Supplier(id)))?;
        let mut node = self.nodes[position].clone();
        let Node::Supplier(supplier) = &mut node else {
            return Err(GraphError::InvalidNode(
                "node is not a supplier".to_string(),
            ));
        };
        let digest = faktor_approval::content_digest(
            b"faktor.supply.avl-change/v1",
            &(
                &self.revision,
                &supplier.id,
                &supplier.authorization,
                &authorization,
            ),
        )?;
        let check = faktor_approval::ApprovalCheck::new(
            faktor_approval::ApprovalKind::SupplyAvlChange,
            format!("supplier:{}", supplier.id.0),
            supplier.revision.get().to_string(),
            digest,
        );
        let token = approval.ok_or_else(|| faktor_approval::ApprovalError::ApprovalRequired {
            kind: check.kind,
            subject: check.subject.clone(),
            bound_revision: check.bound_revision.clone(),
            bound_digest: check.bound_digest.clone(),
        })?;
        token.require(&check)?;
        supplier.authorization = authorization;
        supplier.revision = supplier
            .revision
            .next()
            .map_err(|error| GraphError::InvalidNode(error.to_string()))?;
        self.revision = self
            .revision
            .next()
            .map_err(|error| GraphError::InvalidNode(error.to_string()))?;
        self.nodes[position] = node;
        Ok(())
    }

    /// Rebuild the adjacency, reverse-adjacency and edge-set indexes after
    /// deserialization (serde skips derived indexes). This is `O(V + E)`.
    pub fn rebuild_indexes(&mut self) {
        self.index.clear();
        self.adjacency.clear();
        self.reverse.clear();
        self.edge_set.clear();
        for (position, node) in self.nodes.iter().enumerate() {
            self.index.insert(node.key(), position);
            self.adjacency.entry(node.key()).or_default();
        }
        let mut edges = Vec::with_capacity(self.edges.len());
        for edge in self.edges.drain(..) {
            if self.edge_set.insert(edge) {
                edges.push(edge);
            }
        }
        self.edges = edges;
        for (position, edge) in self.edges.iter().enumerate() {
            self.adjacency.entry(edge.from).or_default().push(position);
            self.reverse.entry(edge.to).or_default().push(position);
        }
    }

    /// A deterministic digest input: the graph revision plus every node and
    /// edge in canonical (sorted) order. Returns bytes to be hashed by the
    /// caller's authority; this crate intentionally has no hash dependency.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut nodes = self.nodes.iter().collect::<Vec<_>>();
        nodes.sort_by_key(|node| node.key());
        let mut edges = self.edges.clone();
        edges.sort_by_key(|edge| (edge.from, edge.to, edge.kind));
        let mut output = Vec::new();
        output.extend_from_slice(format!("rev={}\n", self.revision.get()).as_bytes());
        for node in nodes {
            output.extend_from_slice(format!("{node:?}\n").as_bytes());
        }
        for edge in edges {
            output.extend_from_slice(format!("{edge:?}\n").as_bytes());
        }
        output
    }
}

fn edge_kind_allowed(kind: EdgeKind, from: NodeRef, to: NodeRef) -> bool {
    match kind {
        EdgeKind::Supplies => {
            matches!(from, NodeRef::Supplier(_)) && matches!(to, NodeRef::Part(_))
        }
        EdgeKind::Manufactures => {
            matches!(from, NodeRef::Manufacturer(_)) && matches!(to, NodeRef::Part(_))
        }
        EdgeKind::Operates => {
            matches!(from, NodeRef::Supplier(_)) && matches!(to, NodeRef::Site(_))
        }
        EdgeKind::GovernedBy => {
            matches!(from, NodeRef::Shipment(_)) && matches!(to, NodeRef::Contract(_))
        }
        EdgeKind::RequiresProcess => {
            matches!(from, NodeRef::Part(_)) && matches!(to, NodeRef::Process(_))
        }
        EdgeKind::Qualifies => matches!(from, NodeRef::Qualification(_)) && is_qualifiable(to),
        EdgeKind::AffectsPart => matches!(from, NodeRef::Pcn(_)) && matches!(to, NodeRef::Part(_)),
        EdgeKind::Exposes => {
            matches!(from, NodeRef::Risk(_))
                && matches!(
                    to,
                    NodeRef::Part(_)
                        | NodeRef::Supplier(_)
                        | NodeRef::Site(_)
                        | NodeRef::Shipment(_)
                )
        }
        EdgeKind::LocatedAt => {
            matches!(from, NodeRef::Supplier(_)) && matches!(to, NodeRef::Site(_))
        }
    }
}

fn is_qualifiable(node: NodeRef) -> bool {
    matches!(
        node,
        NodeRef::Supplier(_)
            | NodeRef::Site(_)
            | NodeRef::Part(_)
            | NodeRef::Process(_)
            | NodeRef::Manufacturer(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::basis::Day;

    fn part(id: u32) -> Node {
        Node::Part(Part {
            id: PartId(id),
            mpn: format!("MPN-{id}"),
            description: "resistor".to_string(),
            lifecycle: LifecycleStatus::Active,
            revision: Revision(1),
        })
    }

    fn supplier(id: u32, role: SupplierRole) -> Node {
        Node::Supplier(Supplier {
            id: SupplierId(id),
            name: format!("supplier {id}"),
            role,
            authorization: if role == SupplierRole::AuthorizedDistributor {
                AuthorizationStatus::Authorized
            } else {
                AuthorizationStatus::Unknown
            },
            evidence: vec![],
            revision: Revision(1),
        })
    }

    #[test]
    fn qualification_and_avl_changes_require_human_approval_tokens() {
        let mut graph = SupplyGraph::new(Revision(1));
        graph
            .add_node(Node::Qualification(Qualification {
                id: QualificationId(1),
                subject: NodeRef::Part(PartId(1)),
                kind: QualificationKind::ProcessCapability,
                status: QualificationStatus::EvidencePending,
                valid_until: None,
                evidence: vec![],
                revision: Revision(1),
            }))
            .unwrap();
        let error = graph
            .set_qualification_status(QualificationId(1), QualificationStatus::Approved, None)
            .unwrap_err();
        assert!(matches!(error, GraphError::Approval(_)));
        let qualification = graph
            .nodes
            .iter()
            .find_map(|node| match node {
                Node::Qualification(qualification) => Some(qualification.clone()),
                _ => None,
            })
            .unwrap();
        let digest = faktor_approval::content_digest(
            b"faktor.supply.qualification-change/v1",
            &(
                &graph.revision,
                &qualification.id,
                &qualification.status,
                &QualificationStatus::Approved,
            ),
        )
        .unwrap();
        let token = faktor_approval::ApprovalToken::test_token(
            faktor_approval::ApprovalKind::SupplySupplierQualification,
            "qualification:1",
            "1",
            digest,
        );
        graph
            .set_qualification_status(
                QualificationId(1),
                QualificationStatus::Approved,
                Some(&token),
            )
            .unwrap();
        assert!(graph.nodes.iter().any(|node| matches!(
            node,
            Node::Qualification(qualification)
                if qualification.status == QualificationStatus::Approved
        )));
    }

    #[test]
    fn roles_are_distinct_and_ordered_by_risk() {
        assert!(SupplierRole::Manufacturer.is_manufacturer());
        assert!(!SupplierRole::AuthorizedDistributor.is_manufacturer());
        assert!(!SupplierRole::Broker.is_manufacturer());
        assert!(
            SupplierRole::Manufacturer.traceability_rank()
                < SupplierRole::IndependentDistributor.traceability_rank()
        );
        assert!(
            SupplierRole::IndependentDistributor.traceability_rank()
                < SupplierRole::MarketplaceSeller.traceability_rank()
        );
        assert!(
            SupplierRole::MarketplaceSeller.traceability_rank()
                < SupplierRole::Broker.traceability_rank()
        );
    }

    #[test]
    fn illegal_edges_and_duplicates_are_refused() {
        let mut graph = SupplyGraph::new(Revision(1));
        let part_key = graph.add_node(part(1)).unwrap();
        let supplier_key = graph.add_node(supplier(1, SupplierRole::Broker)).unwrap();
        assert!(graph
            .add_edge(Edge {
                from: part_key,
                to: supplier_key,
                kind: EdgeKind::Supplies,
            })
            .is_err());
        assert!(graph
            .add_edge(Edge {
                from: supplier_key,
                to: part_key,
                kind: EdgeKind::Supplies,
            })
            .is_ok());
        assert!(matches!(
            graph.add_edge(Edge {
                from: supplier_key,
                to: part_key,
                kind: EdgeKind::Supplies,
            }),
            Err(GraphError::DuplicateEdge)
        ));
        assert!(graph
            .add_edge(Edge {
                from: supplier_key,
                to: supplier_key,
                kind: EdgeKind::Supplies,
            })
            .is_err());
    }

    #[test]
    fn missing_endpoint_and_duplicate_node_are_refused() {
        let mut graph = SupplyGraph::new(Revision(1));
        let key = graph.add_node(part(1)).unwrap();
        assert!(matches!(
            graph.add_node(part(1)),
            Err(GraphError::DuplicateNode(_))
        ));
        assert!(matches!(
            graph.add_edge(Edge {
                from: key,
                to: NodeRef::Part(PartId(99)),
                kind: EdgeKind::RequiresProcess,
            }),
            Err(GraphError::MissingEndpoint(_))
        ));
    }

    #[test]
    fn traversal_is_deterministic_and_bounded() {
        let mut graph = SupplyGraph::new(Revision(1));
        let part_key = graph.add_node(part(1)).unwrap();
        let supplier_a = graph
            .add_node(supplier(2, SupplierRole::Manufacturer))
            .unwrap();
        let supplier_b = graph.add_node(supplier(1, SupplierRole::Broker)).unwrap();
        let process = graph
            .add_node(Node::Process(Process {
                id: ProcessId(1),
                name: "SMT".to_string(),
                kind: ProcessKind::Smt,
                capability: "0402..QFN".to_string(),
                revision: Revision(1),
            }))
            .unwrap();
        graph
            .add_node(Node::Qualification(Qualification {
                id: QualificationId(1),
                subject: supplier_a,
                kind: QualificationKind::Ppap,
                status: QualificationStatus::Approved,
                valid_until: Some(Day(30_000)),
                evidence: vec![EvidenceRef::new("ppap-1", Revision(2), None).unwrap()],
                revision: Revision(2),
            }))
            .unwrap();
        graph
            .add_edge(Edge {
                from: supplier_a,
                to: part_key,
                kind: EdgeKind::Supplies,
            })
            .unwrap();
        graph
            .add_edge(Edge {
                from: supplier_b,
                to: part_key,
                kind: EdgeKind::Supplies,
            })
            .unwrap();
        graph
            .add_edge(Edge {
                from: part_key,
                to: process,
                kind: EdgeKind::RequiresProcess,
            })
            .unwrap();
        let suppliers = graph.suppliers_for_part(PartId(1));
        assert_eq!(suppliers, vec![SupplierId(1), SupplierId(2)]);
        let reachable = graph.traverse(supplier_b, MAX_TRAVERSAL_DEPTH);
        assert!(reachable
            .iter()
            .any(|(key, depth)| *key == part_key && *depth == 1));
        assert!(reachable
            .iter()
            .any(|(key, depth)| *key == process && *depth == 2));
        let limited = graph.traverse(part_key, 0);
        assert_eq!(limited, vec![(part_key, 0)]);
        assert_eq!(graph.parts_for_supplier(SupplierId(1)), vec![PartId(1)]);
    }

    #[test]
    fn traversal_is_depth_bounded_and_visits_each_node_once() {
        let mut graph = SupplyGraph::new(Revision(1));
        let part_key = graph.add_node(part(1)).unwrap();
        let supplier_key = graph
            .add_node(supplier(1, SupplierRole::Manufacturer))
            .unwrap();
        let process = graph
            .add_node(Node::Process(Process {
                id: ProcessId(1),
                name: "SMT".to_string(),
                kind: ProcessKind::Smt,
                capability: "0402..QFN".to_string(),
                revision: Revision(1),
            }))
            .unwrap();
        graph
            .add_edge(Edge {
                from: supplier_key,
                to: part_key,
                kind: EdgeKind::Supplies,
            })
            .unwrap();
        graph
            .add_edge(Edge {
                from: part_key,
                to: process,
                kind: EdgeKind::RequiresProcess,
            })
            .unwrap();
        // Breaks if traversal revisits nodes, ignores depth, or fails to clamp MAX.
        assert_eq!(
            graph.traverse(supplier_key, MAX_TRAVERSAL_DEPTH),
            vec![(supplier_key, 0), (part_key, 1), (process, 2)]
        );
        assert_eq!(
            graph.traverse(supplier_key, 1),
            vec![(supplier_key, 0), (part_key, 1)]
        );
        assert_eq!(
            graph.traverse(supplier_key, usize::MAX),
            graph.traverse(supplier_key, MAX_TRAVERSAL_DEPTH)
        );
    }

    #[test]
    fn a_serialized_graph_round_trips_and_validates() {
        let mut graph = SupplyGraph::new(Revision(3));
        let part_key = graph.add_node(part(1)).unwrap();
        let supplier_key = graph
            .add_node(supplier(1, SupplierRole::Manufacturer))
            .unwrap();
        graph
            .add_edge(Edge {
                from: supplier_key,
                to: part_key,
                kind: EdgeKind::Supplies,
            })
            .unwrap();
        let encoded = serde_json::to_string(&graph).unwrap();
        let decoded: SupplyGraph = serde_json::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(
            decoded.suppliers_for_part(PartId(1)),
            vec![SupplierId(1)],
            "plain deserialization must build the adjacency indexes"
        );
        assert!(decoded.node(part_key).is_some());
    }

    #[test]
    fn ten_thousand_parts_and_suppliers_build_and_traverse_linearly() {
        let mut graph = SupplyGraph::new(Revision(1));
        for index in 0..10_000_u32 {
            graph.add_node(part(index)).unwrap();
            graph
                .add_node(Node::Supplier(Supplier {
                    id: SupplierId(index),
                    name: format!("supplier {index}"),
                    role: SupplierRole::Manufacturer,
                    authorization: AuthorizationStatus::Unknown,
                    evidence: vec![],
                    revision: Revision(1),
                }))
                .unwrap();
        }
        for index in 0..10_000_u32 {
            graph
                .add_edge(Edge {
                    from: NodeRef::Supplier(SupplierId(index)),
                    to: NodeRef::Part(PartId(index)),
                    kind: EdgeKind::Supplies,
                })
                .unwrap();
        }
        let started = std::time::Instant::now();
        assert_eq!(
            graph.suppliers_for_part(PartId(9_999)),
            vec![SupplierId(9_999)]
        );
        let reachable = graph.traverse(NodeRef::Supplier(SupplierId(9_999)), 4);
        assert_eq!(reachable.len(), 2);
        assert!(started.elapsed().as_secs() < 10);
    }
}
