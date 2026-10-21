//! Deterministic proof rebinding for renamed tests.
//!
//! A contract compiled BEFORE a rename binds its test obligation to the old
//! selector (`cart::totals_add_up`). When the task itself renames that test
//! (file and/or function), the old selector stops existing and the obligation
//! can never be discharged by the renamed test unless the binding is updated.
//! This module performs that update ONLY when the migration is proven:
//!
//! * a rename edge proves only the PATH relation (git strong rename
//!   detection, or the verification inventory's unique deleted→added
//!   test-target pair) — never semantic preservation,
//! * the declaration adapter proves the semantic selector, the executable
//!   selector, the enclosing target and a normalized executable-body
//!   identity (comments/trivia stripped, execution-changing attributes
//!   included), and
//! * the old and new body identities are EQUAL under the declared symbol
//!   rename pairs (and the test's own proven name mapping), the mapping is
//!   unique, and exactly one candidate matches.
//!
//! Anything else stays unresolved and the caller records the obligation as
//! stale, so a required claim can never be satisfied by a coarse/vacuous
//! selector. Every applied update carries old→new + basis + body digest
//! provenance.

use std::collections::{BTreeMap, BTreeSet};

use faktor_core::contract::{ClaimState, EngineeringContract, EvidenceBinding, ProofObligation};

use crate::review::REVIEW_MAX_CHANGED_FILES;
use crate::test_declaration::{
    adapter_for_path, is_supported_test_path, IdentityToken, TestDeclaration,
};

pub use crate::test_declaration::TestDeclarationAdapter;

/// One changed test-bearing file: the base content and the candidate content.
/// A deletion has `after = None`; a creation has `before = None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFileDelta {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A PROVEN file rename edge (from a real authority: git rename detection or
/// the verification inventory's unique test-move pair). The edge proves the
/// path relation only; semantic preservation is proven separately by the
/// declaration body identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRenameEdge {
    pub from: String,
    pub to: String,
    /// Machine-readable basis (`git_rename`, `inventory_rename`).
    pub basis: String,
}

/// The rebinding evidence gathered for one candidate snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestRebindEvidence {
    pub files: Vec<TestFileDelta>,
    pub renames: Vec<TestRenameEdge>,
}

/// One applied rebind (durable ledger input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRebindRecord {
    pub obligation_id: String,
    pub old_selector: String,
    pub new_selector: String,
    pub basis: String,
    /// Digest of the normalized executable body the migration proved.
    pub body_digest: String,
}

/// Why one obligation's binding was NOT migrated. Every refusal keeps the
/// old binding in place and forces the obligation unresolved; none is a
/// silent "no match".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RebindRefusalCode {
    /// The goal does not declare a rename, so a vanished selector is never
    /// automatically migrated.
    TaskNotRename,
    /// No proof-grade move edge (strong git rename / unique inventory move)
    /// relates the old path to a candidate path.
    NoProvenMove,
    /// The proven destination has no candidate declaration inventory.
    TargetUnavailable,
    /// The normalized executable body is not identical under the declared
    /// name mappings.
    BodyInequivalent,
    /// More than one candidate declaration satisfies the identity.
    AmbiguousCandidate,
    /// Two obligations would migrate onto the same new declaration.
    CrossObligationAmbiguous,
    /// The old→new + body-digest provenance cannot fit the durable bound.
    ProvenanceTooLarge,
}

impl RebindRefusalCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TaskNotRename => "task_not_rename",
            Self::NoProvenMove => "no_proven_move",
            Self::TargetUnavailable => "target_unavailable",
            Self::BodyInequivalent => "body_inequivalent",
            Self::AmbiguousCandidate => "ambiguous_candidate",
            Self::CrossObligationAmbiguous => "cross_obligation_ambiguous",
            Self::ProvenanceTooLarge => "provenance_too_large",
        }
    }
}

/// One typed rebinding refusal (the old obligation stays unresolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebindRefusal {
    pub obligation_id: String,
    pub old_selector: String,
    pub code: RebindRefusalCode,
}

/// The outcome of one rebinding pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestRebindOutcome {
    /// Obligations whose binding changed (the caller persists these through
    /// the dedicated durable rebinding write).
    pub updates: Vec<ProofObligation>,
    /// Required obligations whose old selector is gone and whose mapping was
    /// NOT proven: the caller must force them unresolved (typed refusal).
    pub stale: Vec<String>,
    /// The typed reason for every `stale` entry.
    pub refusals: Vec<RebindRefusal>,
    pub rebinds: Vec<TestRebindRecord>,
}

impl TestRebindOutcome {
    pub fn rebind_count(&self) -> usize {
        self.rebinds.len()
    }

    pub fn is_stale(&self, obligation_id: &str) -> bool {
        self.stale.iter().any(|id| id == obligation_id)
    }

    pub fn refusal(&self, obligation_id: &str) -> Option<&RebindRefusal> {
        self.refusals
            .iter()
            .find(|refusal| refusal.obligation_id == obligation_id)
    }
}

