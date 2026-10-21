use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use engineering_core::{
    AnalysisIdentity, AnalysisKind, AnalysisReceipt, AnalysisWarning, ArtifactRef, Convergence,
    CoreError, Digest, EngineeringValue, PropertyRevision, ReceiptContent, ReceiptResult,
    ReceiptSigner, ReceiptValue, Repeatability,
};
use engineering_units::Uncertainty;
use faktor_cas::Cas;
use faktor_core::cancellation::CancellationToken;
use faktor_core::hash::FileHash;
use faktor_terminal::{CommandOutput, EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};

use crate::case::{AnalysisRequest, CaseError, CaseResult, FrozenCase, MAX_SOLVE_DEADLINE_SECS};
use crate::identity::SolverIdentity;

pub const DETECTION_DEADLINE: Duration = Duration::from_secs(5);
pub const MAX_PROCESS_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REPORT_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SolverError {
    #[error("case error: {0}")]
    Case(#[from] CaseError),
    #[error("core error: {0}")]
    Core(#[from] CoreError),
    #[error("io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("process error: {0}")]
    Process(String),
    #[error("CAS error: {0}")]
    Cas(String),
    #[error("artifact {name:?} is invalid: {reason}")]
    Artifact { name: String, reason: String },
    #[error("solver output file {path:?} exceeds {max} bytes")]
    OutputTooLarge { path: String, max: usize },
    #[error("missing solver output file {path:?}")]
    MissingOutput { path: String },
    #[error("unsupported solver setting {name}={value:?}")]
    UnsupportedSetting { name: String, value: String },
    #[error("solve deadline is zero or exceeds {MAX_SOLVE_DEADLINE_SECS}s")]
    InvalidDeadline,
    #[error("serialization failed: {reason}")]
    Serialization { reason: String },
}

pub type SolverResult<T> = Result<T, SolverError>;

/// The receipt contract used by every solver. It is the same executable
/// receipt type used by in-process engineering kernels.
pub type SolverReceipt = AnalysisReceipt;

#[derive(Debug, Clone)]
pub struct SolveContext {
    pub supervisor: Arc<ProcessSupervisor>,
    pub cas: Arc<Cas>,
    pub cancellation: CancellationToken,
    pub deadline: Duration,
    pub operation_id: u64,
    /// Daemon-local receipt signer. Adapter execution signs the canonical
    /// receipt digest (including the adapter identity and captured output
    /// digest) whenever the signer is installed.
    pub signer: Option<Arc<ReceiptSigner>>,
}

impl SolveContext {
    pub fn new(
        supervisor: Arc<ProcessSupervisor>,
        cas: Arc<Cas>,
        deadline: Duration,
        operation_id: u64,
    ) -> Self {
        Self {
            supervisor,
            cas,
            cancellation: CancellationToken::new(),
            deadline,
            operation_id,
            signer: None,
        }
    }

    pub fn with_signer(mut self, signer: Arc<ReceiptSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    fn bounded_deadline(&self) -> SolverResult<Duration> {
        if self.deadline.is_zero() || self.deadline > Duration::from_secs(MAX_SOLVE_DEADLINE_SECS) {
            return Err(SolverError::InvalidDeadline);
        }
        Ok(self.deadline)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolverCapabilities {
    pub analyses: Vec<AnalysisRequest>,
    pub input_formats: Vec<String>,
    pub external_process: bool,
}

impl SolverCapabilities {
    pub fn supports(&self, analysis: AnalysisRequest) -> bool {
        self.analyses.contains(&analysis)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applicability {
    Supported,
    Unsupported {
        reason: String,
    },
    Unavailable {
        reason: String,
        missing: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDetection {
    pub solver_id: String,
    pub program: String,
    pub state: ToolState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolState {
    Available {
        version: String,
    },
    Unavailable {
        reason: String,
        missing: Vec<String>,
    },
}

impl ToolDetection {
    pub fn available(&self) -> bool {
        matches!(self.state, ToolState::Available { .. })
    }

    pub fn version(&self) -> Option<&str> {
        match &self.state {
            ToolState::Available { version } => Some(version),
            ToolState::Unavailable { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SolverOutcome {
    Executed {
        receipt: AnalysisReceipt,
    },
    Unavailable {
        solver_id: String,
        reason: String,
        missing: Vec<String>,
    },
    Unsupported {
        reason: String,
    },
    Failed {
        solver_id: String,
        reason: String,
        receipt: Option<AnalysisReceipt>,
    },
}

impl SolverOutcome {
    pub fn receipt(&self) -> Option<&AnalysisReceipt> {
        match self {
            Self::Executed { receipt } => Some(receipt),
            Self::Failed { receipt, .. } => receipt.as_ref(),
            Self::Unavailable { .. } | Self::Unsupported { .. } => None,
        }
    }
}

#[async_trait]
pub trait EngineeringSolver: Send + Sync {
    fn identity(&self) -> SolverIdentity;
    fn capabilities(&self) -> SolverCapabilities;
    fn applicability(&self, case: &FrozenCase) -> Applicability;
    /// Optional detection hook. A solver that does not use an external tool
    /// may keep this default; the adapters here override it.
    async fn detect(&self, _context: &SolveContext) -> ToolDetection {
        ToolDetection {
            solver_id: self.identity().id,
            program: String::new(),
            state: ToolState::Unavailable {
                reason: "solver does not implement tool detection".to_string(),
                missing: Vec::new(),
            },
        }
    }
    async fn solve(&self, case: &FrozenCase, context: &SolveContext) -> SolverOutcome;
}

#[derive(Debug, Clone)]
pub struct ProcessRun {
    pub output: CommandOutput,
    pub command: String,
    pub duration_ms: u64,
    pub deadline_ms: u64,
    pub timed_out: bool,
}

pub async fn run_tool(
    context: &SolveContext,
    case: &FrozenCase,
    program: &str,
    args: &[String],
) -> SolverResult<ProcessRun> {
    let deadline = context.bounded_deadline()?;
    let command = format!("{} {}", program, args.join(" "))
        .chars()
        .take(8_000)
        .collect::<String>();
    let cfg = SpawnConfig {
        cmd: program.to_string(),
        args: args.to_vec(),
        cwd: case.workdir.clone(),
        env: EnvSpec::toolchain(),
        owner: ProcessOwner::Engineering {
            operation: case.operation_id,
        },
        capture: true,
        artifact_max: MAX_PROCESS_ARTIFACT_BYTES,
        ..SpawnConfig::default()
    };
    let started = Instant::now();
    let output = context
        .supervisor
        .run(cfg, deadline, context.cancellation.clone())
        .await
        .map_err(|error| SolverError::Process(error.to_string()))?;
    Ok(ProcessRun {
        output,
        command,
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        deadline_ms: deadline.as_millis().min(u128::from(u64::MAX)) as u64,
        timed_out: false,
    })
}

pub async fn detect_program(
    context: &SolveContext,
    solver_id: &str,
    program: &str,
    args: &[&str],
) -> ToolDetection {
    let owner = ProcessOwner::Engineering {
        operation: context.operation_id,
    };
    let mut cfg = SpawnConfig {
        cmd: program.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        env: EnvSpec::toolchain(),
        owner,
        capture: true,
        artifact_max: 64 * 1024,
        ..SpawnConfig::default()
    };
    cfg.cwd = std::env::temp_dir();
    let result = context
        .supervisor
        .run(cfg, DETECTION_DEADLINE, context.cancellation.clone())
        .await;
    match result {
        Err(error) => ToolDetection {
            solver_id: solver_id.to_string(),
            program: program.to_string(),
            state: ToolState::Unavailable {
                reason: error.to_string(),
                missing: vec![program.to_string()],
            },
        },
        Ok(output) => {
            if output.exit_code != Some(0) {
                return ToolDetection {
                    solver_id: solver_id.to_string(),
                    program: program.to_string(),
                    state: ToolState::Unavailable {
                        reason: format!("version probe exited with {:?}", output.exit_code),
                        missing: vec![program.to_string()],
                    },
                };
            }
            ToolDetection {
                solver_id: solver_id.to_string(),
                program: program.to_string(),
                state: ToolState::Available {
                    version: parse_version(&output.excerpt, program),
                },
            }
        }
    }
}

fn parse_version(text: &str, program: &str) -> String {
    for line in text.lines().take(40) {
        let line = line.trim();
        for token in line.split_whitespace() {
            let cleaned = token.trim_matches(|c: char| {
                !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
            });
            if cleaned.len() >= 2
                && cleaned.chars().any(|c| c.is_ascii_digit())
                && (cleaned.contains('.') || cleaned.contains('-'))
                && !cleaned.eq_ignore_ascii_case(program)
            {
                return cleaned.chars().take(64).collect();
            }
        }
    }
    "unknown".to_string()
}

pub struct ProcessEvidence {
    pub digest: Digest,
    pub artifact: Option<ArtifactRef>,
    pub truncated: bool,
}

pub fn process_output_evidence(
    context: &SolveContext,
    output: &CommandOutput,
) -> SolverResult<ProcessEvidence> {
    let Some(uri) = output.artifact.as_deref() else {
        return Ok(ProcessEvidence {
            digest: Digest::of_text(&output.excerpt),
            artifact: None,
            truncated: output.artifact_truncated,
        });
    };
    let hex = uri
        .strip_prefix("artifact://")
        .ok_or_else(|| SolverError::Artifact {
            name: "process-output".to_string(),
            reason: format!("unexpected artifact URI {uri:?}"),
        })?;
    let digest = Digest::from_hex(hex)?;
    let hash = FileHash::from_hex(hex).ok_or_else(|| SolverError::Artifact {
        name: "process-output".to_string(),
        reason: "artifact URI does not contain a 32-byte hash".to_string(),
    })?;
    let bytes = context
        .cas
        .get_bounded(hash, MAX_PROCESS_ARTIFACT_BYTES)
        .map_err(|error| SolverError::Cas(error.to_string()))?
        .ok_or_else(|| SolverError::Artifact {
            name: "process-output".to_string(),
            reason: "artifact is larger than the read bound".to_string(),
        })?;
    if Digest::of_bytes(&bytes) != digest {
        return Err(SolverError::Artifact {
            name: "process-output".to_string(),
            reason: "CAS bytes do not match the advertised digest".to_string(),
        });
    }
    Ok(ProcessEvidence {
        digest,
        artifact: Some(ArtifactRef {
            name: "process-output".to_string(),
            digest,
            byte_len: bytes.len() as u64,
            media_type: "text/plain".to_string(),
        }),
        truncated: output.artifact_truncated,
    })
}

pub fn artifact_from_bytes(
    context: &SolveContext,
    name: &str,
    media_type: &str,
    bytes: &[u8],
) -> SolverResult<ArtifactRef> {
    if bytes.len() > MAX_REPORT_ARTIFACT_BYTES {
        return Err(SolverError::OutputTooLarge {
            path: name.to_string(),
            max: MAX_REPORT_ARTIFACT_BYTES,
        });
    }
    let digest = Digest::of_bytes(bytes);
    let hash = context
        .cas
        .put_bounded(bytes, MAX_REPORT_ARTIFACT_BYTES)
        .map_err(|error| SolverError::Cas(error.to_string()))?;
    if hash.to_hex() != digest.to_hex() {
        return Err(SolverError::Artifact {
            name: name.to_string(),
            reason: "CAS identity differs from the content digest".to_string(),
        });
    }
    Ok(ArtifactRef {
        name: name.to_string(),
        digest,
        byte_len: bytes.len() as u64,
        media_type: media_type.to_string(),
    })
}

pub fn read_output_file(path: &Path, max: usize) -> SolverResult<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|error| SolverError::MissingOutput {
        path: format!("{}: {error}", path.display()),
    })?;
    let metadata = file.metadata().map_err(|error| SolverError::Io {
        path: path.display().to_string(),
        source: error,
    })?;
    if metadata.len() > max as u64 {
        return Err(SolverError::OutputTooLarge {
            path: path.display().to_string(),
            max,
        });
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SolverError::Io {
            path: path.display().to_string(),
            source: error,
        })?;
    if bytes.len() > max {
        return Err(SolverError::OutputTooLarge {
            path: path.display().to_string(),
            max,
        });
    }
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
pub fn receipt_from_process(
    context: &SolveContext,
    case: &FrozenCase,
    identity: AnalysisIdentity,
    run: &ProcessRun,
    convergence: Convergence,
    warnings: Vec<AnalysisWarning>,
    results: Vec<ReceiptResult>,
    mut artifacts: Vec<ArtifactRef>,
    uncertainty: Uncertainty,
    repeatability: Repeatability,
) -> SolverResult<AnalysisReceipt> {
    let evidence = process_output_evidence(context, &run.output)?;
    if let Some(artifact) = evidence.artifact {
        artifacts.push(artifact);
    }
    if results.is_empty() && artifacts.is_empty() {
        return Err(SolverError::Artifact {
            name: "receipt".to_string(),
            reason: "no results or artifacts".to_string(),
        });
    }
    let mut content =
        ReceiptContent::new(case.input_digest, case.model.digest, case.settings_digest);
    content.property_revisions = case.property_revisions.clone();
    content.boundary_conditions = case.boundary_conditions.clone();
    content.convergence = convergence;
    content.warnings = warnings;
    content.results = results;
    content.uncertainty = uncertainty;
    content.mesh_digest = artifacts
        .iter()
        .find(|artifact| artifact.media_type.contains("gmsh") || artifact.name.ends_with(".msh"))
        .map(|artifact| artifact.digest);
    content.artifacts = artifacts;
    content.repeatability = repeatability;
    let mut receipt = AnalysisReceipt::supervised(
        identity,
        content,
        engineering_core::SupervisedProcessEvidence {
            command: run.command.clone(),
            exit_code: run.output.exit_code.unwrap_or(-1),
            output_digest: evidence.digest,
            timed_out: run.timed_out,
            output_truncated: evidence.truncated,
            deadline_ms: run.deadline_ms,
        },
    )?;
    if let Some(signer) = &context.signer {
        signer.sign_receipt(&mut receipt)?;
    }
    Ok(receipt)
}

pub fn failure_receipt(
    context: &SolveContext,
    case: &FrozenCase,
    identity: AnalysisIdentity,
    run: &ProcessRun,
    reason: &str,
) -> SolverResult<AnalysisReceipt> {
    let artifacts = Vec::new();
    let results = vec![ReceiptResult {
        name: "failure".to_string(),
        value: ReceiptValue::Text(reason.chars().take(4_000).collect()),
        uncertainty: Uncertainty::Exact,
    }];
    receipt_from_process(
        context,
        case,
        identity,
        run,
        Convergence::iterative(1, 0.0, 1.0, false),
        vec![AnalysisWarning {
            code: "solver.failure".to_string(),
            message: reason.chars().take(4_000).collect(),
            severity: engineering_core::WarningSeverity::Critical,
        }],
        results,
        artifacts,
        Uncertainty::Exact,
        Repeatability::unverified("solver"),
    )
}

pub fn quantity_result(name: &str, quantity: engineering_units::Quantity) -> ReceiptResult {
    ReceiptResult {
        name: name.to_string(),
        value: ReceiptValue::Quantity(quantity),
        uncertainty: Uncertainty::Exact,
    }
}

pub fn text_result(name: &str, value: impl Into<String>) -> ReceiptResult {
    ReceiptResult {
        name: name.to_string(),
        value: ReceiptValue::Text(value.into()),
        uncertainty: Uncertainty::Exact,
    }
}

pub fn boundary_values(
    values: &[(String, EngineeringValue)],
) -> std::collections::BTreeMap<String, EngineeringValue> {
    values.iter().cloned().collect()
}

pub fn case_input_bytes(case: &FrozenCase, name: &str) -> SolverResult<Vec<u8>> {
    let input = case.input(name).ok_or_else(|| SolverError::MissingOutput {
        path: format!("input {name}"),
    })?;
    read_output_file(&input.path, crate::case::MAX_INPUT_BYTES)
}

pub fn frozen_input_path(case: &FrozenCase, name: &str) -> SolverResult<std::path::PathBuf> {
    Ok(case
        .input(name)
        .ok_or_else(|| SolverError::MissingOutput {
            path: format!("input {name}"),
        })?
        .path
        .clone())
}

pub fn check_frozen(case: &FrozenCase) -> CaseResult<()> {
    case.verify_frozen()
}

pub fn identity_kind() -> AnalysisKind {
    AnalysisKind::SupervisedProcess
}

pub fn identity_for(
    solver_id: &str,
    adapter_version: &str,
    tool_version: &str,
) -> AnalysisIdentity {
    AnalysisIdentity {
        solver_id: solver_id.to_string(),
        solver_version: format!("{adapter_version}+tool/{tool_version}"),
        kind: AnalysisKind::SupervisedProcess,
    }
}

pub fn empty_property_revisions() -> Vec<PropertyRevision> {
    Vec::new()
}
