//! `faktor approval` — the human-operator surface for consequential actions.
//!
//! Agents may create requests through the daemon `approval_request` tool or
//! the `request` subcommand. Only a human holding the owner-only operator key
//! can sign a decision through `decide`; no model tool exposes that command,
//! and the key file is never read by the daemon tool registry.

use std::path::PathBuf;

use clap::Subcommand;
use faktor_approval::{
    load_operator_key, operator_key_path, register_operator_key, save_operator_key, unix_now_ms,
    ApprovalAuthority, ApprovalDraft, DecisionOutcome, OperatorKey, Principal, PrincipalKind,
};
use serde_json::{json, Value};

use crate::expand;

/// Human approval subcommands.
#[derive(Subcommand)]
pub(crate) enum ApprovalAction {
    /// Create (once) an owner-only Ed25519 operator key and register its
    /// public key as a human approval authority.
    Keygen {
        /// Human principal id (also the key filename).
        #[arg(long)]
        principal: String,
        /// Data directory (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Record an agent/user proposal from a typed ApprovalDraft JSON object.
    Request {
        /// JSON file, or `-`/omitted for stdin.
        #[arg(long)]
        input: Option<String>,
        /// Data directory (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Sign exactly one approval decision as a registered human operator.
    Decide {
        /// Request id (`apr-...`).
        #[arg(long)]
        request_id: String,
        /// Approve the request.
        #[arg(long, conflicts_with = "deny")]
        approve: bool,
        /// Deny the request.
        #[arg(long, conflicts_with = "approve")]
        deny: bool,
        /// Human principal id.
        #[arg(long)]
        principal: String,
        /// Operator key path (defaults to the principal's key in the data dir).
        #[arg(long)]
        key: Option<String>,
        /// Decision rationale.
        #[arg(long, default_value = "reviewed")]
        rationale: String,
        /// Data directory (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// List every durable approval request and decision.
    List {
        /// Data directory (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Show one request's status.
    Status {
        /// Request id.
        #[arg(long)]
        request_id: String,
        /// Data directory (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Print the hash-chained audit trail (all requests, or one request).
    Audit {
        /// Optional request id.
        #[arg(long)]
        request_id: Option<String>,
        /// Data directory (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
}

/// Dispatch one approval command and print bounded JSON.
pub(crate) fn command(action: ApprovalAction) -> Result<(), String> {
    let value = execute(&action)?;
    let rendered = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("cannot render result: {error}"))?;
    println!("{rendered}");
    Ok(())
}

fn data_dir(action: &ApprovalAction) -> PathBuf {
    expand(match action {
        ApprovalAction::Keygen { data_dir, .. }
        | ApprovalAction::Request { data_dir, .. }
        | ApprovalAction::Decide { data_dir, .. }
        | ApprovalAction::List { data_dir }
        | ApprovalAction::Status { data_dir, .. }
        | ApprovalAction::Audit { data_dir, .. } => data_dir,
    })
}

fn execute(action: &ApprovalAction) -> Result<Value, String> {
    let data_dir = data_dir(action);
    match action {
        ApprovalAction::Keygen { principal, .. } => {
            let key = OperatorKey::generate();
            let path = save_operator_key(&data_dir, principal, &key, false)
                .map_err(|error| error.to_string())?;
            let registered =
                register_operator_key(&data_dir, principal, &key, unix_now_ms().max(1))
                    .map_err(|error| error.to_string())?;
            Ok(json!({
                "status": "operator_registered",
                "principal": registered.principal_id,
                "key_id": registered.key_id,
                "public_key": registered.public_key,
                "key_path": path.display().to_string(),
            }))
        }
        ApprovalAction::Request { input, .. } => {
            let raw = crate::supply::read_input(input)?;
            let draft: ApprovalDraft = serde_json::from_str(&raw)
                .map_err(|error| format!("invalid approval draft: {error}"))?;
            if draft.requester.kind == PrincipalKind::HumanOperator {
                return Err(
                    "a human operator proposal must use the agent/service principal that prepares the work"
                        .to_string(),
                );
            }
            let authority = ApprovalAuthority::open(&data_dir).map_err(|e| e.to_string())?;
            let request = authority
                .propose(draft, unix_now_ms())
                .map_err(|error| error.to_string())?;
            serde_json::to_value(request).map_err(|error| error.to_string())
        }
        ApprovalAction::Decide {
            request_id,
            approve,
            deny,
            principal,
            key,
            rationale,
            ..
        } => {
            if !*approve && !*deny {
                return Err("exactly one of --approve/--deny is required".to_string());
            }
            let principal = Principal::new(principal.clone(), PrincipalKind::HumanOperator)
                .map_err(|error| error.to_string())?;
            let key_path = match key {
                Some(path) => crate::expand(path),
                None => operator_key_path(&data_dir, &principal.id)
                    .map_err(|error| error.to_string())?,
            };
            let operator = load_operator_key(&key_path).map_err(|error| error.to_string())?;
            let outcome = if *approve {
                DecisionOutcome::Approved
            } else {
                DecisionOutcome::Denied {
                    reason: rationale.clone(),
                }
            };
            let authority = ApprovalAuthority::open(&data_dir).map_err(|e| e.to_string())?;
            let decision = authority
                .decide(
                    request_id,
                    principal,
                    &operator,
                    outcome,
                    rationale.clone(),
                    unix_now_ms(),
                )
                .map_err(|error| error.to_string())?;
            serde_json::to_value(decision).map_err(|error| error.to_string())
        }
        ApprovalAction::List { .. } => {
            let authority = ApprovalAuthority::open(&data_dir).map_err(|e| e.to_string())?;
            let views = authority.list().map_err(|error| error.to_string())?;
            serde_json::to_value(views).map_err(|error| error.to_string())
        }
        ApprovalAction::Status { request_id, .. } => {
            let authority = ApprovalAuthority::open(&data_dir).map_err(|e| e.to_string())?;
            let status = authority
                .status(request_id)
                .map_err(|error| error.to_string())?;
            serde_json::to_value(status).map_err(|error| error.to_string())
        }
        ApprovalAction::Audit { request_id, .. } => {
            let authority = ApprovalAuthority::open(&data_dir).map_err(|e| e.to_string())?;
            let entries = authority
                .audit(request_id.as_deref())
                .map_err(|error| error.to_string())?;
            Ok(json!({
                "chain_length": entries.len(),
                "entries": entries,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_then_request_and_agent_decision_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().to_string_lossy().to_string();
        execute(&ApprovalAction::Keygen {
            principal: "ops".to_string(),
            data_dir: data.clone(),
        })
        .unwrap();
        let draft = ApprovalDraft {
            kind: faktor_approval::ApprovalKind::EngineeringConstraintRelaxation,
            subject: "set".to_string(),
            evidence: vec!["cpk".to_string()],
            proposed_change: "10 -> 25 um".to_string(),
            risk: faktor_approval::RiskLevel::High,
            requester: Principal::new("agent-1", PrincipalKind::Agent).unwrap(),
            bound_revision: "7".to_string(),
            bound_digest: "a".repeat(64),
            expires_at_ms: unix_now_ms() + 60_000,
        };
        let request: Value = execute(&ApprovalAction::Request {
            input: Some(write_json(&draft)),
            data_dir: data.clone(),
        })
        .unwrap();
        let request_id = request["id"].as_str().unwrap().to_string();
        let denied = execute(&ApprovalAction::Decide {
            request_id: request_id.clone(),
            approve: true,
            deny: false,
            principal: "agent-1".to_string(),
            key: None,
            rationale: "self".to_string(),
            data_dir: data,
        });
        assert!(denied.is_err(), "only a registered human key may decide");
    }

    fn write_json<T: serde::Serialize>(value: &T) -> String {
        let path = std::env::temp_dir().join(format!(
            "faktor-approval-draft-{}-{}.json",
            std::process::id(),
            unix_now_ms()
        ));
        std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path.to_string_lossy().to_string()
    }
}
