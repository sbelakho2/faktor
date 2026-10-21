//! Supply cases executed through the real cost/logistics/unit kernels.

use engineering_units::UnitRegistry;
use faktor_supply_core::{
    Assumption, Basis, Currency, Day, EvidenceRef, Money, Provenance, Rate, Revision, SourceKind,
    Term, Units,
};
use faktor_supply_cost::{
    not_applicable_term, tco, unknown_term, Duty, ExcessMoq, ExpediteCost, Insurance, Obsolescence,
    QualityCost, Tax, Tco, TcoInput, TcoTerm, WorkingCapital,
};
use faktor_supply_logistics::{
    classify, compute_duty, ClassificationOutcome, ClassificationRequest, CustomsValuation,
    EscalationKind, TariffSchedule, ValuationBasis,
};

use crate::contract::{ControlAnswer, Observation, SutAnswer, SutOutcome, SutUncertainty};
use crate::BenchError;

pub(crate) const SUPPLIER_ID: &str = "supply/cheaper-supplier-tco";
pub(crate) const HS_ID: &str = "customs/hs-classification-ambiguity";
pub(crate) const UNIT_ID: &str = "units/torque-lbf-ft";
pub(crate) const UNKNOWN_COST_ID: &str = "supply/unknown-cost-not-zero";

const CURRENCY: Currency = Currency::USD;
const FIXTURE_DAY: Day = Day(20_000);

fn case_error(id: &str, detail: impl std::fmt::Display) -> BenchError {
    BenchError::Case {
        id: id.to_string(),
        detail: detail.to_string(),
    }
}

fn money(amount: &str) -> Money {
    Money::parse(amount, CURRENCY).expect("static fixture money")
}

fn rate(ppm: i64) -> Rate {
    Rate::new(ppm).expect("static fixture rate")
}

fn units(value: u64) -> Units {
    Units::new(value).expect("static fixture units")
}

fn evidence_provenance(id: &str) -> Provenance {
    Provenance::new(
        SourceKind::SupplierDocument,
        EvidenceRef::new(id, Revision(1), Some("fixture-digest")).expect("static fixture evidence"),
        FIXTURE_DAY,
        "benchmark fixture quote revision",
        vec!["hypothetical quote terms".to_string()],
        "a binding quote or invoice with different terms",
    )
    .expect("static fixture provenance")
}

fn assumption_provenance(id: &str) -> Provenance {
    Provenance::new(
        SourceKind::InternalEstimate,
        EvidenceRef::new(id, Revision(1), Some("fixture-digest")).expect("static fixture evidence"),
        FIXTURE_DAY,
        "explicit benchmark assumption",
        vec!["hypothetical planning scenario".to_string()],
        "actual carrying, tax, or obsolescence records",
    )
    .expect("static fixture provenance")
}

fn evidence<T>(value: T, id: &str) -> Term<T> {
    Term::evidence(value, evidence_provenance(id))
}

fn assumed<T>(value: T, id: &str, statement: &str, falsifier: &str) -> Term<T> {
    Term::assumption(
        value,
        Assumption::new(
            id,
            statement,
            "benchmark fixture assumption with explicit provenance",
            assumption_provenance(id),
            falsifier,
        )
        .expect("static fixture assumption"),
    )
}

struct QuoteProfile {
    name: &'static str,
    unit_price: &'static str,
    quantity: u64,
    moq: u64,
    origin_freight: &'static str,
    export_handling: &'static str,
    international_freight: &'static str,
    import_handling: &'static str,
    brokerage: &'static str,
    final_mile: &'static str,
    defect_ppm: i64,
    expedite_ppm: i64,
    cash_days: u32,
    carrying_days: u32,
    obsolescence_ppm: i64,
}