fn refuse(
    refusals: &mut Vec<RebindRefusal>,
    obligation_id: &str,
    old_selector: &str,
    code: RebindRefusalCode,
) {
    refusals.push(RebindRefusal {
        obligation_id: obligation_id.to_string(),
        old_selector: old_selector.to_string(),
        code,
    });
}

/// TRUE when `path` is a test-bearing source path some declaration adapter
/// can prove.
pub fn supports_test_path(path: &str) -> bool {
    is_supported_test_path(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    obligation_index: usize,
    new_binding: EvidenceBinding,
    basis: String,
    old_selector: String,
    new_selector: String,
    body_digest: String,
    /// Identity of the candidate declarations (cross-obligation uniqueness).
    candidate: Vec<(String, String)>,
}

/// Rebind every test-selector obligation whose old selector no longer exists
/// on the candidate to its uniquely proven renamed declaration.
pub fn rebind_test_obligations(
    contract: &mut EngineeringContract,
    goal: &str,
    evidence: &TestRebindEvidence,
) -> TestRebindOutcome {
    let mut outcome = TestRebindOutcome::default();
    let substitutions = rename_pairs(goal);
    let goal_is_rename = goal
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word.eq_ignore_ascii_case("rename"));
    let old_inventory = inventory(&evidence.files, true);
    let new_inventory = inventory(&evidence.files, false);
    let renames = rename_edges(evidence, &old_inventory, &new_inventory, &substitutions);

    let mut plans: Vec<Plan> = Vec::new();
    let mut refusals: Vec<RebindRefusal> = Vec::new();
    for (index, obligation) in contract.proof_obligations.iter().enumerate() {
        let targets = binding_targets(&obligation.evidence_binding);
        if targets.is_empty() {
            continue;
        }
        let mut old_declarations: Vec<&TestDeclaration> = targets
            .iter()
            .filter_map(|target| resolve_declaration(target, &old_inventory))
            .collect();
        old_declarations.sort_by(|a, b| {
            a.semantic_selector
                .cmp(&b.semantic_selector)
                .then_with(|| a.executable_selector.cmp(&b.executable_selector))
        });
        old_declarations.dedup_by(|a, b| {
            a.semantic_selector == b.semantic_selector
                && a.executable_selector == b.executable_selector
        });
        if old_declarations.is_empty() {
            continue;
        }
        // A declaration that still exists at the SAME path with the SAME
        // semantic selector needs no rebinding. A selector that merely
        // reappears in another file does not count: that is a move, and the
        // move must be proven by an edge plus body identity.
        let stale: Vec<&TestDeclaration> = old_declarations
            .into_iter()
            .filter(|declaration| {
                new_inventory
                    .get(&declaration.path)
                    .is_none_or(|candidates| {
                        !candidates.iter().any(|candidate| {
                            candidate.semantic_selector == declaration.semantic_selector
                        })
                    })
            })
            .collect();
        if stale.is_empty() {
            continue;
        }
        if !goal_is_rename {
            refuse(
                &mut refusals,
                &obligation.id,
                &stale[0].semantic_selector,
                RebindRefusalCode::TaskNotRename,
            );
            continue;
        }
        let mut mapped: Vec<&TestDeclaration> = Vec::new();
        let mut digests: Vec<String> = Vec::new();
        let mut basis_parts: Vec<String> = Vec::new();
        let mut refusal: Option<RebindRefusalCode> = None;
        for declaration in &stale {
            let (new_path, edge_basis) =
                match resolve_new_path(declaration, &new_inventory, &renames) {
                    Some(found) => found,
                    None => {
                        refusal = Some(RebindRefusalCode::NoProvenMove);
                        break;
                    }
                };
            let Some(candidates) = new_inventory.get(&new_path) else {
                refusal = Some(RebindRefusalCode::TargetUnavailable);
                break;
            };
            let matching: Vec<(&TestDeclaration, String)> = candidates
                .iter()
                .filter_map(|candidate| {
                    body_equivalence_digest(declaration, candidate, &substitutions)
                        .map(|digest| (candidate, digest))
                })
                .filter(|(candidate, _)| {
                    !was_unchanged_elsewhere(declaration, candidate, &old_inventory, &substitutions)
                })
                .collect();
            if matching.len() != 1 {
                refusal = Some(if matching.is_empty() {
                    RebindRefusalCode::BodyInequivalent
                } else {
                    RebindRefusalCode::AmbiguousCandidate
                });
                break;
            }
            let (candidate, digest) = matching[0].clone();
            mapped.push(candidate);
            basis_parts.push(format!(
                "{edge_basis};{}:{}->{}",
                declaration.enclosing_target.as_deref().unwrap_or(""),
                declaration.semantic_selector,
                candidate.semantic_selector
            ));
            digests.push(digest);
        }
        if refusal.is_some() || mapped.is_empty() || digests.is_empty() {
            refuse(
                &mut refusals,
                &obligation.id,
                &stale[0].semantic_selector,
                refusal.unwrap_or(RebindRefusalCode::BodyInequivalent),
            );
            continue;
        }
        let new_binding = binding_for(&mapped);
        if new_binding == obligation.evidence_binding {
            continue;
        }
        let body_digest = digests[0].clone();
        let basis = format!(
            "match:unique;body:equivalent;body_digest:{body_digest};{}",
            basis_parts.join("; ")
        );
        // Never let provenance truncation silently drop the old→new/basis
        // record: an oversized migration refuses typed instead.
        if basis.len() > faktor_core::contract::MAX_BINDING_UPDATE_BASIS_BYTES {
            refuse(
                &mut refusals,
                &obligation.id,
                &stale[0].semantic_selector,
                RebindRefusalCode::ProvenanceTooLarge,
            );
            continue;
        }
        let mut candidate: Vec<(String, String)> = mapped
            .iter()
            .map(|declaration| {
                (
                    declaration.semantic_selector.clone(),
                    declaration.executable_selector.clone(),
                )
            })
            .collect();
        candidate.sort();
        candidate.dedup();
        plans.push(Plan {
            obligation_index: index,
            new_binding,
            basis,
            old_selector: stale
                .iter()
                .map(|declaration| declaration.semantic_selector.clone())
                .collect::<Vec<_>>()
                .join(","),
            new_selector: mapped
                .iter()
                .map(|declaration| declaration.semantic_selector.clone())
                .collect::<Vec<_>>()
                .join(","),
            body_digest,
            candidate,
        });
    }

    // Cross-obligation uniqueness: two old declarations may never map to the
    // same new declaration. Duplicate candidates make BOTH plans ambiguous.
    let mut claimed: BTreeMap<(String, String), BTreeSet<usize>> = BTreeMap::new();
    for (index, plan) in plans.iter().enumerate() {
        for candidate in &plan.candidate {
            claimed.entry(candidate.clone()).or_default().insert(index);
        }
    }
    let ambiguous: BTreeSet<usize> = claimed
        .values()
        .filter(|owners| owners.len() > 1)
        .flatten()
        .copied()
        .collect();
    for (index, plan) in plans.into_iter().enumerate() {
        let obligation = &mut contract.proof_obligations[plan.obligation_index];
        if ambiguous.contains(&index) {
            refuse(
                &mut refusals,
                &obligation.id,
                &plan.old_selector,
                RebindRefusalCode::CrossObligationAmbiguous,
            );
            continue;
        }
        if obligation.rebind_evidence_binding(plan.new_binding, plan.basis.clone()) {
            outcome.rebinds.push(TestRebindRecord {
                obligation_id: obligation.id.clone(),
                old_selector: plan.old_selector,
                new_selector: plan.new_selector,
                basis: plan.basis,
                body_digest: plan.body_digest,
            });
            outcome.updates.push(obligation.clone());
        }
    }
    outcome.refusals = refusals;
    outcome.stale = outcome
        .refusals
        .iter()
        .map(|refusal| refusal.obligation_id.clone())
        .collect();
    outcome.stale.sort();
    outcome.stale.dedup();
    outcome
}

