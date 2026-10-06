//! `runtime::tool_loop`: cohesive slice of the agent runtime.

use super::*;

/// Tool output at or above this byte count is archived through the durable
/// evidence authority (normalized + compressed + retrievable backing); the
/// wire result carries the bounded compact reference plus the existing
/// excerpt.
pub(crate) const TOOL_OUTPUT_EVIDENCE_MIN_BYTES: usize = 8 * 1024;

/// Artifact storage handed to tools (bounded writes to the CAS). The REAL
/// sink carries the daemon's configured-secret registry (the SAME instance
/// the egress scan uses), and every stored byte buffer is exact-redacted
/// through it BEFORE the CAS put / inline materialization — a tool that
/// echoes a configured credential can never land it in durable storage raw.
#[derive(Clone)]
pub enum ToolArtifactSink {
    Real {
        writer: Arc<ArtifactWriter>,
        secrets: Option<Arc<faktor_security::registry::SecretRegistry>>,
    },
    Null,
}

impl ToolArtifactSink {
    pub fn store(
        &self,
        kind: &str,
        bytes: &[u8],
        max_inline: usize,
    ) -> faktor_core::Result<faktor_context::ArtifactRef> {
        match self {
            ToolArtifactSink::Real { writer, secrets } => {
                let scrubbed;
                let payload = match secrets {
                    Some(registry) if !registry.scan_exact(bytes).is_empty() => {
                        scrubbed = registry.redact_bytes(bytes);
                        scrubbed.as_slice()
                    }
                    _ => bytes,
                };
                writer.store(kind, payload, max_inline)
            }
            ToolArtifactSink::Null => Ok(faktor_context::ArtifactRef {
                inline: Some(String::from_utf8_lossy(bytes).to_string()),
                artifact: None,
                summary: "null sink".into(),
                size: bytes.len(),
            }),
        }
    }
}

/// The scheduler's ownership sets for one tool invocation, derived from the
/// tool's declared path args (read_file/search ⇒ reads; write_file ⇒ writes).
/// This is the ONLY source for `ScheduledOp::reads/writes` — tools never
/// hand the scheduler raw paths from any other channel.
///
/// The declared spellings are canonicalized against the SESSION workspace
/// root (the same root the tool executes against) through
/// [`OwnershipSet::canonicalized`] — the ONE path-identity authority — before
/// they become scheduler resources, so `src/a`, `./src/a`, `src/x/../a` and
/// `src//a` are one resource exactly as the rooted filesystem treats them as
/// one object. A session with no resolvable workspace passes `None`: nothing
/// executes against a root there, so the declared spelling is kept (today's
/// behavior).
pub(crate) fn ownership_sets(
    tool: &Arc<Tool>,
    input: &serde_json::Value,
    workspace_root: Option<&std::path::Path>,
) -> (OwnershipSet, OwnershipSet) {
    let ownership = tool.ownership(input);
    let read_only = shell_read_only(input);
    ownership_sets_for(
        tool.capability.as_ref(),
        ownership,
        workspace_root,
        read_only,
    )
}

/// TRUE when the tool input marks this shell execution read-only (P1-8): the
/// spawn layer installs kernel write denial and the scheduler takes NO write
/// ownership, so a verification command never consumes a path-constrained
/// ChangeBudget and never serializes against workspace writers.
pub(crate) fn shell_read_only(input: &serde_json::Value) -> bool {
    input
        .get("read_only")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// The tool's DECLARED sets in the policy path vocabulary (the
/// repository-relative spellings the model supplied), with the P0-2 shell
/// rule applied but WITHOUT workspace-root canonicalization. The
/// ChangeBudget gate observes exactly these: its `allowed_paths` are
/// repository-relative, so it must never see the canonical absolute identity
/// the scheduler uses.
pub(crate) fn declared_ownership_sets(
    tool: &Arc<Tool>,
    input: &serde_json::Value,
) -> (OwnershipSet, OwnershipSet) {
    let ownership = tool.ownership(input);
    let read_only = shell_read_only(input);
    ownership_sets_for(tool.capability.as_ref(), ownership, None, read_only)
}

/// The P0-2 ownership rule, factored for direct testing: a generic shell
/// capability owns the whole workspace (both directions); every other tool
/// keeps its declared ownership, canonicalized against `workspace_root` when
/// one is resolvable. The shell sentinel is returned BEFORE the normalizer:
/// `**` is a policy token, not a path, and canonicalization must never turn
/// it into one (the scheduler's [`OwnershipSet::canonicalized`] also
/// preserves it defensively).
pub(crate) fn ownership_sets_for(
    capability: Option<&faktor_core::capability::Capability>,
    ownership: crate::tool::Ownership,
    workspace_root: Option<&std::path::Path>,
    read_only: bool,
) -> (OwnershipSet, OwnershipSet) {
    if matches!(
        capability,
        Some(faktor_core::capability::Capability::ExecuteShell { .. })
    ) {
        if read_only {
            // Kernel write denial: the read-only shell owns the workspace
            // for READS only and takes NO write ownership — it runs
            // concurrently with writers and needs no change manifest.
            return (
                OwnershipSet::workspace_root(),
                OwnershipSet::new(Vec::new()),
            );
        }
        return (
            OwnershipSet::workspace_root(),
            OwnershipSet::workspace_root(),
        );
    }
    let normalized = |paths: Vec<String>| match workspace_root {
        Some(root) => OwnershipSet::new(paths).canonicalized(root),
        None => OwnershipSet::new(paths),
    };
    (normalized(ownership.reads), normalized(ownership.writes))
}

#[cfg(test)]
mod shell_ownership_tests {
    use super::*;
    use faktor_core::capability::Capability;

    #[test]
    fn a_generic_shell_owns_the_whole_workspace_for_writes() {
        let root = tempfile::tempdir().unwrap();
        let (_reads, writes) = ownership_sets_for(
            Some(&Capability::ExecuteShell {
                command: String::new(),
            }),
            crate::tool::Ownership {
                reads: vec!["src/a.rs".into()],
                writes: Vec::new(),
            },
            Some(root.path()),
            false,
        );
        assert_eq!(
            writes.entries(),
            &["**".to_string()],
            "canonicalization must not turn the shell sentinel into a path"
        );
        assert!(
            writes.overlaps(&OwnershipSet::new(["src/a.rs".to_string()])),
            "a shell write must serialize with any workspace writer"
        );
        assert!(
            writes.overlaps(&OwnershipSet::new(["docs/readme.md".to_string()])),
            "a shell write must serialize with EVERY workspace path"
        );
    }

    #[test]
    fn a_full_workspace_shell_write_is_refused_by_a_path_constrained_budget() {
        use faktor_core::state::ChangeBudget;
        let budget = ChangeBudget {
            allowed_paths: vec!["docs".into()],
            ..Default::default()
        };
        let observations = faktor_session::budget::ChangeObservations {
            changed_paths: vec!["**".to_string()],
            ..Default::default()
        };
        assert!(
            faktor_session::budget::check_change_budget(&budget, &observations).is_err(),
            "a docs-only task must refuse the generic shell BEFORE execution"
        );
    }

    #[test]
    fn non_shell_tools_keep_their_declared_ownership() {
        let root = tempfile::tempdir().unwrap();
        let (reads, writes) = ownership_sets_for(
            None,
            crate::tool::Ownership {
                reads: vec!["src/a.rs".into()],
                writes: vec!["src/b.rs".into()],
            },
            Some(root.path()),
            false,
        );
        assert!(
            reads.overlaps(&OwnershipSet::new(["src/a.rs".to_string()]).canonicalized(root.path()))
        );
        assert!(!writes
            .overlaps(&OwnershipSet::new(["src/a.rs".to_string()]).canonicalized(root.path())));
    }

    #[test]
    fn declared_ownership_stays_relative_for_policy_gates_but_scheduler_identity_is_canonical() {
        let root = tempfile::tempdir().unwrap();
        let tool = Arc::new(Tool {
            name: "write_file".into(),
            description: "w".into(),
            input_schema: serde_json::json!({}),
            resource_class: faktor_core::resource::ResourceClass::DiskWrite,
            capability: None,
            recovery_hint: RecoveryHint::WorkspaceWrite,
            path_args: vec!["path".into()],
            execute: Arc::new(|_ctx, _args| Box::pin(async move { Ok(ToolOutcome::default()) })),
        });
        let args = serde_json::json!({"path": "./src/x/../a.rs"});
        let (_, declared) = declared_ownership_sets(&tool, &args);
        assert_eq!(
            declared.entries(),
            &["./src/x/../a.rs".to_string()],
            "the ChangeBudget vocabulary stays repository-relative and declared"
        );
        let (_, scheduled) = ownership_sets(&tool, &args, Some(root.path()));
        let (_, same_target) = ownership_sets(
            &tool,
            &serde_json::json!({"path": "src/a.rs"}),
            Some(root.path()),
        );
        assert!(
            scheduled.overlaps(&same_target),
            "scheduler identity canonicalizes alias spellings to one resource"
        );
    }
}

/// The typed kind of one refused tool call. A refusal is durable turn
/// history, never a dangling call: the tag rides the durable tool_result
/// excerpt so a denial is machine-distinguishable from an execution failure
/// without parsing prose. Tags are stable, lowercase and never renumbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolDenialKind {
    /// The model called a tool that is not registered.
    UnknownTool,
    /// The interactive permission hop denied the call.
    PermissionDenied,
    /// The semantic capability gate removed the tool's class.
    CapabilityRefused,
    /// A PreTool lifecycle hook denied the call.
    HookDenied,
    /// The secret gate detected a credential in the tool input.
    SecretDetected,
    /// The task's ChangeBudget refused the declared write.
    ChangeBudgetRefused,
    /// The model named a registered tool that is not in the active tool
    /// bundle the request it answered actually carried (a lazy tool that
    /// was never activated this session).
    NotInActiveBundle,
}

