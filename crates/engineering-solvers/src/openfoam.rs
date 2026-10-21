use async_trait::async_trait;
use engineering_core::{
    AnalysisIdentity, AnalysisWarning, Convergence, Repeatability, WarningSeverity,
};
use engineering_units::{Quantity, Uncertainty};

use crate::case::{AnalysisRequest, FrozenCase};
use crate::identity::SolverIdentity;
use crate::solver::{
    check_frozen, detect_program, identity_kind, quantity_result, receipt_from_process, run_tool,
    text_result, Applicability, EngineeringSolver, SolveContext, SolverCapabilities, SolverOutcome,
    ToolDetection, ToolState,
};

pub const OPENFOAM_VERSION: &str = "1.0.0";
const PROGRAMS: [&str; 3] = ["foamRun", "simpleFoam", "pimpleFoam"];

#[derive(Debug, Clone)]
pub struct OpenFoamSolver {
    programs: Vec<String>,
}

impl Default for OpenFoamSolver {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenFoamSolver {
    pub fn new() -> Self {
        Self {
            programs: PROGRAMS
                .iter()
                .map(|program| (*program).to_string())
                .collect(),
        }
    }

    fn identity(&self) -> SolverIdentity {
        SolverIdentity::new(
            "openfoam",
            OPENFOAM_VERSION,
            "OpenFOAM steady CFD through the process supervisor",
        )
    }

    fn analysis_identity(&self, tool_version: &str) -> AnalysisIdentity {
        AnalysisIdentity {
            solver_id: "openfoam/steady-cfd".to_string(),
            solver_version: format!("{OPENFOAM_VERSION}+tool/{tool_version}"),
            kind: identity_kind(),
        }
    }

    fn case_ready(&self, case: &FrozenCase) -> Result<(), Applicability> {
        let Some(directory) = &case.case_directory else {
            return Err(Applicability::Unavailable {
                reason: "no frozen OpenFOAM case directory".to_string(),
                missing: vec![
                    "case/system/controlDict".to_string(),
                    "case/constant".to_string(),
                    "case/0".to_string(),
                ],
            });
        };
        let required = ["system/controlDict", "constant", "0"];
        let mut missing = Vec::new();
        for prefix in required {
            let present = directory
                .files
                .keys()
                .any(|path| path == prefix || path.starts_with(&format!("{prefix}/")));
            if !present {
                missing.push(format!("case/{prefix}"));
            }
        }
        if !missing.is_empty() {
            return Err(Applicability::Unavailable {
                reason: "the frozen directory is not an OpenFOAM case".to_string(),
                missing,
            });
        }
        Ok(())
    }

    async fn detect_any(&self, context: &SolveContext) -> ToolDetection {
        let mut last = None;
        for program in &self.programs {
            let detection = detect_program(context, &self.identity().id, program, &["-help"]).await;
            if detection.available() {
                return detection;
            }
            last = Some(detection);
        }
        last.unwrap_or(ToolDetection {
            solver_id: self.identity().id,
            program: self
                .programs
                .first()
                .cloned()
                .unwrap_or_else(|| "foamRun".to_string()),
            state: ToolState::Unavailable {
                reason: "no OpenFOAM solver executable was found".to_string(),
                missing: self.programs.clone(),
            },
        })
    }

    async fn run_case(
        &self,
        case: &FrozenCase,
        context: &SolveContext,
        program: &str,
        version: &str,
    ) -> SolverOutcome {
        let Some(directory) = &case.case_directory else {
            return SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: "frozen case directory disappeared".to_string(),
                receipt: None,
            };
        };
        let args = vec!["-case".to_string(), directory.root.display().to_string()];
        let run = match run_tool(context, case, program, &args).await {
            Ok(run) => run,
            Err(error) => {
                return SolverOutcome::Failed {
                    solver_id: self.identity().id,
                    reason: error.to_string(),
                    receipt: None,
                }
            }
        };
        let lower = run.output.excerpt.to_ascii_lowercase();
        let fatal = lower
            .lines()
            .filter(|line| line.contains("foam fatal"))
            .count();
        let warnings = lower
            .lines()
            .filter(|line| line.contains("foam warning"))
            .count();
        let mut analysis_warnings = Vec::new();
        if fatal > 0 {
            analysis_warnings.push(AnalysisWarning {
                code: "openfoam.fatal".to_string(),
                message: format!("{fatal} FOAM FATAL marker(s) in the solver output"),
                severity: WarningSeverity::Critical,
            });
        }
        if warnings > 0 {
            analysis_warnings.push(AnalysisWarning {
                code: "openfoam.warning".to_string(),
                message: format!("{warnings} FOAM Warning marker(s) in the solver output"),
                severity: WarningSeverity::Warning,
            });
        }
        let clean_exit = !run.timed_out && run.output.exit_code == Some(0) && fatal == 0;
        let convergence = Convergence {
            converged: false,
            iterations: 1,
            residual: 0.0,
            tolerance: 0.0,
            note: Some(if clean_exit {
                "solver exited cleanly, but residual/continuity convergence was not interpreted; inspect the log artifact"
                    .to_string()
            } else {
                "solver failed or timed out; no convergence claim".to_string()
            }),
        };
        let summary: String = run.output.excerpt.chars().take(4_000).collect();
        let results = vec![
            quantity_result(
                "foam_fatal_markers",
                Quantity::dimensionless(fatal as f64).expect("bounded count"),
            ),
            quantity_result(
                "foam_warning_markers",
                Quantity::dimensionless(warnings as f64).expect("bounded count"),
            ),
            text_result("solver_output", summary),
        ];
        match receipt_from_process(
            context,
            case,
            self.analysis_identity(version),
            &run,
            convergence,
            analysis_warnings,
            results,
            Vec::new(),
            Uncertainty::Exact,
            Repeatability::unverified(program),
        ) {
            Ok(receipt) => {
                if clean_exit {
                    SolverOutcome::Executed { receipt }
                } else {
                    SolverOutcome::Failed {
                        solver_id: self.identity().id,
                        reason: format!(
                            "OpenFOAM did not complete cleanly: exit={:?}, timed_out={}, fatal={fatal}",
                            run.output.exit_code, run.timed_out
                        ),
                        receipt: Some(receipt),
                    }
                }
            }
            Err(error) => SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: error.to_string(),
                receipt: None,
            },
        }
    }
}

#[async_trait]
impl EngineeringSolver for OpenFoamSolver {
    fn identity(&self) -> SolverIdentity {
        OpenFoamSolver::identity(self)
    }

    fn capabilities(&self) -> SolverCapabilities {
        SolverCapabilities {
            analyses: vec![AnalysisRequest::SteadyCfd],
            input_formats: vec!["openfoam_case".to_string()],
            external_process: true,
        }
    }

    fn applicability(&self, case: &FrozenCase) -> Applicability {
        if !self.capabilities().supports(case.analysis) {
            return Applicability::Unsupported {
                reason: format!("OpenFOAM does not support analysis {:?}", case.analysis),
            };
        }
        match self.case_ready(case) {
            Ok(()) => Applicability::Supported,
            Err(applicability) => applicability,
        }
    }

    async fn detect(&self, context: &SolveContext) -> ToolDetection {
        self.detect_any(context).await
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
        let detection = self.detect_any(context).await;
        let ToolDetection { program, state, .. } = detection;
        let version = match state {
            ToolState::Available { version } => version,
            ToolState::Unavailable { reason, missing } => {
                return SolverOutcome::Unavailable {
                    solver_id: self.identity().id,
                    reason,
                    missing,
                }
            }
        };
        self.run_case(case, context, &program, &version).await
    }
}
