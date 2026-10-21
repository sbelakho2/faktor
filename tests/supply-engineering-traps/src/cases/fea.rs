use engineering_solvers::{AnalysisRequest, EngineeringSolver, GmshSolver};

use super::{case_error, falsifiable};
use crate::fixtures::{
    convergence_pathology, manufactured_solution_study, mesh_fixture_probe, observed_order,
    REFINEMENT_LEVELS,
};
use crate::{CaseBuilder, CaseOutcome, CaseReport, ExpectedKind, Metrics};

pub(crate) const FEA_ID: &str = "fea/mesh-convergence-pathology";

pub(crate) fn mesh_convergence() -> Result<CaseReport, crate::BenchError> {
    let mesh = mesh_fixture_probe();
    let study = manufactured_solution_study()
        .map_err(|error| case_error(FEA_ID, format!("manufactured solution: {error}")))?;
    let mut orders = Vec::new();
    for pair in study.windows(2) {
        if let Some(order) = observed_order(pair[0].1, pair[1].1) {
            orders.push(order);
        }
    }
    let min_order = orders.iter().copied().fold(f64::INFINITY, f64::min);
    let final_order = orders.last().copied().unwrap_or(f64::NAN);
    let errors: Vec<f64> = study.iter().map(|(_, error)| *error).collect();
    let valid_study = min_order >= 1.8 && convergence_pathology(&errors).is_none();
    let pathological_non_monotone = convergence_pathology(&[0.40, 0.35, 0.36, 0.22]);
    let pathological_stall = convergence_pathology(&[0.40, 0.20, 0.10, 0.095]);
    let adversarial = pathological_non_monotone.is_some() && pathological_stall.is_some();
    let gmsh = GmshSolver::new();
    let capabilities = gmsh.capabilities();
    let mesh_only = capabilities.analyses == vec![AnalysisRequest::Mesh3d]
        && capabilities.input_formats == vec!["geo".to_string()];
    let physics = mesh.ok() && valid_study && adversarial && mesh_only;
    let test = falsifiable(
        "The reported stress is mesh-independent and the solver has converged.",
        "Refine the mesh over at least three levels and track the quantity of interest; compare the finest result with an independent reference or analytical limit and record the observed order and residual.",
        "The quantity changes by less than the stated tolerance over the last levels, the refinement is monotone, and the observed order is consistent with the discretization.",
        "The quantity does not converge, oscillates, stalls, or changes by more than the tolerance under refinement.",
        "Mesh generator, structural solver with convergence residuals, independent analytical/reference model.",
        &[
            "the bundled Gmsh adapter is mesh-generation only (AnalysisRequest::Mesh3d)",
            "a solver exit code and element count are not a convergence study",
            "a manufactured-solution Poisson study shows second-order error reduction in the control",
        ],
    );
    let mut evidence = vec![
        format!(
            "mesh fixture: {} nodes / {} elements from a bounded MSH2 file",
            mesh.ok_nodes, mesh.ok_elements
        ),
        format!(
            "truncated mesh refusal: {:?}; empty mesh refusal: {:?}",
            mesh.truncated_error, mesh.empty_error
        ),
        format!(
            "manufactured-solution refinement levels {:?}; observed orders {:?}",
            REFINEMENT_LEVELS.to_vec(),
            orders
        ),
        format!(
            "pathological controls: non-monotone={:?}; stalled={:?}",
            pathological_non_monotone, pathological_stall
        ),
        format!(
            "bundled solver capability: analyses={:?}, formats={:?}",
            capabilities.analyses, capabilities.input_formats
        ),
    ];
    evidence.push(format!("mesh-only adapter: {mesh_only}"));
    CaseBuilder::new(FEA_ID, "fea", "A mesh is not a converged finite-element result", ExpectedKind::Refused)
        .shallow("The solver exited successfully and the mesh has many elements, so the stress is converged.")
        .outcome(CaseOutcome::Refused {
            code: "MeshConvergenceNotEstablished".to_string(),
            reason: if mesh_only {
                "the bundled adapter only generates meshes; no structural solve/convergence receipt exists, and the pathological refinement controls are refused".to_string()
            } else {
                "the mesh/convergence evidence is not sufficient for a stress result".to_string()
            },
            required: vec![
                "structural solver with a convergence residual and tolerance".to_string(),
                "three-level mesh-independence study of the quantity of interest".to_string(),
                "element-quality metrics and boundary conditions".to_string(),
                "independent reference or analytical limit for the quantity of interest".to_string(),
            ],
            receipt_bound: false,
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(mesh_only)
                .with_missing_info(true)
                .with_boundaries(true)
                .with_uncertainty(true)
                .with_trap(true)
                .with_experiment(true)
                .with_refusal(true),
        )
        .evidence(evidence)
        .missing(vec![
            "mesh-independence study and convergence tolerance".to_string(),
            "element quality, aspect ratio and boundary conditions".to_string(),
            "independent reference solution or experimental validation".to_string(),
        ])
        .boundaries(vec![
            "refinement must be monotone and reduce the error".to_string(),
            "the observed order must be consistent with the discretization".to_string(),
            "mesh counts and files are bounded before parsing".to_string(),
        ])
        .uncertainty(format!(
            "The manufactured-solution control observed order is {:.3} at the finest pair; the stress result itself has no quantified discretization uncertainty until a convergence study exists.",
            final_order
        ))
        .test(test)
        .build()
}