impl ToolDenialKind {
    pub(crate) fn tag(self) -> &'static str {
        match self {
            ToolDenialKind::UnknownTool => "unknown_tool",
            ToolDenialKind::PermissionDenied => "permission_denied",
            ToolDenialKind::CapabilityRefused => "capability_refused",
            ToolDenialKind::HookDenied => "hook_denied",
            ToolDenialKind::SecretDetected => "secret_detected",
            ToolDenialKind::ChangeBudgetRefused => "change_budget_refused",
            ToolDenialKind::NotInActiveBundle => "not_in_active_bundle",
        }
    }
}

/// One refused tool call of a batch: the durable call identity plus the
/// typed refusal and the refusal path's own reason text (bounded/redacted
/// only when it reaches the durable surfaces below).
#[derive(Debug, Clone)]
pub(crate) struct DeniedToolCall {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) kind: ToolDenialKind,
    pub(crate) reason: String,
}

impl DeniedToolCall {
    /// The durable tool_result excerpt: a stable typed tag + the bounded,
    /// credential-redacted reason. The model sees WHY the call was refused;
    /// the shape is machine-checkable without parsing prose.
    pub(crate) fn excerpt(&self) -> String {
        let reason = redact_secrets_bounded(&self.reason, 400);
        truncate(
            &format!("tool call denied ({}): {reason}", self.kind.tag()),
            2000,
        )
    }

    /// The bounded ledger line (TurnSummary failures -> TaskLedger
    /// known_failures + typed ledger failures), mirroring the failed-tool
    /// path.
    pub(crate) fn ledger_line(&self) -> String {
        truncate(
            &format!(
                "{} refused ({}): {}",
                self.name,
                self.kind.tag(),
                redact_secrets_bounded(&self.reason, 400)
            ),
            400,
        )
    }
}

/// Re-validate stored invocation args against the tool's input schema
/// "where feasible" (P0: a hostile descriptor is a loud error, never a blind
/// replay): object schemas with a `required` list and typed `properties` are
/// checked; anything looser passes through unchanged. The result is the
/// canonical JSON to re-execute.
pub(crate) fn validate_args_against_schema(
    tool: &Tool,
    args: &serde_json::Value,
) -> faktor_core::Result<serde_json::Value> {
    let obj = args.as_object().ok_or_else(|| {
        Error::malformed(format!(
            "tool {} invocation args must be a JSON object, found {}",
            tool.name,
            serde_json::to_string(args).unwrap_or_default()
        ))
    })?;
    let schema = &tool.input_schema;
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        for (key, prop) in props {
            let Some(expect_type) = prop.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            let Some(value) = obj.get(key) else {
                continue;
            };
            let ok = match expect_type {
                "string" => value.is_string(),
                "integer" | "number" => value.is_number(),
                "boolean" => value.is_boolean(),
                "object" => value.is_object(),
                "array" => value.is_array(),
                _ => true,
            };
            if !ok {
                return Err(Error::malformed(format!(
                    "tool {} arg `{key}` must be {expect_type}",
                    tool.name
                )));
            }
        }
    }
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for req in required {
            let Some(key) = req.as_str() else {
                continue;
            };
            if !obj.contains_key(key) {
                return Err(Error::malformed(format!(
                    "tool {} invocation is missing required arg `{key}`",
                    tool.name
                )));
            }
        }
    }
    Ok(serde_json::Value::Object(obj.clone()))
}

/// Tool versions from AVAILABLE METADATA only (no subprocess/network probe):
/// this build's agent version, the workspace-declared rust-version when the
/// compiler embedded it, and the documented `RUSTUP_TOOLCHAIN` process
/// variable when it is present and sane. Everything else is omitted — an
/// absent toolchain is never guessed.
pub(crate) fn fingerprint_tool_versions() -> Vec<ToolVersion> {
    let mut tools = vec![ToolVersion {
        tool: "faktor-agent".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }];
    if let Some(rust_version) = option_env!("CARGO_PKG_RUST_VERSION") {
        tools.push(ToolVersion {
            tool: "rust-version".to_string(),
            version: rust_version.to_string(),
        });
    }
    if let Ok(toolchain) = std::env::var("RUSTUP_TOOLCHAIN") {
        let sane = !toolchain.is_empty()
            && toolchain.len() <= faktor_core::state::MAX_ENVIRONMENT_FINGERPRINT_VERSION_BYTES
            && toolchain.bytes().all(|b| b.is_ascii_graphic());
        if sane {
            tools.push(ToolVersion {
                tool: "rustup-toolchain".to_string(),
                version: toolchain,
            });
        }
    }
    tools.truncate(faktor_core::state::MAX_ENVIRONMENT_FINGERPRINT_TOOLS);
    tools
}

/// Typed verdict of the bounded durable activation-fact scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolActivationScan {
    /// The activation fact was found inside the bound (an explicit empty
    /// set from `/source off` is still a decisive fact).
    Found,
    /// The walk reached the end of the fact table inside the bound: the
    /// absence is decisive — the session is genuinely fresh.
    Absent,
    /// The page bound was exhausted while older pages still existed, so the
    /// scan could NOT prove the session's set. The safe inactive default
    /// applies and the exhaustion is emitted as a loud typed diagnostic —
    /// never a silent revert to inactive.
    BoundExhausted,
    /// The durable read failed (already warned): the safe inactive default.
    ReadFailed,
}

/// One load of the session's durable activation set with its typed scan
/// verdict ([`ToolActivationScan`]).
#[derive(Debug)]
pub(crate) struct ToolActivationLoad {
    pub(crate) set: ToolActivationSet,
    pub(crate) scan: ToolActivationScan,
}

impl ToolActivationLoad {
    /// Whether the durable flag exists (an empty [set](Self::set) still
    /// counts: it is the explicit deactivation fact).
    pub(crate) fn found(&self) -> bool {
        self.scan == ToolActivationScan::Found
    }
}

