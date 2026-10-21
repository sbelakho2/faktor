//! Neutral engineering solver contract and supervised adapters.
//!
//! A solver receives a frozen [`FrozenCase`] (content digests re-checked
//! before every run), declares capabilities/applicability, and executes a
//! real external tool through the workspace [`ProcessSupervisor`]. The
//! result is an executable analysis receipt binding inputs, model, property
//! revisions, boundary conditions, settings, convergence, warnings, results,
//! uncertainty, artifacts and repeatability.
//!
//! A missing tool is a typed [`SolverOutcome::Unavailable`], never a fabricated
//! result. There is no shell interpolation: adapters pass argv arrays and
//! validate every setting they read.

pub mod case;
pub mod gmsh;
pub mod identity;
pub mod kicad;
pub mod ngspice;
pub mod openfoam;
pub mod solver;

use std::sync::Arc;

pub use case::{
    AnalysisRequest, CaseError, CaseResult, CaseSpec, FrozenCase, FrozenDirectory, FrozenFile,
    FrozenInput, ModelRef, MAX_CASE_BYTES, MAX_CASE_FILES, MAX_CASE_FILE_BYTES, MAX_INPUT_BYTES,
    MAX_INPUT_FILES, MAX_SETTINGS, MAX_SETTINGS_BYTES, MAX_SOLVE_DEADLINE_SECS,
};
pub use gmsh::{mesh_stats, GmshSolver, MeshStats, GMSH_VERSION};
pub type GmshAdapter = GmshSolver;
pub use identity::SolverIdentity;
pub use kicad::{KiCadCliSolver, KICAD_VERSION};
pub type KiCadCliAdapter = KiCadCliSolver;
pub use ngspice::{NgspiceSolver, NGSPICE_VERSION};
pub type NgspiceAdapter = NgspiceSolver;
pub use openfoam::{OpenFoamSolver, OPENFOAM_VERSION};
pub type OpenFoamAdapter = OpenFoamSolver;
pub use solver::{
    artifact_from_bytes, boundary_values, case_input_bytes, check_frozen, detect_program,
    failure_receipt, frozen_input_path, identity_for, identity_kind, process_output_evidence,
    quantity_result, read_output_file, receipt_from_process, run_tool, text_result, Applicability,
    EngineeringSolver, ProcessEvidence, ProcessRun, SolveContext, SolverCapabilities, SolverError,
    SolverOutcome, SolverReceipt, SolverResult, ToolDetection, ToolState, DETECTION_DEADLINE,
    MAX_PROCESS_ARTIFACT_BYTES, MAX_REPORT_ARTIFACT_BYTES,
};

#[derive(Default)]
pub struct SolverRegistry {
    solvers: Vec<Arc<dyn EngineeringSolver>>,
}

impl SolverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bundled() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(KiCadCliSolver::new()));
        registry.register(Arc::new(NgspiceSolver::new()));
        registry.register(Arc::new(GmshSolver::new()));
        registry.register(Arc::new(OpenFoamSolver::new()));
        registry
    }

    pub fn register(&mut self, solver: Arc<dyn EngineeringSolver>) {
        self.solvers.push(solver);
    }

    pub fn solvers(&self) -> &[Arc<dyn EngineeringSolver>] {
        &self.solvers
    }

    pub fn len(&self) -> usize {
        self.solvers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.solvers.is_empty()
    }

    /// Solvers that declare the requested analysis and find the case
    /// applicable. Availability still requires an explicit detection call.
    pub fn applicable<'a>(&'a self, case: &'a FrozenCase) -> Vec<&'a Arc<dyn EngineeringSolver>> {
        self.solvers
            .iter()
            .filter(|solver| {
                solver.capabilities().supports(case.analysis)
                    && matches!(solver.applicability(case), Applicability::Supported)
            })
            .collect()
    }
}
