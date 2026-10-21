use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{require_text, CoreError, CoreResult};
use crate::value::{EngineeringValue, Provenance, RevisionId};

pub const DEFAULT_MAX_NODES: usize = 10_000;
pub const DEFAULT_MAX_EDGES: usize = 50_000;
/// Hard ceiling accepted from a deserialized graph's configured limits.
pub const MAX_NODES_LIMIT: usize = 1_000_000;
/// Hard ceiling accepted from a deserialized graph's configured limits.
pub const MAX_EDGES_LIMIT: usize = 5_000_000;

/// Discoverability aliases: the graph is a typed node/edge graph.
pub type NodeType = NodeKind;
pub type EdgeType = EdgeKind;
pub type EngineeringDesignGraph = DesignGraph;

macro_rules! graph_id {
    ($name:ident) => {
        #[derive(
            JsonSchema,
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(raw: u64) -> Self {
                assert!(raw != 0, concat!(stringify!($name), " starts at 1"));
                Self(raw)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

graph_id!(NodeId);
graph_id!(EdgeId);

#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Requirement,
    Constraint,
    Function,
    Interface,
    Architecture,
    Component,
    Material,
    Geometry,
    Net,
    Signal,
    PowerRail,
    LoadCase,
    Environment,
    Tolerance,
    Datum,
    ManufacturingProcess,
    FailureMode,
    Hazard,
    Inspection,
    Test,
    Equation,
    PhysicalModel,
    SimulationCase,
    SimulationResult,
    Assumption,
    Uncertainty,
    Evidence,
    Decision,
}

impl NodeKind {
    pub const ALL: [Self; 28] = [
        Self::Requirement,
        Self::Constraint,
        Self::Function,
        Self::Interface,
        Self::Architecture,
        Self::Component,
        Self::Material,
        Self::Geometry,
        Self::Net,
        Self::Signal,
        Self::PowerRail,
        Self::LoadCase,
        Self::Environment,
        Self::Tolerance,
        Self::Datum,
        Self::ManufacturingProcess,
        Self::FailureMode,
        Self::Hazard,
        Self::Inspection,
        Self::Test,
        Self::Equation,
        Self::PhysicalModel,
        Self::SimulationCase,
        Self::SimulationResult,
        Self::Assumption,
        Self::Uncertainty,
        Self::Evidence,
        Self::Decision,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requirement => "Requirement",
            Self::Constraint => "Constraint",
            Self::Function => "Function",
            Self::Interface => "Interface",
            Self::Architecture => "Architecture",
            Self::Component => "Component",
            Self::Material => "Material",
            Self::Geometry => "Geometry",
            Self::Net => "Net",
            Self::Signal => "Signal",
            Self::PowerRail => "PowerRail",
            Self::LoadCase => "LoadCase",
            Self::Environment => "Environment",
            Self::Tolerance => "Tolerance",
            Self::Datum => "Datum",
            Self::ManufacturingProcess => "ManufacturingProcess",
            Self::FailureMode => "FailureMode",
            Self::Hazard => "Hazard",
            Self::Inspection => "Inspection",
            Self::Test => "Test",
            Self::Equation => "Equation",
            Self::PhysicalModel => "PhysicalModel",
            Self::SimulationCase => "SimulationCase",
            Self::SimulationResult => "SimulationResult",
            Self::Assumption => "Assumption",
            Self::Uncertainty => "Uncertainty",
            Self::Evidence => "Evidence",
            Self::Decision => "Decision",
        }
    }
}

#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Refines,
    Satisfies,
    Constrains,
    Implements,
    Interfaces,
    DependsOn,
    EvidencedBy,
    ValidatedBy,
    CausedBy,
    Mitigates,
    HasModel,
    HasCase,
    Produces,
    Assumes,
    AppliesTo,
    DerivedFrom,
    Measures,
    ConflictsWith,
    HasTolerance,
    HasDatum,
    HasProcess,
    HasFailureMode,
    HasHazard,
    HasLoadCase,
    HasEnvironment,
    RequiresInspection,
    Verifies,
}