impl AgentRuntime {
    /// Crash-resume transcript integrity: every assistant tool_call part the
    /// wire carries needs an answering tool_result part, or the next request
    /// is protocol-invalid (real providers reject a call with no result, and
    /// a model may hallucinate an outcome). The live batch paths answer
    /// every call they resolve — including refusals — so with NO open run
    /// rows an unanswered call can only be crash residue: a refusal whose
    /// typed result write was lost, or a call the crashed driver never
    /// resolved. This repair walks the durable transcript NEWEST-FIRST and
    /// stops at the first message that carries no tool part at all (the
    /// turn's tool cluster is contiguous at the tail; the walk is additionally
    /// capped), appending ONE typed `interrupted` result per unanswered call.
    /// Calls already answered are skipped, so re-running after a repair is a
    /// no-op (idempotent). Run rows MUST be empty: a deferred replay answers
    /// its own call, and a second result for the same call would itself be
    /// protocol-invalid.
    pub(crate) fn answer_dangling_tool_calls(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> faktor_core::Result<usize> {
        const MAX_SCAN: usize = 128;
        const MAX_REPAIRS: usize = 32;
        let mut calls: Vec<(String, String)> = Vec::new();
        let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cursor: Option<i64> = None;
        let mut scanned = 0usize;
        'scan: loop {
            let page = handle.messages_before(cursor, 100)?;
            if page.is_empty() {
                break;
            }
            // The page is newest-first: the walk stays newest-first so the
            // tool-cluster stop is exact (nothing older than the first
            // non-tool message can belong to this turn's tail).
            for row in page.iter() {
                if scanned >= MAX_SCAN {
                    break 'scan;
                }
                scanned += 1;
                let mut saw_tool_part = false;
                for part in handle.parts_of(row.id)? {
                    let Some(call_id) = part
                        .data
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .filter(|id| !id.is_empty())
                    else {
                        continue;
                    };
                    match part.kind.as_str() {
                        // Only the call states the wire carries (the history
                        // reconstruction skips pending/partial calls) can be
                        // dangling on the wire.
                        "tool_call"
                            if matches!(
                                part.data.get("state").and_then(|v| v.as_str()),
                                Some("completed") | Some("error")
                            ) =>
                        {
                            let name = part
                                .data
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            calls.push((call_id.to_string(), name));
                            saw_tool_part = true;
                        }
                        "tool_call" => saw_tool_part = true,
                        "tool_result" => {
                            answered.insert(call_id.to_string());
                            saw_tool_part = true;
                        }
                        _ => {}
                    }
                }
                // The turn's tool cluster ends here: nothing older can be
                // dangling for THIS turn (the walk is bounded regardless).
                if !saw_tool_part {
                    break 'scan;
                }
            }
            let Some(oldest) = page.last() else { break };
            if scanned >= MAX_SCAN || oldest.seq <= 1 {
                break;
            }
            cursor = Some(oldest.seq);
        }
        // Collected newest-first (the walk order); repair in durable order.
        calls.reverse();
        let mut repaired = 0usize;
        for (call_id, name) in calls {
            if answered.contains(&call_id) {
                continue;
            }
            if repaired >= MAX_REPAIRS {
                tracing::error!(
                    session = %handle.id(),
                    bound = MAX_REPAIRS,
                    "dangling tool-call repair hit its bound; the remaining calls are answered at a later open"
                );
                break;
            }
            let seq = handle.proposed_message_seq()?;
            let mid = handle.put_message(seq, "assistant", serde_json::json!({ "parts": [] }))?;
            let body = ToolResultBody {
                excerpt: interrupted_tool_excerpt(&name),
                exit_code: Some(1),
                artifact: None,
                slice_hint: None,
            };
            handle.put_tool_result_part(mid, &call_id, &body)?;
            repaired += 1;
        }
        if repaired > 0 {
            tracing::warn!(
                session = %handle.id(),
                repaired,
                "answered dangling tool calls left by an interrupted turn (protocol-valid transcript)"
            );
        }
        Ok(repaired)
    }

    /// Validate a stored replay invocation. A hostile descriptor (missing
    /// fields, unknown tool, args that do not satisfy the tool's input
    /// schema where feasible) is a loud error — recovery NEVER blind-replays.
    pub(crate) fn validate_replay_descriptor(
        &self,
        row: &ToolRunRow,
        raw: &serde_json::Value,
    ) -> faktor_core::Result<ReplayDescriptor> {
        let desc: ReplayDescriptor = serde_json::from_value(raw.clone()).map_err(|e| {
            Error::malformed(format!(
                "tool_run {} carries a hostile replay descriptor: {e}",
                row.op_id
            ))
        })?;
        if desc.tool_name != row.tool {
            return Err(Error::malformed(format!(
                "tool_run {} replay descriptor names tool {:?}, row says {:?}",
                row.op_id, desc.tool_name, row.tool
            )));
        }
        if desc.recovery_kind != "idempotent" {
            return Err(Error::malformed(format!(
                "tool_run {} replay descriptor declares unsupported recovery kind {:?}",
                row.op_id, desc.recovery_kind
            )));
        }
        let tool = self.deps.tools.get(&desc.tool_name).ok_or_else(|| {
            Error::malformed(format!(
                "tool_run {} cannot replay: tool {:?} is not registered",
                row.op_id, desc.tool_name
            ))
        })?;
        validate_args_against_schema(&tool, &desc.validated_args)?;
        Ok(desc)
    }

    /// Replay ONE deferred idempotent run: a NEW PHYSICAL attempt of the
    /// SAME logical operation. Journals ReplayStarted exactly once, bumps the
    /// attempt counter on the run row, re-executes the stored invocation
    /// ONCE with the previously-granted permission, links the outcome to the
    /// original tool call, and finishes the row (ToolCompleted). The turn
    /// identity (record + op ids) is untouched.
    pub(crate) async fn replay_tool_run(
        &self,
        handle: &faktor_session::SessionHandle,
        row: &ToolRunRow,
    ) -> faktor_core::Result<()> {
        let raw = row.replay_descriptor.as_ref().ok_or_else(|| {
            Error::malformed(format!("tool_run {} has no replay descriptor", row.op_id))
        })?;
        let desc = self.validate_replay_descriptor(row, raw)?;
        let tool = self
            .deps
            .tools
            .get(&desc.tool_name)
            .ok_or_else(|| Error::not_found(format!("tool {}", desc.tool_name)))?;
        let state = handle.state()?;
        if state != AgentState::ExecutingTool {
            return Err(Error::conflict(format!(
                "replay of {} requires the machine at ExecutingTool, found {state:?}",
                row.op_id
            )));
        }
        // Journal the replay start (self-transition, exactly once per run).
        handle
            .append_journal_event(
                faktor_core::event::EventKind::ReplayStarted,
                state,
                Some(row.op_id),
                Some(serde_json::json!({
                    "tool": row.tool,
                    "attempt": row.attempt + 1,
                    "turn_op_id": desc.original_turn_op_id.raw(),
                })),
            )
            .await?;
        let attempt = handle.bump_tool_attempt(row.op_id)?;
        // Reconstruct the original invocation context (the permission hop
        // was already resolved pre-crash; a replay is its continuation).
        // P0-48: the session's effective root re-points at the live shadow
        // when the crashed drive was shadowed — the replay re-executes
        // exactly where the original run executed.
        let root = self.deps.session.resolve_workspace_root(handle.id())?;
        let workspace = match &root {
            Some(root) => self
                .deps
                .workspaces
                .open(desc.workspace_id, root.clone())
                .ok()
                .map(Arc::new),
            None => None,
        };
        let sandbox = match (&self.deps.sandbox, &root) {
            (Some(base), Some(root)) => Some(Arc::new(faktor_sandbox::PermissionEngine::new(
                base.policy().clone(),
                Some(root.clone()),
            ))),
            _ => None,
        };
        let ctx = ToolRunCtx {
            session_id: handle.id(),
            permission_granted: true,
            op_id: row.op_id,
            identity: WorkspaceIdentity::new(desc.workspace_id, desc.worktree_id, desc.task_id),
            cancellation: CancellationToken::new(),
            artifacts: Arc::new(self.deps.artifact_sink(handle.id())),
            tool_call_mode: self.deps.tool_call_mode,
            workspace: workspace.clone(),
            edit: self.deps.edit.clone(),
            snapshots: self.deps.snapshots.clone(),
            sandbox: sandbox.clone(),
            supervisor: self.deps.supervisor.clone(),
            deadline_ms: self.deps.tool_deadline_ms,
        };
        let mut outcome = match (tool.execute)(ctx, desc.validated_args.clone()).await {
            Ok(o) => o,
            Err(e) => {
                // The replay itself failed: honest completion of the attempt.
                handle.finish_tool_run(row.op_id, "failed", EffectStatus::Unknown)?;
                return Err(e);
            }
        };
        // Redact echoed credentials on the replay path too: the replayed
        // result journals the same durable tool-result message the live
        // path does, and must obey the same sanitization.
        self.sanitize_outcome_text(&mut outcome);
        if let Some(pc) = &outcome.postcondition {
            let v = serde_json::to_value(pc)
                .map_err(|e| Error::malformed(format!("postcondition serialization: {e}")))?;
            handle.record_tool_postcondition(row.op_id, &v)?;
        }
        // Link the outcome to the ORIGINAL tool call (never a duplicate
        // message): the model sees exactly one result for the call.
        let call_id = self.find_original_call_id(handle, &row.tool, &row.args)?;
        let seq = handle.proposed_message_seq()?;
        let mid = handle
            .append_message(seq, "assistant", serde_json::json!({ "parts": [] }))
            .await?;
        let body = ToolResultBody {
            excerpt: self.redact_configured_secrets(&truncate(&outcome.text, 2000)),
            exit_code: outcome.exit_code,
            artifact: outcome.artifact,
            slice_hint: outcome.slice_hint,
        };
        handle.append_tool_result_part(mid, &call_id, &body).await?;
        handle.finish_tool_run(row.op_id, "completed", outcome.effect_status)?;
        tracing::info!(
            session = %handle.id(),
            op = %row.op_id,
            tool = %row.tool,
            attempt,
            "replayed interrupted idempotent tool run"
        );
        Ok(())
    }

