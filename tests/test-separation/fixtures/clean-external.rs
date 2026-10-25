// TEST-SEPARATION: oracle=external-fixture isolation=pure-unit
//
// Clean control: expected values are owned by a reviewed fixture, the SUT
// output is opaque, and the evaluator never calls the SUT for the expected
// value.
#[test]
fn clean_external_fixture() {
    const EXPECTED_TOTAL: i64 = 1_234;
    let observed = sut::total_minor(&fixture_request());
    assert_eq!(observed, EXPECTED_TOTAL);
}
