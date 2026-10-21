use std::path::PathBuf;

use async_trait::async_trait;
use engineering_core::{AnalysisIdentity, Convergence, Repeatability};
use engineering_units::{Quantity, Uncertainty};

use crate::case::{AnalysisRequest, FrozenCase};
use crate::identity::SolverIdentity;
use crate::solver::{
    artifact_from_bytes, check_frozen, detect_program, failure_receipt, identity_kind,
    quantity_result, read_output_file, receipt_from_process, run_tool, Applicability,
    EngineeringSolver, ProcessRun, SolveContext, SolverCapabilities, SolverError, SolverOutcome,
    ToolDetection, ToolState,
};

pub const GMSH_VERSION: &str = "1.0.0";
const GMSH_PROGRAM: &str = "gmsh";
const MAX_MESH_BYTES: usize = 64 * 1024 * 1024;
const MAX_MESH_ELEMENTS: usize = 50_000_000;

#[derive(Debug, Clone)]
pub struct GmshSolver {
    program: String,
}

impl Default for GmshSolver {
    fn default() -> Self {
        Self::new()
    }
}

impl GmshSolver {
    pub fn new() -> Self {
        Self {
            program: GMSH_PROGRAM.to_string(),
        }
    }

    fn identity(&self) -> SolverIdentity {
        SolverIdentity::new(
            "gmsh",
            GMSH_VERSION,
            "Gmsh 3D meshing through the process supervisor",
        )
    }

    fn analysis_identity(&self, tool_version: &str) -> AnalysisIdentity {
        AnalysisIdentity {
            solver_id: "gmsh/mesh".to_string(),
            solver_version: format!("{GMSH_VERSION}+tool/{tool_version}"),
            kind: identity_kind(),
        }
    }

    fn geometry_input(&self, case: &FrozenCase) -> Result<PathBuf, Applicability> {
        let Some(input) = case.input("geometry") else {
            return Err(Applicability::Unavailable {
                reason: "no frozen geometry input named 'geometry'".to_string(),
                missing: vec!["gmsh .geo file".to_string()],
            });
        };
        if input.path.extension().and_then(|ext| ext.to_str()) != Some("geo") {
            return Err(Applicability::Unavailable {
                reason: "geometry input is not a .geo file".to_string(),
                missing: vec![".geo geometry".to_string()],
            });
        }
        Ok(input.path.clone())
    }

    fn settings(&self, case: &FrozenCase) -> Result<MeshSettings, SolverError> {
        let dimension = case.settings_u64("dimension").unwrap_or(3);
        if !(1..=3).contains(&dimension) {
            return Err(SolverError::UnsupportedSetting {
                name: "dimension".to_string(),
                value: dimension.to_string(),
            });
        }
        let mesh_format = case.settings_string("mesh_format").unwrap_or("msh2");
        if mesh_format != "msh2" {
            return Err(SolverError::UnsupportedSetting {
                name: "mesh_format".to_string(),
                value: mesh_format.to_string(),
            });
        }
        let characteristic_length = case.settings_f64("characteristic_length");
        if let Some(value) = characteristic_length {
            if !value.is_finite() || value <= 0.0 || value > 1e6 {
                return Err(SolverError::UnsupportedSetting {
                    name: "characteristic_length".to_string(),
                    value: value.to_string(),
                });
            }
        }
        let order = case.settings_u64("order").unwrap_or(1);
        if !(1..=2).contains(&order) {
            return Err(SolverError::UnsupportedSetting {
                name: "order".to_string(),
                value: order.to_string(),
            });
        }
        let algorithm = case.settings_string("algorithm").map(str::to_string);
        if let Some(algorithm) = &algorithm {
            const ALLOWED: [&str; 12] = [
                "auto",
                "meshadapt",
                "del2d",
                "front2d",
                "delquad",
                "quadqs",
                "initial2d",
                "del3d",
                "front3d",
                "mmg3d",
                "hxt",
                "initial3d",
            ];
            if !ALLOWED.contains(&algorithm.as_str()) {
                return Err(SolverError::UnsupportedSetting {
                    name: "algorithm".to_string(),
                    value: algorithm.clone(),
                });
            }
        }
        Ok(MeshSettings {
            dimension,
            characteristic_length,
            order,
            algorithm,
        })
    }

