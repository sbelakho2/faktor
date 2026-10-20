//! Regression tests split from lib.rs to keep the source-size authority.

use super::*;

#[cfg(test)]
mod inner {
    use super::*;

    #[tokio::test]
    async fn resolved_model_reports_the_concrete_wire_id_for_durable_receipts() {
        // The runtime records the CONCRETE id it calls; a synthetic alias
        // must never reach the durable turn envelope or a call receipt.
        let provider = OllamaProvider::permissive_for_tests(OllamaConfig::new(Some(
            "http://127.0.0.1:1".into(),
        )));
        provider
            .probed
            .write()
            .unwrap()
            .insert("qwen3.8:latest".into(), ModelCapabilities::default());
        assert_eq!(provider.resolved_model("default"), "qwen3.8:latest");
        assert_eq!(provider.resolved_model("qwen3.8:latest"), "qwen3.8:latest");
    }
    #[tokio::test]
    async fn unresolved_default_is_kept_when_nothing_is_probed() {
        let provider = OllamaProvider::permissive_for_tests(OllamaConfig::new(Some(
            "http://127.0.0.1:1".into(),
        )));
        assert_eq!(provider.resolved_model_name("default"), "default");
    }
}
