use std::path::PathBuf;

use async_trait::async_trait;
use engineering_core::{
    AnalysisIdentity, AnalysisWarning, Convergence, Repeatability, WarningSeverity,
};
use engineering_eda::{parse_board, parse_drc_json, DrcReport};
use engineering_units::{Quantity, Uncertainty};

use crate::case::{AnalysisRequest, FrozenCase};
use crate::identity::SolverIdentity;
use crate::solver::{
    artifact_from_bytes, case_input_bytes, check_frozen, detect_program, failure_receipt,
    identity_kind, quantity_result, read_output_file, receipt_from_process, run_tool, text_result,
    Applicability, EngineeringSolver, ProcessRun, SolveContext, SolverCapabilities, SolverOutcome,
    ToolDetection, ToolState, MAX_REPORT_ARTIFACT_BYTES,
};

pub const KICAD_VERSION: &str = "1.0.0";
const KICAD_PROGRAM: &str = "kicad-cli";
const MAX_STATS_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct KiCadCliSolver {
    program: String,
}

impl Default for KiCadCliSolver {
    fn default() -> Self {
        Self::new()
    }
}

impl KiCadCliSolver {
    pub fn new() -> Self {
        Self {
            program: KICAD_PROGRAM.to_string(),
        }
    }

    fn identity(&self) -> SolverIdentity {
        SolverIdentity::new(
            "kicad-cli",
            KICAD_VERSION,
            "KiCad CLI PCB DRC and statistics export through the process supervisor",
        )
    }

    fn analysis_identity(&self, tool_version: &str, suffix: &str) -> AnalysisIdentity {
        AnalysisIdentity {
            solver_id: format!("kicad-cli/{suffix}"),
            solver_version: format!("{KICAD_VERSION}+tool/{tool_version}"),
            kind: identity_kind(),
        }
    }

    fn board_input(&self, case: &FrozenCase) -> Result<PathBuf, Applicability> {
        let Some(input) = case.input("board") else {
            return Err(Applicability::Unavailable {
                reason: "no frozen board input named 'board'".to_string(),
                missing: vec!["board (.kicad_pcb)".to_string()],
            });
        };
        if input.path.extension().and_then(|ext| ext.to_str()) != Some("kicad_pcb") {
            return Err(Applicability::Unavailable {
                reason: "board input does not have the .kicad_pcb extension".to_string(),
                missing: vec!["KiCad PCB file".to_string()],
            });
        }
        Ok(input.path.clone())
    }

