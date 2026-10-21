//! Deterministic engineering models.
//!
//! Every model is a pure Rust kernel with explicit applicability, typed
//! refusals, dimensional checks, uncertainty and an executable analysis
//! receipt. No model silently fills a missing input with zero. The shipped
//! starters are:
//!
//! * bolted-joint torque → preload with a scattered nut factor;
//! * MOSFET conduction loss coupled to a junction-to-ambient thermal chain;
//! * injection-molding draft, wall-uniformity and flow-ratio DFM checks;
//! * op-amp loop gain/phase margin with output/input capacitance;
//! * flyback CCM/DCM currents, leakage-energy clamp and Vds stress;
//! * three-node transient thermal network with a required interface layer;
//! * S-N/ε-N fatigue damage inside explicit validity regimes;
//! * PCB return-path loop inductance and rule-driven spacing checks;
//! * conditional material-property lookup by temper, thickness and temperature.

pub mod analog;
pub mod bolt;
pub mod common;
pub mod emc;
pub mod fatigue;
pub mod flyback;
pub mod materials;
pub mod molding;
pub mod mosfet;
pub mod thermal_network;

pub use analog::{
    identity as analog_identity, opamp_loop_stability, OpAmpStabilityInput, OpAmpStabilityResult,
    OPAMP_STABILITY_MODEL, VERSION as ANALOG_VERSION,
};
pub use bolt::{
    identity as bolt_identity, torque_preload, NutFactorScatter, TorquePreloadInput,
    TorquePreloadResult, BOLTED_JOINT_MODEL, VERSION as BOLT_VERSION,
};
pub use common::{
    AssumptionRecord, CrossCheckEvidence, ModelContext, ModelError, ModelReport, ModelResult,
    ModelStatus, QuantityRange, Refusal, RefusalCode, MODEL_VERSION,
};
pub use emc::{
    emc_pcb_check, identity as emc_identity, EmcPcbInput, EmcPcbResult, EmcRuleCheck, EmcRules,
    PcbStackup, EMC_PCB_MODEL, VERSION as EMC_VERSION,
};
pub use fatigue::{
    fatigue_damage, identity as fatigue_identity, BlockDamage, FatigueBlock, FatigueInput,
    FatigueRegime, FatigueResult, MeanStressCorrection, SnCurve, StrainLifeCurve, FATIGUE_MODEL,
    VERSION as FATIGUE_VERSION,
};
pub use flyback::{
    flyback_converter, identity as flyback_identity, FlybackInput, FlybackMode, FlybackResult,
    FLYBACK_MODEL, VERSION as FLYBACK_VERSION,
};
pub use materials::{
    identity as materials_identity, lookup_material_property, MaterialPropertyQuery,
    MaterialPropertyRecord, MaterialPropertyResult, MATERIAL_PROPERTY_MODEL,
    VERSION as MATERIALS_VERSION,
};
pub use molding::{
    check_moldability, identity as molding_identity, CheckStatus, ManufacturabilityCheck,
    MoldabilityInput, MoldabilityResult, MoldingRules, MOLDING_MODEL, VERSION as MOLDING_VERSION,
};
pub use mosfet::{
    identity as mosfet_identity, mosfet_conduction_thermal, Estimate, MosfetThermalInput,
    MosfetThermalResult, MOSFET_THERMAL_MODEL, VERSION as MOSFET_VERSION,
};
pub use thermal_network::{
    identity as thermal_network_identity, thermal_network, ThermalLayer, ThermalNetworkInput,
    ThermalNetworkResult, ThermalTransientPoint, THERMAL_NETWORK_MODEL,
    VERSION as THERMAL_NETWORK_VERSION,
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
            analog_identity().solver_id,
            flyback_identity().solver_id,
            thermal_network_identity().solver_id,
            fatigue_identity().solver_id,
            emc_identity().solver_id,
            materials_identity().solver_id,
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
        for id in &ids[3..] {
            assert!(id.starts_with("engineering-models/"));
        }
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        assert!(ids.iter().all(|id| !id.contains("placeholder")));
        assert!(!MODEL_VERSION.is_empty());
    }
}
