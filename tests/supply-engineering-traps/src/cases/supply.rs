use faktor_supply_core::{
    Assumption, Currency, Day, EvidenceRef, Money, Provenance, Rate, Revision, SourceKind, Term,
    Units,
};
use faktor_supply_cost::{
    not_applicable_term, tco, unknown_term, Duty, ExcessMoq, ExpediteCost, Insurance, Obsolescence,
    QualityCost, Tax, TcoInput, TcoTerm, WorkingCapital,
};
use faktor_supply_logistics::{
    classify, compute_duty, ClassificationOutcome, ClassificationRequest, CustomsValuation,
    EscalationKind, LogisticsError, TariffSchedule, ValuationBasis,
};

use super::{case_error, falsifiable, money_text};
use crate::{CaseBuilder, CaseOutcome, CaseReport, ExpectedKind, Metrics, Observation};

pub(crate) const SUPPLIER_ID: &str = "supply/cheaper-supplier-tco";
pub(crate) const HS_ID: &str = "customs/hs-classification-ambiguity";
pub(crate) const UNKNOWN_COST_ID: &str = "supply/unknown-cost-not-zero";

const CURRENCY: Currency = Currency::USD;
const FIXTURE_DAY: Day = Day(20_000);

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

fn known_total(report: &faktor_supply_cost::Tco, case: &str) -> Result<Money, crate::BenchError> {
    report
        .total
        .as_known()
        .copied()
        .ok_or_else(|| case_error(case, "TCO total is unexpectedly unavailable"))
}

