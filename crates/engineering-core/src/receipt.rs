use std::collections::BTreeMap;

use engineering_units::{Quantity, Uncertainty};
use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::{require_text, CoreError, CoreResult};
use crate::value::{EngineeringValue, RevisionId, UnavailableValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisKind {
    /// Deterministic arithmetic executed inside the Rust process.
    InProcessKernel,
    /// A real external tool executed through the process supervisor.
    SupervisedProcess,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisIdentity {
    pub solver_id: String,
    pub solver_version: String,
    pub kind: AnalysisKind,
}

impl AnalysisIdentity {
    pub fn validate(&self) -> CoreResult<()> {
        require_text("analysis.solver_id", &self.solver_id, 256)?;
        require_text("analysis.solver_version", &self.solver_version, 256)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisWarning {
    pub code: String,
    pub message: String,
    pub severity: WarningSeverity,
}

impl AnalysisWarning {
    pub fn validate(&self) -> CoreResult<()> {
        require_text("warning.code", &self.code, 128)?;
        require_text("warning.message", &self.message, 4_096)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Convergence {
    pub converged: bool,
    pub iterations: u32,
    pub residual: f64,
    pub tolerance: f64,
    pub note: Option<String>,
}

impl Convergence {
    pub fn exact() -> Self {
        Self {
            converged: true,
            iterations: 1,
            residual: 0.0,
            tolerance: 0.0,
            note: Some("closed-form / exact deterministic kernel".to_string()),
        }
    }

    pub fn iterative(iterations: u32, residual: f64, tolerance: f64, converged: bool) -> Self {
        Self {
            converged,
            iterations,
            residual,
            tolerance,
            note: None,
        }
    }

    pub fn validate(&self) -> CoreResult<()> {
        if !self.residual.is_finite() || !self.tolerance.is_finite() {
            return Err(CoreError::InvalidReceipt {
                reason: "convergence residual/tolerance must be finite".to_string(),
            });
        }
        if self.tolerance < 0.0 {
            return Err(CoreError::InvalidReceipt {
                reason: "convergence tolerance must be >= 0".to_string(),
            });
        }
        if self.iterations > 1_000_000 {
            return Err(CoreError::InvalidReceipt {
                reason: "convergence iteration count exceeds the bound".to_string(),
            });
        }
        if let Some(note) = &self.note {
            require_text("convergence.note", note, 1_024)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptValue {
    Quantity(Quantity),
    Text(String),
    Unavailable(UnavailableValue),
}

impl ReceiptValue {
    fn validate(&self) -> CoreResult<()> {
        match self {
            Self::Quantity(quantity) => {
                if quantity.value().is_finite() {
                    Ok(())
                } else {
                    Err(CoreError::InvalidReceipt {
                        reason: "result quantity is not finite".to_string(),
                    })
                }
            }
            Self::Text(text) => require_text("result.text", text, 8_192),
            Self::Unavailable(reason) => reason.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReceiptResult {
    pub name: String,
    pub value: ReceiptValue,
    pub uncertainty: Uncertainty,
}

impl ReceiptResult {
    pub fn validate(&self) -> CoreResult<()> {
        require_text("result.name", &self.name, 256)?;
        self.value.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyRevision {
    pub property: String,
    pub revision: RevisionId,
    pub digest: Option<Digest>,
}

impl PropertyRevision {
    pub fn new(property: impl Into<String>, revision: RevisionId) -> CoreResult<Self> {
        let value = Self {
            property: property.into(),
            revision,
            digest: None,
        };
        require_text("property_revision.property", &value.property, 256)?;
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub name: String,
    pub digest: Digest,
    pub byte_len: u64,
    pub media_type: String,
}

impl ArtifactRef {
    pub fn validate(&self) -> CoreResult<()> {
        require_text("artifact.name", &self.name, 256)?;
        require_text("artifact.media_type", &self.media_type, 128)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repeatability {
    /// True only when a repeat run (or a bitwise-deterministic kernel) backs
    /// the claim. External tools are not assumed reproducible by default.
    pub bitwise_reproducible: bool,
    pub repeats: u32,
    pub basis: String,
}

impl Repeatability {
    pub fn deterministic() -> Self {
        Self {
            bitwise_reproducible: true,
            repeats: 0,
            basis: "pure deterministic kernel; identical f64 inputs produce identical results without a repeat run"
                .to_string(),
        }
    }

    pub fn unverified(tool: &str) -> Self {
        Self {
            bitwise_reproducible: false,
            repeats: 0,
            basis: format!(
                "no repeat run performed; {tool} output may include timestamps or scheduling order"
            ),
        }
    }

    pub fn validate(&self) -> CoreResult<()> {
        require_text("repeatability.basis", &self.basis, 1_024)
    }
}

/// What actually executed. A receipt is never accepted on prose alone: the
/// gate validation re-checks this evidence and the recorded digests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionEvidence {
    InProcess {
        completed: bool,
        runtime_ms: u64,
        result_digest: Digest,
    },
    SupervisedProcess {
        command: String,
        exit_code: i32,
        output_digest: Digest,
        timed_out: bool,
        output_truncated: bool,
        deadline_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisReceipt {
    pub identity: AnalysisIdentity,
    pub input_digest: Digest,
    pub model_digest: Digest,
    pub property_revisions: Vec<PropertyRevision>,
    pub boundary_conditions: BTreeMap<String, EngineeringValue>,
    pub settings_digest: Digest,
    /// Digest of the mesh artifact when the analysis is mesh-based.
    pub mesh_digest: Option<Digest>,
    pub convergence: Convergence,
    pub warnings: Vec<AnalysisWarning>,
    pub results: Vec<ReceiptResult>,
    pub uncertainty: Uncertainty,
    pub artifacts: Vec<ArtifactRef>,
    pub repeatability: Repeatability,
    pub executed: bool,
    pub execution: ExecutionEvidence,
    /// Digest of the central applicability selection that governed this
    /// analysis. The response records the full selection; the receipt binds
    /// it into the canonical signed digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_digest: Option<Digest>,
    /// Ed25519 attestation over [`AnalysisReceipt::canonical_digest`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<crate::attestation::ReceiptAttestation>,
}

/// Everything except identity/execution that a kernel or tool run records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReceiptContent {
    pub input_digest: Digest,
    pub model_digest: Digest,
    pub property_revisions: Vec<PropertyRevision>,
    pub boundary_conditions: BTreeMap<String, EngineeringValue>,
    pub settings_digest: Digest,
    /// Digest of the mesh artifact when the analysis is mesh-based.
    pub mesh_digest: Option<Digest>,
    pub convergence: Convergence,
    pub warnings: Vec<AnalysisWarning>,
    pub results: Vec<ReceiptResult>,
    pub uncertainty: Uncertainty,
    pub artifacts: Vec<ArtifactRef>,
    pub repeatability: Repeatability,
    /// Optional digest of the selected model/applicability record.
    pub selection_digest: Option<Digest>,
}

impl ReceiptContent {
    pub fn new(input_digest: Digest, model_digest: Digest, settings_digest: Digest) -> Self {
        Self {
            input_digest,
            model_digest,
            property_revisions: Vec::new(),
            boundary_conditions: BTreeMap::new(),
            settings_digest,
            mesh_digest: None,
            convergence: Convergence::exact(),
            warnings: Vec::new(),
            results: Vec::new(),
            uncertainty: Uncertainty::Exact,
            artifacts: Vec::new(),
            repeatability: Repeatability::deterministic(),
            selection_digest: None,
        }
    }

    pub fn with_property_revision(mut self, revision: PropertyRevision) -> Self {
        self.property_revisions.push(revision);
        self
    }

    pub fn with_boundary_condition(
        mut self,
        name: impl Into<String>,
        value: EngineeringValue,
    ) -> Self {
        self.boundary_conditions.insert(name.into(), value);
        self
    }

    pub fn with_convergence(mut self, convergence: Convergence) -> Self {
        self.convergence = convergence;
        self
    }

    pub fn with_warning(mut self, warning: AnalysisWarning) -> Self {
        self.warnings.push(warning);
        self
    }

    pub fn with_result(mut self, result: ReceiptResult) -> Self {
        self.results.push(result);
        self
    }

    pub fn with_uncertainty(mut self, uncertainty: Uncertainty) -> Self {
        self.uncertainty = uncertainty;
        self
    }

    pub fn with_artifact(mut self, artifact: ArtifactRef) -> Self {
        self.artifacts.push(artifact);
        self
    }

    pub fn with_repeatability(mut self, repeatability: Repeatability) -> Self {
        self.repeatability = repeatability;
        self
    }

    pub fn with_selection_digest(mut self, digest: Digest) -> Self {
        self.selection_digest = Some(digest);
        self
    }
}

impl AnalysisReceipt {
    pub fn kernel(
        identity: AnalysisIdentity,
        content: ReceiptContent,
        runtime_ms: u64,
    ) -> CoreResult<Self> {
        if identity.kind != AnalysisKind::InProcessKernel {
            return Err(CoreError::InvalidReceipt {
                reason: "kernel receipt identity must be in_process_kernel".to_string(),
            });
        }
        let result_digest = Digest::of_json(&content.results)?;
        let receipt = Self {
            identity,
            input_digest: content.input_digest,
            model_digest: content.model_digest,
            property_revisions: content.property_revisions,
            boundary_conditions: content.boundary_conditions,
            settings_digest: content.settings_digest,
            mesh_digest: content.mesh_digest,
            convergence: content.convergence,
            warnings: content.warnings,
            results: content.results,
            uncertainty: content.uncertainty,
            artifacts: content.artifacts,
            repeatability: content.repeatability,
            executed: true,
            execution: ExecutionEvidence::InProcess {
                completed: true,
                runtime_ms,
                result_digest,
            },
            selection_digest: content.selection_digest,
            attestation: None,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn supervised(
        identity: AnalysisIdentity,
        content: ReceiptContent,
        process: SupervisedProcessEvidence,
    ) -> CoreResult<Self> {
        if identity.kind != AnalysisKind::SupervisedProcess {
            return Err(CoreError::InvalidReceipt {
                reason: "process receipt identity must be supervised_process".to_string(),
            });
        }
        let receipt = Self {
            identity,
            input_digest: content.input_digest,
            model_digest: content.model_digest,
            property_revisions: content.property_revisions,
            boundary_conditions: content.boundary_conditions,
            settings_digest: content.settings_digest,
            mesh_digest: content.mesh_digest,
            convergence: content.convergence,
            warnings: content.warnings,
            results: content.results,
            uncertainty: content.uncertainty,
            artifacts: content.artifacts,
            repeatability: content.repeatability,
            executed: true,
            execution: ExecutionEvidence::SupervisedProcess {
                command: process.command,
                exit_code: process.exit_code,
                output_digest: process.output_digest,
                timed_out: process.timed_out,
                output_truncated: process.output_truncated,
                deadline_ms: process.deadline_ms,
            },
            selection_digest: content.selection_digest,
            attestation: None,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn results_digest(&self) -> CoreResult<Digest> {
        Digest::of_json(&self.results)
    }

    pub fn validate(&self) -> CoreResult<()> {
        self.identity.validate()?;
        if !self.executed {
            return Err(CoreError::InvalidReceipt {
                reason: "receipt is marked not executed".to_string(),
            });
        }
        for revision in &self.property_revisions {
            require_text("property_revision.property", &revision.property, 256)?;
        }
        for (name, value) in &self.boundary_conditions {
            require_text("boundary_condition.name", name, 256)?;
            value.validate()?;
        }
        self.convergence.validate()?;
        for warning in &self.warnings {
            warning.validate()?;
        }
        for result in &self.results {
            result.validate()?;
        }
        for artifact in &self.artifacts {
            artifact.validate()?;
        }
        self.repeatability.validate()?;
        match (&self.identity.kind, &self.execution) {
            (
                AnalysisKind::InProcessKernel,
                ExecutionEvidence::InProcess {
                    completed,
                    result_digest,
                    ..
                },
            ) => {
                if !completed {
                    return Err(CoreError::InvalidReceipt {
                        reason: "in-process receipt is not completed".to_string(),
                    });
                }
                let expected = self.results_digest()?;
                if *result_digest != expected {
                    return Err(CoreError::ResultDigestMismatch(*result_digest));
                }
            }
            (
                AnalysisKind::SupervisedProcess,
                ExecutionEvidence::SupervisedProcess {
                    command,
                    deadline_ms,
                    output_digest,
                    ..
                },
            ) => {
                require_text("execution.command", command, 8_192)?;
                if *deadline_ms == 0 || *deadline_ms > 3_600_000 {
                    return Err(CoreError::InvalidReceipt {
                        reason: "execution deadline is outside the bounded range".to_string(),
                    });
                }
                // The captured process-output artifact must be the exact bytes
                // the receipt claims executed; a rewritten output digest next
                // to the genuine artifact is a forged receipt.
                for artifact in self
                    .artifacts
                    .iter()
                    .filter(|artifact| artifact.name == "process-output")
                {
                    if artifact.digest != *output_digest {
                        return Err(CoreError::InvalidReceipt {
                            reason:
                                "process-output artifact digest disagrees with execution evidence"
                                    .to_string(),
                        });
                    }
                }
            }
            _ => {
                return Err(CoreError::InvalidReceipt {
                    reason: "analysis identity and execution evidence disagree".to_string(),
                });
            }
        }
        if self.results.is_empty() && self.artifacts.is_empty() {
            return Err(CoreError::InvalidReceipt {
                reason: "receipt records neither results nor artifacts".to_string(),
            });
        }
        if self.attestation.is_some() {
            self.verify_attestation()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisedProcessEvidence {
    pub command: String,
    pub exit_code: i32,
    pub output_digest: Digest,
    pub timed_out: bool,
    pub output_truncated: bool,
    pub deadline_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ProvenanceKind;

    fn identity() -> AnalysisIdentity {
        AnalysisIdentity {
            solver_id: "test-kernel".to_string(),
            solver_version: "1.0.0".to_string(),
            kind: AnalysisKind::InProcessKernel,
        }
    }

    fn content() -> ReceiptContent {
        let mut content = ReceiptContent::new(
            Digest::of_text("input"),
            Digest::of_text("model"),
            Digest::of_text("settings"),
        );
        content.results.push(ReceiptResult {
            name: "answer".to_string(),
            value: ReceiptValue::Text("42".to_string()),
            uncertainty: Uncertainty::Exact,
        });
        let source =
            crate::value::Provenance::new(ProvenanceKind::Calculation, "unit test").unwrap();
        content.boundary_conditions.insert(
            "temperature".to_string(),
            EngineeringValue::known(
                engineering_units::Quantity::dimensionless(300.0).unwrap(),
                source,
                RevisionId::new(1),
            )
            .unwrap(),
        );
        content
    }

    #[test]
    fn a_kernel_receipt_binds_results_to_a_digest() {
        let receipt = AnalysisReceipt::kernel(identity(), content(), 1).unwrap();
        assert!(receipt.executed);
        assert_eq!(receipt.results.len(), 1);
        assert_eq!(receipt.results_digest().unwrap().to_hex().len(), 64);
    }

    #[test]
    fn a_lying_result_digest_is_detected() {
        let mut receipt = AnalysisReceipt::kernel(identity(), content(), 1).unwrap();
        if let ExecutionEvidence::InProcess { result_digest, .. } = &mut receipt.execution {
            *result_digest = Digest::of_text("not the results");
        }
        assert!(matches!(
            receipt.validate(),
            Err(CoreError::ResultDigestMismatch(_))
        ));
    }

    fn supervised_identity() -> AnalysisIdentity {
        AnalysisIdentity {
            solver_id: "test-tool".to_string(),
            solver_version: "1.0.0+tool/9.9".to_string(),
            kind: AnalysisKind::SupervisedProcess,
        }
    }

    #[test]
    fn supervised_receipt_binds_the_captured_output_digest() {
        let mut content = content();
        content.artifacts.push(ArtifactRef {
            name: "process-output".to_string(),
            digest: Digest::of_text("captured output"),
            byte_len: 15,
            media_type: "text/plain".to_string(),
        });
        let evidence = SupervisedProcessEvidence {
            command: "tool --run".to_string(),
            exit_code: 0,
            output_digest: Digest::of_text("captured output"),
            timed_out: false,
            output_truncated: false,
            deadline_ms: 1_000,
        };
        let mut receipt =
            AnalysisReceipt::supervised(supervised_identity(), content, evidence).unwrap();
        if let ExecutionEvidence::SupervisedProcess { output_digest, .. } = &mut receipt.execution {
            *output_digest = Digest::of_text("rewritten output");
        }
        assert!(matches!(
            receipt.validate(),
            Err(CoreError::InvalidReceipt { .. })
        ));
    }

    #[test]
    fn a_not_executed_receipt_is_refused() {
        let mut receipt = AnalysisReceipt::kernel(identity(), content(), 1).unwrap();
        receipt.executed = false;
        assert!(receipt.validate().is_err());
    }
}