    async fn solve_mesh(
        &self,
        case: &FrozenCase,
        context: &SolveContext,
        geometry: &std::path::Path,
        version: &str,
    ) -> SolverOutcome {
        let settings = match self.settings(case) {
            Ok(settings) => settings,
            Err(error) => {
                return SolverOutcome::Failed {
                    solver_id: self.identity().id,
                    reason: error.to_string(),
                    receipt: None,
                }
            }
        };
        let mesh_path = case
            .workdir
            .join(format!("gmsh-mesh-{}.msh", case.operation_id));
        let _ = std::fs::remove_file(&mesh_path);
        let mut args = vec![
            geometry.display().to_string(),
            format!("-{}", settings.dimension),
            "-format".to_string(),
            "msh2".to_string(),
            "-order".to_string(),
            settings.order.to_string(),
            "-o".to_string(),
            mesh_path.display().to_string(),
        ];
        if let Some(algorithm) = &settings.algorithm {
            args.push("-algo".to_string());
            args.push(algorithm.clone());
        }
        if let Some(length) = settings.characteristic_length {
            args.push("-clmax".to_string());
            args.push(format!("{length}"));
        }
        let run = match run_tool(context, case, &self.program, &args).await {
            Ok(run) => run,
            Err(error) => {
                return SolverOutcome::Failed {
                    solver_id: self.identity().id,
                    reason: error.to_string(),
                    receipt: None,
                }
            }
        };
        if run.timed_out || run.output.exit_code != Some(0) {
            return self.failed_with_receipt(
                context,
                case,
                version,
                &run,
                &format!(
                    "gmsh did not complete cleanly: exit={:?}, timed_out={}",
                    run.output.exit_code, run.timed_out
                ),
            );
        }
        let bytes = match read_output_file(&mesh_path, MAX_MESH_BYTES) {
            Ok(bytes) => bytes,
            Err(error) => {
                return self.failed_with_receipt(
                    context,
                    case,
                    version,
                    &run,
                    &format!("mesh output is not readable: {error}"),
                )
            }
        };
        let stats = match mesh_stats(&bytes) {
            Ok(stats) => stats,
            Err(reason) => {
                return self.failed_with_receipt(
                    context,
                    case,
                    version,
                    &run,
                    &format!("mesh output is not a bounded MSH2 file: {reason}"),
                )
            }
        };
        let artifact = match artifact_from_bytes(
            context,
            "gmsh-mesh.msh",
            "application/x-gmsh-msh2",
            &bytes,
        ) {
            Ok(artifact) => artifact,
            Err(error) => {
                return SolverOutcome::Failed {
                    solver_id: self.identity().id,
                    reason: error.to_string(),
                    receipt: None,
                }
            }
        };
        let results = vec![
            quantity_result(
                "mesh_nodes",
                Quantity::dimensionless(stats.nodes as f64).expect("bounded node count"),
            ),
            quantity_result(
                "mesh_elements",
                Quantity::dimensionless(stats.elements as f64).expect("bounded element count"),
            ),
            quantity_result(
                "mesh_bytes",
                Quantity::dimensionless(bytes.len() as f64).expect("bounded mesh size"),
            ),
        ];
        match receipt_from_process(
            context,
            case,
            self.analysis_identity(version),
            &run,
            Convergence {
                converged: true,
                iterations: 1,
                residual: 0.0,
                tolerance: 0.0,
                note: Some(format!(
                    "Gmsh produced {} nodes and {} elements; no mesh-quality convergence study is implied",
                    stats.nodes, stats.elements
                )),
            },
            Vec::new(),
            results,
            vec![artifact],
            Uncertainty::Exact,
            Repeatability::unverified("gmsh"),
        ) {
            Ok(receipt) => SolverOutcome::Executed { receipt },
            Err(error) => SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: error.to_string(),
                receipt: None,
            },
        }
    }

    fn failed_with_receipt(
        &self,
        context: &SolveContext,
        case: &FrozenCase,
        version: &str,
        run: &ProcessRun,
        reason: &str,
    ) -> SolverOutcome {
        match failure_receipt(context, case, self.analysis_identity(version), run, reason) {
            Ok(receipt) => SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: reason.to_string(),
                receipt: Some(receipt),
            },
            Err(error) => SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: format!("{reason}; no receipt: {error}"),
                receipt: None,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct MeshSettings {
    dimension: u64,
    characteristic_length: Option<f64>,
    order: u64,
    algorithm: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshStats {
    pub nodes: usize,
    pub elements: usize,
}

pub fn mesh_stats(bytes: &[u8]) -> Result<MeshStats, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "mesh is not UTF-8 MSH2".to_string())?;
    let mut lines = text.lines();
    let mut saw_mesh_format = false;
    let mut nodes = None;
    let mut elements = None;
    while let Some(line) = lines.next() {
        match line.trim() {
            "$MeshFormat" => {
                saw_mesh_format = true;
                let version = lines.next().unwrap_or("").trim();
                if !version.starts_with("2.2") {
                    return Err(format!("unsupported mesh version {version:?}"));
                }
            }
            "$Nodes" => {
                let count = lines
                    .next()
                    .ok_or_else(|| "missing node count".to_string())?
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "invalid node count".to_string())?;
                nodes = Some(count);
            }
            "$Elements" => {
                let count = lines
                    .next()
                    .ok_or_else(|| "missing element count".to_string())?
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "invalid element count".to_string())?;
                elements = Some(count);
            }
            _ => {}
        }
    }
    if !saw_mesh_format {
        return Err("missing $MeshFormat".to_string());
    }
    let nodes = nodes.ok_or_else(|| "missing $Nodes".to_string())?;
    let elements = elements.ok_or_else(|| "missing $Elements".to_string())?;
    if nodes == 0 || elements == 0 {
        return Err("mesh is empty".to_string());
    }
    if nodes > MAX_MESH_ELEMENTS || elements > MAX_MESH_ELEMENTS {
        return Err("mesh count exceeds bound".to_string());
    }
    Ok(MeshStats { nodes, elements })
}

