use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engineering_eda::parse_board;
use engineering_solvers::{
    AnalysisRequest, CaseSpec, EngineeringSolver, FrozenCase, GmshSolver, KiCadCliSolver, ModelRef,
    NgspiceSolver, OpenFoamSolver, SolveContext, SolverOutcome,
};
use faktor_cas::Cas;
use faktor_core::hash::FileHash;
use faktor_terminal::ProcessSupervisor;

const BOARD: &str = r#"(kicad_pcb
  (version 20241229)
  (generator "faktor-test")
  (general (thickness 1.6))
  (layers
    (0 "F.Cu" signal)
    (2 "B.Cu" signal)
    (25 "Edge.Cuts" user))
  (net 0 "")
  (gr_rect (start 100 100) (end 120 110) (stroke (width 0.1) (type default)) (fill none) (layer "Edge.Cuts"))
)"#;

const GEO: &str = r#"SetFactory("OpenCASCADE");
Box(1) = {0, 0, 0, 10, 5, 2};
Mesh.CharacteristicLengthMax = 4;
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    cas: Arc<Cas>,
    supervisor: Arc<ProcessSupervisor>,
    work: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(Cas::open(dir.path().join("cas")).unwrap());
        let supervisor = ProcessSupervisor::new(cas.clone());
        Self {
            work: dir.path().to_path_buf(),
            _dir: dir,
            cas,
            supervisor,
        }
    }

    fn context(&self) -> SolveContext {
        SolveContext::new(
            self.supervisor.clone(),
            self.cas.clone(),
            Duration::from_secs(60),
            9001,
        )
    }

    fn write(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = self.work.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn case(
        &self,
        id: &str,
        analysis: AnalysisRequest,
        model: ModelRef,
        input_name: &str,
        input_path: &Path,
    ) -> FrozenCase {
        FrozenCase::freeze(
            CaseSpec::new(
                id,
                engineering_core::RevisionId::new(1),
                9001,
                analysis,
                &self.work,
                model,
            )
            .with_input(input_name, input_path),
        )
        .unwrap()
    }
}

