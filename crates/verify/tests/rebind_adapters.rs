//! Real-fixture regressions for the neutral declaration adapters and the
//! body-identity gate: a rename edge alone never migrates a proof binding.

use faktor_core::contract::{
    AcceptedStrength, ContractOrigin, EngineeringContract, EvidenceBinding, ProofEvidenceKind,
    Requirement,
};
use faktor_verify::rebind::{
    rebind_test_obligations, RebindRefusalCode, TestFileDelta, TestRebindEvidence, TestRenameEdge,
};
use faktor_verify::test_declaration::{adapter_for_path, TestDeclaration};

const TS_OLD: &str = include_str!("fixtures/rebind/cart.test.ts");
const TS_NEW: &str = include_str!("fixtures/rebind/totals.test.ts");
const TS_WEAK: &str = include_str!("fixtures/rebind/weakened.test.ts");
const TS_HOOKS: &str = include_str!("fixtures/rebind/hooks.test.ts");
const TS_HOOKS_REMOVED: &str = include_str!("fixtures/rebind/hooks-removed.test.ts");
const TS_SKIPPED: &str = include_str!("fixtures/rebind/skipped.test.ts");
const JAVA_OLD: &str = include_str!("fixtures/rebind/CartTest.java");
const JAVA_NEW: &str = include_str!("fixtures/rebind/CartTotalsTest.java");
const JAVA_WEAK_OLD: &str = include_str!("fixtures/rebind/WeakCartTest.java");
const JAVA_WEAK_NEW: &str = include_str!("fixtures/rebind/WeakCartSuccessTest.java");
const KOTLIN_OLD: &str = include_str!("fixtures/rebind/CartTest.kt");
const KOTLIN_NEW: &str = include_str!("fixtures/rebind/CartTotalsTest.kt");
const PY_OLD: &str = include_str!("fixtures/rebind/test_cart.py");
const PY_NEW: &str = include_str!("fixtures/rebind/test_totals.py");
const PY_WEAK: &str = include_str!("fixtures/rebind/test_weakened.py");
const GO_OLD: &str = include_str!("fixtures/rebind/cart_test.go");
const GO_NEW: &str = include_str!("fixtures/rebind/totals_test.go");
const GO_WEAK: &str = include_str!("fixtures/rebind/weakened_test.go");

fn declarations(path: &str, content: &str) -> Vec<TestDeclaration> {
    let adapter = adapter_for_path(path).unwrap_or_else(|| panic!("no adapter for {path}"));
    let found = adapter.declarations(path, content);
    assert!(!found.is_empty(), "no declaration proven in {path}");
    found
}

fn binding(path: &str, content: &str) -> EvidenceBinding {
    let declaration = &declarations(path, content)[0];
    let mut targets = vec![declaration.semantic_selector.clone()];
    if declaration.executable_selector != declaration.semantic_selector {
        targets.push(declaration.executable_selector.clone());
    }
    if let Some(target) = &declaration.enclosing_target {
        if declaration.executable_selector.contains(target) && !targets.contains(target) {
            targets.push(target.clone());
        }
    }
    if targets.len() == 1 {
        EvidenceBinding::TestSelector(targets.remove(0))
    } else {
        EvidenceBinding::TestSelectors(targets)
    }
}

fn contract_for(path: &str, content: &str) -> EngineeringContract {
    let claim = format!(
        "test `{}` passes",
        declarations(path, content)[0].semantic_selector
    );
    EngineeringContract {
        proof_obligations: vec![EngineeringContract::obligation_bound(
            &claim,
            ProofEvidenceKind::TestExecution,
            ContractOrigin::Native,
            Requirement::Required,
            binding(path, content),
            AcceptedStrength::Supported,
        )],
        ..Default::default()
    }
}

fn moved(old_path: &str, old: &str, new_path: &str, new: &str) -> TestRebindEvidence {
    TestRebindEvidence {
        files: vec![
            TestFileDelta {
                path: old_path.into(),
                before: Some(old.into()),
                after: None,
            },
            TestFileDelta {
                path: new_path.into(),
                before: None,
                after: Some(new.into()),
            },
        ],
        renames: vec![TestRenameEdge {
            from: old_path.into(),
            to: new_path.into(),
            basis: "git_rename".into(),
        }],
    }
}

fn assert_migrated(
    contract: &EngineeringContract,
    outcome: &faktor_verify::rebind::TestRebindOutcome,
) {
    assert_eq!(outcome.rebinds.len(), 1, "{outcome:?}");
    assert!(outcome.stale.is_empty(), "{outcome:?}");
    let update = &contract.proof_obligations[0].binding_updates[0];
    assert!(!update.basis.is_empty());
    assert!(update.basis.contains("body:equivalent"));
    assert_eq!(outcome.rebinds[0].body_digest.len(), 64);
    assert!(faktor_verify::rebind::rebind_is_fresh(
        &contract.proof_obligations[0]
    ));
}