fn quote_input(profile: &QuoteProfile) -> TcoInput {
    let unit_price = money(profile.unit_price);
    let quantity = units(profile.quantity);
    let ex_works_total = unit_price
        .mul_units(profile.quantity)
        .expect("static fixture ex-works");
    let duty_rate = rate(50_000);
    let duty_value = ex_works_total
        .scale(duty_rate)
        .expect("static fixture duty");
    let taxable_value = ex_works_total
        .checked_add(duty_value)
        .expect("static fixture taxable value");
    let financed = ex_works_total
        .checked_add(money(profile.international_freight))
        .and_then(|value| value.checked_add(duty_value))
        .expect("static fixture financed amount");
    let excess_units = profile.moq.saturating_sub(profile.quantity);
    let prefix = profile.name;
    TcoInput {
        currency: CURRENCY,
        quantity,
        ex_works_unit: evidence(unit_price, &format!("{prefix}-price")),
        origin_inland_freight: evidence(money(profile.origin_freight), &format!("{prefix}-origin")),
        export_handling: evidence(money(profile.export_handling), &format!("{prefix}-export")),
        international_freight: evidence(
            money(profile.international_freight),
            &format!("{prefix}-freight"),
        ),
        insurance: evidence(
            Insurance {
                insured_value: ex_works_total,
                rate: rate(5_000),
                minimum_premium: money("10.00"),
            },
            &format!("{prefix}-insurance"),
        ),
        import_handling: evidence(money(profile.import_handling), &format!("{prefix}-import")),
        duty: evidence(
            Duty {
                customs_value: ex_works_total,
                rate: duty_rate,
                fixed: money("0.00"),
            },
            &format!("{prefix}-duty"),
        ),
        tax: evidence(
            Tax {
                taxable_value,
                rate: rate(80_000),
                recoverable: true,
            },
            &format!("{prefix}-tax"),
        ),
        brokerage: evidence(money(profile.brokerage), &format!("{prefix}-brokerage")),
        final_mile: evidence(money(profile.final_mile), &format!("{prefix}-final-mile")),
        working_capital: assumed(
            WorkingCapital {
                annual_rate: rate(80_000),
                days: profile.cash_days,
                financed_amount: financed,
            },
            &format!("{prefix}-cash-cycle"),
            "cash is tied up for the quoted payment term at an 8% annual cost of capital",
            "bank/ERP evidence of a different cost of capital or payment term",
        ),
        expected_quality: evidence(
            QualityCost {
                defect_ppm: profile.defect_ppm,
                units: quantity,
                cost_per_defect: money("50.00"),
            },
            &format!("{prefix}-quality"),
        ),
        expected_expedite: evidence(
            ExpediteCost {
                probability: rate(profile.expedite_ppm),
                cost: money("500.00"),
            },
            &format!("{prefix}-expedite"),
        ),
        excess_moq_carrying: assumed(
            ExcessMoq {
                excess_units: units(excess_units),
                unit_value: unit_price,
                annual_carrying_rate: rate(200_000),
                days: profile.carrying_days,
            },
            &format!("{prefix}-moq-carrying"),
            "excess MOQ stock is carried at a 20% annual rate for the stated period",
            "a different carrying rate or a MOQ waiver that removes the excess stock",
        ),
        obsolescence: assumed(
            Obsolescence {
                exposure_rate: rate(profile.obsolescence_ppm),
                inventory_value: ex_works_total,
            },
            &format!("{prefix}-obsolescence"),
            "the quoted supplier's inventory carries the stated obsolescence exposure",
            "lifecycle/PCN evidence showing a different exposure",
        ),
    }
}

fn incumbent_profile() -> QuoteProfile {
    QuoteProfile {
        name: "incumbent",
        unit_price: "10.00",
        quantity: 500,
        moq: 100,
        origin_freight: "50.00",
        export_handling: "20.00",
        international_freight: "300.00",
        import_handling: "40.00",
        brokerage: "30.00",
        final_mile: "60.00",
        defect_ppm: 1_000,
        expedite_ppm: 20_000,
        cash_days: 30,
        carrying_days: 90,
        obsolescence_ppm: 10_000,
    }
}

