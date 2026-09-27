use super::*;

#[test]
fn quality_declaration_is_strict_range_checked_and_empty_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.json");
    // A declaration on a LOCAL endpoint is the sanctioned way to
    // authorize its models (unlike pricing tables, which stay refused
    // on local runtimes).
    std::fs::write(
        &path,
        r#"{"providers": [
            {"kind": "ollama", "id": "local",
             "quality": {"coding_reliability": 70, "context_reliability": 80}}
        ]}"#,
    )
    .unwrap();
    let cfg = Config::load_strict(&path).unwrap();
    let q = cfg.providers[0].quality().expect("declared");
    assert_eq!(q.coding_reliability, Some(70));
    assert_eq!(q.context_reliability, Some(80));
    assert!(!q.is_empty());
    // 0 is a REAL declaration (allowed); 100 is the ceiling.
    for value in [0, 100] {
        std::fs::write(
            &path,
            format!(
                r#"{{"providers": [
                    {{"kind": "ollama", "id": "local",
                      "quality": {{"coding_reliability": {value}}}}}
                ]}}"#
            ),
        )
        .unwrap();
        Config::load_strict(&path).unwrap();
    }
    // Out of range is a typed refusal naming the range.
    std::fs::write(
        &path,
        r#"{"providers": [
            {"kind": "ollama", "id": "local",
             "quality": {"coding_reliability": 101}}
        ]}"#,
    )
    .unwrap();
    let e = Config::load_strict(&path).unwrap_err();
    assert!(e.contains("0..=100"), "{e}");
    // An empty table declares nothing: refused.
    std::fs::write(
        &path,
        r#"{"providers": [
            {"kind": "ollama", "id": "local", "quality": {}}
        ]}"#,
    )
    .unwrap();
    let e = Config::load_strict(&path).unwrap_err();
    assert!(e.contains("empty"), "{e}");
    // Unknown keys are parse errors (strict section).
    std::fs::write(
        &path,
        r#"{"providers": [
            {"kind": "ollama", "id": "local",
             "quality": {"coding_reliablity": 70}}
        ]}"#,
    )
    .unwrap();
    assert!(Config::load_strict(&path).is_err());
}