fn inventory(files: &[TestFileDelta], before: bool) -> BTreeMap<String, Vec<TestDeclaration>> {
    let mut out: BTreeMap<String, Vec<TestDeclaration>> = BTreeMap::new();
    for file in files.iter().take(REVIEW_MAX_CHANGED_FILES) {
        let content = if before { &file.before } else { &file.after };
        let Some(content) = content else {
            continue;
        };
        let Some(adapter) = adapter_for_path(&file.path) else {
            continue;
        };
        let declarations = adapter.declarations(&file.path, content);
        if !declarations.is_empty() {
            out.insert(file.path.clone(), declarations);
        }
    }
    out
}

fn rename_edges(
    evidence: &TestRebindEvidence,
    old: &BTreeMap<String, Vec<TestDeclaration>>,
    new: &BTreeMap<String, Vec<TestDeclaration>>,
    substitutions: &[(String, String)],
) -> Vec<TestRenameEdge> {
    // A proof-grade edge is a STRONG git rename (the production gatherer
    // asks for >=50% detection) or the unique inventory move below. A weak
    // similarity pairing is never proof-authoritative, even when a caller
    // supplies it.
    let mut edges: Vec<TestRenameEdge> = evidence
        .renames
        .iter()
        .filter(|edge| proof_grade_rename_edge(edge))
        .take(REVIEW_MAX_CHANGED_FILES * 2)
        .cloned()
        .collect();
    // Inventory fallback: a unique deleted test target and a unique added
    // test target whose declarations map one-to-one (proven body identity)
    // is a proven move even when git cannot pair an untracked destination.
    let deleted: Vec<&String> = old
        .keys()
        .filter(|path| !new.contains_key(*path))
        .filter(|path| is_supported_test_path(path))
        .collect();
    let added: Vec<&String> = new
        .keys()
        .filter(|path| !old.contains_key(*path))
        .filter(|path| is_supported_test_path(path))
        .collect();
    if deleted.len() == 1 && added.len() == 1 {
        let from = deleted[0];
        let to = added[0];
        if !edges
            .iter()
            .any(|edge| &edge.from == from && &edge.to == to)
            && declarations_move_uniquely(&old[from], &new[to], substitutions)
        {
            edges.push(TestRenameEdge {
                from: from.clone(),
                to: to.clone(),
                basis: "inventory_rename".to_string(),
            });
        }
    }
    edges
}

