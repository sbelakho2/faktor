//! Deterministic engineering models.
//!
//! Every model is a pure Rust kernel with explicit applicability, typed
//! refusals, dimensional checks, uncertainty and an executable analysis
//! receipt. No model silently fills a missing input with zero. The shipped
//! starters are:
//!
//! * bolted-joint torque → preload with a scattered nut factor;
//! * MOSFET conduction loss coupled to a junction-to-ambient thermal chain;
//! * injection-molding draft, wall-uniformity and flow-ratio DFM checks.

pub mod bolt;
pub mod common;
pub mod molding;
pub mod mosfet;

pub use bolt::{
    identity as bolt_identity, torque_preload, NutFactorScatter, TorquePreloadInput,
    TorquePreloadResult, BOLTED_JOINT_MODEL, VERSION as BOLT_VERSION,
};
pub use common::{
    AssumptionRecord, CrossCheckEvidence, ModelContext, ModelError, ModelReport, ModelResult,
    ModelStatus, QuantityRange, Refusal, RefusalCode, MODEL_VERSION,
};
pub use molding::{
    check_moldability, identity as molding_identity, CheckStatus, ManufacturabilityCheck,
    MoldabilityInput, MoldabilityResult, MoldingRules, MOLDING_MODEL, VERSION as MOLDING_VERSION,
};
pub use mosfet::{
    identity as mosfet_identity, mosfet_conduction_thermal, Estimate, MosfetThermalInput,
    MosfetThermalResult, MOSFET_THERMAL_MODEL, VERSION as MOSFET_VERSION,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_identities_are_distinct_and_versioned() {
        let ids = [
            bolt_identity().solver_id,
            mosfet_identity().solver_id,
            molding_identity().solver_id,
        ];
        assert_eq!(ids[0], "engineering-models/bolted-joint/torque-preload");
        assert_eq!(
            ids[1],
            "engineering-models/power-electronics/mosfet-conduction-thermal"
        );
        assert_eq!(
            ids[2],
            "engineering-models/manufacturing/injection-moldability"
        );
        assert!(ids.iter().all(|id| !id.contains("placeholder")));
        assert!(!MODEL_VERSION.is_empty());
    }
}
