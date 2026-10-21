use std::path::PathBuf;

use async_trait::async_trait;
use engineering_core::{
    AnalysisIdentity, AnalysisWarning, Convergence, Repeatability, WarningSeverity,
};
use engineering_units::{Quantity, Uncertainty};

use crate::case::{AnalysisRequest, FrozenCase};
use crate::identity::SolverIdentity;
use crate::solver::{
    case_input_bytes, check_frozen, detect_program, failure_receipt, identity_kind,
    quantity_result, receipt_from_process, run_tool, text_result, Applicability, EngineeringSolver,
    ProcessRun, SolveContext, SolverCapabilities, SolverOutcome, ToolDetection, ToolState,
};

pub const NGSPICE_VERSION: &str = "1.0.0";
const NGSPICE_PROGRAM: &str = "ngspice";
const EXTENSIONS: [&str; 4] = ["cir", "sp", "netlist", "spice"];

#[derive(Debug, Clone)]
pub struct NgspiceSolver {
    program: String,
}

impl Default for NgspiceSolver {
    fn default() -> Self {
        Self::new()
    }
}

impl NgspiceSolver {
    pub fn new() -> Self {
        Self {
            program: NGSPICE_PROGRAM.to_string(),
        }
    }

    fn identity(&self) -> SolverIdentity {
        SolverIdentity::new(
            "ngspice",
            NGSPICE_VERSION,
            "ngspice batch circuit simulation through the process supervisor",
        )
    }

    fn analysis_identity(&self, tool_version: &str) -> AnalysisIdentity {
        AnalysisIdentity {
            solver_id: "ngspice/circuit".to_string(),
            solver_version: format!("{NGSPICE_VERSION}+tool/{tool_version}"),
            kind: identity_kind(),
        }
    }

    fn netlist_input(&self, case: &FrozenCase) -> Result<PathBuf, Applicability> {
        let Some(input) = case.input("netlist") else {
            return Err(Applicability::Unavailable {
                reason: "no frozen netlist input named 'netlist'".to_string(),
                missing: vec!["ngspice netlist".to_string()],
            });
        };
        let extension = input
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !EXTENSIONS.contains(&extension.as_str()) {
            return Err(Applicability::Unavailable {
                reason: format!("netlist extension .{extension} is not a spice netlist"),
                missing: vec!["a .cir/.sp/.netlist/.spice file".to_string()],
            });
        }
        Ok(input.path.clone())
    }
}

#[async_trait]
impl EngineeringSolver for NgspiceSolver {
    fn identity(&self) -> SolverIdentity {
        NgspiceSolver::identity(self)
    }

    fn capabilities(&self) -> SolverCapabilities {
        SolverCapabilities {
            analyses: vec![AnalysisRequest::CircuitSimulation],
            input_formats: EXTENSIONS.iter().map(|ext| (*ext).to_string()).collect(),
            external_process: true,
        }
    }

    fn applicability(&self, case: &FrozenCase) -> Applicability {
        if !self.capabilities().supports(case.analysis) {
            return Applicability::Unsupported {
                reason: format!("ngspice does not support analysis {:?}", case.analysis),
            };
        }
        match self.netlist_input(case) {
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
        let netlist = match self.netlist_input(case) {
            Ok(netlist) => netlist,
            Err(_) => {
                return SolverOutcome::Unavailable {
                    solver_id: self.identity().id,
                    reason: "netlist input is unavailable".to_string(),
                    missing: vec!["spice netlist".to_string()],
                }
            }
        };
        if case_input_bytes(case, "netlist").is_err() {
            return SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: "netlist is not readable".to_string(),
                receipt: None,
            };
        }
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
        let args = vec!["-b".to_string(), netlist.display().to_string()];
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
                &version,
                &run,
                &format!(
                    "ngspice did not complete cleanly: exit={:?}, timed_out={}",
                    run.output.exit_code, run.timed_out
                ),
            );
        }
        let excerpt_lower = run.output.excerpt.to_ascii_lowercase();
        let errors = count_markers(
            &excerpt_lower,
            &["fatal", "singular matrix", "timestep too small", "error:"],
        );
        let warnings = count_markers(&excerpt_lower, &["warning"]);
        let convergence = Convergence {
            converged: errors == 0,
            iterations: 1,
            residual: 0.0,
            tolerance: 0.0,
            note: Some(
                "ngspice batch exit status and error-signature scan; numerical `.measure` extraction is not implemented"
                    .to_string(),
            ),
        };
        let mut analysis_warnings = Vec::new();
        if errors > 0 {
            analysis_warnings.push(AnalysisWarning {
                code: "ngspice.error".to_string(),
                message: format!("{errors} error marker(s) in the ngspice output"),
                severity: WarningSeverity::Critical,
            });
        }
        if warnings > 0 {
            analysis_warnings.push(AnalysisWarning {
                code: "ngspice.warning".to_string(),
                message: format!("{warnings} warning marker(s) in the ngspice output"),
                severity: WarningSeverity::Warning,
            });
        }
        let summary: String = run.output.excerpt.chars().take(4_000).collect();
        let results = vec![
            quantity_result(
                "ngspice_error_markers",
                Quantity::dimensionless(errors as f64).expect("bounded count"),
            ),
            quantity_result(
                "ngspice_warning_markers",
                Quantity::dimensionless(warnings as f64).expect("bounded count"),
            ),
            text_result("simulation_output", summary),
        ];
        match receipt_from_process(
            context,
            case,
            self.analysis_identity(&version),
            &run,
            convergence,
            analysis_warnings,
            results,
            Vec::new(),
            Uncertainty::Exact,
            Repeatability::unverified("ngspice"),
        ) {
            Ok(receipt) => SolverOutcome::Executed { receipt },
            Err(error) => SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: error.to_string(),
                receipt: None,
            },
        }
    }
}

fn count_markers(text: &str, markers: &[&str]) -> usize {
    text.lines()
        .filter(|line| markers.iter().any(|marker| line.contains(marker)))
        .count()
}

impl NgspiceSolver {
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