    async fn solve_drc(
        &self,
        case: &FrozenCase,
        context: &SolveContext,
        board: &std::path::Path,
        version: &str,
    ) -> SolverOutcome {
        let report_path = case
            .workdir
            .join(format!("kicad-drc-{}.json", case.operation_id));
        let _ = std::fs::remove_file(&report_path);
        let args = vec![
            "pcb".to_string(),
            "drc".to_string(),
            "--format".to_string(),
            "json".to_string(),
            "--severity-all".to_string(),
            "--output".to_string(),
            report_path.display().to_string(),
            board.display().to_string(),
        ];
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
                "drc",
                &run,
                &format!(
                    "kicad-cli DRC did not complete cleanly: exit={:?}, timed_out={}",
                    run.output.exit_code, run.timed_out
                ),
            );
        }
        let bytes = match read_output_file(&report_path, MAX_REPORT_ARTIFACT_BYTES) {
            Ok(bytes) => bytes,
            Err(error) => {
                return self.failed_with_receipt(
                    context,
                    case,
                    version,
                    "drc",
                    &run,
                    &format!("DRC report is not readable: {error}"),
                )
            }
        };
        let report: DrcReport = match parse_drc_json(&bytes) {
            Ok(report) => report,
            Err(error) => {
                return self.failed_with_receipt(
                    context,
                    case,
                    version,
                    "drc",
                    &run,
                    &format!("DRC report is malformed: {error}"),
                )
            }
        };
        let artifact =
            match artifact_from_bytes(context, "kicad-drc.json", "application/json", &bytes) {
                Ok(artifact) => artifact,
                Err(error) => {
                    return SolverOutcome::Failed {
                        solver_id: self.identity().id,
                        reason: error.to_string(),
                        receipt: None,
                    }
                }
            };
        let counts = report.counts();
        let results = vec![
            quantity_result(
                "drc_errors",
                Quantity::dimensionless(counts.errors as f64).expect("bounded count is finite"),
            ),
            quantity_result(
                "drc_warnings",
                Quantity::dimensionless(counts.warnings as f64).expect("bounded count is finite"),
            ),
            quantity_result(
                "drc_exclusions",
                Quantity::dimensionless(counts.exclusions as f64).expect("bounded count is finite"),
            ),
            quantity_result(
                "drc_unknown_severity",
                Quantity::dimensionless(counts.unknown as f64).expect("bounded count is finite"),
            ),
            quantity_result(
                "unconnected_items",
                Quantity::dimensionless(counts.unconnected as f64)
                    .expect("bounded count is finite"),
            ),
            text_result(
                "kicad_version",
                report
                    .kicad_version
                    .clone()
                    .unwrap_or_else(|| version.to_string()),
            ),
        ];
        let mut warnings = Vec::new();
        for violation in report.violations.iter().take(32) {
            warnings.push(AnalysisWarning {
                code: format!("drc.{}", violation.kind),
                message: format!("{:?}: {}", violation.severity, violation.description)
                    .chars()
                    .take(2_000)
                    .collect(),
                severity: match violation.severity {
                    engineering_eda::DrcSeverity::Error => WarningSeverity::Critical,
                    engineering_eda::DrcSeverity::Warning => WarningSeverity::Warning,
                    engineering_eda::DrcSeverity::Exclusion => WarningSeverity::Info,
                    engineering_eda::DrcSeverity::Unknown => WarningSeverity::Warning,
                },
            });
        }
        let convergence = Convergence {
            converged: true,
            iterations: 1,
            residual: 0.0,
            tolerance: 0.0,
            note: Some("DRC verdicts produced by kicad-cli for the frozen board".to_string()),
        };
        match receipt_from_process(
            context,
            case,
            self.analysis_identity(version, "drc"),
            &run,
            convergence,
            warnings,
            results,
            vec![artifact],
            Uncertainty::Exact,
            Repeatability::unverified("kicad-cli"),
        ) {
            Ok(receipt) => SolverOutcome::Executed { receipt },
            Err(error) => SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: error.to_string(),
                receipt: None,
            },
        }
    }

    async fn solve_stats(
        &self,
        case: &FrozenCase,
        context: &SolveContext,
        board: &std::path::Path,
        version: &str,
    ) -> SolverOutcome {
        let stats_path = case
            .workdir
            .join(format!("kicad-stats-{}.txt", case.operation_id));
        let _ = std::fs::remove_file(&stats_path);
        let args = vec![
            "pcb".to_string(),
            "export".to_string(),
            "stats".to_string(),
            "-o".to_string(),
            stats_path.display().to_string(),
            board.display().to_string(),
        ];
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
                "stats",
                &run,
                &format!(
                    "kicad-cli stats export failed: exit={:?}, timed_out={}",
                    run.output.exit_code, run.timed_out
                ),
            );
        }
        let bytes = match read_output_file(&stats_path, MAX_STATS_BYTES) {
            Ok(bytes) if !bytes.is_empty() => bytes,
            Ok(_) => {
                return self.failed_with_receipt(
                    context,
                    case,
                    version,
                    "stats",
                    &run,
                    "kicad-cli produced an empty statistics file",
                )
            }
            Err(error) => {
                return self.failed_with_receipt(
                    context,
                    case,
                    version,
                    "stats",
                    &run,
                    &format!("statistics file is not readable: {error}"),
                )
            }
        };
        let artifact = match artifact_from_bytes(context, "kicad-stats.txt", "text/plain", &bytes) {
            Ok(artifact) => artifact,
            Err(error) => {
                return SolverOutcome::Failed {
                    solver_id: self.identity().id,
                    reason: error.to_string(),
                    receipt: None,
                }
            }
        };
        let summary: String = String::from_utf8_lossy(&bytes)
            .chars()
            .take(4_000)
            .collect();
        match receipt_from_process(
            context,
            case,
            self.analysis_identity(version, "stats"),
            &run,
            Convergence::exact(),
            Vec::new(),
            vec![text_result("board_statistics", summary)],
            vec![artifact],
            Uncertainty::Exact,
            Repeatability::unverified("kicad-cli"),
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
        suffix: &str,
        run: &ProcessRun,
        reason: &str,
    ) -> SolverOutcome {
        match failure_receipt(
            context,
            case,
            self.analysis_identity(version, suffix),
            run,
            reason,
        ) {
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

#[async_trait]
impl EngineeringSolver for KiCadCliSolver {
    fn identity(&self) -> SolverIdentity {
        KiCadCliSolver::identity(self)
    }

    fn capabilities(&self) -> SolverCapabilities {
        SolverCapabilities {
            analyses: vec![AnalysisRequest::Drc, AnalysisRequest::BoardExport],
            input_formats: vec!["kicad_pcb".to_string()],
            external_process: true,
        }
    }

    fn applicability(&self, case: &FrozenCase) -> Applicability {
        if !self.capabilities().supports(case.analysis) {
            return Applicability::Unsupported {
                reason: format!("KiCad CLI does not support analysis {:?}", case.analysis),
            };
        }
        match self.board_input(case) {
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
        let board = match self.board_input(case) {
            Ok(board) => board,
            Err(applicability) => {
                return match applicability {
                    Applicability::Unsupported { reason } => SolverOutcome::Unsupported { reason },
                    Applicability::Unavailable { reason, missing } => SolverOutcome::Unavailable {
                        solver_id: self.identity().id,
                        reason,
                        missing,
                    },
                    Applicability::Supported => SolverOutcome::Unsupported {
                        reason: "inconsistent applicability".to_string(),
                    },
                }
            }
        };
        let bytes = match case_input_bytes(case, "board") {
            Ok(bytes) => bytes,
            Err(error) => {
                return SolverOutcome::Failed {
                    solver_id: self.identity().id,
                    reason: error.to_string(),
                    receipt: None,
                }
            }
        };
        if let Err(error) = parse_board(&bytes) {
            return SolverOutcome::Failed {
                solver_id: self.identity().id,
                reason: format!("KiCad PCB parse failed: {error}"),
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
        match case.analysis {
            AnalysisRequest::Drc => self.solve_drc(case, context, &board, &version).await,
            AnalysisRequest::BoardExport => self.solve_stats(case, context, &board, &version).await,
            _ => SolverOutcome::Unsupported {
                reason: format!("KiCad CLI adapter does not support {:?}", case.analysis),
            },
        }
    }
}