pub(crate) fn supplier() -> Result<CaseReport, crate::BenchError> {
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
    let all_terms = incumbent.lines.len() == 15 && cheap.lines.len() == 15;
    let all_based = incumbent
        .lines
        .iter()
        .chain(cheap.lines.iter())
        .all(|line| !line.amount.is_known() || !line.basis.is_empty());
    let physics = all_terms
        && all_based
        && incumbent.unavailable_terms.is_empty()
        && cheap.unavailable_terms.is_empty()
        && cheap_total.micros() > incumbent_total.micros()
        && cheap_per_unit.micros() > incumbent_per_unit.micros()
        && landed_difference.micros() > price_only_saving.micros();
    let test = falsifiable(
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
    );
    let evidence = vec![
        format!("incumbent landed total: {}", money_text(incumbent_total)),
        format!("cheap landed total: {}", money_text(cheap_total)),
        format!("price-only saving at 500 units: {}", money_text(price_only_saving)),
        format!("landed difference: {}", money_text(landed_difference)),
        format!(
            "per-unit incumbent {:?}, cheap {:?}",
            incumbent_per_unit, cheap_per_unit
        ),
        "all 15 TCO terms are present and each known term carries evidence or an explicit assumption".to_string(),
    ];
    CaseBuilder::new(SUPPLIER_ID, "supply/tco", "Cheaper unit price vanishes under landed TCO", ExpectedKind::Solved)
        .shallow("$8.50 is cheaper than $10.00, so the cheap supplier saves money.")
        .outcome(CaseOutcome::Solved {
            observations: vec![
                Observation::money("incumbent landed total", incumbent_total),
                Observation::money("cheap landed total", cheap_total),
                Observation::money("price-only saving", price_only_saving),
                Observation::money("landed cost difference", landed_difference),
                Observation::money("incumbent per unit", incumbent_per_unit),
                Observation::money("cheap per unit", cheap_per_unit),
                Observation::text("recommendation", "incumbent"),
            ],
            notes: vec![
                "the recommendation is the incumbent because the required-quantity landed TCO is lower".to_string(),
                "no unknown term was zero-filled".to_string(),
            ],
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(all_based)
                .with_missing_info(all_terms)
                .with_boundaries(true)
                .with_uncertainty(true)
                .with_trap(cheap_total.micros() > incumbent_total.micros())
                .with_experiment(true)
                .with_refusal(cheap_total.micros() > incumbent_total.micros()),
        )
        .evidence(evidence)
        .missing(vec![
            "binding quote at the required quantity and MOQ".to_string(),
            "actual freight, duty and cash-cycle terms".to_string(),
            "verified lot yield/quality evidence".to_string(),
        ])
        .boundaries(vec![
            "MOQ boundary: 1000-unit minimum against 500 required units".to_string(),
            "excess-MOQ carrying is charged on the 500 excess units".to_string(),
            "cash cycle is charged for 90 days on the financed amount".to_string(),
        ])
        .uncertainty("Money and quantities are exact integer arithmetic; the remaining uncertainty is commercial evidence (quotes, yield, cash terms), not floating-point rounding.")
        .test(test)
        .build()
}

pub(crate) fn hs_classification() -> Result<CaseReport, crate::BenchError> {
    let request: ClassificationRequest =
        serde_json::from_str(include_str!("../../fixtures/hs_ambiguous.json"))
            .map_err(|error| case_error(HS_ID, format!("classification fixture: {error}")))?;
    let classification = classify(&request).map_err(|error| case_error(HS_ID, error))?;
    let ambiguous = match &classification.outcome {
        ClassificationOutcome::Ambiguous { codes } => {
            codes == &["853222".to_string(), "853229".to_string()]
        }
        _ => false,
    };
    let escalation = classification
        .escalation
        .as_ref()
        .ok_or_else(|| case_error(HS_ID, "ambiguous classification produced no escalation"))?;
    let binding = escalation.kind == EscalationKind::BindingRulingRequired;
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
    let duty_refused = matches!(
        compute_duty(
            &classification,
            &tariff,
            "US",
            &valuation,
            None,
            FIXTURE_DAY
        ),
        Err(LogisticsError::ClassificationNotFinal(_))
    );
    let mut missing_request = request.clone();
    missing_request.facts.composition = None;
    missing_request.facts.function = None;
    missing_request.facts.essential_character = None;
    let missing_classification =
        classify(&missing_request).map_err(|error| case_error(HS_ID, error))?;
    let missing_detected = match &missing_classification.outcome {
        ClassificationOutcome::InsufficientFacts { missing } => !missing.is_empty(),
        _ => false,
    };
    let missing_escalation = missing_classification
        .escalation
        .as_ref()
        .is_some_and(|escalation| escalation.kind == EscalationKind::ProductFactsRequired);
    let physics = ambiguous && binding && duty_refused && missing_detected && missing_escalation;
    let evidence_refs = classification.evidence.len();
    let test = falsifiable(
        "The product falls under HS 853222.",
        "Request a binding tariff classification ruling from the destination customs authority, supplying composition, function, essential character and the candidate headings; then recompute duty from the ruling.",
        "The authority issues exactly one binding code and the duty recomputes from that code with current tariff evidence.",
        "The ruling selects a different code, confirms ambiguity, or requires product facts that are missing.",
        "Binding customs ruling, current tariff schedule, laboratory/product composition evidence.",
        &[
            "two candidate classifications are viable on the supplied facts",
            "duty computation refuses to run on an unfinalized classification",
            "removing composition/function facts produces a typed insufficient-facts escalation",
        ],
    );
    CaseBuilder::new(HS_ID, "customs", "Classification ambiguity must escalate, not pick a code", ExpectedKind::Escalated)
        .shallow("Choose the candidate with the lower duty rate and declare it.")
        .outcome(CaseOutcome::Escalated {
            escalation: "binding_ruling_required".to_string(),
            reason: escalation.reason.clone(),
            authority: escalation.authority.clone(),
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(evidence_refs > 0)
                .with_missing_info(missing_detected && missing_escalation)
                .with_boundaries(true)
                .with_uncertainty(true)
                .with_trap(ambiguous)
                .with_experiment(true)
                .with_refusal(duty_refused && binding),
        )
        .evidence(vec![
            format!("classification outcome: {:?}", classification.outcome),
            format!("escalation: {:?}: {}", escalation.kind, escalation.reason),
            format!("evidence references bound to candidates: {evidence_refs}"),
            "duty computation refused on the unfinalized classification".to_string(),
            format!(
                "missing-facts control: {:?}, escalation {:?}",
                missing_classification.outcome, missing_classification.escalation
            ),
        ])
        .missing(vec![
            "a binding classification ruling".to_string(),
            "product composition, function and essential character when unknown".to_string(),
        ])
        .boundaries(vec![
            "exactly one viable candidate is required before duty/tax computation".to_string(),
            "multiple viable candidates are escalated, never averaged or ranked by duty".to_string(),
        ])
        .uncertainty("The classification is a conflict between authoritative candidates, not a statistical uncertainty; it remains unresolved until a binding ruling.")
        .test(test)
        .build()
}

pub(crate) fn unknown_cost() -> Result<CaseReport, crate::BenchError> {
    let mut input = quote_input(&incumbent_profile());
    input.duty =
        unknown_term::<Duty>("tariff rate is unavailable for the unresolved classification");
    let report = tco(&input).map_err(|error| case_error(UNKNOWN_COST_ID, error))?;
    let total_unavailable = !report.total.is_known()
        && !report.per_unit.is_known()
        && report.unavailable_terms == vec![TcoTerm::Duty];
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
    let physics = total_unavailable && control_known && control_zero;
    let test = falsifiable(
        "The unknown duty term can be treated as zero without changing the decision.",
        "Obtain the binding classification and current tariff rate, compute the duty, and recompute the landed total; compare the resulting decision with the zero-assumption decision.",
        "The duty is confirmed to be zero, or the decision is unchanged after the real duty is included.",
        "The real duty is non-zero and changes the total or the recommendation, proving the zero substitution was wrong.",
        "Binding ruling, current tariff schedule, customs entry or broker calculation.",
        &[
            "an unknown duty term makes the total and per-unit TCO unavailable",
            "an explicitly not-applicable duty contributes zero and is flagged separately",
        ],
    );
    CaseBuilder::new(UNKNOWN_COST_ID, "supply/tco", "An unknown cost term is unavailable, never zero", ExpectedKind::Unavailable)
        .shallow("Duty is unknown but probably small, so set it to zero and compare totals.")
        .outcome(CaseOutcome::Unavailable {
            reason: format!(
                "unknown terms make the landed total unavailable: {:?}",
                report.unavailable_terms
            ),
            required: vec![
                "binding HS classification".to_string(),
                "current tariff rate for the destination and origin".to_string(),
                "customs valuation basis".to_string(),
            ],
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(total_unavailable)
                .with_missing_info(total_unavailable)
                .with_boundaries(control_zero)
                .with_uncertainty(true)
                .with_trap(total_unavailable)
                .with_experiment(true)
                .with_refusal(total_unavailable),
        )
        .evidence(vec![
            format!("unknown run: total={:?}, per_unit={:?}", report.total, report.per_unit),
            format!("unavailable terms: {:?}", report.unavailable_terms),
            format!("not-applicable control: total known={control_known}, duty zero={control_zero}"),
        ])
        .missing(vec![
            "binding HS classification".to_string(),
            "current tariff rate".to_string(),
        ])
        .boundaries(vec![
            "unknown is not the same as explicitly not applicable".to_string(),
            "an unknown term propagates to total and per-unit unavailability".to_string(),
        ])
        .uncertainty("The unknown term carries a typed reason and makes the total unavailable; it is not assigned a zero or a point estimate.")
        .test(test)
        .build()
}