fn cheap_profile() -> QuoteProfile {
    QuoteProfile {
        name: "cheap",
        unit_price: "8.50",
        quantity: 500,
        moq: 1_000,
        origin_freight: "120.00",
        export_handling: "40.00",
        international_freight: "900.00",
        import_handling: "80.00",
        brokerage: "60.00",
        final_mile: "120.00",
        defect_ppm: 20_000,
        expedite_ppm: 100_000,
        cash_days: 90,
        carrying_days: 180,
        obsolescence_ppm: 20_000,
    }
}

fn known_total(report: &Tco, case: &str) -> Result<Money, BenchError> {
    report
        .total
        .as_known()
        .copied()
        .ok_or_else(|| case_error(case, "TCO total is unexpectedly unavailable"))
}

fn money_text(amount: Money) -> String {
    let micros = amount.micros();
    let sign = if micros < 0 { "-" } else { "" };
    let magnitude = micros.unsigned_abs();
    format!(
        "{sign}{}.{:06} {}",
        magnitude / 1_000_000,
        magnitude % 1_000_000,
        amount.currency.as_str()
    )
}

fn coverage(report: &Tco) -> Vec<String> {
    let mut coverage = Vec::new();
    for line in &report.lines {
        for basis in &line.basis {
            match basis {
                Basis::Provenance(provenance) => coverage.push(provenance.evidence.id.clone()),
                Basis::Assumption(assumption) => coverage.push(assumption.id.clone()),
            }
        }
    }
    coverage.sort();
    coverage.dedup();
    coverage
}

#[allow(clippy::too_many_arguments)]
fn answer(
    id: &str,
    outcome: SutOutcome,
    missing: Vec<String>,
    boundaries: Vec<String>,
    uncertainty: SutUncertainty,
    coverage: Vec<String>,
    evidence: Vec<String>,
    experiments: Vec<engineering_core::FalsifiableTest>,
    controls: Vec<ControlAnswer>,
) -> SutAnswer {
    SutAnswer {
        id: id.to_string(),
        outcome,
        gate_states: Vec::new(),
        missing_info: missing,
        boundary_conditions: boundaries,
        uncertainty,
        assumption_coverage: coverage,
        evidence,
        experiments,
        controls,
    }
}

fn test(
    claim: &str,
    procedure: &str,
    pass_criterion: &str,
    refutation: &str,
    instrument: &str,
    evidence: &[&str],
) -> engineering_core::FalsifiableTest {
    engineering_core::FalsifiableTest {
        claim: claim.to_string(),
        procedure: procedure.to_string(),
        pass_criterion: pass_criterion.to_string(),
        refutation: refutation.to_string(),
        instrument: instrument.to_string(),
        evidence: evidence.iter().map(|item| (*item).to_string()).collect(),
    }
}

