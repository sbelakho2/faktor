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
    action_digest, authorize_with_approvals, batna, compare_bids, compare_incoterms, generate_rfq,
    negotiation_plan, ActionRequest, AuthorizationGrant, NormalizationContext, Quote, RfqPackage,
    RfqRequest,
};
use faktor_supply_logistics::{
    classify, compute_duty, Classification, ClassificationRequest, CustomsValuation, OriginClaim,
    TariffSchedule,
};
use faktor_supply_planning::{plan, PlanningInput};
use faktor_supply_quality::{assess_with_approval as assess_qualification, Qualification};
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
    /// Derive an evidence-bound qualification state. A `qualified` or
    /// `conditional` result requires a current human qualification approval.
    Qualify {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
        /// Data directory holding durable approvals.
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
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
    /// Authorize an action against explicit grants AND a current human
    /// approval bound to the exact action. Never sends anything.
    Authorize {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
        /// Data directory holding durable approvals.
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Run the closed Engineering -> Supply loop: evaluate every candidate
    /// against the revision-bound requirement set, derive tests, qualify,
    /// normalize quotes and emit one typed SupplyDecision with evidence.
    Decision {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
        /// Data directory holding the daemon-local engineering signing key.
        /// The key is created on first use and reused across restarts, so the
        /// resulting receipts verify with `faktor engineering verify`.
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
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
        | SupplyAction::Qualify { input, .. }
        | SupplyAction::Classify { input }
        | SupplyAction::Duty { input }
        | SupplyAction::Plan { input }
        | SupplyAction::Rfq { input }
        | SupplyAction::NormalizeBids { input }
        | SupplyAction::Authorize { input, .. }
        | SupplyAction::Decision { input, .. }
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
        SupplyAction::Qualify { data_dir, .. } => {
            let request: QualifyRequest = parse(&raw)?;
            let authority = faktor_approval::ApprovalAuthority::open(&crate::expand(data_dir))
                .map_err(|error| error.to_string())?;
            let check = faktor_supply_quality::qualification_approval_check(&request.qualification)
                .map_err(|error| error.to_string())?;
            let approval = match authority.authorize(&check, faktor_approval::unix_now_ms()) {
                Ok(token) => Some(token),
                Err(error) if hard_approval_failure(&error) => return Err(error.to_string()),
                Err(error @ faktor_approval::ApprovalError::Denied { .. }) => {
                    return Ok(error.to_json())
                }
                Err(_) => None,
            };
            match assess_qualification(&request.qualification, request.as_of, approval.as_ref()) {
                Ok(result) => to_value(result)?,
                Err(faktor_supply_core::ExactError::Approval(error)) => error.to_json(),
                Err(error) => return Err(error.to_string()),
            }
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
        SupplyAction::Authorize { data_dir, .. } => {
            let request: AuthorizeRequest = parse(&raw)?;
            let authority = faktor_approval::ApprovalAuthority::open(&crate::expand(data_dir))
                .map_err(|error| error.to_string())?;
            let mut approvals = Vec::new();
            for grant in &request.grants {
                let digest =
                    action_digest(&request.request, grant).map_err(|error| error.to_string())?;
                let check = faktor_approval::ApprovalCheck::new(
                    grant.action.approval_kind(),
                    request.request.scope.clone(),
                    grant.revision.get().to_string(),
                    digest,
                );
                match authority.authorize(&check, faktor_approval::unix_now_ms()) {
                    Ok(token) => approvals.push(token),
                    Err(error) if hard_approval_failure(&error) => return Err(error.to_string()),
                    Err(error @ faktor_approval::ApprovalError::Denied { .. }) => {
                        return Ok(error.to_json())
                    }
                    Err(_) => {}
                }
            }
            let decision = authorize_with_approvals(&request.request, &request.grants, &approvals);
            to_value(decision)?
        }
        SupplyAction::Decision { data_dir, .. } => {
            let request: faktor_supply_engineering::LoopRequest = parse(&raw)?;
            let signer = engineering_core::load_or_create_signing_key(&crate::expand(data_dir))
                .map_err(|error| error.to_string())?;
            let authority = faktor_approval::ApprovalAuthority::open(&crate::expand(data_dir))
                .map_err(|error| error.to_string())?;
            match faktor_supply_engineering::run_with_authority(&request, &signer, &authority) {
                Ok(outcome) => to_value(outcome)?,
                Err(error) => match error.approval_json() {
                    Some(approval) => approval,
                    None => return Err(error.to_string()),
                },
            }
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

/// Store/lock failures are never "no approval": they are reported as hard
/// errors so a corrupt ledger cannot be silently downgraded to a refusal.
fn hard_approval_failure(error: &faktor_approval::ApprovalError) -> bool {
    matches!(
        error,
        faktor_approval::ApprovalError::Store(_)
            | faktor_approval::ApprovalError::Busy(_)
            | faktor_approval::ApprovalError::Conflict(_)
            | faktor_approval::ApprovalError::Invalid(_)
    )
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
        let data_dir =
            std::env::temp_dir().join(format!("faktor-supply-authorize-{}", std::process::id()));
        let value = execute(&SupplyAction::Authorize {
            input: Some(path),
            data_dir: data_dir.to_string_lossy().to_string(),
        })
        .unwrap();
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