impl EdgeKind {
    pub fn allows(self, from: NodeKind, to: NodeKind) -> bool {
        use EdgeKind as E;
        use NodeKind as N;
        match self {
            E::Refines => {
                matches!(
                    from,
                    N::Requirement | N::Constraint | N::Function | N::Architecture | N::Interface
                ) && from == to
            }
            E::Satisfies => {
                matches!(
                    from,
                    N::Component
                        | N::Architecture
                        | N::Interface
                        | N::PhysicalModel
                        | N::ManufacturingProcess
                ) && matches!(to, N::Requirement | N::Constraint)
            }
            E::Constrains => {
                from == N::Constraint
                    && matches!(
                        to,
                        N::Component
                            | N::Geometry
                            | N::Interface
                            | N::Architecture
                            | N::ManufacturingProcess
                            | N::Material
                    )
            }
            E::Implements => {
                matches!(
                    from,
                    N::Component | N::Architecture | N::ManufacturingProcess
                ) && matches!(to, N::Function | N::Requirement)
            }
            E::Interfaces => {
                from == N::Interface
                    && matches!(
                        to,
                        N::Component
                            | N::Architecture
                            | N::Interface
                            | N::Net
                            | N::Signal
                            | N::PowerRail
                    )
            }
            E::DependsOn => {
                !matches!(from, N::Evidence | N::Assumption)
                    && !matches!(to, N::Evidence | N::Requirement | N::Constraint)
            }
            E::EvidencedBy => from != N::Evidence && to == N::Evidence,
            E::ValidatedBy => {
                matches!(
                    from,
                    N::Requirement
                        | N::Constraint
                        | N::Component
                        | N::PhysicalModel
                        | N::SimulationCase
                        | N::Decision
                        | N::Architecture
                        | N::Interface
                ) && matches!(
                    to,
                    N::Test | N::SimulationResult | N::Inspection | N::Evidence
                )
            }
            E::CausedBy => {
                matches!(from, N::FailureMode | N::Hazard)
                    && matches!(
                        to,
                        N::Component
                            | N::Material
                            | N::Geometry
                            | N::ManufacturingProcess
                            | N::Environment
                            | N::LoadCase
                            | N::Interface
                            | N::Tolerance
                            | N::FailureMode
                            | N::Hazard
                    )
            }
            E::Mitigates => {
                matches!(
                    from,
                    N::Constraint
                        | N::Component
                        | N::Inspection
                        | N::Test
                        | N::ManufacturingProcess
                        | N::Tolerance
                        | N::Datum
                ) && matches!(to, N::FailureMode | N::Hazard)
            }
            E::HasModel => {
                matches!(
                    from,
                    N::Component | N::Function | N::Interface | N::Architecture | N::SimulationCase
                ) && matches!(to, N::Equation | N::PhysicalModel)
            }
            E::HasCase => {
                matches!(
                    from,
                    N::PhysicalModel | N::Component | N::Architecture | N::Interface
                ) && to == N::SimulationCase
            }
            E::Produces => from == N::SimulationCase && to == N::SimulationResult,
            E::Assumes => {
                matches!(
                    from,
                    N::Equation
                        | N::PhysicalModel
                        | N::SimulationCase
                        | N::SimulationResult
                        | N::Decision
                ) && to == N::Assumption
            }
            E::AppliesTo => {
                matches!(
                    from,
                    N::Tolerance | N::LoadCase | N::Environment | N::Uncertainty | N::Assumption
                ) && matches!(
                    to,
                    N::Component
                        | N::Geometry
                        | N::Material
                        | N::Interface
                        | N::Architecture
                        | N::Equation
                        | N::PhysicalModel
                        | N::SimulationCase
                )
            }
            E::DerivedFrom => {
                matches!(
                    from,
                    N::Component
                        | N::Material
                        | N::Geometry
                        | N::Equation
                        | N::PhysicalModel
                        | N::SimulationResult
                        | N::Requirement
                        | N::Constraint
                        | N::Interface
                        | N::Tolerance
                ) && matches!(
                    to,
                    N::Component
                        | N::Material
                        | N::Geometry
                        | N::Equation
                        | N::PhysicalModel
                        | N::SimulationResult
                        | N::Requirement
                        | N::Constraint
                )
            }
            E::Measures => {
                matches!(from, N::Inspection | N::Test)
                    && matches!(
                        to,
                        N::Component
                            | N::Geometry
                            | N::Tolerance
                            | N::Material
                            | N::ManufacturingProcess
                            | N::Interface
                    )
            }
            E::ConflictsWith => true,
            E::HasTolerance => {
                matches!(
                    from,
                    N::Component | N::Geometry | N::Interface | N::ManufacturingProcess
                ) && to == N::Tolerance
            }
            E::HasDatum => {
                matches!(
                    from,
                    N::Geometry | N::Tolerance | N::Inspection | N::ManufacturingProcess
                ) && to == N::Datum
            }
            E::HasProcess => {
                matches!(from, N::Component | N::Geometry | N::Material)
                    && to == N::ManufacturingProcess
            }
            E::HasFailureMode => {
                matches!(
                    from,
                    N::Component
                        | N::Material
                        | N::Geometry
                        | N::ManufacturingProcess
                        | N::Environment
                        | N::Interface
                ) && to == N::FailureMode
            }
            E::HasHazard => {
                matches!(
                    from,
                    N::Component | N::Function | N::Environment | N::Architecture
                ) && to == N::Hazard
            }
            E::HasLoadCase => {
                matches!(
                    from,
                    N::Component | N::Interface | N::Architecture | N::Environment
                ) && to == N::LoadCase
            }
            E::HasEnvironment => {
                matches!(from, N::Component | N::Architecture | N::Interface)
                    && to == N::Environment
            }
            E::RequiresInspection => {
                matches!(from, N::Component | N::ManufacturingProcess | N::Material)
                    && to == N::Inspection
            }
            E::Verifies => {
                matches!(
                    from,
                    N::Test | N::Inspection | N::SimulationResult | N::Evidence
                ) && matches!(
                    to,
                    N::Requirement
                        | N::Constraint
                        | N::Component
                        | N::PhysicalModel
                        | N::SimulationCase
                        | N::Decision
                )
            }
        }
    }
}

