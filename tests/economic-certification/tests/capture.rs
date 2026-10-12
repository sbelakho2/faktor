#![cfg(unix)]
//! Live-paid capture lane (opt-in): runs REAL local (native Ollama) and
//! frontier (OpenAI-compatible) models through the complete coding-benchmark
//! pipeline and freezes their outputs as a `capture_source = live` artifact,
//! then immediately replays it through the wired outcome ledger.
//!
//! `#[ignore]`-gated: the lane needs real endpoints and (for frontier) an API
//! key, spends money, and is NOT part of the normal test run. A missing
//! `FAKTOR_ECON_CAPTURE=1` (or a missing lane configuration) is a DOCUMENTED
//! SKIP — the test passes with a note, so `--ignored` stays safe on machines
//! without local models or keys.
//!
//! How to run (frontier and/or local):
//!
//! ```sh
//! FAKTOR_ECON_CAPTURE=1 \
//! FAKTOR_ECON_LOCAL_MODEL=qwen3:8b \
//! FAKTOR_ECON_FRONTIER_MODEL=<model-id> \
//! FAKTOR_ECON_FRONTIER_API_KEY=<key> \
//! FAKTOR_ECON_FRONTIER_INPUT_MICRO_PER_MTOK=<exact price> \
//! FAKTOR_ECON_FRONTIER_OUTPUT_MICRO_PER_MTOK=<exact price> \
//! cargo test -p faktor-tests-router-economics --test capture -- --ignored --nocapture
//! ```
//!
//! Artifacts land in `tests/economic-certification/frozen/<capture-id>/`
//! (`capture.json` + a replay `report.json`); commit them to publish real
//! numbers, and never present a fixture capture as model evidence.

use faktor_tests_router_economics::capture::{capture_config_from_env, run_capture};

#[test]
#[ignore = "[live-paid] real local (Ollama) + frontier models through the coding-benchmark pipeline (needs FAKTOR_ECON_CAPTURE=1 plus lane config/keys); run explicitly, never in CI"]
fn live_coding_capture_requires_env_and_freezes_real_outputs() {
    let config = match capture_config_from_env() {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!(
                "SKIP: live capture not enabled (set FAKTOR_ECON_CAPTURE=1 plus \
                 FAKTOR_ECON_LOCAL_MODEL and/or FAKTOR_ECON_FRONTIER_MODEL/...); \
                 run with -- --ignored and the documented env vars"
            );
            return;
        }
        Err(e) => panic!("capture configuration error: {e}"),
    };
    let (capture, outcome) = run_capture(&config).expect("live capture");
    println!(
        "capture {}: {} run(s), {} verified; artifact {}; replay report {}",
        outcome.capture_id,
        outcome.runs,
        outcome.verified,
        outcome.capture_path,
        outcome.report_path
    );
    assert_eq!(capture.runs.len(), outcome.runs);
    assert!(capture.runs.iter().all(|r| !r.attempts.is_empty()));
    assert_eq!(
        capture.source,
        faktor_tests_router_economics::frozen::CaptureSource::Live
    );
}