fn proof_grade_rename_edge(edge: &TestRenameEdge) -> bool {
    if edge.from.is_empty() || edge.to.is_empty() || edge.from == edge.to {
        return false;
    }
    let basis = edge.basis.trim().to_ascii_lowercase();
    !basis.is_empty()
        && !basis.contains("1%")
        && !basis.contains("1pct")
        && !basis.contains("pct")
        && !basis.contains("percent")
        && !basis.contains("weak")
        && !basis.contains("similarity")
}

fn declarations_move_uniquely(
    old: &[TestDeclaration],
    new: &[TestDeclaration],
    substitutions: &[(String, String)],
) -> bool {
    let mut claimed: BTreeSet<usize> = BTreeSet::new();
    for declaration in old {
        let matches: Vec<usize> = new
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                body_equivalence_digest(declaration, candidate, substitutions).is_some()
            })
            .map(|(index, _)| index)
            .collect();
        if matches.len() != 1 {
            return false;
        }
        // Two old declarations may never claim the SAME new declaration: the
        // move is not one-to-one, so it is not proven.
        if !claimed.insert(matches[0]) {
            return false;
        }
    }
    claimed.len() == old.len() && claimed.len() == new.len()
}

fn binding_targets(binding: &EvidenceBinding) -> Vec<String> {
    match binding {
        EvidenceBinding::TestSelector(selector) => vec![selector.clone()],
        EvidenceBinding::TestSelectors(selectors) => selectors.clone(),
        _ => Vec::new(),
    }
}

fn resolve_declaration<'a>(
    target: &str,
    inventory: &'a BTreeMap<String, Vec<TestDeclaration>>,
) -> Option<&'a TestDeclaration> {
    let by_selector: Vec<&TestDeclaration> = inventory
        .values()
        .flatten()
        .filter(|declaration| declaration.semantic_selector == target)
        .collect();
    if by_selector.len() == 1 {
        return by_selector.first().copied();
    }
    if by_selector.len() > 1 {
        return None;
    }
    let by_executable: Vec<&TestDeclaration> = inventory
        .values()
        .flatten()
        .filter(|declaration| declaration.executable_selector == target)
        .collect();
    (by_executable.len() == 1).then(|| by_executable[0])
}

fn resolve_new_path(
    declaration: &TestDeclaration,
    new: &BTreeMap<String, Vec<TestDeclaration>>,
    renames: &[TestRenameEdge],
) -> Option<(String, String)> {
    if new.contains_key(&declaration.path) {
        return Some((declaration.path.clone(), "same_target".to_string()));
    }
    let mut edges: Vec<&TestRenameEdge> = renames
        .iter()
        .filter(|edge| edge.from == declaration.path)
        .collect();
    edges.sort_by(|a, b| a.to.cmp(&b.to));
    edges.dedup_by(|a, b| a.to == b.to);
    if edges.len() == 1 {
        let edge = edges[0];
        if new.contains_key(&edge.to) {
            return Some((
                edge.to.clone(),
                format!("{}:{}->{}", edge.basis, edge.from, edge.to),
            ));
        }
    }
    None
}

fn was_unchanged_elsewhere(
    old: &TestDeclaration,
    candidate: &TestDeclaration,
    inventory: &BTreeMap<String, Vec<TestDeclaration>>,
    substitutions: &[(String, String)],
) -> bool {
    if candidate.semantic_selector == old.semantic_selector {
        return false;
    }
    inventory.values().flatten().any(|other| {
        other.semantic_selector != old.semantic_selector
            && other.executable_selector == candidate.executable_selector
            && body_equivalence_digest(other, candidate, substitutions).is_some()
    })
}

fn binding_for(declarations: &[&TestDeclaration]) -> EvidenceBinding {
    let mut targets: Vec<String> = Vec::new();
    for declaration in declarations {
        // Semantic selector FIRST: the claim re-point and the ledger's
        // `selector()` identity must name the test, not its executable
        // filter.
        if !targets.contains(&declaration.semantic_selector) {
            targets.push(declaration.semantic_selector.clone());
        }
        if declaration.executable_selector != declaration.semantic_selector
            && !targets.contains(&declaration.executable_selector)
        {
            targets.push(declaration.executable_selector.clone());
        }
        if let Some(target) = &declaration.enclosing_target {
            if declaration.executable_selector.contains(target) && !targets.contains(target) {
                targets.push(target.clone());
            }
        }
    }
    match targets.len() {
        0 => EvidenceBinding::Category,
        1 => EvidenceBinding::TestSelector(targets.remove(0)),
        _ => EvidenceBinding::TestSelectors(targets),
    }
}