fn supplier() -> Result<SutAnswer, BenchError> {
    let incumbent_input = quote_input(&incumbent_profile());
    let cheap_input = quote_input(&cheap_profile());
    let incumbent = tco(&incumbent_input).map_err(|error| case_error(SUPPLIER_ID, error))?;
    let cheap = tco(&cheap_input).map_err(|error| case_error(SUPPLIER_ID, error))?;
    let incumbent_total = known_total(&incumbent, SUPPLIER_ID)?;
    let cheap_total = known_total(&cheap, SUPPLIER_ID)?;
    let incumbent_per_unit = incumbent
        .per_unit
        .as_known()
        .copied()
        .ok_or_else(|| case_error(SUPPLIER_ID, "incumbent per-unit is unavailable"))?;
    let cheap_per_unit = cheap
        .per_unit
        .as_known()
        .copied()
        .ok_or_else(|| case_error(SUPPLIER_ID, "cheap per-unit is unavailable"))?;
    let price_only_saving = money("10.00")
        .checked_sub(money("8.50"))
        .and_then(|difference| difference.mul_units(500))
        .expect("static fixture saving");
    let landed_difference = cheap_total
        .checked_sub(incumbent_total)
        .expect("static fixture difference");
    let mut coverage_lines = coverage(&incumbent);
    coverage_lines.extend(coverage(&cheap));
    coverage_lines.sort();
    coverage_lines.dedup();
    let evidence = vec![
        format!("incumbent landed total: {}", money_text(incumbent_total)),
        format!("cheap landed total: {}", money_text(cheap_total)),
        format!("price-only saving at 500 units: {}", money_text(price_only_saving)),
        format!("landed difference: {}", money_text(landed_difference)),
        "all 15 TCO terms are present and each known term carries evidence or an explicit assumption"
            .to_string(),
    ];
    Ok(answer(
        SUPPLIER_ID,
        SutOutcome::Solved {
            observations: vec![
                Observation::money("incumbent landed total", incumbent_total),
                Observation::money("cheap landed total", cheap_total),
                Observation::money("price-only saving", price_only_saving),
                Observation::money("landed cost difference", landed_difference),
                Observation::money("incumbent per unit", incumbent_per_unit),
                Observation::money("cheap per unit", cheap_per_unit),
                Observation::text("recommendation", "incumbent"),
            ],
        },
        vec![
            "binding quote at the required quantity and MOQ".to_string(),
            "actual freight, duty and cash-cycle terms".to_string(),
            "verified lot yield/quality evidence".to_string(),
        ],
        vec![
            "MOQ boundary: 1000-unit minimum against 500 required units".to_string(),
            "excess-MOQ carrying is charged on the 500 excess units".to_string(),
            "cash cycle is charged for 90 days on the financed amount".to_string(),
        ],
        SutUncertainty::Exact,
        coverage_lines,
        evidence,
        vec![test(
            "The cheaper unit price saves money at the required 500-unit quantity.",
            "Obtain binding quotes at the required quantity, including MOQ, freight, duties, payment terms and lot yield; verify the invoice and incoming inspection yield.",
            "The cheap supplier's risk-adjusted landed cost for the required quantity is below the incumbent's.",
            "MOQ, freight, yield/quality, cash cycle or carrying costs make the cheap supplier's landed cost equal or higher.",
            "Binding quotations, freight invoices, customs entries, incoming-inspection records, ERP cash-cycle data.",
            &[
                "cheap MOQ is 1000 against a 500-unit requirement; 500 excess units are carried",
                "cheap international freight is 900 versus 300",
                "cheap defect exposure is 20000 ppm versus 1000 ppm",
                "cheap cash cycle is 90 days versus 30 days",
            ],
        )],
        Vec::new(),
    ))
}

