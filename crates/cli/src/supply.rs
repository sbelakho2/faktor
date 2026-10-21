//! `faktor supply` — typed, deterministic Supply commands over JSON.
//!
//! Every command reads one bounded JSON document (file or stdin), runs a pure
//! kernel from the supply crates, and prints the typed result as JSON on
//! stdout. There are no model calls, no network access, no vendor defaults and
//! no implicit actions. Errors go to stderr with a non-zero exit.

use std::io::Read;

use clap::Subcommand;
use faktor_supply_core::{Day, SupplyDecision, SupplyGraph};
use faktor_supply_cost::{should_cost, tco, ShouldCostInput, TcoInput};
use faktor_supply_execution::{
    authorize, batna, compare_bids, compare_incoterms, generate_rfq, negotiation_plan,
    ActionRequest, AuthorizationGrant, NormalizationContext, Quote, RfqPackage, RfqRequest,
};
use faktor_supply_logistics::{
    classify, compute_duty, Classification, ClassificationRequest, CustomsValuation, OriginClaim,
    TariffSchedule,
};
use faktor_supply_planning::{plan, PlanningInput};
use faktor_supply_quality::{assess as assess_qualification, Qualification};
use faktor_supply_supplier::{assess as assess_supplier, SourcingRequirement, SupplierProfile};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

/// Hard bound on one CLI JSON document.
pub(crate) const MAX_SUPPLY_INPUT_BYTES: u64 = 8 * 1024 * 1024;