#[async_trait]
impl EngineeringSolver for GmshSolver {
    fn identity(&self) -> SolverIdentity {
        GmshSolver::identity(self)
    }

    fn capabilities(&self) -> SolverCapabilities {
        SolverCapabilities {
            analyses: vec![AnalysisRequest::Mesh3d],
            input_formats: vec!["geo".to_string()],
            external_process: true,
        }
    }

    fn applicability(&self, case: &FrozenCase) -> Applicability {
        if !self.capabilities().supports(case.analysis) {
            return Applicability::Unsupported {
                reason: format!("gmsh does not support analysis {:?}", case.analysis),
            };
        }
        match self.geometry_input(case) {
            Ok(_) => Applicability::Supported,
            Err(applicability) => applicability,
        }
    }

    async fn detect(&self, context: &SolveContext) -> ToolDetection {
        detect_program(context, &self.identity().id, &self.program, &["--version"]).await
    }

    async fn solve(&self, case: &FrozenCase, context: &SolveContext) -> SolverOutcome {
        if let Err(error) = check_frozen(case) {
            return SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: format!("frozen case verification failed: {error}"),
                receipt: None,
            };
        }
        match self.applicability(case) {
            Applicability::Supported => {}
            Applicability::Unsupported { reason } => return SolverOutcome::Unsupported { reason },
            Applicability::Unavailable { reason, missing } => {
                return SolverOutcome::Unavailable {
                    solver_id: self.identity().id,
                    reason,
                    missing,
                }
            }
        }
        let geometry = match self.geometry_input(case) {
            Ok(geometry) => geometry,
            Err(_) => {
                return SolverOutcome::Unavailable {
                    solver_id: self.identity().id,
                    reason: "geometry input is unavailable".to_string(),
                    missing: vec![".geo geometry".to_string()],
                }
            }
        };
        let detection = self.detect(context).await;
        let version = match detection.state {
            ToolState::Available { version } => version,
            ToolState::Unavailable { reason, missing } => {
                return SolverOutcome::Unavailable {
                    solver_id: self.identity().id,
                    reason,
                    missing,
                }
            }
        };
        self.solve_mesh(case, context, &geometry, &version).await
    }
}
