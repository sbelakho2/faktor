// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//
// Planted class (a): the test derives its expected value from the SUT result
// object, so a wrong SUT can score itself green.
#[test]
fn planted_self_oracle() {
    let result = sut::score_the_work();
    let expected = result.score;
    assert_eq!(result.score, expected);
}