fn hs_classification() -> Result<SutAnswer, BenchError> {
    let request: ClassificationRequest =
        serde_json::from_str(include_str!("../../fixtures/hs_ambiguous.json"))
            .map_err(|error| case_error(HS_ID, format!("classification fixture: {error}")))?;
    let classification = classify(&request).map_err(|error| case_error(HS_ID, error))?;
    let (candidates, reason) = match &classification.outcome {
        ClassificationOutcome::Ambiguous { codes } => (
            codes.clone(),
            format!("multiple viable classifications: {}", codes.join(", ")),
        ),
        other => {
            return Err(case_error(
                HS_ID,
                format!("fixture is no longer ambiguous: {other:?}"),
            ))
        }
    };
    let escalation = classification
        .escalation
        .as_ref()
        .ok_or_else(|| case_error(HS_ID, "ambiguous classification produced no escalation"))?;
    if escalation.kind != EscalationKind::BindingRulingRequired {
        return Err(case_error(
            HS_ID,
            "ambiguous classification did not require a binding ruling",
        ));
    }
    let valuation = CustomsValuation {
        goods_value: money("5000.00"),
        international_freight: money("300.00"),
        insurance: money("25.00"),
        basis: ValuationBasis::Cif,
    };
    let tariff = TariffSchedule {
        entries: Vec::new(),
        revision: Revision(1),
    };
    if compute_duty(
        &classification,
        &tariff,
        "US",
        &valuation,
        None,
        FIXTURE_DAY,
    )
    .is_ok()
    {
        return Err(case_error(
            HS_ID,
            "duty computation did not refuse the unfinalized classification",
        ));
    }
    let mut missing_request = request.clone();
    missing_request.facts.composition = None;
    missing_request.facts.function = None;
    missing_request.facts.essential_character = None;
    let missing_classification =
        classify(&missing_request).map_err(|error| case_error(HS_ID, error))?;
    let missing_control = match &missing_classification.outcome {
        ClassificationOutcome::InsufficientFacts { missing } if !missing.is_empty() => {
            ControlAnswer {
                name: "missing-facts".to_string(),
                outcome: SutOutcome::ResearchRequired {
                    gate: engineering_core::GateId::Formulation,
                    reason: "product facts are insufficient to classify".to_string(),
                    sources_required: missing.clone(),
                },
                uncertainty: SutUncertainty::NotEstimated {
                    reason: "classification is unresolved".to_string(),
                },
            }
        }
        other => {
            return Err(case_error(
                HS_ID,
                format!("missing-facts control did not refuse: {other:?}"),
            ))
        }
    };
    let mut coverage = classification
        .evidence
        .iter()
        .map(|evidence| evidence.id.clone())
        .collect::<Vec<_>>();
    coverage.extend(candidates.clone());
    coverage.push("binding ruling".to_string());
    coverage.sort();
    coverage.dedup();
    Ok(answer(
        HS_ID,
        SutOutcome::Ambiguous {
            candidates,
            reason,
            authority: escalation.authority.clone(),
        },
        vec![
            "product composition".to_string(),
            "product function".to_string(),
            "essential character".to_string(),
            "a binding ruling from the customs authority".to_string(),
        ],
        vec![
            "exactly one viable candidate is required before duty/tax computation".to_string(),
            "multiple viable candidates are escalated, never averaged or ranked by duty".to_string(),
        ],
        SutUncertainty::NotEstimated {
            reason: "the classification is a conflict between authoritative candidates, not a statistical uncertainty".to_string(),
        },
        coverage,
        vec![
            format!("classification outcome: {:?}", classification.outcome),
            format!("escalation: {:?}: {}", escalation.kind, escalation.reason),
            format!(
                "evidence references bound to candidates: {}",
                classification.evidence.len()
            ),
            "duty computation refused on the unfinalized classification".to_string(),
        ],
        vec![test(
            "The product falls under HS 853222.",
            "Request a binding ruling from the destination customs authority, supplying composition, function, essential character and the candidate headings; then recompute duty from the ruling.",
            "The authority issues exactly one binding code and the duty recomputes from that code with current tariff evidence.",
            "The ruling selects a different code, confirms ambiguity, or requires product facts that are missing.",
            "Binding customs ruling, current tariff schedule, laboratory/product composition evidence.",
            &[
                "two candidate classifications are viable on the supplied facts",
                "duty computation refuses to run on an unfinalized classification",
                "removing composition/function facts produces a typed insufficient-facts escalation",
            ],
        )],
        vec![missing_control],
    ))
}

