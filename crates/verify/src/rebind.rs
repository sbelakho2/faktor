//! Deterministic proof rebinding for renamed tests.
//!
//! A contract compiled BEFORE a rename binds its test obligation to the old
//! selector (`cart::totals_add_up`). When the task itself renames that test
//! (file and/or function), the old selector stops existing and the obligation
//! can never be discharged by the renamed test unless the binding is updated.
//! This module performs that update ONLY when the mapping is proven:
//!
//! * a rename edge exists (git rename detection, or the verification
//!   inventory's unique deleted→added test-target pair),
//! * the old and new declarations are the same test target (same path or the
//!   proven rename edge), and
//! * exactly one new declaration matches the old one (same executable test
//!   name, or an equivalent body under the task's declared rename pairs).
//!
//! Anything else stays unresolved and the caller records the obligation as
//! stale, so a required claim can never be satisfied by a coarse/vacuous
//! selector. Every applied update carries old→new + basis provenance.

use std::collections::BTreeMap;

use faktor_core::contract::{ClaimState, EngineeringContract, EvidenceBinding, ProofObligation};

/// One changed test-bearing file: the base content and the candidate content.
/// A deletion has `after = None`; a creation has `before = None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFileDelta {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A PROVEN file rename edge (from a real authority: git rename detection or
/// the verification inventory's unique test-move pair).
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
    pub rebinds: Vec<TestRebindRecord>,
}

impl TestRebindOutcome {
    pub fn rebind_count(&self) -> usize {
        self.rebinds.len()
    }

    pub fn is_stale(&self, obligation_id: &str) -> bool {
        self.stale.iter().any(|id| id == obligation_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Declaration {
    path: String,
    selector: String,
    executable: String,
    target: Option<String>,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    obligation_index: usize,
    old_binding: EvidenceBinding,
    new_binding: EvidenceBinding,
    basis: String,
    old_selector: String,
    new_selector: String,
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
    let mut blocked: Vec<String> = Vec::new();
    for (index, obligation) in contract.proof_obligations.iter().enumerate() {
        let targets = binding_targets(&obligation.evidence_binding);
        if targets.is_empty() {
            continue;
        }
        let mut old_declarations: Vec<&Declaration> = targets
            .iter()
            .filter_map(|target| resolve_declaration(target, &old_inventory))
            .collect();
        old_declarations.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.selector.cmp(&b.selector))
        });
        old_declarations.dedup_by(|a, b| a.path == b.path && a.selector == b.selector);
        if old_declarations.is_empty() {
            continue;
        }
        // A declaration selector that still exists on the candidate needs no
        // rebinding; a derived coarse filter alone is not a declaration.
        let stale: Vec<&Declaration> = old_declarations
            .into_iter()
            .filter(|declaration| {
                !new_inventory
                    .values()
                    .flatten()
                    .any(|candidate| candidate.selector == declaration.selector)
            })
            .collect();
        if stale.is_empty() {
            continue;
        }
        if !goal_is_rename {
            blocked.push(obligation.id.clone());
            continue;
        }
        let mut mapped: Vec<&Declaration> = Vec::new();
        let mut basis_parts: Vec<String> = Vec::new();
        let mut unresolved = false;
        for declaration in &stale {
            let (new_path, edge_basis) =
                match resolve_new_path(declaration, &new_inventory, &renames) {
                    Some(found) => found,
                    None => {
                        unresolved = true;
                        break;
                    }
                };
            let Some(candidates) = new_inventory.get(&new_path) else {
                unresolved = true;
                break;
            };
            let matching: Vec<&Declaration> = candidates
                .iter()
                .filter(|candidate| {
                    candidate.executable == declaration.executable
                        || equivalent_body(declaration, candidate, &substitutions)
                })
                .filter(|candidate| {
                    !was_unchanged_elsewhere(declaration, candidate, &old_inventory, &substitutions)
                })
                .collect();
            if matching.len() != 1 {
                unresolved = true;
                break;
            }
            mapped.push(matching[0]);
            basis_parts.push(format!(
                "{edge_basis};{}:{}->{}",
                declaration.path, declaration.selector, matching[0].selector
            ));
        }
        if unresolved || mapped.is_empty() {
            blocked.push(obligation.id.clone());
            continue;
        }
        let new_binding = binding_for(&mapped);
        if new_binding == obligation.evidence_binding {
            continue;
        }
        let basis = format!("match:unique;body:equivalent;{}", basis_parts.join("; "));
        let mut candidate: Vec<(String, String)> = mapped
            .iter()
            .map(|declaration| (declaration.path.clone(), declaration.executable.clone()))
            .collect();
        candidate.sort();
        candidate.dedup();
        plans.push(Plan {
            obligation_index: index,
            old_binding: obligation.evidence_binding.clone(),
            new_binding,
            basis,
            old_selector: stale
                .iter()
                .map(|declaration| declaration.selector.clone())
                .collect::<Vec<_>>()
                .join(","),
            new_selector: mapped
                .iter()
                .map(|declaration| declaration.selector.clone())
                .collect::<Vec<_>>()
                .join(","),
            candidate,
        });
    }