/// The task's declared rename pairs (`Rename old to new`). Used to prove that
/// a test body changed ONLY by the declared symbol rename.
fn rename_pairs(goal: &str) -> Vec<(String, String)> {
    let raw: Vec<&str> = goal.split_whitespace().collect();
    let lower: Vec<String> = raw.iter().map(|word| word.to_ascii_lowercase()).collect();
    let Some(rename_at) = lower.iter().position(|word| word.contains("rename")) else {
        return Vec::new();
    };
    let Some(to_at) = (rename_at + 1..lower.len()).find(|index| {
        lower[*index].trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_') == "to"
    }) else {
        return Vec::new();
    };
    if to_at == 0 || to_at + 1 >= raw.len() {
        return Vec::new();
    }
    let old = raw[to_at - 1]
        .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .to_string();
    let new = raw[to_at + 1]
        .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .to_string();
    if old.is_empty() || new.is_empty() || old == new {
        Vec::new()
    } else {
        vec![(new, old)]
    }
}

/// TRUE when `new` is the EXACT old executable body under the declared
/// symbol rename pairs and the test's own proven name mapping. Returns the
/// digest of the normalized (mapped) body, which is the durable provenance
/// recorded on the binding update. Any difference in an assertion, fixture
/// setup, expected-error attribute, literal or control flow refuses.
fn body_equivalence_digest(
    old: &TestDeclaration,
    new: &TestDeclaration,
    substitutions: &[(String, String)],
) -> Option<String> {
    if old.body_identity.tokens.is_empty() || new.body_identity.tokens.is_empty() {
        return None;
    }
    let mut map: BTreeMap<&str, &str> = BTreeMap::new();
    for (from, to) in substitutions {
        map.insert(from.as_str(), to.as_str());
    }
    if old.executable_selector != new.executable_selector {
        map.insert(
            new.executable_selector.as_str(),
            old.executable_selector.as_str(),
        );
        // Language adapters whose executable selector is qualified
        // (`Class#method`, `module::test`) also carry the BARE callable name
        // in the declaration signature; map that too.
        let old_tail = callable_tail(&old.executable_selector);
        let new_tail = callable_tail(&new.executable_selector);
        if old_tail != new_tail {
            map.insert(new_tail, old_tail);
        }
        // A qualified JVM selector carries the enclosing class, which is
        // also the declared name of constructors/references in the shared
        // class context.
        if let (Some((old_class, _)), Some((new_class, _))) = (
            old.executable_selector.split_once('#'),
            new.executable_selector.split_once('#'),
        ) {
            if old_class != new_class {
                map.insert(new_class, old_class);
            }
        }
    }
    let mapped: Vec<IdentityToken> = new
        .body_identity
        .tokens
        .iter()
        .map(|token| match token {
            IdentityToken::Ident(name) => IdentityToken::Ident(
                map.get(name.as_str())
                    .map(|mapped| mapped.to_string())
                    .unwrap_or_else(|| name.clone()),
            ),
            other => other.clone(),
        })
        .collect();
    if mapped != old.body_identity.tokens {
        return None;
    }
    Some(old.body_identity.digest.clone())
}

fn callable_tail(selector: &str) -> &str {
    selector
        .rsplit_once('#')
        .map(|(_, tail)| tail)
        .or_else(|| selector.rsplit_once("::").map(|(_, tail)| tail))
        .unwrap_or(selector)
}