fn assert_refused(
    contract: &EngineeringContract,
    outcome: &faktor_verify::rebind::TestRebindOutcome,
) {
    assert!(outcome.rebinds.is_empty(), "{outcome:?}");
    assert_eq!(
        outcome.stale,
        vec![contract.proof_obligations[0].id.clone()]
    );
    assert!(contract.proof_obligations[0].binding_updates.is_empty());
}

const GOAL: &str = "Rename the test without changing behavior.";

#[test]
fn javascript_ts_adapters_prove_valid_moves_and_refuse_rewrites() {
    for (old_path, new_path) in [
        ("test/cart.test.ts", "test/totals.test.ts"),
        ("test/cart.test.js", "test/totals.test.js"),
    ] {
        let mut contract = contract_for(old_path, TS_OLD);
        let outcome = rebind_test_obligations(
            &mut contract,
            GOAL,
            &moved(old_path, TS_OLD, new_path, TS_NEW),
        );
        assert_migrated(&contract, &outcome);
        assert!(contract.proof_obligations[0]
            .claim
            .contains("cart totals::total cents add up"));
    }
    // Same describe + same test name, moved file, body weakened: refused.
    let mut contract = contract_for("test/cart.test.ts", TS_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "test/cart.test.ts",
            TS_OLD,
            "test/weakened.test.ts",
            TS_WEAK,
        ),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn jvm_java_and_kotlin_adapters_prove_valid_moves_and_refuse_execution_changes() {
    let java_old = "src/test/java/com/example/invoicing/CartTest.java";
    let java_new = "src/test/java/com/example/invoicing/CartTotalsTest.java";
    let mut contract = contract_for(java_old, JAVA_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(java_old, JAVA_OLD, java_new, JAVA_NEW),
    );
    assert_migrated(&contract, &outcome);

    let kotlin_old = "src/test/kotlin/com/example/invoicing/CartTest.kt";
    let kotlin_new = "src/test/kotlin/com/example/invoicing/CartTotalsTest.kt";
    let mut contract = contract_for(kotlin_old, KOTLIN_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(kotlin_old, KOTLIN_OLD, kotlin_new, KOTLIN_NEW),
    );
    assert_migrated(&contract, &outcome);

    // expected-error -> expected-success (same class/method/body, attribute
    // removed): refused.
    let weak_old = "src/test/java/com/example/invoicing/WeakCartTest.java";
    let weak_new = "src/test/java/com/example/invoicing/WeakCartSuccessTest.java";
    let mut contract = contract_for(weak_old, JAVA_WEAK_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(weak_old, JAVA_WEAK_OLD, weak_new, JAVA_WEAK_NEW),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn python_adapter_proves_valid_moves_and_refuses_removed_assertions() {
    let old_path = "tests/test_cart.py";
    let new_path = "tests/test_totals.py";
    let mut contract = contract_for(old_path, PY_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(old_path, PY_OLD, new_path, PY_NEW),
    );
    assert_migrated(&contract, &outcome);

    // Same file stem (same semantic selector), different directory, body
    // replaced with `assert True`: refused.
    let mut contract = contract_for("tests/test_cart.py", PY_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("tests/test_cart.py", PY_OLD, "suite/test_cart.py", PY_WEAK),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn go_adapter_proves_valid_moves_and_refuses_removed_assertions() {
    let mut contract = contract_for("cart_test.go", GO_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("cart_test.go", GO_OLD, "totals_test.go", GO_NEW),
    );
    assert_migrated(&contract, &outcome);

    // Same package + same Test function name, moved file, body gutted:
    // refused.
    let mut contract = contract_for("cart_test.go", GO_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("cart_test.go", GO_OLD, "weakened_test.go", GO_WEAK),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn adapters_never_emit_dynamic_or_unprovable_declarations() {
    let dynamic = r#"
const name = "dynamic";
test(name, () => {
  assert.ok(true);
});
test.each([[1, 2]])("table %s", (a, b) => {
  assert.equal(a + b, 3);
});
"#;
    assert!(adapter_for_path("test/dynamic.test.js")
        .unwrap()
        .declarations("test/dynamic.test.js", dynamic)
        .is_empty());

    // A named hook reference is not a provable setup body, so its scope's
    // tests are withheld instead of being proven with an incomplete identity.
    let named_hook = r#"
const setup = () => {
  globalThis.items = [10, 20];
};
beforeEach(setup);
test("totals add up", () => {
  assert.equal(computeTotal(globalThis.items), 30);
});
"#;
    assert!(adapter_for_path("test/named-hook.test.js")
        .unwrap()
        .declarations("test/named-hook.test.js", named_hook)
        .is_empty());

    let python = r#"
def test_ok():
    assert True

class TestGroup:
    def test_also_ok(self):
        assert True
"#;
    let found = declarations("tests/test_group.py", python);
    assert_eq!(found.len(), 2);
    assert!(found.iter().any(|declaration| {
        declaration.semantic_selector == "test_group::TestGroup::test_also_ok"
    }));
}

#[test]
fn rust_body_identity_ignores_literal_braces_and_still_proves_an_exact_move() {
    let body = r##"
#[test]
fn totals_add_up() {
    let open = "{";
    let close = '}';
    /* a } comment is not a delimiter */
    assert_eq!(compute_total(&[10, 20]), 30);
}
"##;
    let weakened = r##"
#[test]
fn totals_add_up() {
    let open = "{";
    let close = '}';
    /* a } comment is not a delimiter */
}
"##;

    // The exact same declaration moved to a new file still migrates.
    let mut contract = contract_for("tests/cart.rs", body);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("tests/cart.rs", body, "tests/cart_total.rs", body),
    );
    assert_migrated(&contract, &outcome);

    // Removing the assertion after a brace-bearing literal/comment must
    // change the body identity; a naive brace count truncates before it.
    let mut contract = contract_for("tests/cart.rs", body);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("tests/cart.rs", body, "tests/cart_total.rs", weakened),
    );
    assert_refused(&contract, &outcome);
    assert_eq!(
        outcome
            .refusal(&contract.proof_obligations[0].id)
            .unwrap()
            .code,
        RebindRefusalCode::BodyInequivalent
    );
}

#[test]
fn weak_similarity_authority_is_never_used_for_a_migration() {
    // Two deleted and two added test files defeat the unique inventory
    // fallback, so the only path relation offered is the weak git edge.
    let old = "#[test]\nfn totals_add_up() { assert_eq!(compute_total(&[10, 20]), 30); }\n";
    let other = "#[test]\nfn other_test() { assert!(other()); }\n";
    let evidence = TestRebindEvidence {
        files: vec![
            TestFileDelta {
                path: "tests/cart.rs".into(),
                before: Some(old.into()),
                after: None,
            },
            TestFileDelta {
                path: "tests/other.rs".into(),
                before: Some(other.into()),
                after: None,
            },
            TestFileDelta {
                path: "tests/cart_total.rs".into(),
                before: None,
                after: Some(old.into()),
            },
            TestFileDelta {
                path: "tests/other_total.rs".into(),
                before: None,
                after: Some(other.into()),
            },
        ],
        renames: vec![TestRenameEdge {
            from: "tests/cart.rs".into(),
            to: "tests/cart_total.rs".into(),
            basis: "git_rename_1pct".into(),
        }],
    };
    let mut contract = contract_for("tests/cart.rs", old);
    let outcome = rebind_test_obligations(&mut contract, GOAL, &evidence);
    assert_refused(&contract, &outcome);
    assert_eq!(
        outcome
            .refusal(&contract.proof_obligations[0].id)
            .unwrap()
            .code,
        RebindRefusalCode::NoProvenMove
    );
}

#[test]
fn javascript_fixture_hooks_and_suite_modifiers_are_part_of_executable_identity() {
    // Removing a beforeEach that supplies the fixture changes execution even
    // though the test callback text is byte-identical.
    let mut contract = contract_for("test/hooks.test.ts", TS_HOOKS);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "test/hooks.test.ts",
            TS_HOOKS,
            "test/hooks-removed.test.ts",
            TS_HOOKS_REMOVED,
        ),
    );
    assert_refused(&contract, &outcome);
    assert_eq!(
        outcome
            .refusal(&contract.proof_obligations[0].id)
            .unwrap()
            .code,
        RebindRefusalCode::BodyInequivalent
    );

    // `describe.skip` makes the identical test body non-executable: the
    // adapter withholds the declaration rather than proving preservation.
    let mut contract = contract_for("test/cart.test.ts", TS_OLD);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "test/cart.test.ts",
            TS_OLD,
            "test/skipped.test.ts",
            TS_SKIPPED,
        ),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn jvm_shared_setup_is_part_of_the_proven_test_identity() {
    let old = r#"
package com.example.invoicing;

import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.assertEquals;

class CartTest {
    CartTest() {}

    private int[] items;

    @BeforeEach
    void setUp() {
        items = new int[] { 10, 20 };
    }

    @Test
    void totalsAddUp() {
        assertEquals(30, computeTotal(items));
    }
}
"#;
    let removed = r#"
package com.example.invoicing;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.assertEquals;

class CartTest {
    private int[] items;

    @Test
    void totalsAddUp() {
        assertEquals(30, computeTotal(items));
    }
}
"#;
    let mut contract = contract_for("src/test/java/com/example/invoicing/CartTest.java", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "src/test/java/com/example/invoicing/CartTest.java",
            old,
            "src/test/java/com/example/invoicing/CartMovedTest.java",
            removed,
        ),
    );
    assert_refused(&contract, &outcome);
    assert_eq!(
        outcome
            .refusal(&contract.proof_obligations[0].id)
            .unwrap()
            .code,
        RebindRefusalCode::BodyInequivalent
    );

    // The unchanged fixture still proves an exact move (the class rename
    // changes the binding, so a real migration must occur).
    let renamed = old
        .replace("class CartTest", "class CartMovedTest")
        .replace("CartTest()", "CartMovedTest()");
    let mut contract = contract_for("src/test/java/com/example/invoicing/CartTest.java", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "src/test/java/com/example/invoicing/CartTest.java",
            old,
            "src/test/java/com/example/invoicing/CartMovedTest.java",
            &renamed,
        ),
    );
    assert_migrated(&contract, &outcome);
}

#[test]
fn python_fixture_setup_is_part_of_the_proven_test_identity() {
    let old = r#"
import pytest
from cart import compute_total, cart_total


@pytest.fixture
def items():
    return [10, 20]


def test_totals_add_up(items):
    assert compute_total(items) == 30
    assert cart_total(items) == 30
"#;
    let removed = r#"
from cart import compute_total, cart_total


def test_totals_add_up(items):
    assert compute_total(items) == 30
    assert cart_total(items) == 30
"#;
    let mut contract = contract_for("tests/test_cart.py", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("tests/test_cart.py", old, "tests/test_moved.py", removed),
    );
    assert_refused(&contract, &outcome);

    let mut contract = contract_for("tests/test_cart.py", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("tests/test_cart.py", old, "tests/test_moved.py", old),
    );
    assert_migrated(&contract, &outcome);
}

#[test]
fn go_shared_helper_is_part_of_the_proven_test_identity() {
    let old = r#"
package cart

import "testing"

func fixtureItems() []int {
	return []int{10, 20}
}

func TestTotalsAddUp(t *testing.T) {
	items := fixtureItems()
	if computeTotal(items) != 30 {
		t.Fatalf("compute total = %d", computeTotal(items))
	}
}
"#;
    let removed = r#"
package cart

import "testing"

func fixtureItems() []int {
	return []int{}
}

func TestTotalsAddUp(t *testing.T) {
	items := fixtureItems()
	if computeTotal(items) != 30 {
		t.Fatalf("compute total = %d", computeTotal(items))
	}
}
"#;
    let mut contract = contract_for("cart_test.go", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("cart_test.go", old, "totals_moved_test.go", removed),
    );
    assert_refused(&contract, &outcome);

    let renamed = old.replace("TestTotalsAddUp", "TestTotalCentsAddUp");
    let mut contract = contract_for("cart_test.go", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("cart_test.go", old, "totals_moved_test.go", &renamed),
    );
    assert_migrated(&contract, &outcome);
}

#[test]
fn commented_out_rust_tests_are_never_proof_declarations() {
    let source = r#"
/*
#[test]
fn commented_totals() {
    assert!(true);
}
*/
#[test]
fn real_test() {
    assert_eq!(1 + 1, 2);
}
"#;
    let found = declarations("tests/cart.rs", source);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].semantic_selector, "cart::real_test");
}