fn model(id: &str) -> ModelRef {
    ModelRef::new(id, "1.0.0").unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kicad_drc_runs_for_real_or_is_typed_unavailable() {
    let fixture = Fixture::new();
    let board = fixture.write("board.kicad_pcb", BOARD.as_bytes());
    parse_board(BOARD.as_bytes()).unwrap();
    let case = fixture.case(
        "kicad",
        AnalysisRequest::Drc,
        model("kicad-drc"),
        "board",
        &board,
    );
    let solver = KiCadCliSolver::new();
    let outcome = solver.solve(&case, &fixture.context()).await;
    match outcome {
        SolverOutcome::Executed { receipt } => {
            eprintln!(
                "live: kicad-cli DRC executed ({}, {} artifact(s), solver {})",
                receipt.identity.solver_version,
                receipt.artifacts.len(),
                receipt.identity.solver_id
            );
            assert!(receipt.executed);
            assert!(receipt.convergence.converged);
            assert!(receipt.identity.solver_id.contains("kicad"));
            assert!(receipt
                .results
                .iter()
                .any(|result| result.name == "drc_errors"));
            assert!(!receipt.artifacts.is_empty());
            let artifact = &receipt.artifacts[0];
            let hash = FileHash::from_hex(&artifact.digest.to_hex()).unwrap();
            let bytes = fixture.cas.get_bounded(hash, 64 * 1024 * 1024).unwrap();
            assert!(bytes.is_some());
            assert_eq!(bytes.unwrap().len() as u64, artifact.byte_len);
        }
        SolverOutcome::Unavailable { missing, .. } => {
            assert!(missing.iter().any(|item| item.contains("kicad-cli")));
        }
        other => panic!("unexpected KiCad outcome: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kicad_stats_export_runs_for_real_or_is_typed_unavailable() {
    let fixture = Fixture::new();
    let board = fixture.write("stats.kicad_pcb", BOARD.as_bytes());
    let case = fixture.case(
        "kicad-stats",
        AnalysisRequest::BoardExport,
        model("kicad-stats"),
        "board",
        &board,
    );
    let solver = KiCadCliSolver::new();
    match solver.solve(&case, &fixture.context()).await {
        SolverOutcome::Executed { receipt } => {
            assert!(receipt
                .results
                .iter()
                .any(|result| result.name == "board_statistics"));
            assert!(!receipt.artifacts.is_empty());
        }
        SolverOutcome::Unavailable {
            reason, missing, ..
        } => {
            // Breaks if an absent tool is reported as an empty/unavailable success.
            assert!(!reason.trim().is_empty());
            assert!(missing.iter().any(|item| item.contains("kicad-cli")));
        }
        other => panic!("unexpected KiCad stats outcome: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gmsh_meshing_runs_for_real_or_is_typed_unavailable() {
    let fixture = Fixture::new();
    let geometry = fixture.write("box.geo", GEO.as_bytes());
    let mut spec = CaseSpec::new(
        "gmsh",
        engineering_core::RevisionId::new(1),
        9001,
        AnalysisRequest::Mesh3d,
        &fixture.work,
        model("gmsh-mesh"),
    )
    .with_input("geometry", &geometry)
    .with_setting("dimension", serde_json::json!(3))
    .with_setting("characteristic_length", serde_json::json!(4.0));
    spec.settings
        .insert("order".to_string(), serde_json::json!(1));
    let case = FrozenCase::freeze(spec).unwrap();
    let solver = GmshSolver::new();
    match solver.solve(&case, &fixture.context()).await {
        SolverOutcome::Executed { receipt } => {
            let nodes = receipt
                .results
                .iter()
                .find(|result| result.name == "mesh_nodes")
                .and_then(|result| match &result.value {
                    engineering_core::ReceiptValue::Quantity(quantity) => Some(quantity.value()),
                    _ => None,
                })
                .unwrap_or(0.0);
            eprintln!(
                "live: gmsh executed ({}, {nodes} nodes, solver {})",
                receipt.identity.solver_version, receipt.identity.solver_id
            );
            assert!(receipt.executed);
            assert!(receipt.convergence.converged);
            assert!(receipt.results.iter().any(|result| {
                result.name == "mesh_nodes"
                    && matches!(
                        result.value,
                        engineering_core::ReceiptValue::Quantity(ref quantity)
                            if quantity.value() > 0.0
                    )
            }));
            let artifact = receipt
                .artifacts
                .iter()
                .find(|artifact| artifact.name == "gmsh-mesh.msh")
                .expect("mesh artifact");
            let hash = FileHash::from_hex(&artifact.digest.to_hex()).unwrap();
            assert!(fixture
                .cas
                .get_bounded(hash, 64 * 1024 * 1024)
                .unwrap()
                .is_some());
        }
        SolverOutcome::Unavailable { missing, .. } => {
            assert!(missing.iter().any(|item| item.contains("gmsh")));
        }
        other => panic!("unexpected gmsh outcome: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ngspice_is_supervised_and_typed_when_absent() {
    let fixture = Fixture::new();
    let netlist = fixture.write(
        "rc.cir",
        b"* rc\nV1 in 0 5\nR1 in out 1k\nC1 out 0 1u\n.tran 1u 10u\n.end\n",
    );
    let case = fixture.case(
        "ngspice",
        AnalysisRequest::CircuitSimulation,
        model("ngspice-circuit"),
        "netlist",
        &netlist,
    );
    let solver = NgspiceSolver::new();
    match solver.solve(&case, &fixture.context()).await {
        SolverOutcome::Executed { receipt } => {
            assert!(receipt.executed);
            assert!(receipt.identity.solver_id.contains("ngspice"));
        }
        SolverOutcome::Unavailable {
            reason, missing, ..
        } => {
            eprintln!("live: ngspice unavailable: {reason}");
            assert!(missing.iter().any(|item| item.contains("ngspice")));
        }
        other => panic!("unexpected ngspice outcome: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openfoam_detection_is_typed_when_absent() {
    let fixture = Fixture::new();
    let solver = OpenFoamSolver::new();
    let detection = solver.detect(&fixture.context()).await;
    match &detection.state {
        engineering_solvers::ToolState::Unavailable { reason, missing } => {
            // Breaks if an absent external tool is silently treated as available.
            assert!(!reason.trim().is_empty(), "typed refusal needs a reason");
            assert!(matches!(
                missing.as_slice(),
                [program]
                    if program == "foamRun"
                        || program == "simpleFoam"
                        || program == "pimpleFoam"
            ));
        }
        engineering_solvers::ToolState::Available { version } => {
            assert!(!version.trim().is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mutated_frozen_input_never_reaches_a_solver_process() {
    let fixture = Fixture::new();
    let board = fixture.write("mutating.kicad_pcb", BOARD.as_bytes());
    let case = fixture.case(
        "mutated",
        AnalysisRequest::Drc,
        model("kicad-drc"),
        "board",
        &board,
    );
    std::fs::write(&board, b"(kicad_pcb (version 1))").unwrap();
    let outcome = KiCadCliSolver::new().solve(&case, &fixture.context()).await;
    match outcome {
        SolverOutcome::Failed {
            reason, receipt, ..
        } => {
            assert!(reason.contains("mutated") || reason.contains("frozen"));
            assert!(receipt.is_none());
        }
        other => panic!("mutated input must be refused before execution: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_analysis_is_typed() {
    let fixture = Fixture::new();
    let board = fixture.write("unsupported.kicad_pcb", BOARD.as_bytes());
    let case = fixture.case(
        "unsupported",
        AnalysisRequest::CircuitSimulation,
        model("kicad-drc"),
        "board",
        &board,
    );
    match KiCadCliSolver::new().solve(&case, &fixture.context()).await {
        SolverOutcome::Unsupported { reason } => assert!(reason.contains("CircuitSimulation")),
        other => panic!("unexpected outcome: {other:?}"),
    }
}