/// Supply subcommands.
#[derive(Subcommand)]
pub(crate) enum SupplyAction {
    /// Exact should-cost per unit from materials, route, overhead and margin.
    ShouldCost {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Risk-adjusted landed TCO from evidenced terms.
    Tco {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Evaluate an engineering-grade supplier profile against a requirement.
    SupplierFit {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Derive an evidence-bound qualification state.
    Qualify {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Classify a product into candidate HS codes (ambiguity escalates).
    Classify {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Compute duty/tax for a finalized classification and tariff input.
    Duty {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// EBOM x demand x scrap -> time-phased plan, safety stock, ABC/XYZ.
    Plan {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Generate an RFQ package and deterministic questionnaire.
    Rfq {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Normalize quotes, compare Incoterms and build a BATNA scaffold.
    NormalizeBids {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Authorize an action against explicit grants. Never sends anything.
    Authorize {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Run the closed Engineering -> Supply loop: evaluate every candidate
    /// against the revision-bound requirement set, derive tests, qualify,
    /// normalize quotes and emit one typed SupplyDecision with evidence.
    Decision {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Validate a typed SupplyDecision document (revision/evidence/conflict
    /// bounds and candidate references).
    DecisionCheck {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
    /// Validate and summarize a typed revisioned supply graph.
    GraphCheck {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
    },
}

/// Dispatch one Supply command, print JSON, and return a process status.
pub(crate) fn command(action: SupplyAction) -> Result<(), String> {
    let value = execute(&action)?;
    let rendered = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("cannot render result: {error}"))?;
    println!("{rendered}");
    Ok(())
}

fn input_path(action: &SupplyAction) -> &Option<String> {
    match action {
        SupplyAction::ShouldCost { input }
        | SupplyAction::Tco { input }
        | SupplyAction::SupplierFit { input }
        | SupplyAction::Qualify { input }
        | SupplyAction::Classify { input }
        | SupplyAction::Duty { input }
        | SupplyAction::Plan { input }
        | SupplyAction::Rfq { input }
        | SupplyAction::NormalizeBids { input }
        | SupplyAction::Authorize { input }
        | SupplyAction::Decision { input }
        | SupplyAction::DecisionCheck { input }
        | SupplyAction::GraphCheck { input } => input,
    }
}

fn execute(action: &SupplyAction) -> Result<Value, String> {
    let raw = read_input(input_path(action))?;
    let value = match action {
        SupplyAction::ShouldCost { .. } => {
            let input: ShouldCostInput = parse(&raw)?;
            let result = should_cost(&input).map_err(|error| error.to_string())?;
            to_value(result)?
        }
        SupplyAction::Tco { .. } => {
            let input: TcoInput = parse(&raw)?;
            let result = tco(&input).map_err(|error| error.to_string())?;
            to_value(result)?
        }
        SupplyAction::SupplierFit { .. } => {
            let request: FitRequest = parse(&raw)?;
            let result = assess_supplier(&request.profile, &request.requirement)
                .map_err(|error| error.to_string())?;
            to_value(result)?
        }
        SupplyAction::Qualify { .. } => {
            let request: QualifyRequest = parse(&raw)?;
            let result = assess_qualification(&request.qualification, request.as_of)
                .map_err(|error| error.to_string())?;
            to_value(result)?
        }
        SupplyAction::Classify { .. } => {
            let request: ClassificationRequest = parse(&raw)?;
            let result = classify(&request).map_err(|error| error.to_string())?;
            to_value(result)?
        }
        SupplyAction::Duty { .. } => {
            let request: DutyRequest = parse(&raw)?;
            let result = compute_duty(
                &request.classification,
                &request.tariff,
                &request.country,
                &request.valuation,
                request.origin.as_ref(),
                request.as_of,
            )
            .map_err(|error| error.to_string())?;
            to_value(result)?
        }
        SupplyAction::Plan { .. } => {
            let input: PlanningInput = parse(&raw)?;
            let result = plan(&input).map_err(|error| error.to_string())?;
            to_value(result)?
        }
        SupplyAction::Rfq { .. } => {
            let request: RfqRequest = parse(&raw)?;
            let result = generate_rfq(&request).map_err(|error| error.to_string())?;
            to_value(result)?
        }
        SupplyAction::NormalizeBids { .. } => {
            let request: NormalizeRequest = parse(&raw)?;
            let comparison = compare_bids(&request.quotes, &request.package, &request.context)
                .map_err(|error| error.to_string())?;
            let batna = batna(&comparison);
            let negotiation = negotiation_plan(&request.package, &comparison);
            let incoterms = request
                .quotes
                .iter()
                .map(|quote| {
                    compare_incoterms(quote, &request.context)
                        .map(|rows| json!({"quote_id": quote.id, "rows": rows}))
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<Vec<_>, String>>()?;
            json!({
                "comparison": comparison,
                "batna": batna,
                "negotiation": negotiation,
                "incoterms": incoterms,
            })
        }
        SupplyAction::Authorize { .. } => {
            let request: AuthorizeRequest = parse(&raw)?;
            let decision = authorize(&request.request, &request.grants);
            to_value(decision)?
        }
        SupplyAction::Decision { .. } => {
            let request: faktor_supply_engineering::LoopRequest = parse(&raw)?;
            let outcome =
                faktor_supply_engineering::run(&request).map_err(|error| error.to_string())?;
            to_value(outcome)?
        }
        SupplyAction::DecisionCheck { .. } => {
            let decision: SupplyDecision = parse(&raw)?;
            decision.validate().map_err(|error| error.to_string())?;
            json!({ "valid": true, "decision": decision })
        }
        SupplyAction::GraphCheck { .. } => {
            let mut graph: SupplyGraph = parse(&raw)?;
            graph.rebuild_indexes();
            graph.validate().map_err(|error| error.to_string())?;
            json!({
                "valid": true,
                "revision": graph.revision,
                "nodes": graph.node_count(),
                "edges": graph.edge_count(),
            })
        }
    };
    Ok(value)
}

fn to_value<T: serde::Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| format!("cannot encode result: {error}"))
}

fn parse<T: DeserializeOwned>(raw: &str) -> Result<T, String> {
    serde_json::from_str(raw).map_err(|error| format!("invalid JSON input: {error}"))
}

pub(crate) fn read_input(path: &Option<String>) -> Result<String, String> {
    let mut buffer = String::new();
    match path.as_deref() {
        None | Some("-") => {
            let mut stdin = std::io::stdin().lock().take(MAX_SUPPLY_INPUT_BYTES + 1);
            stdin
                .read_to_string(&mut buffer)
                .map_err(|error| format!("cannot read stdin: {error}"))?;
        }
        Some(path) => {
            let file = std::fs::File::open(path)
                .map_err(|error| format!("cannot open {path}: {error}"))?;
            let metadata = file
                .metadata()
                .map_err(|error| format!("cannot stat {path}: {error}"))?;
            if metadata.len() > MAX_SUPPLY_INPUT_BYTES {
                return Err(format!(
                    "input {path} exceeds the {MAX_SUPPLY_INPUT_BYTES}-byte bound"
                ));
            }
            let mut reader = file.take(MAX_SUPPLY_INPUT_BYTES + 1);
            reader
                .read_to_string(&mut buffer)
                .map_err(|error| format!("cannot read {path}: {error}"))?;
        }
    }
    if buffer.len() as u64 > MAX_SUPPLY_INPUT_BYTES {
        return Err(format!(
            "input exceeds the {MAX_SUPPLY_INPUT_BYTES}-byte bound"
        ));
    }
    Ok(buffer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FitRequest {
    profile: SupplierProfile,
    requirement: SourcingRequirement,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QualifyRequest {
    qualification: Qualification,
    as_of: Day,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DutyRequest {
    classification: Classification,
    tariff: TariffSchedule,
    country: String,
    valuation: CustomsValuation,
    origin: Option<OriginClaim>,
    as_of: Day,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalizeRequest {
    package: RfqPackage,
    quotes: Vec<Quote>,
    context: NormalizationContext,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeRequest {
    request: ActionRequest,
    grants: Vec<AuthorizationGrant>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_supply_core::{EvidenceRef, PartId, Revision, SupplierId};

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("supply")
            .join(name);
        path.to_string_lossy().to_string()
    }

    #[test]
    fn cli_should_cost_emits_exact_json() {
        let value = execute(&SupplyAction::ShouldCost {
            input: Some(fixture("should_cost.json")),
        })
        .unwrap();
        assert_eq!(
            value["ex_works"]["value"]["micros"].as_i64().unwrap(),
            26_292_284
        );
        assert_eq!(value["currency"].as_str().unwrap(), "USD");
    }

    #[test]
    fn cli_authorize_refuses_without_a_grant() {
        let path = fixture("authorize_denied.json");
        let value = execute(&SupplyAction::Authorize { input: Some(path) }).unwrap();
        assert_eq!(value["decision"].as_str().unwrap(), "denied");
        assert_eq!(value["reason"].as_str().unwrap(), "no_grant");
    }

    #[test]
    fn cli_rejects_oversized_or_invalid_input() {
        let path =
            std::env::temp_dir().join(format!("faktor-supply-invalid-{}.json", std::process::id()));
        std::fs::write(&path, "{\"not\":\"a should-cost\"}").unwrap();
        let result = execute(&SupplyAction::ShouldCost {
            input: Some(path.to_string_lossy().to_string()),
        });
        assert!(result.is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn transport_types_are_available_to_the_cli_module() {
        let _: PartId = PartId(1);
        let _: SupplierId = SupplierId(1);
        let _: Revision = Revision(1);
        let _: EvidenceRef = EvidenceRef::new("x", Revision(1), None).unwrap();
    }
}