fn unit_conversion() -> Result<SutAnswer, BenchError> {
    let units = UnitRegistry::si();
    let lbf_ft = units
        .require("lbf*ft")
        .map_err(|error| case_error(UNIT_ID, error))?;
    let newton_metre = units
        .require("N*m")
        .map_err(|error| case_error(UNIT_ID, error))?;
    let torque = engineering_units::Quantity::new(100.0, lbf_ft.clone())
        .map_err(|error| case_error(UNIT_ID, error))?;
    let converted = torque
        .to_unit(newton_metre.clone())
        .map_err(|error| case_error(UNIT_ID, error))?;
    let back = converted
        .to_unit(lbf_ft.clone())
        .map_err(|error| case_error(UNIT_ID, error))?;
    let expected = 100.0 * 4.448_221_615_260_5 * 0.304_8;
    let relative_error = ((converted.value() - expected) / expected).abs();
    let round_trip_error = (back.value() - 100.0).abs();
    let shortfall = (expected - 100.0) / expected;
    if relative_error >= 1e-12 || round_trip_error >= 1e-9 {
        return Err(case_error(
            UNIT_ID,
            "unit conversion is outside exact bounds",
        ));
    }
    let incompatible = engineering_units::Quantity::new(
        100.0,
        units
            .require("N")
            .map_err(|error| case_error(UNIT_ID, error))?,
    )
    .map_err(|error| case_error(UNIT_ID, error))?;
    let dimension_refused = torque.add(&incompatible).is_err();
    let unknown_symbol_refused = units.require("lbf-ft").is_err();
    if !dimension_refused || !unknown_symbol_refused {
        return Err(case_error(
            UNIT_ID,
            "a dimensional/unit control was accepted",
        ));
    }
    Ok(answer(
        UNIT_ID,
        SutOutcome::Solved {
            observations: vec![
                Observation::quantity("100 lbf*ft in N*m", converted.value(), "N*m"),
                Observation::quantity("round trip in lbf*ft", back.value(), "lbf*ft"),
                Observation::quantity("shallow 100 N*m shortfall fraction", shortfall, "1"),
            ],
        },
        vec![
            "a torque standard and calibration certificate for the actual instrument".to_string(),
        ],
        vec![
            "conversion is only valid between units of the same dimension (energy/torque)"
                .to_string(),
            "affine temperature units are not multiplied or divided".to_string(),
        ],
        SutUncertainty::Exact,
        vec![
            "unit registry lbf factor 4.4482216152605".to_string(),
            "unit registry ft factor 0.3048".to_string(),
            "newton metre".to_string(),
        ],
        vec![
            "unit registry scales: lbf = 4.4482216152605 N, ft = 0.3048 m".to_string(),
            "the registry refuses unknown symbols and dimensionally incompatible arithmetic"
                .to_string(),
        ],
        vec![test(
            "100 lbf*ft is the same torque as 100 N*m.",
            "Compare the torque tool against a deadweight/lever primary standard or a calibrated torque transducer in both unit settings; convert the indicated values with exact SI definitions.",
            "The two readings agree after conversion within the tool's declared tolerance and the dimensional check refuses force/energy mixing.",
            "The converted values differ beyond tolerance, or the tool treats lbf*ft and N*m as interchangeable numbers.",
            "Deadweight/lever primary standard or calibrated torque transducer with traceable certificate.",
            &[
                "1 lbf = 4.4482216152605 N and 1 ft = 0.3048 m exactly by definition",
                "100 lbf*ft = 135.58179483314 N*m, a 26.2% difference from the shallow 100 N*m answer",
            ],
        )],
        vec![
            ControlAnswer {
                name: "dimension-mismatch".to_string(),
                outcome: SutOutcome::Refused {
                    code: "DimensionMismatch".to_string(),
                    reason: "torque cannot be added to force".to_string(),
                    required: vec!["a quantity with the torque dimension".to_string()],
                },
                uncertainty: SutUncertainty::NotEstimated {
                    reason: "the operation was refused".to_string(),
                },
            },
            ControlAnswer {
                name: "unknown-symbol".to_string(),
                outcome: SutOutcome::Refused {
                    code: "UnknownUnit".to_string(),
                    reason: "lbf-ft is not a registered unit symbol".to_string(),
                    required: vec!["a registered torque unit".to_string()],
                },
                uncertainty: SutUncertainty::NotEstimated {
                    reason: "the unit was refused".to_string(),
                },
            },
        ],
    ))
}