    /// The tool_call part id the ORIGINAL run answered (name + args match):
    /// replayed results must reference it or the model sees an orphan.
    pub(crate) fn find_original_call_id(
        &self,
        handle: &faktor_session::SessionHandle,
        tool: &str,
        args: &serde_json::Value,
    ) -> faktor_core::Result<String> {
        const MAX_SCAN: usize = 400;
        let mut cursor: Option<i64> = None;
        let mut scanned = 0usize;
        loop {
            let page = handle.messages_before(cursor, 100)?;
            if page.is_empty() {
                break;
            }
            for row in page.iter() {
                if scanned >= MAX_SCAN {
                    break;
                }
                scanned += 1;
                for part in handle.parts_of(row.id)? {
                    if part.kind == "tool_call"
                        && part.data.get("name").and_then(|n| n.as_str()) == Some(tool)
                        && part.data.get("input") == Some(args)
                    {
                        if let Some(id) = part
                            .data
                            .get("tool_call_id")
                            .and_then(|i| i.as_str())
                            .filter(|i| !i.is_empty())
                        {
                            return Ok(id.to_string());
                        }
                    }
                }
            }
            if scanned >= MAX_SCAN || page.last().unwrap().seq <= 1 {
                break;
            }
            cursor = Some(page.last().unwrap().seq);
        }
        Err(Error::malformed(format!(
            "replay of {tool} cannot find its original tool call in the journal"
        )))
    }

    /// Interior hop from the state ONE tool batch left behind back to
    /// `WaitingForModel`, using ONLY the session machine's legal edges.
    ///
    /// The legal tool-batch interior (and its one missing edge):
    ///
    /// ```text
    /// batch outcome                      legal interior hops
    /// ---------------------------------  ----------------------------------------
    /// every tool completed               Validating -> UpdatingMemory
    ///                                    UpdatingMemory -> WaitingForModel
    ///
    /// mixed: >=1 completed AND           Validating -> FailedRecoverable
    /// >=1 failed recoverably             (the failed finishes; completed
    ///                                    finishes resolve FIRST — the
    ///                                    reverse order is illegal)
    ///                                    FailedRecoverable -> Preparing
    ///                                    (the explicit retry/re-plan hop:
    ///                                    the ONLY forward edge out of a
    ///                                    recoverable failure)
    ///                                    Preparing -> BuildingContext
    ///                                    BuildingContext -> WaitingForModel
    ///
    /// every submitted tool failed        no hop: the turn's classified end
    ///                                    is FailedRecoverable itself
    /// ```
    ///
    /// `FailedRecoverable -> Validating`/`UpdatingMemory` are deliberately
    /// absent from [`AgentState::allowed_transitions`]: a recoverable
    /// failure may only re-enter the turn through its preparation entry,
    /// never by pretending the failure did not happen. The failure stays a
    /// per-tool record and the turn continues with it visible to the model.
    pub(crate) async fn walk_tool_batch_to_waiting(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
    ) -> faktor_core::Result<()> {
        let targets: &[AgentState] = match handle.state()? {
            AgentState::FailedRecoverable => &[
                AgentState::Preparing,
                AgentState::BuildingContext,
                AgentState::WaitingForModel,
            ],
            _ => &[AgentState::UpdatingMemory, AgentState::WaitingForModel],
        };
        for target in targets {
            handle
                .append_journal_event(
                    faktor_core::event::EventKind::PhaseChanged,
                    *target,
                    Some(op_id),
                    None,
                )
                .await?;
        }
        Ok(())
    }