#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesignNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub properties: BTreeMap<String, EngineeringValue>,
    pub provenance: Provenance,
    pub revision: RevisionId,
}

impl DesignNode {
    pub fn validate(&self) -> CoreResult<()> {
        require_text("node.name", &self.name, 512)?;
        self.provenance.validate()?;
        for (name, value) in &self.properties {
            require_text("node.property", name, 256)?;
            value.validate()?;
            if value.revision != self.revision {
                return Err(CoreError::RevisionMismatch {
                    expected: self.revision,
                    found: value.revision,
                });
            }
        }
        Ok(())
    }
}

#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesignEdge {
    pub id: EdgeId,
    pub kind: EdgeKind,
    pub from: NodeId,
    pub to: NodeId,
    pub provenance: Provenance,
    pub revision: RevisionId,
}

#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesignGraph {
    revision: RevisionId,
    nodes: BTreeMap<NodeId, DesignNode>,
    edges: BTreeMap<EdgeId, DesignEdge>,
    next_node: u64,
    next_edge: u64,
    max_nodes: usize,
    max_edges: usize,
}

impl DesignGraph {
    pub fn new(revision: RevisionId) -> Self {
        Self {
            revision,
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            next_node: 1,
            next_edge: 1,
            max_nodes: DEFAULT_MAX_NODES,
            max_edges: DEFAULT_MAX_EDGES,
        }
    }

    pub fn with_limits(
        revision: RevisionId,
        max_nodes: usize,
        max_edges: usize,
    ) -> CoreResult<Self> {
        if max_nodes == 0 || max_edges == 0 {
            return Err(CoreError::GraphLimit {
                what: "configured limit",
                limit: 0,
            });
        }
        if max_nodes > MAX_NODES_LIMIT || max_edges > MAX_EDGES_LIMIT {
            return Err(CoreError::GraphLimit {
                what: "configured limit",
                limit: max_nodes.max(max_edges),
            });
        }
        Ok(Self {
            revision,
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            next_node: 1,
            next_edge: 1,
            max_nodes,
            max_edges,
        })
    }