/// A helper used by tests and the persistence validator: a rebind must always
/// reset the evaluated state (the old proof never survives the new binding).
pub fn rebind_is_fresh(obligation: &ProofObligation) -> bool {
    obligation.state == ClaimState::Unknown
        && obligation.confidence_bps.is_none()
        && obligation.proofs.is_empty()
        && obligation.evidence_refs.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::contract::{AcceptedStrength, ContractOrigin, Requirement};

    fn obligation() -> ProofObligation {
        EngineeringContract::obligation_bound(
            "existing test `cart::totals_add_up` passes after the change",
            faktor_core::contract::ProofEvidenceKind::TestExecution,
            ContractOrigin::Native,
            Requirement::Required,
            EvidenceBinding::TestSelectors(vec![
                "cart::totals_add_up".into(),
                "totals_add_up".into(),
            ]),
            AcceptedStrength::Supported,
        )
    }

    fn contract() -> EngineeringContract {
        EngineeringContract {
            proof_obligations: vec![obligation()],
            ..Default::default()
        }
    }

    fn delta(path: &str, before: Option<&str>, after: Option<&str>) -> TestFileDelta {
        TestFileDelta {
            path: path.into(),
            before: before.map(str::to_string),
            after: after.map(str::to_string),
        }
    }

    const BEFORE: &str = r#"
#[test]
fn totals_add_up() {
    let items = fixture_items();
    assert_eq!(compute_total(&items), 30);
    assert_eq!(cart_total(&items), 30);
}
"#;
    const AFTER_SYMBOL_RENAME: &str = r#"
#[test]
fn total_cents_adds_up() {
    let items = fixture_items();
    assert_eq!(total_cents(&items), 30);
    assert_eq!(cart_total(&items), 30);
}
"#;
    const GOAL: &str =
        "Rename the public helper compute_total to total_cents across the crate, including tests.";

    fn evidence(before: Option<&str>, after: Option<&str>) -> TestRebindEvidence {
        TestRebindEvidence {
            files: vec![delta("tests/cart.rs", before, after)],
            renames: Vec::new(),
        }
    }

    fn moved_evidence(before: &str, after: &str) -> TestRebindEvidence {
        TestRebindEvidence {
            files: vec![
                delta("tests/cart.rs", Some(before), None),
                delta("tests/cart_total.rs", None, Some(after)),
            ],
            renames: vec![TestRenameEdge {
                from: "tests/cart.rs".into(),
                to: "tests/cart_total.rs".into(),
                basis: "git_rename".into(),
            }],
        }
    }

    fn assert_unresolved(contract: &EngineeringContract, outcome: &TestRebindOutcome) {
        assert!(outcome.rebinds.is_empty(), "{outcome:?}");
        assert_eq!(
            outcome.stale,
            vec![contract.proof_obligations[0].id.clone()]
        );
        let refusal = outcome
            .refusal(&contract.proof_obligations[0].id)
            .expect("a stale obligation carries a typed refusal");
        assert!(!refusal.old_selector.is_empty());
        assert!(!refusal.code.as_str().is_empty());
        assert!(contract.proof_obligations[0].binding_updates.is_empty());
        assert_eq!(
            contract.proof_obligations[0].evidence_binding,
            EvidenceBinding::TestSelectors(vec![
                "cart::totals_add_up".into(),
                "totals_add_up".into()
            ])
        );
    }

    #[test]
    fn unique_symbol_rename_rebinds_with_provenance_and_fresh_state() {
        let mut contract = contract();
        let outcome = rebind_test_obligations(
            &mut contract,
            GOAL,
            &evidence(Some(BEFORE), Some(AFTER_SYMBOL_RENAME)),
        );
        assert_eq!(outcome.rebinds.len(), 1, "{outcome:?}");
        assert!(outcome.stale.is_empty());
        let obligation = &contract.proof_obligations[0];
        assert_eq!(obligation.binding_updates.len(), 1);
        let update = &obligation.binding_updates[0];
        assert_eq!(
            update.old_binding,
            EvidenceBinding::TestSelectors(vec![
                "cart::totals_add_up".into(),
                "totals_add_up".into()
            ])
        );
        match &update.new_binding {
            EvidenceBinding::TestSelectors(targets) => {
                assert!(targets.contains(&"cart::total_cents_adds_up".to_string()));
                assert!(targets.contains(&"total_cents_adds_up".to_string()));
            }
            other => panic!("expected a test-selector binding, got {other:?}"),
        }
        assert_eq!(
            update.new_binding.selector(),
            Some("cart::total_cents_adds_up")
        );
        assert!(update.basis.contains("same_target"));
        assert!(update.basis.contains("match:unique"));
        assert!(update.basis.contains("body:equivalent"));
        let record = &outcome.rebinds[0];
        assert_eq!(record.body_digest.len(), 64);
        assert!(update.basis.contains(&record.body_digest));
        assert!(obligation.claim.contains("cart::total_cents_adds_up"));
        assert!(rebind_is_fresh(obligation));
        assert_eq!(outcome.updates.len(), 1);
    }

    #[test]
    fn valid_file_rename_with_identical_body_rebinds() {
        let mut contract = contract();
        let evidence = TestRebindEvidence {
            files: vec![
                delta("tests/cart.rs", Some(BEFORE), None),
                delta("tests/cart_total.rs", None, Some(BEFORE)),
            ],
            renames: vec![TestRenameEdge {
                from: "tests/cart.rs".into(),
                to: "tests/cart_total.rs".into(),
                basis: "git_rename".into(),
            }],
        };
        let outcome = rebind_test_obligations(&mut contract, GOAL, &evidence);
        assert_eq!(outcome.rebinds.len(), 1, "{outcome:?}");
        let update = &contract.proof_obligations[0].binding_updates[0];
        assert!(update
            .basis
            .contains("git_rename:tests/cart.rs->tests/cart_total.rs"));
        match &update.new_binding {
            EvidenceBinding::TestSelectors(targets) => {
                assert!(targets.contains(&"cart_total::totals_add_up".to_string()));
                assert!(targets.contains(&"totals_add_up".to_string()));
            }
            other => panic!("expected a test-selector binding, got {other:?}"),
        }
    }

    #[test]
    fn comments_and_trivia_only_still_prove_identical_body_identity() {
        let reformatted = r#"
#[test]
fn totals_add_up()
{
    let items = fixture_items( ); // setup comment
    assert_eq!( compute_total( &items ), 30 );
    /* second assertion */ assert_eq!( cart_total( &items ), 30 );
}
"#;
        let mut contract = contract();
        let outcome =
            rebind_test_obligations(&mut contract, GOAL, &moved_evidence(BEFORE, reformatted));
        assert_eq!(outcome.rebinds.len(), 1, "{outcome:?}");
        assert!(outcome.stale.is_empty());
    }

    #[test]
    fn moved_and_renamed_test_with_identical_body_rebinds() {
        let after = r#"
#[test]
fn total_cents_adds_up() {
    let items = fixture_items();
    assert_eq!(compute_total(&items), 30);
    assert_eq!(cart_total(&items), 30);
}
"#;
        let mut contract = contract();
        let evidence = TestRebindEvidence {
            files: vec![
                delta("tests/cart.rs", Some(BEFORE), None),
                delta("tests/cart_total.rs", None, Some(after)),
            ],
            renames: vec![TestRenameEdge {
                from: "tests/cart.rs".into(),
                to: "tests/cart_total.rs".into(),
                basis: "git_rename".into(),
            }],
        };
        let outcome = rebind_test_obligations(
            &mut contract,
            "Move the cart test to cart_total.rs and rename its test.",
            &evidence,
        );
        assert_eq!(outcome.rebinds.len(), 1, "{outcome:?}");
        match &contract.proof_obligations[0].binding_updates[0].new_binding {
            EvidenceBinding::TestSelectors(targets) => {
                assert!(targets.contains(&"cart_total::total_cents_adds_up".to_string()));
            }
            other => panic!("expected a test-selector binding, got {other:?}"),
        }
    }

    #[test]
    fn moved_unit_test_file_with_identical_body_rebinds() {
        let unit = r#"
#[cfg(test)]
mod tests {
    #[test]
    fn totals_add_up() {
        assert_eq!(compute_total(&[10, 20]), 30);
    }
}
"#;
        let mut contract = EngineeringContract {
            proof_obligations: vec![EngineeringContract::obligation_bound(
                "existing test `cart::tests::totals_add_up` passes after the change",
                faktor_core::contract::ProofEvidenceKind::TestExecution,
                ContractOrigin::Native,
                Requirement::Required,
                EvidenceBinding::TestSelector("cart::tests::totals_add_up".into()),
                AcceptedStrength::Supported,
            )],
            ..Default::default()
        };
        let evidence = TestRebindEvidence {
            files: vec![
                delta("src/cart.rs", Some(unit), None),
                delta("src/cart_total.rs", None, Some(unit)),
            ],
            renames: vec![TestRenameEdge {
                from: "src/cart.rs".into(),
                to: "src/cart_total.rs".into(),
                basis: "git_rename".into(),
            }],
        };
        let outcome = rebind_test_obligations(
            &mut contract,
            "Rename the cart module without changing behavior.",
            &evidence,
        );
        assert_eq!(outcome.rebinds.len(), 1, "{outcome:?}");
        assert_eq!(
            contract.proof_obligations[0].evidence_binding,
            EvidenceBinding::TestSelector("cart_total::tests::totals_add_up".into())
        );
    }

    #[test]
    fn same_name_vacuous_assertion_never_rebinds() {
        let weakened = r#"
#[test]
fn totals_add_up() {
    assert!(true);
}
"#;
        let mut contract = contract();
        let outcome =
            rebind_test_obligations(&mut contract, GOAL, &moved_evidence(BEFORE, weakened));
        assert_unresolved(&contract, &outcome);
        assert_eq!(
            outcome
                .refusal(&contract.proof_obligations[0].id)
                .unwrap()
                .code,
            RebindRefusalCode::BodyInequivalent
        );
    }

    #[test]
    fn removed_assertion_never_rebinds() {
        let weakened = r#"
#[test]
fn totals_add_up() {
    let items = fixture_items();
    let _ = compute_total(&items);
}
"#;
        let mut contract = contract();
        let outcome =
            rebind_test_obligations(&mut contract, GOAL, &moved_evidence(BEFORE, weakened));
        assert_unresolved(&contract, &outcome);
    }

    #[test]
    fn fixture_setup_removed_never_rebinds() {
        let weakened = r#"
#[test]
fn totals_add_up() {
    assert_eq!(compute_total(&[10, 20]), 30);
    assert_eq!(cart_total(&[10, 20]), 30);
}
"#;
        let mut contract = contract();
        let outcome =
            rebind_test_obligations(&mut contract, GOAL, &moved_evidence(BEFORE, weakened));
        assert_unresolved(&contract, &outcome);
    }

    #[test]
    fn expected_error_to_expected_success_never_rebinds() {
        let before = r#"
#[test]
#[should_panic(expected = "division by zero")]
fn totals_add_up() {
    assert_eq!(compute_total(&[10, 20]), 30);
}
"#;
        let after = r#"
#[test]
fn totals_add_up() {
    assert_eq!(compute_total(&[10, 20]), 30);
}
"#;
        let mut contract = contract();
        let outcome = rebind_test_obligations(&mut contract, GOAL, &moved_evidence(before, after));
        assert_unresolved(&contract, &outcome);
    }

    #[test]
    fn one_percent_similarity_rewrite_with_same_name_never_rebinds() {
        let before = r#"
#[test]
fn totals_add_up() {
    let items = fixture_items();
    assert_eq!(compute_total(&items), 30);
    assert_eq!(cart_total(&items), 30);
}
"#;
        let after = r#"
#[test]
fn totals_add_up() {
    let rows = load_rows();
    let mut seen = 0usize;
    for row in rows {
        seen += row.weight;
    }
    assert!(seen > 0);
}
"#;
        let mut contract = contract();
        let evidence = TestRebindEvidence {
            files: vec![
                delta("tests/cart.rs", Some(before), None),
                delta("tests/cart_total.rs", None, Some(after)),
            ],
            renames: vec![TestRenameEdge {
                from: "tests/cart.rs".into(),
                to: "tests/cart_total.rs".into(),
                basis: "git_rename".into(),
            }],
        };
        let outcome = rebind_test_obligations(&mut contract, GOAL, &evidence);
        assert_unresolved(&contract, &outcome);
    }

    #[test]
    fn ambiguous_or_duplicate_candidates_stay_unresolved_typed() {
        let duplicate = r#"
#[test]
fn total_cents_adds_up() {
    let items = fixture_items();
    assert_eq!(total_cents(&items), 30);
    assert_eq!(cart_total(&items), 30);
}
#[test]
fn total_cents_also_adds_up() {
    let items = fixture_items();
    assert_eq!(total_cents(&items), 30);
    assert_eq!(cart_total(&items), 30);
}
"#;
        let mut contract = contract();
        let outcome = rebind_test_obligations(
            &mut contract,
            GOAL,
            &evidence(Some(BEFORE), Some(duplicate)),
        );
        assert!(outcome.rebinds.is_empty(), "{outcome:?}");
        assert_eq!(
            outcome.stale,
            vec![contract.proof_obligations[0].id.clone()]
        );
        assert_eq!(
            outcome
                .refusal(&contract.proof_obligations[0].id)
                .unwrap()
                .code,
            RebindRefusalCode::AmbiguousCandidate
        );
        assert!(contract.proof_obligations[0].binding_updates.is_empty());
    }

    #[test]
    fn no_rename_task_never_rebinds() {
        let mut contract = contract();
        let outcome = rebind_test_obligations(
            &mut contract,
            "Fix the failing totals test.",
            &evidence(Some(BEFORE), Some(AFTER_SYMBOL_RENAME)),
        );
        assert!(outcome.rebinds.is_empty());
        assert_eq!(outcome.stale.len(), 1);
        assert_eq!(
            outcome
                .refusal(&contract.proof_obligations[0].id)
                .unwrap()
                .code,
            RebindRefusalCode::TaskNotRename
        );
        assert!(contract.proof_obligations[0].binding_updates.is_empty());
    }

    #[test]
    fn unprovable_body_change_stays_unresolved() {
        let changed = r#"
#[test]
fn total_cents_adds_up() {
    let items = fixture_items();
    assert_eq!(total_cents(&items), 999);
    assert_eq!(cart_total(&items), 30);
}
"#;
        let mut contract = contract();
        let outcome =
            rebind_test_obligations(&mut contract, GOAL, &evidence(Some(BEFORE), Some(changed)));
        assert!(outcome.rebinds.is_empty());
        assert_eq!(outcome.stale.len(), 1);
    }

    #[test]
    fn inventory_move_requires_one_to_one_declaration_identity() {
        let before = r#"
#[test]
fn totals_add_up() {
    assert_eq!(compute_total(&[10, 20]), 30);
}
#[test]
fn totals_add_up_again() {
    assert_eq!(compute_total(&[10, 20]), 30);
}
"#;
        let after = r#"
#[test]
fn total_cents_adds_up() {
    assert_eq!(total_cents(&[10, 20]), 30);
}
#[test]
fn unrelated_test() {
    assert!(other());
}
"#;
        let mut contract = contract();
        let evidence = TestRebindEvidence {
            files: vec![
                delta("tests/cart.rs", Some(before), None),
                delta("tests/cart_total.rs", None, Some(after)),
            ],
            renames: Vec::new(),
        };
        let outcome = rebind_test_obligations(&mut contract, GOAL, &evidence);
        assert!(outcome.rebinds.is_empty(), "{outcome:?}");
        assert_eq!(
            outcome
                .refusal(&contract.proof_obligations[0].id)
                .unwrap()
                .code,
            RebindRefusalCode::NoProvenMove
        );
    }

    #[test]
    fn cross_obligation_duplicate_mapping_is_ambiguous() {
        let mut second = obligation();
        second.id = "po1:second".into();
        second.claim = "existing test `cart::totals_add_up` passes again".into();
        let mut contract = EngineeringContract {
            proof_obligations: vec![obligation(), second],
            ..Default::default()
        };
        let outcome = rebind_test_obligations(
            &mut contract,
            GOAL,
            &evidence(Some(BEFORE), Some(AFTER_SYMBOL_RENAME)),
        );
        assert!(outcome.rebinds.is_empty(), "{outcome:?}");
        assert_eq!(outcome.stale.len(), 2);
        assert!(outcome
            .refusals
            .iter()
            .all(|refusal| refusal.code == RebindRefusalCode::CrossObligationAmbiguous));
    }
}