    // Cross-obligation uniqueness: two old declarations may never map to the
    // same new declaration. Duplicate candidates make BOTH plans ambiguous.
    let mut claimed: BTreeMap<(String, String), std::collections::BTreeSet<usize>> =
        BTreeMap::new();
    for (index, plan) in plans.iter().enumerate() {
        for candidate in &plan.candidate {
            claimed.entry(candidate.clone()).or_default().insert(index);
        }
    }
    let ambiguous: std::collections::BTreeSet<usize> = claimed
        .values()
        .filter(|owners| owners.len() > 1)
        .flatten()
        .copied()
        .collect();
    for (index, plan) in plans.into_iter().enumerate() {
        let obligation = &mut contract.proof_obligations[plan.obligation_index];
        if ambiguous.contains(&index) {
            blocked.push(obligation.id.clone());
            continue;
        }
        if obligation.rebind_evidence_binding(plan.new_binding, plan.basis.clone()) {
            outcome.rebinds.push(TestRebindRecord {
                obligation_id: obligation.id.clone(),
                old_selector: plan.old_selector,
                new_selector: plan.new_selector,
                basis: plan.basis,
            });
            outcome.updates.push(obligation.clone());
        }
    }
    outcome.stale = blocked;
    outcome.stale.sort();
    outcome.stale.dedup();
    outcome
}

fn inventory(files: &[TestFileDelta], before: bool) -> BTreeMap<String, Vec<Declaration>> {
    let mut out: BTreeMap<String, Vec<Declaration>> = BTreeMap::new();
    for file in files {
        let content = if before { &file.before } else { &file.after };
        let Some(content) = content else {
            continue;
        };
        let declarations: Vec<Declaration> =
            faktor_core::contract_compile::rust_test_declarations_for_path(&file.path, content)
                .into_iter()
                .map(|declaration| Declaration {
                    path: file.path.clone(),
                    selector: declaration.selector,
                    executable: declaration.executable,
                    target: declaration.target,
                    body: declaration.body,
                })
                .collect();
        if !declarations.is_empty() {
            out.insert(file.path.clone(), declarations);
        }
    }
    out
}

fn rename_edges(
    evidence: &TestRebindEvidence,
    old: &BTreeMap<String, Vec<Declaration>>,
    new: &BTreeMap<String, Vec<Declaration>>,
    substitutions: &[(String, String)],
) -> Vec<TestRenameEdge> {
    let mut edges = evidence.renames.clone();
    // Inventory fallback: a unique deleted test target and a unique added
    // test target whose declarations map one-to-one is a proven move even
    // when git cannot pair an untracked destination.
    let deleted: Vec<&String> = old
        .keys()
        .filter(|path| !new.contains_key(*path))
        .filter(|path| path.ends_with(".rs"))
        .collect();
    let added: Vec<&String> = new
        .keys()
        .filter(|path| !old.contains_key(*path))
        .filter(|path| path.ends_with(".rs"))
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

fn declarations_move_uniquely(
    old: &[Declaration],
    new: &[Declaration],
    substitutions: &[(String, String)],
) -> bool {
    let mut mapped = 0usize;
    for declaration in old {
        let matches: Vec<&Declaration> = new
            .iter()
            .filter(|candidate| {
                candidate.executable == declaration.executable
                    || equivalent_body(declaration, candidate, substitutions)
            })
            .collect();
        if matches.len() != 1 {
            return false;
        }
        mapped += 1;
    }
    mapped == old.len() && mapped == new.len()
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
    inventory: &'a BTreeMap<String, Vec<Declaration>>,
) -> Option<&'a Declaration> {
    let by_selector: Vec<&Declaration> = inventory
        .values()
        .flatten()
        .filter(|declaration| declaration.selector == target)
        .collect();
    if by_selector.len() == 1 {
        return by_selector.first().copied();
    }
    if by_selector.len() > 1 {
        return None;
    }
    let by_executable: Vec<&Declaration> = inventory
        .values()
        .flatten()
        .filter(|declaration| declaration.executable == target)
        .collect();
    (by_executable.len() == 1).then(|| by_executable[0])
}

fn resolve_new_path(
    declaration: &Declaration,
    new: &BTreeMap<String, Vec<Declaration>>,
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
    old: &Declaration,
    candidate: &Declaration,
    inventory: &BTreeMap<String, Vec<Declaration>>,
    substitutions: &[(String, String)],
) -> bool {
    if candidate.selector == old.selector {
        return false;
    }
    inventory.values().flatten().any(|other| {
        other.selector != old.selector
            && other.executable == candidate.executable
            && equivalent_body(other, candidate, substitutions)
    })
}

