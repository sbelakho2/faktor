use std::collections::BTreeMap;

use engineering_core::{
    Convergence, DimensionCheck, Provenance, ProvenanceKind, ValueState, WarningSeverity,
};
use engineering_units::{Dimension, Quantity, Uncertainty, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    require_finite, require_positive, unavailable_result, value_in, warning, AssumptionRecord,
    CrossCheckEvidence, ModelContext, ModelError, ModelReport, ModelResult, ModelStatus, Refusal,
    RefusalCode, MODEL_VERSION,
};

pub const FATIGUE_MODEL: &str = "mechanical/fatigue-damage";
const MODEL_EXPRESSION: &str = "sigma_a,eq = sigma_a/(1-sigma_m/UTS) [Goodman] or sigma_a/(1-sigma_m/sigma_f') [Morrow]; N_f = 0.5 (sigma_a,eq/sigma_f')^(1/b); eps_a = (sigma_f'-sigma_m)/E (2N)^b + eps_f' (2N)^c; D = sum n_i/N_i";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeanStressCorrection {
    Goodman,
    Morrow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnCurve {
    pub stress_coefficient: Quantity,
    pub stress_exponent: f64,
    pub endurance_limit: Option<Quantity>,
    pub endurance_cycles: f64,
    pub yield_strength: Quantity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrainLifeCurve {
    pub stress_coefficient: Quantity,
    pub stress_exponent: f64,
    pub ductility_coefficient: f64,
    pub ductility_exponent: f64,
    pub elastic_modulus: Quantity,
    pub transition_cycles: f64,
    pub max_total_strain: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FatigueBlock {
    pub description: Option<String>,
    pub stress_amplitude: Option<Quantity>,
    pub strain_amplitude: Option<Quantity>,
    pub mean_stress: Quantity,
    pub cycles: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FatigueInput {
    pub blocks: Vec<FatigueBlock>,
    pub uts: Quantity,
    pub sn: Option<SnCurve>,
    pub strain_life: Option<StrainLifeCurve>,
    pub mean_stress_correction: MeanStressCorrection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FatigueRegime {
    InfiniteLife,
    StressLife,
    StrainLife,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockDamage {
    pub description: String,
    pub regime: FatigueRegime,
    pub cycles: f64,
    pub corrected_amplitude: Quantity,
    pub life_cycles: ValueState<f64>,
    pub damage: ValueState<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FatigueResult {
    pub damage: f64,
    pub infinite_life: bool,
    pub blocks: Vec<BlockDamage>,
    pub minimum_life_cycles: Option<f64>,
    pub life_factor: Option<f64>,
}

fn assumption(id: &str, statement: &str, citation: &str) -> AssumptionRecord {
    AssumptionRecord::new(
        id,
        statement,
        Provenance::new(ProvenanceKind::EngineeringPractice, citation)
            .expect("static non-empty provenance"),
    )
}

fn assumptions() -> Vec<AssumptionRecord> {
    vec![
        assumption(
            "asm-basquin-regime",
            "The S-N branch uses the Basquin relation only below the yield strength and above the declared endurance limit; low-cycle stress above yield is refused rather than extrapolated.",
            "Fatigue-materials knowledge pack: S-N validity regime.",
        ),
        assumption(
            "asm-strain-life-regime",
            "The strain-life branch uses the Coffin-Manson total-strain relation only up to the declared transition cycles and maximum strain.",
            "Fatigue-materials knowledge pack: strain-life validity regime.",
        ),
        assumption(
            "asm-miner-linear",
            "Block damage accumulates linearly by Miner's rule; load-sequence interaction and mean-stress relaxation are outside the model.",
            "Fatigue-materials knowledge pack: Miner damage.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "sigma_a,eq = sigma_a/(1-sigma_m/UTS)",
            Dimension::PRESSURE,
            Dimension::PRESSURE.checked_div(Dimension::SCALAR)?,
        )?,
        DimensionCheck::new(
            "N_f = 0.5 (sigma_a/sigma_f')^(1/b)",
            Dimension::SCALAR,
            Dimension::SCALAR.checked_div(Dimension::SCALAR)?,
        )?,
        DimensionCheck::new(
            "eps_elastic = sigma/E",
            Dimension::SCALAR,
            Dimension::PRESSURE.checked_div(Dimension::PRESSURE)?,
        )?,
        DimensionCheck::new(
            "D = sum n_i/N_i",
            Dimension::SCALAR,
            Dimension::SCALAR.checked_div(Dimension::SCALAR)?,
        )?,
    ])
}

fn pascal(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("Pa")?)?)
}

fn dimensionless(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::dimensionless(value)?)
}

fn refusal_report(
    input: &FatigueInput,
    context: &ModelContext,
    refusal: Refusal,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<FatigueResult>> {
    let reason = refusal.reason.clone();
    let mut bcs = BTreeMap::new();
    bcs.insert("uts".to_string(), boundary_value(&input.uts, context)?);
    let warnings = vec![warning(
        "model.refused",
        reason.clone(),
        WarningSeverity::Critical,
    )];
    let results = vec![unavailable_result(
        "damage",
        reason.clone(),
        vec!["a fatigue curve and load blocks inside its validity regime".to_string()],
    )?];
    let receipt = build_receipt(
        FATIGUE_MODEL,
        input,
        MODEL_EXPRESSION,
        &input,
        context,
        bcs,
        Convergence::iterative(1, 0.0, 1.0, false),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;
    Ok(ModelReport {
        status: ModelStatus::Refused,
        value: ValueState::unavailable(reason, vec!["valid fatigue analysis".to_string()])?,
        refusal: Some(refusal),
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

fn solve_strain_life(
    curve: &StrainLifeCurve,
    strain_amplitude: f64,
    mean_stress: f64,
    uts: f64,
    correction: MeanStressCorrection,
) -> Option<f64> {
    let sigma_f = value_in(&curve.stress_coefficient, "Pa").ok()?;
    let elastic_modulus = value_in(&curve.elastic_modulus, "Pa").ok()?;
    let elastic_coefficient = match correction {
        MeanStressCorrection::Morrow => (sigma_f - mean_stress) / elastic_modulus,
        MeanStressCorrection::Goodman => {
            if uts <= 0.0 || mean_stress >= uts {
                return None;
            }
            sigma_f / elastic_modulus * (1.0 - mean_stress / uts)
        }
    };
    let function = |cycles: f64| {
        let reversals = 2.0 * cycles;
        elastic_coefficient * reversals.powf(curve.stress_exponent)
            + curve.ductility_coefficient * reversals.powf(curve.ductility_exponent)
            - strain_amplitude
    };
    let mut low = 0.5;
    let mut high = curve.transition_cycles.max(1.0);
    let mut f_low = function(low);
    let mut f_high = function(high);
    if !f_low.is_finite() || !f_high.is_finite() {
        return None;
    }
    if f_low == 0.0 {
        return Some(low);
    }
    if f_low * f_high > 0.0 {
        return None;
    }
    for _ in 0..200 {
        let middle = (low * high).sqrt();
        let value = function(middle);
        if value == 0.0 {
            return Some(middle);
        }
        if f_low * value <= 0.0 {
            high = middle;
            f_high = value;
        } else {
            low = middle;
            f_low = value;
        }
        if (high - low).abs() <= 1e-9 * high.max(1.0) {
            break;
        }
    }
    let solution = (low * high).sqrt();
    let _ = f_high;
    Some(solution)
}

fn analyze_block(
    block: &FatigueBlock,
    input: &FatigueInput,
    uts: f64,
) -> Result<BlockDamage, Refusal> {
    if !block.cycles.is_finite() || block.cycles <= 0.0 {
        return Err(Refusal::new(
            RefusalCode::OutsideValidity,
            "block cycle count must be finite and positive",
            vec!["a positive cycle count".to_string()],
        ));
    }
    require_dimension("mean_stress", &block.mean_stress, Dimension::PRESSURE).map_err(|error| {
        Refusal::new(
            RefusalCode::OutsideValidity,
            error.to_string(),
            vec!["mean stress with pressure dimension".to_string()],
        )
    })?;
    let mean = value_in(&block.mean_stress, "Pa").map_err(|error| {
        Refusal::new(
            RefusalCode::OutsideValidity,
            error.to_string(),
            vec!["mean stress in pascals".to_string()],
        )
    })?;
    if mean >= uts {
        return Err(Refusal::new(
            RefusalCode::OutsideValidity,
            "mean stress is at or above the ultimate tensile strength; the Goodman/Morrow correction has no finite positive denominator",
            vec!["a mean stress below the ultimate tensile strength".to_string()],
        ));
    }
    match (&block.stress_amplitude, &block.strain_amplitude) {
        (Some(stress), None) => {
            require_dimension("stress_amplitude", stress, Dimension::PRESSURE).map_err(
                |error| {
                    Refusal::new(
                        RefusalCode::OutsideValidity,
                        error.to_string(),
                        vec!["stress amplitude with pressure dimension".to_string()],
                    )
                },
            )?;
            let amplitude = value_in(stress, "Pa").map_err(|error| {
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    error.to_string(),
                    vec!["stress amplitude in pascals".to_string()],
                )
            })?;
            if amplitude < 0.0 {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    "stress amplitude must be non-negative",
                    vec!["a non-negative stress amplitude".to_string()],
                ));
            }
            if amplitude == 0.0 {
                return Ok(BlockDamage {
                    description: block
                        .description
                        .clone()
                        .unwrap_or_else(|| "block".to_string()),
                    regime: FatigueRegime::InfiniteLife,
                    cycles: block.cycles,
                    corrected_amplitude: pascal(0.0).expect("zero amplitude"),
                    life_cycles: ValueState::Unavailable(
                        engineering_core::UnavailableValue::new(
                            "zero stress amplitude produces no fatigue damage; life is infinite",
                            vec!["a non-zero stress amplitude".to_string()],
                        )
                        .expect("static unavailable"),
                    ),
                    damage: ValueState::Known(0.0),
                });
            }
            let Some(sn) = &input.sn else {
                return Err(Refusal::new(
                    RefusalCode::MissingProperty,
                    "no S-N curve is supplied for the stress-life block",
                    vec!["revision-bound S-N curve with exponent and endurance limit".to_string()],
                ));
            };
            let yield_strength = value_in(&sn.yield_strength, "Pa").map_err(|error| {
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    error.to_string(),
                    vec!["yield strength in pascals".to_string()],
                )
            })?;
            if amplitude > yield_strength {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    format!(
                        "stress amplitude {:.6e} Pa exceeds yield strength {:.6e} Pa; S-N is outside its validity regime and a strain-life block is required",
                        amplitude, yield_strength
                    ),
                    vec!["strain-life parameters and a strain-amplitude block for the low-cycle regime".to_string()],
                ));
            }
            let corrected = match input.mean_stress_correction {
                MeanStressCorrection::Goodman => amplitude / (1.0 - mean / uts),
                MeanStressCorrection::Morrow => {
                    let sigma_f = value_in(&sn.stress_coefficient, "Pa").map_err(|error| {
                        Refusal::new(
                            RefusalCode::OutsideValidity,
                            error.to_string(),
                            vec!["fatigue strength coefficient in pascals".to_string()],
                        )
                    })?;
                    if mean >= sigma_f {
                        return Err(Refusal::new(
                            RefusalCode::OutsideValidity,
                            "mean stress is at or above the fatigue strength coefficient; Morrow correction is outside its validity",
                            vec!["a mean stress below the fatigue strength coefficient".to_string()],
                        ));
                    }
                    amplitude / (1.0 - mean / sigma_f)
                }
            };
            let endurance = sn
                .endurance_limit
                .as_ref()
                .map(|quantity| value_in(quantity, "Pa"))
                .transpose()
                .map_err(|error| {
                    Refusal::new(
                        RefusalCode::OutsideValidity,
                        error.to_string(),
                        vec!["endurance limit in pascals".to_string()],
                    )
                })?;
            if let Some(limit) = endurance {
                if corrected <= limit {
                    return Ok(BlockDamage {
                        description: block.description.clone().unwrap_or_else(|| "block".to_string()),
                        regime: FatigueRegime::InfiniteLife,
                        cycles: block.cycles,
                        corrected_amplitude: pascal(corrected).expect("finite corrected amplitude"),
                        life_cycles: ValueState::Unavailable(
                            engineering_core::UnavailableValue::new(
                                "stress is at or below the declared endurance limit; life is treated as infinite under the run-out definition",
                                vec!["a finite-life test beyond the endurance limit".to_string()],
                            )
                            .expect("static unavailable"),
                        ),
                        damage: ValueState::Known(0.0),
                    });
                }
            }
            let sigma_f = value_in(&sn.stress_coefficient, "Pa").map_err(|error| {
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    error.to_string(),
                    vec!["fatigue strength coefficient in pascals".to_string()],
                )
            })?;
            require_positive("stress_coefficient", sigma_f).map_err(|error| {
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    error.to_string(),
                    vec!["a positive stress coefficient".to_string()],
                )
            })?;
            require_finite("stress_exponent", sn.stress_exponent).map_err(|error| {
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    error.to_string(),
                    vec!["a finite stress exponent".to_string()],
                )
            })?;
            if sn.stress_exponent >= 0.0 {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    "S-N exponent must be negative for a decreasing finite-life curve",
                    vec!["a negative Basquin exponent".to_string()],
                ));
            }
            let life = 0.5 * (corrected / sigma_f).powf(1.0 / sn.stress_exponent);
            if !life.is_finite() || life <= 0.0 || life > 1e15 {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    "the corrected stress produces a life outside the finite S-N curve range",
                    vec!["a revision-bound S-N curve covering the required life".to_string()],
                ));
            }
            Ok(BlockDamage {
                description: block
                    .description
                    .clone()
                    .unwrap_or_else(|| "block".to_string()),
                regime: FatigueRegime::StressLife,
                cycles: block.cycles,
                corrected_amplitude: pascal(corrected).expect("finite corrected amplitude"),
                life_cycles: ValueState::Known(life),
                damage: ValueState::Known(block.cycles / life),
            })
        }
        (None, Some(strain)) => {
            require_dimension("strain_amplitude", strain, Dimension::SCALAR).map_err(|error| {
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    error.to_string(),
                    vec!["dimensionless strain amplitude".to_string()],
                )
            })?;
            let amplitude = value_in(strain, "1").map_err(|error| {
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    error.to_string(),
                    vec!["strain amplitude as a dimensionless ratio".to_string()],
                )
            })?;
            if amplitude <= 0.0 {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    "strain amplitude must be positive",
                    vec!["a positive strain amplitude".to_string()],
                ));
            }
            let Some(curve) = &input.strain_life else {
                return Err(Refusal::new(
                    RefusalCode::MissingProperty,
                    "no strain-life curve is supplied for the strain block",
                    vec![
                        "revision-bound Coffin-Manson parameters and transition cycles".to_string(),
                    ],
                ));
            };
            if amplitude > curve.max_total_strain {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    format!(
                        "strain amplitude {amplitude} exceeds the fitted maximum {}; the strain-life curve is outside its validity",
                        curve.max_total_strain
                    ),
                    vec!["a strain-life curve covering the required strain range".to_string()],
                ));
            }
            let sigma_f = value_in(&curve.stress_coefficient, "Pa").map_err(|error| {
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    error.to_string(),
                    vec!["fatigue strength coefficient in pascals".to_string()],
                )
            })?;
            if mean >= sigma_f && input.mean_stress_correction == MeanStressCorrection::Morrow {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    "mean stress is at or above the fatigue strength coefficient; the strain-life Morrow correction is outside its validity",
                    vec!["a mean stress below the fatigue strength coefficient".to_string()],
                ));
            }
            let Some(life) =
                solve_strain_life(curve, amplitude, mean, uts, input.mean_stress_correction)
            else {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    "the strain-life equation has no solution inside the declared transition regime",
                    vec!["a strain-life curve covering the required strain and life".to_string()],
                ));
            };
            if life > curve.transition_cycles {
                return Err(Refusal::new(
                    RefusalCode::OutsideValidity,
                    "the strain-life solution crosses the transition to the S-N regime; a stress-life curve is required",
                    vec!["an S-N curve for the long-life regime".to_string()],
                ));
            }
            Ok(BlockDamage {
                description: block
                    .description
                    .clone()
                    .unwrap_or_else(|| "block".to_string()),
                regime: FatigueRegime::StrainLife,
                cycles: block.cycles,
                corrected_amplitude: dimensionless(amplitude).expect("finite strain amplitude"),
                life_cycles: ValueState::Known(life),
                damage: ValueState::Known(block.cycles / life),
            })
        }
        _ => Err(Refusal::new(
            RefusalCode::OutsideValidity,
            "each block must supply exactly one of stress amplitude or strain amplitude",
            vec!["exactly one fatigue amplitude measure per block".to_string()],
        )),
    }
}