#[test]
fn go_helpers_with_struct_return_types_still_contribute_their_real_body() {
    let old = r#"
package cart

import "testing"

type fixture struct {
	items []int
}

func fixtureValue() fixture {
	return fixture{items: []int{10, 20}}
}

func TestTotalsAddUp(t *testing.T) {
	value := fixtureValue()
	if computeTotal(value.items) != 30 {
		t.Fatalf("compute total = %d", computeTotal(value.items))
	}
}
"#;
    let changed = old.replace("return fixture{items: []int{10, 20}}", "return fixture{}");
    let mut contract = contract_for("cart_test.go", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved("cart_test.go", old, "totals_moved_test.go", &changed),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn in_file_python_class_rename_with_identical_body_rebinds() {
    let old = r#"
class TestGroup:
    def test_totals_add_up(self):
        items = [10, 20]
        assert compute_total(items) == 30
        assert cart_total(items) == 30
"#;
    let renamed = old.replace("class TestGroup", "class RenamedGroup");
    let mut contract = contract_for("tests/test_cart.py", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        "Rename TestGroup to RenamedGroup.",
        &TestRebindEvidence {
            files: vec![TestFileDelta {
                path: "tests/test_cart.py".into(),
                before: Some(old.into()),
                after: Some(renamed),
            }],
            renames: Vec::new(),
        },
    );
    assert_migrated(&contract, &outcome);
    assert_eq!(
        contract.proof_obligations[0].evidence_binding.selector(),
        Some("test_cart::RenamedGroup::test_totals_add_up")
    );
}

#[test]
fn python_moving_a_test_to_a_class_with_different_setup_refuses() {
    let old = r#"
class TestGood:
    def setup_method(self):
        self.items = [10, 20]

    def test_totals_add_up(self):
        assert compute_total(self.items) == 30


class TestOther:
    def setup_method(self):
        self.items = []
"#;
    let moved_to_other = r#"
class TestGood:
    def setup_method(self):
        self.items = [10, 20]


class TestOther:
    def setup_method(self):
        self.items = []

    def test_totals_add_up(self):
        assert compute_total(self.items) == 30
"#;
    let mut contract = contract_for("tests/test_cart.py", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &TestRebindEvidence {
            files: vec![TestFileDelta {
                path: "tests/test_cart.py".into(),
                before: Some(old.into()),
                after: Some(moved_to_other.into()),
            }],
            renames: Vec::new(),
        },
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn javascript_regex_literals_never_truncate_the_executable_body() {
    let old = r#"
test("totals add up", () => {
  const pattern = /}/;
  assert.equal(computeTotal([10, 20]), 30);
  assert.match("}", pattern);
});
"#;
    let weakened = r#"
test("totals add up", () => {
  const pattern = /}/;
  assert.match("}", pattern);
});
"#;
    let mut contract = contract_for("test/cart.test.js", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "test/cart.test.js",
            old,
            "test/cart_total.test.js",
            weakened,
        ),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn javascript_suite_level_setup_is_part_of_the_proven_identity() {
    let old = r#"
describe("cart", () => {
  let items = [10, 20];
  test("totals add up", () => {
    assert.equal(computeTotal(items), 30);
    assert.equal(cartTotal(items), 30);
  });
});
"#;
    let weakened = old.replace("let items = [10, 20];", "let items = [];");
    let mut contract = contract_for("test/cart.test.js", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "test/cart.test.js",
            old,
            "test/cart_total.test.js",
            &weakened,
        ),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn javascript_top_level_setup_is_part_of_the_proven_identity() {
    let old = r#"
const items = [10, 20];
test("totals add up", () => {
  assert.equal(computeTotal(items), 30);
  assert.equal(cartTotal(items), 30);
});
"#;
    let weakened = old.replace("const items = [10, 20];", "const items = [];");
    let mut contract = contract_for("test/cart.test.js", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "test/cart.test.js",
            old,
            "test/cart_total.test.js",
            &weakened,
        ),
    );
    assert_refused(&contract, &outcome);
}

#[test]
fn javascript_import_path_changes_do_not_block_an_identical_body_move() {
    let old = r#"
import { computeTotal, cartTotal } from "../src/cart.js";
test("totals add up", () => {
  const items = [10, 20];
  assert.equal(computeTotal(items), 30);
  assert.equal(cartTotal(items), 30);
});
"#;
    let moved_file = old.replace("../src/cart.js", "../../src/cart.js");
    let mut contract = contract_for("test/cart.test.js", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "test/cart.test.js",
            old,
            "test/deep/cart_total.test.js",
            &moved_file,
        ),
    );
    assert!(outcome.refusals.is_empty(), "{outcome:?}");
    assert!(outcome.stale.is_empty());
}

#[test]
fn javascript_no_semicolon_imports_do_not_hide_top_level_setup() {
    let old = r#"
import { computeTotal } from "../src/cart.js"
const items = [10, 20]
test("totals add up", () => {
  assert.equal(computeTotal(items), 30);
});
"#;
    let weakened = old.replace("const items = [10, 20]", "const items = []");
    let mut contract = contract_for("test/cart.test.js", old);
    let outcome = rebind_test_obligations(
        &mut contract,
        GOAL,
        &moved(
            "test/cart.test.js",
            old,
            "test/cart_total.test.js",
            &weakened,
        ),
    );
    assert_refused(&contract, &outcome);
}
