use serde::{Deserialize, Serialize};

/// The adapter identity, independent of the detected tool version. The tool
/// version is appended to the receipt identity at execution time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolverIdentity {
    pub id: String,
    pub version: String,
    pub description: String,
}

impl SolverIdentity {
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            description: description.into(),
        }
    }

    pub fn analysis_identity(&self, tool_version: &str) -> engineering_core::AnalysisIdentity {
        engineering_core::AnalysisIdentity {
            solver_id: self.id.clone(),
            solver_version: format!("{}+tool/{tool_version}", self.version),
            kind: engineering_core::AnalysisKind::SupervisedProcess,
        }
    }
}