pub fn fatigue_damage(
    input: &FatigueInput,
    context: &ModelContext,
) -> ModelResult<ModelReport<FatigueResult>> {
    let checks = checks()?;
    require_dimension("uts", &input.uts, Dimension::PRESSURE)?;
    require_positive("uts", value_in(&input.uts, "Pa")?)?;
    if input.blocks.is_empty() || input.blocks.len() > 256 {
        return Err(ModelError::InvalidInput {
            field: "blocks",
            reason: "1..=256 load blocks are required".to_string(),
        });
    }
    let uts = value_in(&input.uts, "Pa")?;
    if let Some(curve) = &input.strain_life {
        require_finite("transition_cycles", curve.transition_cycles)?;
        require_finite("max_total_strain", curve.max_total_strain)?;
        if curve.transition_cycles <= 0.5 || curve.max_total_strain <= 0.0 {
            return Err(ModelError::InvalidInput {
                field: "strain_life",
                reason:
                    "transition cycles must exceed 0.5 and maximum total strain must be positive"
                        .to_string(),
            });
        }
        require_dimension(
            "strain_life.elastic_modulus",
            &curve.elastic_modulus,
            Dimension::PRESSURE,
        )?;
        require_dimension(
            "strain_life.stress_coefficient",
            &curve.stress_coefficient,
            Dimension::PRESSURE,
        )?;
    }
    if let Some(sn) = &input.sn {
        require_finite("endurance_cycles", sn.endurance_cycles)?;
        if sn.endurance_cycles <= 0.0 {
            return Err(ModelError::InvalidInput {
                field: "endurance_cycles",
                reason: "must be positive".to_string(),
            });
        }
        require_dimension(
            "sn.stress_coefficient",
            &sn.stress_coefficient,
            Dimension::PRESSURE,
        )?;
        require_dimension("sn.yield_strength", &sn.yield_strength, Dimension::PRESSURE)?;
    }
    let mut damages = Vec::with_capacity(input.blocks.len());
    let mut minimum_life: Option<f64> = None;
    for block in &input.blocks {
        match analyze_block(block, input, uts) {
            Ok(damage) => {
                if let ValueState::Known(life) = &damage.life_cycles {
                    minimum_life = Some(minimum_life.map_or(*life, |value| value.min(*life)));
                }
                damages.push(damage);
            }
            Err(refusal) => return refusal_report(input, context, refusal, checks),
        }
    }
    let total: f64 = damages
        .iter()
        .map(|block| match &block.damage {
            ValueState::Known(value) => *value,
            ValueState::Unavailable(_) => 0.0,
        })
        .sum();
    if !total.is_finite() || total < 0.0 {
        return refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                "accumulated damage is not finite",
                vec!["finite per-block lives and cycles".to_string()],
            ),
            checks,
        );
    }
    let infinite_life = total == 0.0;
    let life_factor = if total > 0.0 { Some(1.0 / total) } else { None };
    let mut warnings = Vec::new();
    if total >= 1.0 {
        warnings.push(warning(
            "fatigue.damage",
            "Miner damage is at or above one; the declared load history exhausts the fatigue life",
            WarningSeverity::Critical,
        ));
    } else if total > 0.5 {
        warnings.push(warning(
            "fatigue.margin",
            "Miner damage exceeds 0.5; the remaining life margin is small",
            WarningSeverity::Warning,
        ));
    }
    let mut bcs = BTreeMap::new();
    bcs.insert("uts".to_string(), boundary_value(&input.uts, context)?);
    if let Some(sn) = &input.sn {
        bcs.insert(
            "stress_coefficient".to_string(),
            boundary_value(&sn.stress_coefficient, context)?,
        );
        bcs.insert(
            "yield_strength".to_string(),
            boundary_value(&sn.yield_strength, context)?,
        );
    }
    let mut results = vec![
        quantity_result("miner_damage", dimensionless(total)?, Uncertainty::Exact),
        quantity_result(
            "block_count",
            dimensionless(input.blocks.len() as f64)?,
            Uncertainty::Exact,
        ),
    ];
    if let Some(factor) = life_factor {
        results.push(quantity_result(
            "life_factor",
            dimensionless(factor)?,
            Uncertainty::Exact,
        ));
    }
    if let Some(life) = minimum_life {
        results.push(quantity_result(
            "minimum_block_life_cycles",
            dimensionless(life)?,
            Uncertainty::Exact,
        ));
    }
    let cross_check = {
        let recomputed: f64 = damages
            .iter()
            .map(|block| match (&block.life_cycles, &block.damage) {
                (ValueState::Known(life), ValueState::Known(damage)) => {
                    (block.cycles / life - damage).abs()
                }
                _ => 0.0,
            })
            .sum();
        Some(CrossCheckEvidence::new(
            "per-block n/N residual recomputation",
            recomputed,
            0.0,
            1e-9,
        ))
    };
    let receipt = build_receipt(
        FATIGUE_MODEL,
        input,
        MODEL_EXPRESSION,
        &input,
        context,
        bcs,
        Convergence::exact(),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;
    Ok(ModelReport {
        status: ModelStatus::Solved,
        value: ValueState::known(FatigueResult {
            damage: total,
            infinite_life,
            blocks: damages,
            minimum_life_cycles: minimum_life,
            life_factor,
        }),
        refusal: None,
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check,
    })
}

pub fn identity() -> engineering_core::AnalysisIdentity {
    model_identity(FATIGUE_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;