fn binding_for(declarations: &[&Declaration]) -> EvidenceBinding {
    let mut targets: Vec<String> = Vec::new();
    for declaration in declarations {
        // Semantic selector FIRST: the claim re-point and the ledger's
        // `selector()` identity must name the test, not its executable
        // filter.
        if !targets.contains(&declaration.selector) {
            targets.push(declaration.selector.clone());
        }
        if declaration.executable != declaration.selector
            && !targets.contains(&declaration.executable)
        {
            targets.push(declaration.executable.clone());
        }
        if let Some(target) = &declaration.target {
            if declaration.executable.contains(target) && !targets.contains(target) {
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

/// TRUE when the candidate body is the old body under the declared rename
/// pairs (and the test's own rename). Both sides are tokenized, whitespace is
/// ignored, and string literals are opaque (never rewritten).
fn equivalent_body(
    old: &Declaration,
    new: &Declaration,
    substitutions: &[(String, String)],
) -> bool {
    if old.body.is_empty() || new.body.is_empty() {
        return false;
    }
    let mut map: BTreeMap<&str, &str> = BTreeMap::new();
    for (from, to) in substitutions {
        map.insert(from.as_str(), to.as_str());
    }
    if old.executable != new.executable {
        map.insert(new.executable.as_str(), old.executable.as_str());
    }
    let old_tokens = tokens(&old.body);
    let new_tokens = tokens(&new.body)
        .into_iter()
        .map(|token| match token {
            Token::Ident(name) => Token::Ident(
                map.get(name.as_str())
                    .map(|mapped| mapped.to_string())
                    .unwrap_or(name),
            ),
            other => other,
        })
        .collect::<Vec<_>>();
    old_tokens == new_tokens
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Literal(String),
    Punct(char),
}

fn tokens(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            out.push(Token::Ident(source[start..index].to_string()));
            continue;
        }
        if byte.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || bytes[index] == b'_'
                    || bytes[index] == b'.')
            {
                index += 1;
            }
            out.push(Token::Literal(source[start..index].to_string()));
            continue;
        }
        if byte == b'"' {
            let start = index;
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\\' {
                    index = (index + 2).min(bytes.len());
                    continue;
                }
                if bytes[index] == b'"' {
                    index += 1;
                    break;
                }
                index += 1;
            }
            out.push(Token::Literal(source[start..index].to_string()));
            continue;
        }
        out.push(Token::Punct(byte as char));
        index += 1;
    }
    out
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
    assert_eq!(compute_total(&[10, 20]), 30);
    assert_eq!(cart_total(&[1, 2, 3]), 6);
}
"#;
    const AFTER_SYMBOL_RENAME: &str = r#"
#[test]
fn total_cents_adds_up() {
    assert_eq!(total_cents(&[10, 20]), 30);
    assert_eq!(cart_total(&[1, 2, 3]), 6);
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
        assert!(obligation.binding_updates.len() == 1);
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
        assert!(obligation.claim.contains("cart::total_cents_adds_up"));
        assert!(rebind_is_fresh(obligation));
        assert_eq!(outcome.updates.len(), 1);
    }

    #[test]
    fn file_rename_with_git_edge_rebinds_the_new_selector() {
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
    fn ambiguous_or_duplicate_candidates_stay_unresolved_typed() {
        let duplicate = r#"
#[test]
fn total_cents_adds_up() {
    assert_eq!(total_cents(&[10, 20]), 30);
}
#[test]
fn total_cents_also_adds_up() {
    assert_eq!(total_cents(&[10, 20]), 30);
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
    fn no_rename_task_never_rebinds() {
        let mut contract = contract();
        let outcome = rebind_test_obligations(
            &mut contract,
            "Fix the failing totals test.",
            &evidence(Some(BEFORE), Some(AFTER_SYMBOL_RENAME)),
        );
        assert!(outcome.rebinds.is_empty());
        assert_eq!(outcome.stale.len(), 1);
        assert!(contract.proof_obligations[0].binding_updates.is_empty());
    }

    #[test]
    fn unprovable_body_change_stays_unresolved() {
        let changed = r#"
#[test]
fn total_cents_adds_up() {
    assert_eq!(total_cents(&[10, 20]), 999);
}
"#;
        let mut contract = contract();
        let outcome =
            rebind_test_obligations(&mut contract, GOAL, &evidence(Some(BEFORE), Some(changed)));
        assert!(outcome.rebinds.is_empty());
        assert_eq!(outcome.stale.len(), 1);
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
    }
}
