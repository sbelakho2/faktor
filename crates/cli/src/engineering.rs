//! `faktor engineering` — deterministic Engineering kernels over JSON.
//!
//! `analyse` runs the closed Engineering -> Supply loop: a revision-bound
//! requirement set, typed candidate facts, knowledge-pack tests, supplier fit,
//! should-cost and risk-adjusted TCO. `verify` recomputes the decision,
//! requirement-set, analysis-receipt and result-digest bindings of a stored
//! loop result. There are no model calls, no network access and no side
//! effects; orders and messages stay behind the Supply authorization gate.

use clap::Subcommand;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use faktor_supply_engineering::{LoopOutcome, LoopRequest};

/// Engineering subcommands.
#[derive(Subcommand)]
pub(crate) enum EngineeringAction {
    /// Evaluate every supply candidate against a frozen requirement set and
    /// return the engineering evaluations, derived tests and closed-loop
    /// decision with its receipt.
    Analyse {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
        /// Data directory holding the daemon-local engineering signing key.
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Verify a stored closed-loop result: decision/requirement bounds,
    /// receipt binding, result digest, 11-gate readiness and the absence of
    /// ungated order/message actions.
    Verify {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
        /// Data directory holding the daemon-local engineering signing key.
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
}

/// Dispatch one Engineering command and print bounded JSON.
pub(crate) fn command(action: EngineeringAction) -> Result<(), String> {
    let verifying = matches!(action, EngineeringAction::Verify { .. });
    let value = execute(&action)?;
    let rendered = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("cannot render result: {error}"))?;
    println!("{rendered}");
    if verifying && !value["verified"].as_bool().unwrap_or(false) {
        return Err("stored loop result did not verify".to_string());
    }
    Ok(())
}

fn input_path(action: &EngineeringAction) -> &Option<String> {
    match action {
        EngineeringAction::Analyse { input, .. } | EngineeringAction::Verify { input, .. } => input,
    }
}

fn data_dir(action: &EngineeringAction) -> std::path::PathBuf {
    match action {
        EngineeringAction::Analyse { data_dir, .. }
        | EngineeringAction::Verify { data_dir, .. } => crate::expand(data_dir),
    }
}

fn execute(action: &EngineeringAction) -> Result<Value, String> {
    let raw = crate::supply::read_input(input_path(action))?;
    match action {
        EngineeringAction::Analyse { .. } => {
            let request: LoopRequest = parse(&raw)?;
            let signer = engineering_core::load_or_create_signing_key(&data_dir(action))
                .map_err(|error| error.to_string())?;
            let outcome = faktor_supply_engineering::run_with_signer(&request, &signer)
                .map_err(|error| error.to_string())?;
            to_value(outcome)
        }
        EngineeringAction::Verify { .. } => {
            let outcome: LoopOutcome = parse(&raw)?;
            let key = engineering_core::load_signing_key(&data_dir(action))
                .map_err(|error| error.to_string())?;
            let verifier = engineering_core::ReceiptVerifier::trusting_key_id(&key.key_id())
                .map_err(|error| error.to_string())?;
            let verification = outcome
                .verify_with_verifier(&verifier)
                .map_err(|error| error.to_string())?;
            let verified = verification.decision_valid
                && verification.requirement_set_valid
                && verification.receipt_valid
                && verification.result_digest_matches
                && verification.response_ready
                && verification.no_ungated_actions;
            Ok(json!({
                "verified": verified,
                "verification": verification,
            }))
        }
    }
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| format!("cannot encode result: {error}"))
}

fn parse<T: DeserializeOwned>(raw: &str) -> Result<T, String> {
    serde_json::from_str(raw).map_err(|error| format!("invalid JSON input: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyse_refuses_invalid_json() {
        let path = std::env::temp_dir().join(format!(
            "faktor-engineering-invalid-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "{\"not\":\"a loop request\"}").unwrap();
        let result = execute(&EngineeringAction::Analyse {
            input: Some(path.to_string_lossy().to_string()),
            data_dir: std::env::temp_dir()
                .join(format!(
                    "faktor-engineering-invalid-dir-{}",
                    std::process::id()
                ))
                .to_string_lossy()
                .to_string(),
        });
        assert!(result.is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn verify_refuses_an_unbound_document() {
        let path = std::env::temp_dir().join(format!(
            "faktor-engineering-unbound-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "{}").unwrap();
        let result = execute(&EngineeringAction::Verify {
            input: Some(path.to_string_lossy().to_string()),
            data_dir: std::env::temp_dir()
                .join(format!(
                    "faktor-engineering-unbound-dir-{}",
                    std::process::id()
                ))
                .to_string_lossy()
                .to_string(),
        });
        assert!(result.is_err());
        let _ = std::fs::remove_file(path);
    }
}