fn unknown_cost() -> Result<SutAnswer, BenchError> {
    let mut input = quote_input(&incumbent_profile());
    input.duty =
        unknown_term::<Duty>("tariff rate is unavailable for the unresolved classification");
    let report = tco(&input).map_err(|error| case_error(UNKNOWN_COST_ID, error))?;
    let total_unavailable = !report.total.is_known()
        && !report.per_unit.is_known()
        && report.unavailable_terms == vec![TcoTerm::Duty];
    if !total_unavailable {
        return Err(case_error(
            UNKNOWN_COST_ID,
            "an unknown duty did not propagate to total/per-unit unavailability",
        ));
    }
    let mut control_input = quote_input(&incumbent_profile());
    control_input.duty = not_applicable_term::<Duty>("domestic-only shipment has no customs duty");
    let control = tco(&control_input).map_err(|error| case_error(UNKNOWN_COST_ID, error))?;
    let control_known = control.total.is_known();
    let control_line = control
        .line(TcoTerm::Duty)
        .ok_or_else(|| case_error(UNKNOWN_COST_ID, "control has no duty line"))?;
    let control_zero = control_line.not_applicable
        && control_line
            .amount
            .as_known()
            .is_some_and(|amount| amount.is_zero());
    if !control_known || !control_zero {
        return Err(case_error(
            UNKNOWN_COST_ID,
            "the explicit not-applicable control did not solve to zero",
        ));
    }
    let control_total = control.total.as_known().copied().expect("control total");
    Ok(answer(
        UNKNOWN_COST_ID,
        SutOutcome::Unavailable {
            reason: format!(
                "unknown terms make the landed total unavailable: {:?}",
                report.unavailable_terms
            ),
            required: vec![
                "binding HS classification".to_string(),
                "current tariff rate for the destination and origin".to_string(),
                "customs valuation basis".to_string(),
            ],
        },
        vec![
            "binding HS classification".to_string(),
            "current tariff rate".to_string(),
            "customs valuation basis".to_string(),
        ],
        vec![
            "unknown is not the same as explicitly not applicable".to_string(),
            "an unknown term propagates to total and per-unit unavailability".to_string(),
        ],
        SutUncertainty::NotEstimated {
            reason: "the unknown duty carries a typed reason and is not assigned a zero or point estimate".to_string(),
        },
        vec![
            "duty".to_string(),
            "tariff".to_string(),
            "customs classification".to_string(),
        ],
        vec![
            format!("unknown run: total={:?}, per_unit={:?}", report.total, report.per_unit),
            format!("unavailable terms: {:?}", report.unavailable_terms),
            format!("not-applicable control: total known={control_known}, duty zero={control_zero}"),
        ],
        vec![test(
            "The unknown duty term can be treated as zero without changing the decision.",
            "Obtain the binding classification and current tariff rate, compute the duty, and recompute the landed total; compare the resulting decision with the zero-assumption decision.",
            "The duty is confirmed to be zero, or the decision is unchanged after the real duty is included.",
            "The real duty is non-zero and changes the total or the recommendation, proving the zero substitution was wrong.",
            "Binding ruling, current tariff schedule, customs entry or broker calculation.",
            &[
                "an unknown duty term makes the total and per-unit TCO unavailable",
                "an explicitly not-applicable duty contributes zero and is flagged separately",
            ],
        )],
        vec![ControlAnswer {
            name: "not-applicable".to_string(),
            outcome: SutOutcome::Solved {
                observations: vec![Observation::money(
                    "not-applicable landed total",
                    control_total,
                )],
            },
            uncertainty: SutUncertainty::Exact,
        }],
    ))
}

pub(crate) fn run(id: &str) -> Result<SutAnswer, BenchError> {
    let answer = match id {
        SUPPLIER_ID => supplier()?,
        HS_ID => hs_classification()?,
        UNIT_ID => unit_conversion()?,
        UNKNOWN_COST_ID => unknown_cost()?,
        other => return Err(BenchError::UnknownCase(other.to_string())),
    };
    if answer.id != id {
        return Err(case_error(id, "SUT answer id mismatch"));
    }
    if answer.experiments.is_empty() {
        return Err(case_error(id, "SUT produced no falsifiable experiment"));
    }
    Ok(answer)
}