    pub fn revision(&self) -> RevisionId {
        self.revision
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn add_node(
        &mut self,
        kind: NodeKind,
        name: impl Into<String>,
        properties: BTreeMap<String, EngineeringValue>,
        provenance: Provenance,
    ) -> CoreResult<NodeId> {
        if self.nodes.len() >= self.max_nodes {
            return Err(CoreError::GraphLimit {
                what: "nodes",
                limit: self.max_nodes,
            });
        }
        if self.next_node == 0 {
            return Err(CoreError::InvalidValue {
                reason: "node ids start at 1".to_string(),
            });
        }
        if self.nodes.contains_key(&NodeId::new(self.next_node)) {
            return Err(CoreError::InvalidValue {
                reason: format!("node id {} is already occupied", self.next_node),
            });
        }
        let id = NodeId::new(self.next_node);
        let node = DesignNode {
            id,
            kind,
            name: name.into(),
            properties,
            provenance,
            revision: self.revision,
        };
        node.validate()?;
        self.next_node = self.next_node.checked_add(1).ok_or(CoreError::GraphLimit {
            what: "node ids",
            limit: usize::MAX,
        })?;
        self.nodes.insert(id, node);
        Ok(id)
    }

    pub fn add_edge(
        &mut self,
        kind: EdgeKind,
        from: NodeId,
        to: NodeId,
        provenance: Provenance,
    ) -> CoreResult<EdgeId> {
        if self.edges.len() >= self.max_edges {
            return Err(CoreError::GraphLimit {
                what: "edges",
                limit: self.max_edges,
            });
        }
        if from == to {
            return Err(CoreError::InvalidValue {
                reason: format!("self edge {from} is not a relationship"),
            });
        }
        let from_node = self.nodes.get(&from).ok_or(CoreError::NodeNotFound(from))?;
        let to_node = self.nodes.get(&to).ok_or(CoreError::NodeNotFound(to))?;
        if !kind.allows(from_node.kind, to_node.kind) {
            return Err(CoreError::EdgeKindNotAllowed {
                edge: kind,
                from: from_node.kind,
                to: to_node.kind,
            });
        }
        provenance.validate()?;
        if self.next_edge == 0 {
            return Err(CoreError::InvalidValue {
                reason: "edge ids start at 1".to_string(),
            });
        }
        if self.edges.contains_key(&EdgeId::new(self.next_edge)) {
            return Err(CoreError::InvalidValue {
                reason: format!("edge id {} is already occupied", self.next_edge),
            });
        }
        let id = EdgeId::new(self.next_edge);
        self.next_edge = self.next_edge.checked_add(1).ok_or(CoreError::GraphLimit {
            what: "edge ids",
            limit: usize::MAX,
        })?;
        self.edges.insert(
            id,
            DesignEdge {
                id,
                kind,
                from,
                to,
                provenance,
                revision: self.revision,
            },
        );
        Ok(id)
    }

    pub fn node(&self, id: NodeId) -> Option<&DesignNode> {
        self.nodes.get(&id)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &DesignNode> {
        self.nodes.values()
    }

    pub fn nodes_of_kind(&self, kind: NodeKind) -> impl Iterator<Item = &DesignNode> {
        self.nodes.values().filter(move |node| node.kind == kind)
    }

    pub fn edges(&self) -> impl Iterator<Item = &DesignEdge> {
        self.edges.values()
    }

    pub fn edges_from(&self, from: NodeId) -> impl Iterator<Item = &DesignEdge> {
        self.edges.values().filter(move |edge| edge.from == from)
    }

    pub fn edges_to(&self, to: NodeId) -> impl Iterator<Item = &DesignEdge> {
        self.edges.values().filter(move |edge| edge.to == to)
    }

    pub fn validate(&self) -> CoreResult<()> {
        if self.revision.get() == 0 {
            return Err(CoreError::InvalidValue {
                reason: "graph revision ids start at 1".to_string(),
            });
        }
        if self.max_nodes == 0
            || self.max_edges == 0
            || self.max_nodes > MAX_NODES_LIMIT
            || self.max_edges > MAX_EDGES_LIMIT
        {
            return Err(CoreError::GraphLimit {
                what: "configured limit",
                limit: self.max_nodes.max(self.max_edges),
            });
        }
        if self.nodes.len() > self.max_nodes || self.edges.len() > self.max_edges {
            return Err(CoreError::GraphLimit {
                what: "graph size",
                limit: self.max_nodes.max(self.max_edges),
            });
        }
        let max_node = self
            .nodes
            .keys()
            .next_back()
            .map(|id| id.get())
            .unwrap_or(0);
        let max_edge = self
            .edges
            .keys()
            .next_back()
            .map(|id| id.get())
            .unwrap_or(0);
        if self.next_node <= max_node || self.next_edge <= max_edge {
            return Err(CoreError::InvalidValue {
                reason: format!(
                    "next ids ({}, {}) must exceed every stored id ({max_node}, {max_edge})",
                    self.next_node, self.next_edge
                ),
            });
        }
        let mut evidenced = BTreeSet::new();
        for edge in self.edges.values() {
            if edge.id.get() == 0 {
                return Err(CoreError::InvalidValue {
                    reason: "edge ids start at 1".to_string(),
                });
            }
            if edge.kind == EdgeKind::EvidencedBy {
                evidenced.insert(edge.from);
            }
        }
        for node in self.nodes.values() {
            if node.id.get() == 0 {
                return Err(CoreError::InvalidValue {
                    reason: "node ids start at 1".to_string(),
                });
            }
            node.validate()?;
            if node.revision != self.revision {
                return Err(CoreError::RevisionMismatch {
                    expected: self.revision,
                    found: node.revision,
                });
            }
            if node.kind == NodeKind::Decision && !evidenced.contains(&node.id) {
                return Err(CoreError::DecisionWithoutEvidence(node.id));
            }
        }
        for edge in self.edges.values() {
            if edge.revision != self.revision {
                return Err(CoreError::RevisionMismatch {
                    expected: self.revision,
                    found: edge.revision,
                });
            }
            edge.provenance.validate()?;
            let from = self
                .nodes
                .get(&edge.from)
                .ok_or(CoreError::NodeNotFound(edge.from))?;
            let to = self
                .nodes
                .get(&edge.to)
                .ok_or(CoreError::NodeNotFound(edge.to))?;
            if !edge.kind.allows(from.kind, to.kind) {
                return Err(CoreError::EdgeKindNotAllowed {
                    edge: edge.kind,
                    from: from.kind,
                    to: to.kind,
                });
            }
        }
        Ok(())
    }

    /// Deterministic dependency order (Kahn, smallest node id first). The
    /// dependency edge set is fixed: DependsOn, DerivedFrom, Implements,
    /// Produces, HasModel, HasCase, ValidatedBy, EvidencedBy.
    pub fn topological_order(&self) -> CoreResult<Vec<NodeId>> {
        let dependency = |kind: EdgeKind| {
            matches!(
                kind,
                EdgeKind::DependsOn
                    | EdgeKind::DerivedFrom
                    | EdgeKind::Implements
                    | EdgeKind::Produces
                    | EdgeKind::HasModel
                    | EdgeKind::HasCase
                    | EdgeKind::ValidatedBy
                    | EdgeKind::EvidencedBy
            )
        };
        let mut in_degree: BTreeMap<NodeId, usize> = self.nodes.keys().map(|id| (*id, 0)).collect();
        let mut outgoing: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for edge in self.edges.values().filter(|edge| dependency(edge.kind)) {
            let (before, after) = if edge.kind == EdgeKind::EvidencedBy {
                (edge.to, edge.from)
            } else {
                (edge.from, edge.to)
            };
            outgoing.entry(before).or_default().push(after);
            *in_degree
                .get_mut(&after)
                .ok_or(CoreError::NodeNotFound(after))? += 1;
        }
        let mut ready: VecDeque<NodeId> = in_degree
            .iter()
            .filter(|(_, degree)| **degree == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = ready.pop_front() {
            order.push(id);
            if let Some(targets) = outgoing.get(&id) {
                for target in targets {
                    let degree = in_degree
                        .get_mut(target)
                        .ok_or(CoreError::NodeNotFound(*target))?;
                    *degree -= 1;
                    if *degree == 0 {
                        ready.push_back(*target);
                    }
                }
            }
        }
        if order.len() != self.nodes.len() {
            return Err(CoreError::Cycle);
        }
        Ok(order)
    }

    pub fn evidence_for(&self, node: NodeId) -> Vec<NodeId> {
        self.edges_from(node)
            .filter(|edge| edge.kind == EdgeKind::EvidencedBy)
            .map(|edge| edge.to)
            .collect()
    }

    pub fn conflicts(&self) -> Vec<(NodeId, NodeId)> {
        self.edges
            .values()
            .filter(|edge| edge.kind == EdgeKind::ConflictsWith)
            .map(|edge| (edge.from, edge.to))
            .collect()
    }

    pub fn distinct_kinds(&self) -> BTreeSet<NodeKind> {
        self.nodes.values().map(|node| node.kind).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ProvenanceKind;

    fn source() -> Provenance {
        Provenance::new(ProvenanceKind::Specification, "test specification").unwrap()
    }

    fn graph() -> DesignGraph {
        DesignGraph::new(RevisionId::new(7))
    }

    #[test]
    fn every_required_node_kind_is_present() {
        assert_eq!(NodeKind::ALL.len(), 28);
    }

    #[test]
    fn edge_endpoint_compatibility_is_enforced() {
        let mut graph = graph();
        let requirement = graph
            .add_node(
                NodeKind::Requirement,
                "withstand load",
                BTreeMap::new(),
                source(),
            )
            .unwrap();
        let material = graph
            .add_node(NodeKind::Material, "6061 alloy", BTreeMap::new(), source())
            .unwrap();
        assert!(matches!(
            graph.add_edge(EdgeKind::Satisfies, material, requirement, source()),
            Err(CoreError::EdgeKindNotAllowed { .. })
        ));
    }

    #[test]
    fn a_decision_without_evidence_is_refused() {
        let mut graph = graph();
        graph
            .add_node(
                NodeKind::Decision,
                "choose fastener",
                BTreeMap::new(),
                source(),
            )
            .unwrap();
        assert!(matches!(
            graph.validate(),
            Err(CoreError::DecisionWithoutEvidence(_))
        ));
    }

    #[test]
    fn decision_with_evidence_validates() {
        let mut graph = graph();
        let decision = graph
            .add_node(
                NodeKind::Decision,
                "choose fastener",
                BTreeMap::new(),
                source(),
            )
            .unwrap();
        let evidence = graph
            .add_node(
                NodeKind::Evidence,
                "torque test report",
                BTreeMap::new(),
                source(),
            )
            .unwrap();
        graph
            .add_edge(EdgeKind::EvidencedBy, decision, evidence, source())
            .unwrap();
        graph.validate().unwrap();
    }

    #[test]
    fn revision_mismatch_in_a_property_is_refused() {
        let mut graph = graph();
        let value = EngineeringValue::known(
            engineering_units::Quantity::dimensionless(1.0).unwrap(),
            source(),
            RevisionId::new(8),
        )
        .unwrap();
        let mut properties = BTreeMap::new();
        properties.insert("mass".to_string(), value);
        assert!(matches!(
            graph.add_node(NodeKind::Component, "bracket", properties, source()),
            Err(CoreError::RevisionMismatch { .. })
        ));
    }

    #[test]
    fn dependency_cycles_are_typed() {
        let mut graph = graph();
        let a = graph
            .add_node(NodeKind::Component, "a", BTreeMap::new(), source())
            .unwrap();
        let b = graph
            .add_node(NodeKind::Component, "b", BTreeMap::new(), source())
            .unwrap();
        graph.add_edge(EdgeKind::DependsOn, a, b, source()).unwrap();
        graph.add_edge(EdgeKind::DependsOn, b, a, source()).unwrap();
        assert!(matches!(graph.topological_order(), Err(CoreError::Cycle)));
    }

    #[test]
    fn graph_bounds_refuse_unbounded_growth() {
        let mut graph = DesignGraph::with_limits(RevisionId::new(1), 1, 1).unwrap();
        graph
            .add_node(NodeKind::Component, "only", BTreeMap::new(), source())
            .unwrap();
        assert!(matches!(
            graph.add_node(NodeKind::Component, "second", BTreeMap::new(), source()),
            Err(CoreError::GraphLimit { .. })
        ));
    }

    #[test]
    fn deserialized_graph_cannot_forge_derived_id_state() {
        let mut value = serde_json::to_value(graph()).unwrap();
        value["next_node"] = serde_json::json!(1);
        let mut forged: DesignGraph = serde_json::from_value(value).unwrap();
        let id = forged
            .add_node(NodeKind::Component, "first", BTreeMap::new(), source())
            .unwrap();
        assert_eq!(id, NodeId::new(1));
        let mut value = serde_json::to_value(&forged).unwrap();
        value["next_node"] = serde_json::json!(1);
        let collided: DesignGraph = serde_json::from_value(value).unwrap();
        assert!(collided.validate().is_err());
        assert!(matches!(
            {
                let mut collided = collided;
                collided.add_node(NodeKind::Component, "second", BTreeMap::new(), source())
            },
            Err(CoreError::InvalidValue { .. })
        ));
        let mut value = serde_json::to_value(graph()).unwrap();
        value["max_nodes"] = serde_json::json!(usize::MAX);
        let oversized: DesignGraph = serde_json::from_value(value).unwrap();
        assert!(matches!(
            oversized.validate(),
            Err(CoreError::GraphLimit { .. })
        ));
    }

    #[test]
    fn ten_thousand_nodes_and_edges_validate_linearly() {
        let mut graph = DesignGraph::with_limits(RevisionId::new(1), 10_000, 50_000).unwrap();
        let evidence = graph
            .add_node(NodeKind::Evidence, "evidence", BTreeMap::new(), source())
            .unwrap();
        for index in 0..4_999_u32 {
            let node = graph
                .add_node(
                    NodeKind::Component,
                    format!("component {index}"),
                    BTreeMap::new(),
                    source(),
                )
                .unwrap();
            graph
                .add_edge(EdgeKind::EvidencedBy, node, evidence, source())
                .unwrap();
        }
        for index in 0..5_000_u32 {
            let node = graph
                .add_node(
                    NodeKind::Decision,
                    format!("decision {index}"),
                    BTreeMap::new(),
                    source(),
                )
                .unwrap();
            graph
                .add_edge(EdgeKind::EvidencedBy, node, evidence, source())
                .unwrap();
        }
        assert_eq!(graph.node_count(), 10_000);
        assert_eq!(graph.edge_count(), 9_999);
        let started = std::time::Instant::now();
        graph.validate().unwrap();
        let validated = started.elapsed();
        let order_started = std::time::Instant::now();
        assert_eq!(graph.topological_order().unwrap().len(), 10_000);
        eprintln!(
            "perf graph: 10k nodes/edges validate {:?}, topological {:?}",
            validated,
            order_started.elapsed()
        );
        assert!(validated.as_secs() < 5, "validation took {validated:?}");
    }

    #[test]
    fn topological_order_is_deterministic_and_respects_evidence() {
        let mut graph = graph();
        let component = graph
            .add_node(NodeKind::Component, "bracket", BTreeMap::new(), source())
            .unwrap();
        let decision = graph
            .add_node(NodeKind::Decision, "release", BTreeMap::new(), source())
            .unwrap();
        let evidence = graph
            .add_node(NodeKind::Evidence, "inspection", BTreeMap::new(), source())
            .unwrap();
        graph
            .add_edge(EdgeKind::EvidencedBy, decision, evidence, source())
            .unwrap();
        let order = graph.topological_order().unwrap();
        let evidence_index = order.iter().position(|id| *id == evidence).unwrap();
        let decision_index = order.iter().position(|id| *id == decision).unwrap();
        assert!(evidence_index < decision_index);
        assert!(order.contains(&component));
    }
}