    /// Execute tool calls in parallel via the scheduler, feeding results
    /// back. Returns the number of tools actually executed. Every call must
    /// be a MEMBER of `active_bundle` — the exact bundle the answered
    /// request was planned with — or it is refused typed before the
    /// permission hop (see the membership guard below).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_tool_calls(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        active_bundle: &ToolBundle,
        turn_op: OpId,
        detector: &mut LoopDetector,
        ledger: &mut TaskLedger,
        turn_summary: &mut faktor_context::ledger::TurnSummary,
        cancel: &CancellationToken,
        semantic: Option<&SemanticTurnState>,
        calls: Vec<(String, String, serde_json::Value)>,
    ) -> faktor_core::Result<usize> {
        // Resolve the session's workspace ONCE per batch (P0-48 root
        // re-pointing: a live shadow of the session re-points the tools at
        // the shadow root; un-shadowed sessions keep the stored workspace
        // root byte-identically). The real tools (read/write/search/
        // run_command) operate inside the canonical root with a per-session
        // permission engine, never on model-supplied absolute paths. When
        // the session has no resolvable workspace the ctx carries None and
        // the tools error honestly.
        let row = handle.row()?;
        let workspace_id = row.workspace_id;
        let root = self.deps.session.resolve_workspace_root(handle.id())?;
        let workspace = match &root {
            Some(root) => self
                .deps
                .workspaces
                .open(workspace_id, root.clone())
                .ok()
                .map(Arc::new),
            None => None,
        };
        let sandbox = match (&self.deps.sandbox, &root) {
            (Some(base), Some(root)) => Some(Arc::new(faktor_sandbox::PermissionEngine::new(
                base.policy().clone(),
                Some(root.clone()),
            ))),
            _ => None,
        };
        let now_ms = self.deps.clock.now_ms();
        // Change-scope budget (audits 57/105), read ONCE per batch: the edit
        // gate below refuses a mutating tool whose declared write paths leave
        // the task's ChangeBudget BEFORE anything executes. No budget = today's
        // behavior (parity).
        let change_budget = handle.change_budget()?;

        let mut executed = 0usize;
        // Semantic risk reduces tool-batch parallelism (audit 54): None (no
        // provider consulted) keeps the scheduler defaults byte-identically;
        // High/Unknown serializes the batch's resource classes.
        let scheduler = Scheduler::new(handle.id(), self.deps.clock.clone())
            .with_limits(risk_adjusted_resource_limits(semantic.map(|s| s.level)));
        let outcomes: Arc<std::sync::Mutex<HashMap<OpId, ToolOutcome>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        // P0-2 remainder: the actual change set of every generic-shell
        // invocation, filled by the pre/post manifest diff around execution
        // and consumed by the SAME settlement path write_file feeds.
        let shell_changes: Arc<std::sync::Mutex<HashMap<OpId, ShellChangeSet>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let mut submitted: Vec<(OpId, String, String, serde_json::Value)> = Vec::new();
        // Refused calls, with their durable call identity and typed kind:
        // each one is answered by a tool_result part below so the transcript
        // the next wire request is built from never carries a dangling call.
        let mut denied: Vec<DeniedToolCall> = Vec::new();

        for (call_id, name, input) in calls {
            // Loop detection on the call itself (normalized).
            if detector.record_tool_call(&name, &input) {
                return Ok(executed); // drive_turn stops the turn
            }

            let tool = match self.deps.tools.get(&name) {
                Some(t) => t,
                None => {
                    let reason = format!("unknown tool: {name}");
                    detector.record_error(&format!("unknown tool {name}"));
                    // Uniform journal outcome: every other refusal path
                    // journals PermissionDenied, so the unknown-tool refusal
                    // is durable audit too — never a silent skip (the
                    // self-transition is legal from the batch-entry state).
                    handle
                        .append_journal_event(
                            faktor_core::event::EventKind::PermissionDenied,
                            handle.state()?,
                            Some(turn_op),
                            Some(serde_json::json!({ "tool": name, "reason": reason })),
                        )
                        .await?;
                    denied.push(DeniedToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        kind: ToolDenialKind::UnknownTool,
                        reason,
                    });
                    continue;
                }
            };

            // Active-bundle membership guard (docs/acquire.md §4 hardening):
            // the model may only invoke tools the request it is answering
            // actually carried. A REGISTERED tool that is not in the turn's
            // constructed bundle — today the lazy `source_market` before its
            // activation — is refused here with a typed denial BEFORE the
            // permission hop and before any execution, so no permission
            // prompt, no run row and no tool/service call can happen. The
            // refusal reason names ONLY the tool the model itself used:
            // schema and description never leak.
            if !active_bundle.tools.iter().any(|spec| spec.name == name) {
                let reason =
                    format!("tool {name} is not part of the active tool bundle for this turn");
                detector.record_error(&format!("inactive tool {name} refused"));
                // Uniform journal outcome, exactly like the unknown-tool
                // refusal: every refusal is durable audit, never a silent
                // skip (the self-transition is legal from the batch-entry
                // state).
                handle
                    .append_journal_event(
                        faktor_core::event::EventKind::PermissionDenied,
                        handle.state()?,
                        Some(turn_op),
                        Some(serde_json::json!({ "tool": name, "reason": reason })),
                    )
                    .await?;
                denied.push(DeniedToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    kind: ToolDenialKind::NotInActiveBundle,
                    reason,
                });
                continue;
            }

            // Permission hop (journals ToolRequested).
            let capability = tool.capability.clone().unwrap_or(Capability::ExecuteShell {
                command: name.clone(),
            });
            let permission = handle.request_permission(turn_op, &capability)?;
            let decision = self
                .deps
                .permission_requester
                .request(handle.id(), &permission)
                .await?;
            match &decision {
                PermissionDecision::Deny => {
                    handle.resolve_permission(permission.id, PermissionDecision::Deny)?;
                    denied.push(DeniedToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        kind: ToolDenialKind::PermissionDenied,
                        reason: format!("permission denied: {name}"),
                    });
                    continue;
                }
                PermissionDecision::Ask => {
                    return Err(Error::new(
                        ErrorKind::Permission,
                        format!("permission {name} unresolved"),
                    ));
                }
                PermissionDecision::Allow => {
                    handle.resolve_permission(permission.id, PermissionDecision::Allow)?;
                }
            }
            // The tool gate may proceed past Ask-policy verdicts because the
            // interactive hop resolved above.
            let granted = matches!(decision, PermissionDecision::Allow);
            // Semantic capability gate (audit 58): provider DATA can only
            // ever NARROW the session's sandbox envelope. A tool whose
            // capability class falls outside the intersected set is refused
            // like a policy denial — journaled, counted, never executed.
            // The permission hop above has already moved the session into
            // ExecutingTool, so the denial journal is state-legal. Runs only
            // when the provider actually restricts a class (restrictions ==
            // ALL keeps today's sandbox/permission flow byte-identically).
            if let Some(state) = semantic {
                if state.restrictions != faktor_core::CapabilitySet::ALL {
                    let envelope = sandbox_capability_envelope(
                        self.deps.sandbox.as_deref().map(|engine| engine.policy()),
                    );
                    let effective = effective_capabilities(
                        envelope,
                        faktor_core::CapabilitySet::ALL,
                        state.restrictions,
                    );
                    let kind = capability_kind_of(&capability);
                    if !effective.contains(kind) {
                        let reason = if !state.restrictions.contains(kind) {
                            format!(
                                "semantic restriction removed capability '{}'",
                                kind.as_str()
                            )
                        } else {
                            format!("sandbox envelope denies capability '{}'", kind.as_str())
                        };
                        tracing::warn!(tool = %name, "capability gate denied the tool: {reason}");
                        handle
                            .append_journal_event(
                                faktor_core::event::EventKind::PermissionDenied,
                                AgentState::ExecutingTool,
                                Some(turn_op),
                                Some(serde_json::json!({ "tool": name, "reason": reason })),
                            )
                            .await?;
                        denied.push(DeniedToolCall {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            kind: ToolDenialKind::CapabilityRefused,
                            reason: format!("tool {name} denied: {reason}"),
                        });
                        continue;
                    }
                }
            }
            // Lifecycle hook gate (audit): PreTool hooks may deny the call
            // before anything executes; a deny is journaled and the tool is
            // skipped like a permission denial (never silently swallowed).
            if let Some(hooks) = &self.deps.hooks {
                let input = faktor_hooks::HookInput {
                    event: faktor_hooks::HookEvent::PreTool,
                    session_id: Some(handle.id().to_string()),
                    operation_id: Some(turn_op.to_string()),
                    // P1: the anchored session workspace, never the daemon cwd.
                    workspace_root: root.as_deref().map(std::path::Path::to_path_buf),
                    payload: serde_json::json!({ "tool": name, "args": input }),
                    ..Default::default()
                };
                if let faktor_hooks::HookVerdict::Deny { reason } =
                    hooks.run(faktor_hooks::HookEvent::PreTool, &input)
                {
                    tracing::warn!("hook denied tool {name}: {reason}");
                    handle
                        .append_journal_event(
                            faktor_core::event::EventKind::PermissionDenied,
                            AgentState::ExecutingTool,
                            Some(turn_op),
                            Some(serde_json::json!({ "tool": name, "reason": reason })),
                        )
                        .await?;
                    denied.push(DeniedToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        kind: ToolDenialKind::HookDenied,
                        reason: format!("tool {name} denied by hook: {reason}"),
                    });
                    continue;
                }
            }

            // Secret gate (audit round 16 + P0-37): scan the tool's
            // serialized input under the default SecretPolicy BEFORE
            // anything may execute. A detected credential DENIES the call —
            // journaled PermissionDenied, counted as a denial, never
            // executed. The whole-payload scanner inspects every byte
            // (streaming overlap window, no total-input cap by default);
            // only an explicit policy maximum can yield
            // TooLargeForPolicy (fail-closed).
            let secret_hits = faktor_security::scan_secrets(
                &serde_json::to_string(&input).unwrap_or_default(),
                &faktor_security::SecretPolicy::default(),
            );
            if let Some(hit) = secret_hits.first() {
                let reason = format!("secret detected in tool input ({})", hit.kind);
                tracing::warn!(tool = %name, kind = %hit.kind, "secret detected in tool input");
                handle
                    .append_journal_event(
                        faktor_core::event::EventKind::PermissionDenied,
                        AgentState::ExecutingTool,
                        Some(turn_op),
                        Some(serde_json::json!({ "tool": name, "reason": reason })),
                    )
                    .await?;
                denied.push(DeniedToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    kind: ToolDenialKind::SecretDetected,
                    reason: format!("tool {name} denied: {reason}"),
                });
                continue;
            }

            // ChangeBudget edit gate (audits 57/105): a mutating tool whose
            // DECLARED write paths leave the task's change budget is refused
            // before anything executes — journaled like a permission/hook/
            // secret denial, counted as a denial, never executed. Enforced
            // only when a budget exists; semantic fields are Unknown here
            // (the tool reports no semantic entities) and a budget that
            // constrains them therefore takes the documented stronger-
            // verification refusal path. This is the edit/steer gate: a
            // steered run's edits pass through the same tool batch.
            let shell_capability = matches!(
                tool.capability,
                Some(faktor_core::capability::Capability::ExecuteShell { .. })
            );
            if tool.resource_class == faktor_core::resource::ResourceClass::DiskWrite
                || shell_capability
            {
                if let Some(budget) = &change_budget {
                    let (_reads, writes) = declared_ownership_sets(&tool, &input);
                    let observations = faktor_session::budget::ChangeObservations {
                        changed_paths: writes.entries().to_vec(),
                        ..Default::default()
                    };
                    if let Err(violations) =
                        faktor_session::budget::check_change_budget(budget, &observations)
                    {
                        let reason = format!("change budget refused the edit: {violations:?}");
                        tracing::warn!(tool = %name, "change budget refused the edit: {violations:?}");
                        handle
                            .append_journal_event(
                                faktor_core::event::EventKind::PermissionDenied,
                                AgentState::ExecutingTool,
                                Some(turn_op),
                                Some(serde_json::json!({ "tool": name, "reason": reason })),
                            )
                            .await?;
                        denied.push(DeniedToolCall {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            kind: ToolDenialKind::ChangeBudgetRefused,
                            reason: format!("tool {name} denied: {reason}"),
                        });
                        continue;
                    }
                }
            }

            // Op envelope: deadline, retry, cancellation, recovery.
            // The recovery strategy NEVER infers file postconditions from
            // JSON content args (P0): workspace writes record their own
            // FilePostcondition (bytes as written) at execution end and
            // recovery verifies through the workspace file service; until
            // then an interrupted write is an unknown effect.
            let op_id = self.deps.session.try_next_op_id()?;
            // The session ROW is the single source of the worktree/task
            // identity (v8): a standalone session row defaults to 1/1
            // (documented), an adopted row carries the real ids — the
            // descriptor and the execution ctx below both ride them, so a
            // crash replay resumes with the SAME identity that ran before.
            let identity = WorkspaceIdentity::new(row.workspace_id, row.worktree_id, row.task_id);
            let recovery = match &tool.recovery_hint {
                RecoveryHint::WorkspaceWrite => RecoveryStrategy::MarkUnknown,
                RecoveryHint::Idempotent => RecoveryStrategy::Idempotent,
                RecoveryHint::UnknownEffect => RecoveryStrategy::MarkUnknown,
            };
            let replay = if recovery == RecoveryStrategy::Idempotent {
                // Durable replay descriptor: the stored invocation recovery
                // may re-execute ONCE. Args ride as canonical JSON (serde's
                // Map is key-sorted); re-validation against the tool's input
                // schema happens on the recovery path — a hostile descriptor
                // is a loud error, never a blind replay.
                let desc = ReplayDescriptor {
                    tool_name: name.clone(),
                    validated_args: input.clone(),
                    workspace_id: identity.workspace_id,
                    worktree_id: identity.worktree_id,
                    task_id: identity.task_id,
                    original_turn_op_id: turn_op,
                    capability: capability.clone(),
                    recovery_kind: "idempotent".into(),
                };
                serde_json::to_value(desc)
                    .map_err(|e| Error::malformed(format!("replay descriptor: {e}")))?
            } else {
                serde_json::Value::Null
            };
            let op_meta = OpMeta::new(
                op_id,
                handle.id(),
                faktor_core::time::Deadline::at(
                    self.deps
                        .clock
                        .now_ms()
                        .saturating_add(self.deps.tool_deadline_ms as i64),
                ),
                faktor_core::retry::RetryPolicy {
                    max_attempts: 1, // tools are never blindly retried
                    ..Default::default()
                },
                cancel.child(),
                recovery,
                self.deps.clock.now_ms(),
            );
            let op_meta = if replay.is_null() {
                op_meta
            } else {
                op_meta.with_replay(replay)
            };
            let run_handle = handle.start_tool_run(op_meta.clone(), &name, input.clone())?;
            let _ = run_handle;

            // Scheduler task for this tool; the OpMeta envelope (deadline,
            // retry, cancellation, recovery) is passed straight through.
            let ctx = ToolRunCtx {
                session_id: handle.id(),
                permission_granted: granted,
                op_id,
                identity,
                cancellation: op_meta.cancellation.clone(),
                artifacts: Arc::new(self.deps.artifact_sink(handle.id())),
                tool_call_mode: self.deps.tool_call_mode,
                workspace: workspace.clone(),
                edit: self.deps.edit.clone(),
                snapshots: self.deps.snapshots.clone(),
                sandbox: sandbox.clone(),
                supervisor: self.deps.supervisor.clone(),
                deadline_ms: op_meta.deadline.at_ms().saturating_sub(now_ms).max(1) as u64,
            };
            let tool_arc = tool.clone();
            let outcomes = outcomes.clone();
            let shell_changes_for_run = shell_changes.clone();
            let handle_for_run = handle.clone();
            let runtime_for_run = self.clone();
            let args = input.clone();
            let shell_run = matches!(
                tool.capability,
                Some(faktor_core::capability::Capability::ExecuteShell { .. })
            ) && !shell_read_only(&input);
            let (reads, writes) = ownership_sets(&tool, &input, root.as_deref());
            let spec = ScheduledOp {
                meta: op_meta.clone(),
                resources: ResourceRequest {
                    class: tool.resource_class,
                },
                reads,
                writes,
                // Parallel tool batches are independent by design: no tool
                // call in a batch depends on another, so there are no edges.
                // If chains are ever built here, default edges are Success
                // (a dependent runs only after its upstream completed).
                dependencies: vec![],
                run: Arc::new(move || {
                    let tool = tool_arc.clone();
                    let ctx = ctx.clone();
                    let args = args.clone();
                    let outcomes = outcomes.clone();
                    let shell_changes = shell_changes_for_run.clone();
                    let handle = handle_for_run.clone();
                    let runtime = runtime_for_run.clone();
                    Box::pin(async move {
                        // P0-2 remainder: a generic shell executes under the
                        // bounded pre/post workspace-manifest accounting; the
                        // actual change set rides the SAME settlement path
                        // write_file feeds.
                        let outcome = if shell_run {
                            runtime
                                .execute_shell_with_manifest(
                                    &handle,
                                    tool.clone(),
                                    ctx,
                                    args,
                                    turn_op,
                                    &shell_changes,
                                )
                                .await?
                        } else {
                            (tool.execute)(ctx, args).await?
                        };
                        outcomes.lock().unwrap().insert(op_id, outcome);
                        Ok(())
                    })
                }),
            };
            submitted.push((op_id, name.clone(), call_id.clone(), input.clone()));
            // Registration failure must be loud: a lost tool call is a lost
            // effect (P0-17). Duplicates cannot happen (fresh op ids);
            // anything else aborts the batch instead of silently dropping.
            scheduler
                .try_submit(spec)
                .map_err(|e| Error::internal(format!("tool schedule {op_id}: {e}")))?;
        }

        // The scheduler's terminal result is FALLIBLE (P1 error-collapse):
        // a spawned task panic (`ErrorKind::Internal`), a deadlock/starvation
        // classification, a validation refusal or a resource-graph failure
        // must NEVER collapse into an empty done set and ordinary per-tool
        // failures. The live scheduler statuses are inspected so genuinely
        // completed operations stay recorded as completed, every still-open
        // row is resolved honestly with the scheduler failure, and the
        // ORIGINAL typed error is returned after that resolution.
        let scheduler_result = self.run_scheduled_batch(&scheduler).await;
        let scheduler_failure = scheduler_result.as_ref().err().cloned();
        let done: std::collections::HashSet<OpId> = match &scheduler_result {
            Ok(ids) => ids.iter().copied().collect(),
            Err(_) => scheduler
                .statuses()
                .into_iter()
                .filter(|(_, status)| *status == faktor_scheduler::TaskStatus::Done)
                .map(|(id, _)| id)
                .collect(),
        };

        // Two passes over the done set: ALL FileChanged notifications while
        // the machine is still ExecutingTool, THEN all finishes (each finish
        // moves the machine toward Validating — interleaving them with the
        // appends would journal FileChanged from Validating, an illegal
        // transition when a batch contains more than one tool).
        for (op_id, name, _call_id, _input) in submitted.iter() {
            if done.contains(op_id) {
                // P0-2 remainder: a generic shell's ACTUAL change set (from
                // the bounded pre/post manifest diff) rides the durable
                // FileChanged payload — the real paths, never an input-arg
                // inference. Non-shell tools keep the byte-identical payload.
                let mut payload = serde_json::json!({ "tool": name, "effect": "applied" });
                if let Some(set) = shell_changes.lock().unwrap().get(op_id).cloned() {
                    let (rows, list_truncated) = set.durable_changes();
                    let truncated = set.truncated.clone().or_else(|| {
                        list_truncated.then(|| {
                            format!(
                                "change list truncated at {SHELL_CHANGE_DURABLE_MAX} durable rows"
                            )
                        })
                    });
                    payload["changes"] = serde_json::json!(rows);
                    payload["changed"] = serde_json::json!(set.changes.len());
                    payload["changes_truncated"] = serde_json::json!(truncated);
                    payload["before_digest"] = serde_json::json!(set.before_digest);
                    payload["after_digest"] = serde_json::json!(set.after_digest);
                }
                handle
                    .append_journal_event(
                        faktor_core::event::EventKind::FileChanged,
                        AgentState::ExecutingTool,
                        Some(*op_id),
                        Some(payload),
                    )
                    .await?;
            }
        }
        // Resolution ORDER is load-bearing for the session state machine:
        // every `completed` finish moves the machine toward `Validating`,
        // every failed/abandoned finish moves it to `FailedRecoverable`, and
        // `FailedRecoverable -> Validating` is ILLEGAL. Resolving strictly in
        // submit order could therefore let a genuinely completed op (one
        // submitted after a failing one) fail its transition and MASK the
        // batch outcome — including the original typed scheduler error. So
        // every COMPLETED op is resolved first (each is a legal
        // self-transition on `Validating`), then every failed/abandoned op
        // (a legal self-transition on `FailedRecoverable` after the first).
        let mut abandoned: Vec<(OpId, String, String)> = Vec::new();
        for (op_id, name, call_id, input) in submitted {
            if done.contains(&op_id) {
                let mut outcome =
                    outcomes
                        .lock()
                        .unwrap()
                        .remove(&op_id)
                        .unwrap_or_else(|| ToolOutcome {
                            text: "(no output)".into(),
                            exit_code: None,
                            ..Default::default()
                        });
                // Redact any credential the tool echoed BEFORE the text
                // reaches the durable result message or the turn summary.
                self.sanitize_outcome_text(&mut outcome);
                // Workspace writes record their FilePostcondition (bytes as
                // written) on the run row BEFORE the finish: a crash in the
                // window between write and finish is then verified against
                // the REAL expected state, never a JSON-args inference.
                if let Some(pc) = &outcome.postcondition {
                    let v = serde_json::to_value(pc).map_err(|e| {
                        Error::malformed(format!("postcondition serialization: {e}"))
                    })?;
                    handle.record_tool_postcondition(op_id, &v)?;
                }
                handle.finish_tool_run(op_id, "completed", outcome.effect_status)?;
                // PostTool hook (audit): the ToolOutcome exists and the run
                // row is durably finished — the site between the finish and
                // the summary fold. Best-effort: a Deny AFTER execution is
                // audit-only and never fails the turn.
                self.run_hook_best_effort(
                    faktor_hooks::HookEvent::PostTool,
                    handle.id(),
                    Some(op_id),
                    serde_json::json!({ "tool": name, "exit_code": outcome.exit_code }),
                );
                collect_tool_summary(turn_summary, &name, &input, &outcome);
                // P0-2 remainder: the generic shell's actual change set feeds
                // the SAME accounting path write_file uses — checkpoint rows
                // through the session's CheckpointStore, the turn summary's
                // changed files (verification/review inputs), the durable
                // settled envelope and the per-path progress digests. Runs
                // AFTER the run row is finished so the reconciled envelope
                // (which may update a terminal row) is durable.
                if let Some(set) = shell_changes.lock().unwrap().remove(&op_id) {
                    if let Some(ws) = &workspace {
                        self.settle_shell_changes(handle, op_id, turn_op, &set, turn_summary, ws);
                    } else {
                        // No resolvable workspace at settlement: the captured
                        // fact stays unresolved, so the completion gate
                        // refuses certification (never "unchanged").
                        tracing::error!(
                            session = %handle.id(),
                            op = %op_id,
                            "shell change set could not be settled (no workspace); the turn's gate will refuse certification"
                        );
                    }
                }
                let seq = handle.proposed_message_seq()?;
                let mid = handle
                    .append_message(seq, "assistant", serde_json::json!({ "parts": [] }))
                    .await?;
                // Large outputs go through THE durable evidence store (CCR):
                // a bounded compact reference is prepended while the
                // normalized backing stays retrievable through the
                // authority. Small outputs keep today's byte-identical body.
                let excerpt = match self.archive_tool_output(
                    handle,
                    handle.task_id().unwrap_or_else(|_| TaskId::new(1)),
                    &name,
                    outcome.provenance,
                    &outcome.text,
                ) {
                    Some(reference) => {
                        format!("{reference}\n{}", truncate(&outcome.text, 2000))
                    }
                    None => truncate(&outcome.text, 2000),
                };
                // Durable message-part boundary: the configured-secret
                // registry redaction is applied to the EXACT bytes that
                // enter the transcript (belt-and-braces on top of
                // `sanitize_outcome_text`, so no future path can bypass it).
                let excerpt = self.redact_configured_secrets(&excerpt);
                let body = ToolResultBody {
                    excerpt,
                    exit_code: outcome.exit_code,
                    artifact: outcome.artifact,
                    slice_hint: outcome.slice_hint,
                };
                handle.append_tool_result_part(mid, &call_id, &body).await?;
                executed += 1;
                // A completed tool is progress evidence (stall vs progress).
                self.progress_heartbeat(handle.id());
                // P0-79 site c/e: a tool outcome that mutated a file with a
                // NEW digest is RepoStateChanged evidence (bounded per-path
                // digest LRU on the session tracker — an identical rewrite
                // is byte-identical, never progress). Write digests come
                // from the tool's recorded postcondition (the bytes as
                // written), never from JSON args.
                let mut repo_moved = false;
                if let Some(pc) = &outcome.postcondition {
                    repo_moved =
                        self.progress_repo_digest(handle.id(), &pc.relative_path, pc.expected_hash);
                }
                // P0-78 (b): evidence-set observation for the loop
                // detector — read/search-style outcomes (never DiskWrite:
                // writes change the repo, they do not retrieve evidence)
                // with non-trivial result text feed the bounded evidence
                // ring; ≥3 consecutive DIFFERENT commands returning the
                // identical evidence set trip RepeatedEvidenceSet.
                if repo_moved {
                    // The batch's repo state moved: fold one fingerprint
                    // state step (patch → revert → patch detection).
                    let _ = self.note_fingerprint_step(handle.id(), detector, "write");
                }
                let is_write_tool = self
                    .deps
                    .tools
                    .get(&name)
                    .map(|t| t.resource_class == faktor_core::resource::ResourceClass::DiskWrite)
                    .unwrap_or(false);
                let evidence_text = outcome.text.trim();
                if !is_write_tool && !evidence_text.is_empty() {
                    let evidence_hash = evidence_fold_hash(&[evidence_text.as_bytes()]);
                    let key = LoopDetector::tool_key(&name, &input);
                    let key_hash = evidence_fold_hash(&[key.as_bytes()]);
                    let _ = detector.record_tool_evidence(key_hash, evidence_hash);
                }
            } else {
                abandoned.push((op_id, name, call_id));
            }
        }
        for (op_id, name, call_id) in abandoned {
            // A scheduler failure must be resolved honestly, never
            // misrepresented as a per-tool failure: an op the scheduler
            // itself marked Failed stays a tool failure (with the
            // batch-level failure as context); every still-open op
            // (Pending/Running/Blocked/Cancelled) records the scheduler
            // failure that abandoned it.
            let tool_error = match (&scheduler_failure, scheduler.status(op_id)) {
                (Some(err), Some(faktor_scheduler::TaskStatus::Failed)) => {
                    format!("tool {name} failed (scheduler aborted the batch: {err})")
                }
                (Some(err), _) => {
                    format!("scheduler aborted the batch before tool {name} completed: {err}")
                }
                (None, _) => format!("tool {name} failed"),
            };
            // The scheduler's typed failure is the batch outcome and must
            // survive a failed resolution: a lost finish is loud (the row
            // stays durably open for recovery) instead of replacing the
            // scheduler error. On the ordinary path the store error still
            // propagates exactly as before.
            if let Err(err) = handle.finish_tool_run(op_id, "failed", EffectStatus::Unknown) {
                if scheduler_failure.is_some() {
                    tracing::error!(
                        session = %handle.id(),
                        op = %op_id,
                        "durably resolving a still-open tool run after a scheduler failure failed: {err}"
                    );
                } else {
                    return Err(err);
                }
            }
            detector.record_error(&tool_error);
            // A per-tool execution failure is durable turn history, never a
            // dropped outcome: it feeds the turn summary (ledger + memory)
            // exactly like a non-zero-exit outcome does. The summary failure
            // also feeds the durable loop signals — an execute-Err call
            // repeated across turns is the same "stop and re-plan" signal as
            // a failing command.
            if !turn_summary.failures.iter().any(|f| f == &tool_error) {
                turn_summary
                    .failures
                    .push(self.redact_configured_secrets(&truncate(&tool_error, 400)));
            }
            // When the turn continues (or ends at FailedRecoverable) the
            // model must SEE the failure: every tool call of the assistant
            // message needs an answering result part, or the next wire
            // request carries a dangling call (real providers reject it).
            // The scheduler-failure path aborts the turn and writes no
            // further transcript — its rows are resolved above and the
            // typed scheduler error is the outcome.
            if scheduler_failure.is_none() {
                let seq = handle.proposed_message_seq()?;
                let mid = handle
                    .append_message(seq, "assistant", serde_json::json!({ "parts": [] }))
                    .await?;
                let body = ToolResultBody {
                    excerpt: self.redact_configured_secrets(&truncate(&tool_error, 2000)),
                    exit_code: Some(1),
                    artifact: None,
                    slice_hint: None,
                };
                handle.append_tool_result_part(mid, &call_id, &body).await?;
            }
            // ToolError hook (audit): the run failed (execution error,
            // cancellation or scheduler loss) and the row is durably
            // finished. Best-effort: a Deny after the failure is
            // audit-only — the turn is never retroactively failed. The
            // error snippet is bounded (the scheduler keeps no full
            // error text).
            self.run_hook_best_effort(
                faktor_hooks::HookEvent::ToolError,
                handle.id(),
                Some(op_id),
                serde_json::json!({ "tool": name, "error": tool_error }),
            );
            self.progress_heartbeat(handle.id());
        }
        // Refused calls are durable turn history exactly like failed tools:
        // each denial answers its tool call with a typed tool_result part
        // (kind + bounded, credential-redacted reason) so the transcript the
        // next wire request is built from never carries a dangling call —
        // the model sees WHY the call did not run instead of hallucinating
        // an outcome. Nothing executed and no run row exists: the refusal's
        // own semantics are unchanged; this only records the decision that
        // already landed (journaled by the refusal path) plus its ledger
        // line, mirroring the failed-tool path above.
        for denial in &denied {
            if let Err(err) = self.append_denial_result(handle, denial).await {
                if scheduler_failure.is_some() {
                    // The typed scheduler error is the batch outcome and must
                    // survive: a lost denial result is loud (the crash-resume
                    // repair answers the call on the next open), never a mask.
                    tracing::error!(
                        session = %handle.id(),
                        tool = %denial.name,
                        "denied-call result write failed after a scheduler failure: {err}"
                    );
                } else {
                    return Err(err);
                }
            }
            let line = self.redact_configured_secrets(&denial.ledger_line());
            if !turn_summary.failures.iter().any(|f| f == &line) {
                turn_summary.failures.push(line);
            }
        }
        for d in &denied {
            detector.record_error(&d.reason);
        }
        self.progress_heartbeat(handle.id());
        if let Some(err) = scheduler_failure {
            // Every submitted run is durably resolved above; the scheduler's
            // own typed classification is the turn's outcome.
            return Err(err);
        }
        handle.put_task_ledger(serde_json::to_value(ledger)?)?;
        Ok(executed)
    }

    /// Append ONE refused call's durable typed result part (see
    /// [`DeniedToolCall`]). The result is an ERROR result (`exit_code` 1) so
    /// the wire marks it `is_error`; the typed tag + reason ride the
    /// excerpt. The test-only durable-write fault seam can fail the write so
    /// the crash-resume repair is exercised against a genuinely lost result.
    pub(crate) async fn append_denial_result(
        &self,
        handle: &faktor_session::SessionHandle,
        denial: &DeniedToolCall,
    ) -> faktor_core::Result<()> {
        if let Some(err) = self.take_durable_write_fault(DW_SITE_DENIAL_RESULT) {
            tracing::error!(
                session = %handle.id(),
                site = DW_SITE_DENIAL_RESULT,
                tool = %denial.name,
                "denied-call result write failed: {err}"
            );
            return Err(err);
        }
        let seq = handle.proposed_message_seq()?;
        let mid = handle
            .append_message(seq, "assistant", serde_json::json!({ "parts": [] }))
            .await?;
        let body = ToolResultBody {
            excerpt: self.redact_configured_secrets(&denial.excerpt()),
            exit_code: Some(1),
            artifact: None,
            slice_hint: None,
        };
        handle
            .append_tool_result_part(mid, &denial.call_id, &body)
            .await?;
        Ok(())
    }

    /// Run one tool batch to completion through the scheduler. The scheduler
    /// result is NEVER defaulted: see the caller for the typed-error
    /// contract. Test-only fault seam (see [`scheduler_faults_tests`]): an armed
    /// injected failure replaces the scheduler's terminal result so the
    /// resolution path is exercised with the REAL classifications (task
    /// panic → `Internal`, deadlock, validation refusal) without depending on
    /// a scheduler that can be made to panic from the outside.
    pub(crate) async fn run_scheduled_batch(
        &self,
        scheduler: &Scheduler,
    ) -> Result<Vec<OpId>, Error> {
        #[cfg(test)]
        if let Some(err) = scheduler_faults_tests::take(
            self.deps.session.store().root(),
            scheduler_faults_tests::Point::BeforeRun,
        ) {
            return Err(err);
        }
        let result = scheduler.run_to_completion().await;
        #[cfg(test)]
        if let Some(err) = scheduler_faults_tests::take(
            self.deps.session.store().root(),
            scheduler_faults_tests::Point::AfterRun,
        ) {
            return Err(err);
        }
        result
    }

    /// Tool-outcome sanitization (audit round 16): a tool's output may echo
    /// a credential (a command that printed a key). Before ANY part of the
    /// outcome is journaled — the durable tool-result message (text AND the
    /// inline artifact/slice carriers), the turn summary/ledger — it is
    /// scanned under the default SecretPolicy AND the daemon's
    /// configured-secret registry (the SAME instance the egress scan uses),
    /// and on a hit redacted in place. Benign output is byte-identical
    /// (redaction runs only when a hit exists); scanning covers the whole
    /// payload (streaming overlap window, P0-37) and never panics on hostile
    /// output. The registry pass closes the fault where a command
    /// `cat`-ing a configured provider key left the raw value in the journal
    /// and the next provider request was egress-blocked.
    pub(crate) fn sanitize_outcome_text(&self, outcome: &mut ToolOutcome) {
        outcome.text = self.sanitize_tool_outcome_text(&outcome.text);
        if let Some(artifact) = outcome.artifact.as_mut() {
            *artifact = self.sanitize_tool_outcome_text(artifact);
        }
        if let Some(slice_hint) = outcome.slice_hint.as_mut() {
            *slice_hint = self.sanitize_tool_outcome_text(slice_hint);
        }
    }

    /// The pattern + configured-registry redaction of one tool-outcome text
    /// carrier. Benign text is returned byte-identical.
    fn sanitize_tool_outcome_text(&self, text: &str) -> String {
        let policy = faktor_security::SecretPolicy::default();
        let mut out = if faktor_security::scan_secrets(text, &policy).is_empty() {
            text.to_string()
        } else {
            faktor_security::redact(text, &policy)
        };
        if let Some(registry) = &self.deps.secret_registry {
            if !registry.scan_exact(out.as_bytes()).is_empty() {
                out = registry.redact_text(&out);
            }
        }
        out
    }

    /// The configured-secret half of tool-output sanitization, exposed so
    /// every durable surface carrying tool text (message-part excerpts,
    /// ledger lines) can apply the identical exact redaction even when the
    /// text did not originate from `ToolOutcome::text`. Benign text is
    /// byte-identical; with no registry wired this is a pass-through.
    pub(crate) fn redact_configured_secrets(&self, text: &str) -> String {
        match &self.deps.secret_registry {
            Some(registry) if !registry.scan_exact(text.as_bytes()).is_empty() => {
                registry.redact_text(text)
            }
            _ => text.to_string(),
        }
    }

    /// The Implement-phase tool bundle under the session's activation set.
    /// The set is folded with the newest user text; a changed set (or a
    /// non-empty set whose durable flag fell out of the bounded scan) is
    /// persisted. An empty set yields EXACTLY `bundle_for_phase` bytes.
    pub(crate) fn tools_bundle_for_turn(
        &self,
        handle: &faktor_session::SessionHandle,
        history: &[RequestMessage],
        capabilities: &faktor_core::model::ModelCapabilities,
    ) -> ToolBundle {
        let load = self.load_tool_activation(handle);
        let text = Self::newest_user_text(history);
        let next = if text.is_empty() {
            load.set.clone()
        } else {
            self.deps.tools.activation_for_text(&load.set, &text)
        };
        if next != load.set || (!load.found() && !next.is_empty()) {
            self.store_tool_activation(handle, &next);
        }
        self.deps.tools.bundle_for_phase_with_activation(
            RouterPhase::Implement,
            capabilities,
            &next,
        )
    }
}
