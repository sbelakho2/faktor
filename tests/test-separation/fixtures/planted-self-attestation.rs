// TEST-SEPARATION: oracle=independent-reference isolation=separate-process
//
// Planted class (b): the test sets the SUT's own metric and then checks the
// SUT's own verified flag.
#[test]
fn planted_self_attestation() {
    let mut report = sut::Report::new();
    report.with_physics(true).with_authority(true);
    assert!(report.is_verified());
    assert!(sut::ReceiptVerifier::self_attested().accepts(report));
}
