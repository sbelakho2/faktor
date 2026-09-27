//! Ledger row decoding and validation (mechanically split from `ledger`).

use super::*;

// ---------------------------------------------------------------- decode/encode

pub(crate) fn entry_tag_of(payload: &LedgerPayload) -> &'static str {
    match payload {
        LedgerPayload::GoalSet { .. } => ENTRY_GOAL_SET,
        LedgerPayload::CriteriaSet { .. } => ENTRY_CRITERIA_SET,
        LedgerPayload::BlockerOpened { .. } => ENTRY_BLOCKER_OPENED,
        LedgerPayload::BlockerResolved { .. } => ENTRY_BLOCKER_RESOLVED,
        LedgerPayload::Decision { .. } => ENTRY_DECISION,
        LedgerPayload::PlanStepAdded { .. } => ENTRY_PLAN_STEP_ADDED,
        LedgerPayload::ChildAgentStarted { .. } => ENTRY_CHILD_AGENT_STARTED,
        LedgerPayload::ChildAgentFinished { .. } => ENTRY_CHILD_AGENT_FINISHED,
        LedgerPayload::RoutingDecision { .. } => ENTRY_ROUTING_DECISION,
        LedgerPayload::EpochBumped { .. } => ENTRY_EPOCH_BUMPED,
        LedgerPayload::FailureRecorded { .. } => ENTRY_FAILURE_RECORDED,
        LedgerPayload::VerifyRun { .. } => ENTRY_VERIFY_RUN,
        LedgerPayload::TurnCompleted { .. } => ENTRY_TURN_COMPLETED,
        LedgerPayload::LearningRecord { .. } => ENTRY_LEARNING_RECORD,
        LedgerPayload::EditTxnPrepared { .. } => ENTRY_EDIT_TXN_PREPARED,
        LedgerPayload::EditTxnProgress { .. } => ENTRY_EDIT_TXN_PROGRESS,
        LedgerPayload::EditTxnCommitted { .. } => ENTRY_EDIT_TXN_COMMITTED,
        LedgerPayload::EditTxnRolledBack { .. } => ENTRY_EDIT_TXN_ROLLED_BACK,
        LedgerPayload::TournamentStarted { .. } => ENTRY_TOURNAMENT_STARTED,
        LedgerPayload::CandidateSettled { .. } => ENTRY_CANDIDATE_SETTLED,
        LedgerPayload::TournamentDecided { .. } => ENTRY_TOURNAMENT_DECIDED,
        LedgerPayload::ChildPresentationChanged { .. } => ENTRY_CHILD_PRESENTATION_CHANGED,
        LedgerPayload::BoardPost { .. } => ENTRY_BOARD_POST,
        LedgerPayload::BoardRead { .. } => ENTRY_BOARD_READ,
        LedgerPayload::BoardReceipt { .. } => ENTRY_BOARD_RECEIPT,
        LedgerPayload::BoardReset { .. } => ENTRY_BOARD_RESET,
        LedgerPayload::CompletionContractSet { .. } => ENTRY_COMPLETION_CONTRACT_SET,
        LedgerPayload::CompletionStepStatus { .. } => ENTRY_COMPLETION_STEP_STATUS,
        LedgerPayload::IntegrationRecorded { .. } => ENTRY_INTEGRATION_RECORD,
        LedgerPayload::RunBaseRecorded { .. } => ENTRY_RUN_BASE,
        LedgerPayload::VerifiedGitArtifactRecorded { .. } => ENTRY_VERIFIED_GIT_ARTIFACT,
        LedgerPayload::IntegrationTxnRecorded { .. } => ENTRY_INTEGRATION_TXN,
        LedgerPayload::ExternalOperationRecorded { .. } => ENTRY_EXTERNAL_OPERATION,
        LedgerPayload::TerminalCreated { .. } => ENTRY_TERMINAL_CREATED,
        LedgerPayload::TerminalRunning { .. } => ENTRY_TERMINAL_RUNNING,
        LedgerPayload::TerminalExited { .. } => ENTRY_TERMINAL_EXITED,
        LedgerPayload::TerminalKilled { .. } => ENTRY_TERMINAL_KILLED,
        LedgerPayload::TerminalLost { .. } => ENTRY_TERMINAL_LOST,
        LedgerPayload::TerminalReconciled { .. } => ENTRY_TERMINAL_RECONCILED,
    }
}

/// Project one decoded typed entry onto a durable terminal record (`None`
/// for every non-terminal entry). The record's `kind` is the entry tag and
/// its `detail` the kind's bounded audit text.
pub(crate) fn terminal_record_of(entry: TypedLedgerEntry) -> Option<TerminalLedgerRecord> {
    let (kind, row, exit_code, detail) = match entry.payload {
        LedgerPayload::TerminalCreated { row } => {
            (TerminalEventKind::Created, row, None, String::new())
        }
        LedgerPayload::TerminalRunning { row } => {
            (TerminalEventKind::Running, row, None, String::new())
        }
        LedgerPayload::TerminalExited { row, exit_code } => {
            (TerminalEventKind::Exited, row, exit_code, String::new())
        }
        LedgerPayload::TerminalKilled { row, reason } => {
            (TerminalEventKind::Killed, row, None, reason)
        }
        LedgerPayload::TerminalLost { row, reason } => (TerminalEventKind::Lost, row, None, reason),
        LedgerPayload::TerminalReconciled { row, disposition } => {
            (TerminalEventKind::Reconciled, row, None, disposition)
        }
        _ => return None,
    };
    Some(TerminalLedgerRecord {
        seq: entry.seq,
        kind,
        row,
        exit_code,
        detail,
    })
}

/// Decode one typed payload from its row. Unknown `entry_type` or unknown
/// `schema_ver` => loud `Malformed` (corrupt), never a silent parse.
/// Schema-shape violations are equally loud.
pub(crate) fn decode_payload(
    entry_type: &str,
    schema_ver: i64,
    json: &serde_json::Value,
) -> Result<LedgerPayload, SessionError> {
    if schema_ver != LEDGER_ENTRY_SCHEMA_V {
        return Err(SessionError::Malformed(format!(
            "ledger entry {entry_type:?} has unknown schema version {schema_ver} \
             (this reader understands v{LEDGER_ENTRY_SCHEMA_V}); refusing to parse"
        )));
    }
    let decode = |tag: &str| -> Result<LedgerPayload, SessionError> {
        serde_json::from_value(json.clone()).map_err(|e| {
            SessionError::Malformed(format!(
                "ledger entry {tag} payload violates its v1 schema: {e}"
            ))
        })
    };
    match entry_type {
        ENTRY_GOAL_SET => decode(entry_type),
        ENTRY_CRITERIA_SET => decode(entry_type),
        ENTRY_BLOCKER_OPENED => decode(entry_type),
        ENTRY_BLOCKER_RESOLVED => decode(entry_type),
        ENTRY_DECISION => decode(entry_type),
        ENTRY_PLAN_STEP_ADDED => decode(entry_type),
        ENTRY_CHILD_AGENT_STARTED => decode(entry_type),
        ENTRY_CHILD_AGENT_FINISHED => decode(entry_type),
        ENTRY_ROUTING_DECISION => decode(entry_type),
        ENTRY_EPOCH_BUMPED => decode(entry_type),
        ENTRY_FAILURE_RECORDED => decode(entry_type),
        ENTRY_VERIFY_RUN => decode(entry_type),
        ENTRY_TURN_COMPLETED => decode(entry_type),
        ENTRY_LEARNING_RECORD => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::LearningRecord { record, payload } = &decoded {
                validate_learning_record(record, payload)?;
            }
            Ok(decoded)
        }
        ENTRY_EDIT_TXN_PREPARED => decode(entry_type),
        ENTRY_EDIT_TXN_PROGRESS => decode(entry_type),
        ENTRY_EDIT_TXN_COMMITTED => decode(entry_type),
        ENTRY_EDIT_TXN_ROLLED_BACK => decode(entry_type),
        ENTRY_TOURNAMENT_STARTED => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::TournamentStarted {
                tournament_id,
                run_family,
                goal,
                criteria,
                candidates,
            } = &decoded
            {
                validate_tournament_started(tournament_id, run_family, goal, criteria, candidates)?;
            }
            Ok(decoded)
        }
        ENTRY_CANDIDATE_SETTLED => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::CandidateSettled {
                tournament_id,
                settlement,
            } = &decoded
            {
                validate_tournament_id(tournament_id, "tournament id")?;
                validate_tournament_settlement(settlement)?;
            }
            Ok(decoded)
        }
        ENTRY_TOURNAMENT_DECIDED => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::TournamentDecided {
                tournament_id,
                winner,
                outcome,
                rationale,
            } = &decoded
            {
                validate_tournament_id(tournament_id, "tournament id")?;
                if let Some(w) = winner {
                    validate_tournament_text(w, "tournament winner")?;
                }
                if !matches!(
                    outcome.as_str(),
                    TOURNAMENT_OUTCOME_DECIDED | TOURNAMENT_OUTCOME_ABORTED
                ) {
                    return Err(SessionError::Malformed(format!(
                        "ledger tournament_decided outcome {outcome:?} is not decided|aborted"
                    )));
                }
                if outcome == TOURNAMENT_OUTCOME_DECIDED && winner.is_none() {
                    return Err(SessionError::Malformed(
                        "ledger tournament_decided with outcome decided requires a winner".into(),
                    ));
                }
                if rationale.is_empty() || rationale.len() > MAX_TOURNAMENT_OUTCOME {
                    return Err(SessionError::Malformed(
                        "ledger tournament_decided rationale must be 1..=MAX_TOURNAMENT_OUTCOME bytes"
                            .into(),
                    ));
                }
            }
            Ok(decoded)
        }
        ENTRY_CHILD_PRESENTATION_CHANGED => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::ChildPresentationChanged {
                child_id,
                from,
                to,
                at_ms,
            } = &decoded
            {
                validate_child_presentation(child_id, *from, *to, *at_ms)?;
            }
            Ok(decoded)
        }
        ENTRY_BOARD_POST => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::BoardPost {
                board_id,
                post_id,
                author_child,
                author_session,
                subject,
                body,
                refs,
                revision,
            } = &decoded
            {
                validate_board_post(
                    *board_id,
                    *post_id,
                    *author_child,
                    *author_session,
                    subject,
                    body,
                    refs,
                    *revision,
                )?;
            }
            Ok(decoded)
        }
        ENTRY_BOARD_READ => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::BoardRead {
                board_id,
                child,
                post_id,
            } = &decoded
            {
                validate_board_read(*board_id, *child, *post_id)?;
            }
            Ok(decoded)
        }
        ENTRY_BOARD_RECEIPT => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::BoardReceipt {
                board_id,
                child,
                post_id,
                action,
                note,
            } = &decoded
            {
                validate_board_receipt(*board_id, *child, *post_id, action, note)?;
            }
            Ok(decoded)
        }
        ENTRY_BOARD_RESET => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::BoardReset {
                board_id,
                previous_revision,
                new_revision,
            } = &decoded
            {
                validate_board_reset(*board_id, *previous_revision, *new_revision)?;
            }
            Ok(decoded)
        }
        ENTRY_COMPLETION_CONTRACT_SET => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::CompletionContractSet {
                task_id,
                revision,
                contract,
            } = &decoded
            {
                validate_completion_contract_set(*task_id, *revision, contract)?;
            }
            Ok(decoded)
        }
        ENTRY_COMPLETION_STEP_STATUS => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::CompletionStepStatus {
                task_id,
                revision,
                detail,
                at_ms,
                snapshot,
                ..
            } = &decoded
            {
                validate_completion_step_status(
                    *task_id,
                    *revision,
                    detail,
                    *at_ms,
                    snapshot.as_deref(),
                )?;
            }
            Ok(decoded)
        }
        ENTRY_INTEGRATION_RECORD => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::IntegrationRecorded { record } = &decoded {
                validate_integration_record(record)?;
            }
            Ok(decoded)
        }
        ENTRY_RUN_BASE => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::RunBaseRecorded { record } = &decoded {
                validate_run_base_record(record)?;
            }
            Ok(decoded)
        }
        ENTRY_INTEGRATION_TXN => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::IntegrationTxnRecorded { row } = &decoded {
                validate_integration_txn(row)?;
            }
            Ok(decoded)
        }
        ENTRY_VERIFIED_GIT_ARTIFACT => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::VerifiedGitArtifactRecorded { artifact } = &decoded {
                validate_verified_git_artifact(artifact)?;
            }
            Ok(decoded)
        }
        ENTRY_EXTERNAL_OPERATION => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::ExternalOperationRecorded { record } = &decoded {
                validate_external_operation(record)?;
            }
            Ok(decoded)
        }
        ENTRY_TERMINAL_CREATED
        | ENTRY_TERMINAL_RUNNING
        | ENTRY_TERMINAL_EXITED
        | ENTRY_TERMINAL_KILLED
        | ENTRY_TERMINAL_LOST
        | ENTRY_TERMINAL_RECONCILED => {
            let decoded = decode(entry_type)?;
            match &decoded {
                LedgerPayload::TerminalCreated { row } => {
                    validate_terminal_row(row, "terminal_created")?
                }
                LedgerPayload::TerminalRunning { row } => {
                    validate_terminal_row(row, "terminal_running")?
                }
                LedgerPayload::TerminalExited { row, .. } => {
                    validate_terminal_row(row, "terminal_exited")?
                }
                LedgerPayload::TerminalKilled { row, reason } => {
                    validate_terminal_row(row, "terminal_killed")?;
                    validate_terminal_detail(reason, "terminal_killed reason")?;
                }
                LedgerPayload::TerminalLost { row, reason } => {
                    validate_terminal_row(row, "terminal_lost")?;
                    validate_terminal_detail(reason, "terminal_lost reason")?;
                }
                LedgerPayload::TerminalReconciled { row, disposition } => {
                    validate_terminal_row(row, "terminal_reconciled")?;
                    if !matches!(
                        disposition.as_str(),
                        TERMINAL_RECONCILE_KILLED | TERMINAL_RECONCILE_COLLECTED
                    ) {
                        return Err(SessionError::Malformed(format!(
                            "ledger terminal_reconciled disposition {disposition:?} is not \
                             killed|collected"
                        )));
                    }
                }
                _ => {
                    return Err(SessionError::Internal(
                        "terminal entry decode returned a non-terminal payload".into(),
                    ))
                }
            }
            Ok(decoded)
        }
        other => Err(SessionError::Malformed(format!(
            "ledger entry type {other:?} is unknown to this reader"
        ))),
    }
}

/// Shape bounds of one durable terminal row, shared by every appender and
/// the strict decoder (a hostile raw row fails loudly on read too). The
/// terminal UUID is the authority key and is never pid-derived; the pid and
/// the OS start time are the process identity (a start time of 0 = unknown,
/// which recovery treats as unverifiable — never "alive by pid").
pub(crate) fn validate_terminal_row(
    row: &TerminalDurableRow,
    what: &str,
) -> Result<(), SessionError> {
    if row.terminal_id.is_empty() || row.terminal_id.len() > MAX_TERMINAL_ID_BYTES {
        return Err(SessionError::Malformed(format!(
            "ledger {what} terminal_id must be 1..={MAX_TERMINAL_ID_BYTES} bytes"
        )));
    }
    if !row.terminal_id.is_ascii()
        || row.terminal_id.contains('/')
        || row.terminal_id.contains('\\')
        || row.terminal_id.chars().any(|c| c.is_control())
    {
        return Err(SessionError::Malformed(format!(
            "ledger {what} terminal_id must be printable ASCII without '/' or '\\'"
        )));
    }
    if row.session_id == 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} session_id must be non-zero"
        )));
    }
    if row.operation_id == 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} operation_id must be non-zero"
        )));
    }
    if row.pid == 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} pid must be non-zero"
        )));
    }
    if row.start_time_ms < 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} start_time_ms must be >= 0 (0 = unverified identity)"
        )));
    }
    if row.at_ms <= 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} at_ms must be positive"
        )));
    }
    if let Some(agent_id) = &row.agent_id {
        if agent_id.is_empty() || agent_id.len() > MAX_TERMINAL_ID_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger {what} agent_id must be 1..={MAX_TERMINAL_ID_BYTES} bytes when set"
            )));
        }
        if !agent_id.is_ascii() || agent_id.chars().any(|c| c.is_control()) {
            return Err(SessionError::Malformed(format!(
                "ledger {what} agent_id must be printable ASCII"
            )));
        }
    }
    // The effective execution profile is bounded audit evidence: empty means
    // a legacy row (readable), a set value must never be hostile.
    if row.execution_profile.len() > MAX_TERMINAL_PROFILE_BYTES {
        return Err(SessionError::Oversized(format!(
            "ledger {what} execution_profile of {} bytes exceeds {MAX_TERMINAL_PROFILE_BYTES}",
            row.execution_profile.len()
        )));
    }
    if row.execution_profile.contains('\0')
        || row
            .execution_profile
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(SessionError::Malformed(format!(
            "ledger {what} execution_profile contains control/NUL characters"
        )));
    }
    Ok(())
}

/// One bounded terminal audit detail (kill/lost reason). Non-empty and no
/// NUL: the detail lands in operator-visible projections.
pub(crate) fn validate_terminal_detail(detail: &str, what: &str) -> Result<(), SessionError> {
    if detail.is_empty() {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must be non-empty"
        )));
    }
    if detail.len() > MAX_TERMINAL_DETAIL_BYTES {
        return Err(SessionError::Oversized(format!(
            "ledger {what} of {} bytes exceeds {MAX_TERMINAL_DETAIL_BYTES}",
            detail.len()
        )));
    }
    if detail.contains('\0') {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must not contain NUL"
        )));
    }
    Ok(())
}

/// Shape bounds of one `learning_record` row. Shared by the appender
/// (rejects BEFORE any byte is journaled) and the decoder (a hostile raw
/// row must fail loudly on read too). The payload's inner schema is the
/// learning crate's; this layer owns only its kind and its bound.
pub(crate) fn validate_learning_record(record: &str, payload: &str) -> Result<(), SessionError> {
    if !matches!(
        record,
        LEARNING_RECORD_EPISODE | LEARNING_RECORD_LEARNING | LEARNING_RECORD_REMOVED
    ) {
        return Err(SessionError::Malformed(format!(
            "ledger learning record kind {record:?} is not episode|learning|removed"
        )));
    }
    if payload.is_empty() {
        return Err(SessionError::Malformed(
            "ledger learning record payload must be non-empty".into(),
        ));
    }
    if payload.len() > MAX_LEARNING_RECORD_PAYLOAD {
        return Err(SessionError::Oversized(format!(
            "ledger learning record payload of {} bytes exceeds MAX_LEARNING_RECORD_PAYLOAD",
            payload.len()
        )));
    }
    Ok(())
}

pub(crate) fn check_text(value: &str, what: &str) -> Result<(), SessionError> {
    if value.is_empty() {
        return Err(SessionError::Malformed(format!(
            "ledger entry {what} must be non-empty"
        )));
    }
    if value.len() > MAX_LEDGER_TEXT {
        return Err(SessionError::Oversized(format!(
            "ledger entry {what} of {} bytes exceeds {MAX_LEDGER_TEXT}",
            value.len()
        )));
    }
    Ok(())
}

pub(crate) fn validate_tournament_id(value: &str, what: &str) -> Result<(), SessionError> {
    if value.is_empty() || value.len() > MAX_TOURNAMENT_ID {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must be 1..={MAX_TOURNAMENT_ID} bytes"
        )));
    }
    if !value.is_ascii()
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(|c| c.is_control())
    {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must be printable ASCII without '/' or '\\\\'"
        )));
    }
    Ok(())
}

pub(crate) fn validate_tournament_text(value: &str, what: &str) -> Result<(), SessionError> {
    if value.is_empty() {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must be non-empty"
        )));
    }
    if value.len() > MAX_TOURNAMENT_TEXT {
        return Err(SessionError::Oversized(format!(
            "ledger {what} of {} bytes exceeds MAX_TOURNAMENT_TEXT ({MAX_TOURNAMENT_TEXT})",
            value.len()
        )));
    }
    Ok(())
}

pub(crate) fn validate_tournament_checks(
    checks: &[TournamentCheckSpec],
) -> Result<(), SessionError> {
    if checks.is_empty() {
        return Err(SessionError::Malformed(
            "ledger candidate settlement requires the derived check set".into(),
        ));
    }
    if checks.len() > MAX_TOURNAMENT_CRITERIA {
        return Err(SessionError::Oversized(format!(
            "ledger candidate settlement of {} checks exceeds MAX_TOURNAMENT_CRITERIA",
            checks.len()
        )));
    }
    for c in checks {
        validate_tournament_text(&c.id, "tournament check id")?;
        validate_tournament_text(&c.spec, "tournament check spec")?;
    }
    Ok(())
}

pub(crate) fn validate_tournament_started(
    tournament_id: &str,
    run_family: &str,
    goal: &str,
    criteria: &[TournamentCriterionRow],
    candidates: &[TournamentCandidateRow],
) -> Result<(), SessionError> {
    validate_tournament_id(tournament_id, "tournament id")?;
    validate_tournament_id(run_family, "tournament run family")?;
    validate_tournament_text(goal, "tournament goal")?;
    if criteria.is_empty() || criteria.len() > MAX_TOURNAMENT_CRITERIA {
        return Err(SessionError::Malformed(format!(
            "ledger tournament_started must carry 1..={MAX_TOURNAMENT_CRITERIA} criteria"
        )));
    }
    let mut seen_criteria: Vec<&str> = Vec::with_capacity(criteria.len());
    for c in criteria {
        validate_tournament_text(&c.id, "tournament criterion id")?;
        validate_tournament_text(&c.spec, "tournament criterion spec")?;
        if seen_criteria.contains(&c.id.as_str()) {
            return Err(SessionError::Malformed(format!(
                "ledger tournament_started carries duplicate criterion id {:?}",
                c.id
            )));
        }
        seen_criteria.push(&c.id);
    }
    if !(MIN_TOURNAMENT_CANDIDATES..=MAX_TOURNAMENT_CANDIDATES).contains(&candidates.len()) {
        return Err(SessionError::Malformed(format!(
            "ledger tournament_started carries {} candidates outside the supported band {MIN_TOURNAMENT_CANDIDATES}..={MAX_TOURNAMENT_CANDIDATES}",
            candidates.len()
        )));
    }
    let mut seen_children: Vec<&str> = Vec::with_capacity(candidates.len());
    for c in candidates {
        validate_tournament_id(&c.child_id, "tournament candidate child id")?;
        if !c.worktree.is_empty() && c.worktree.len() > MAX_LEDGER_TEXT {
            return Err(SessionError::Oversized(
                "ledger tournament candidate worktree exceeds MAX_LEDGER_TEXT".into(),
            ));
        }
        if c.base_revision.len() > MAX_LEDGER_TEXT {
            return Err(SessionError::Oversized(
                "ledger tournament candidate base revision exceeds MAX_LEDGER_TEXT".into(),
            ));
        }
        if seen_children.contains(&c.child_id.as_str()) {
            return Err(SessionError::Malformed(format!(
                "ledger tournament_started carries duplicate candidate child id {:?}",
                c.child_id
            )));
        }
        seen_children.push(&c.child_id);
    }
    Ok(())
}

pub(crate) fn validate_tournament_settlement(
    settlement: &TournamentSettlementRow,
) -> Result<(), SessionError> {
    validate_tournament_id(&settlement.child_id, "candidate child id")?;
    if settlement.worktree.len() > MAX_LEDGER_TEXT {
        return Err(SessionError::Oversized(
            "ledger candidate settlement worktree exceeds MAX_LEDGER_TEXT".into(),
        ));
    }
    if settlement.base_revision.len() > MAX_LEDGER_TEXT {
        return Err(SessionError::Oversized(
            "ledger candidate settlement base revision exceeds MAX_LEDGER_TEXT".into(),
        ));
    }
    if !matches!(
        settlement.state.as_str(),
        TOURNAMENT_STATE_DONE | TOURNAMENT_STATE_FAILED | TOURNAMENT_STATE_CANCELLED
    ) {
        return Err(SessionError::Malformed(format!(
            "ledger candidate settlement state {:?} is not done|failed|cancelled",
            settlement.state
        )));
    }
    if settlement.verification == Some(0) {
        return Err(SessionError::Malformed(
            "ledger candidate settlement verification record id cannot be 0".into(),
        ));
    }
    validate_tournament_checks(&settlement.checks)?;
    match (&settlement.review, &settlement.reviewer) {
        (Some(rank), Some(reviewer)) => {
            if !matches!(
                rank.as_str(),
                TOURNAMENT_REVIEW_BLOCK | TOURNAMENT_REVIEW_CONCERN | TOURNAMENT_REVIEW_CLEAN
            ) {
                return Err(SessionError::Malformed(format!(
                    "ledger candidate review {rank:?} is not block|concern|clean"
                )));
            }
            validate_tournament_id(reviewer, "candidate reviewer")?;
        }
        (None, None) => {}
        (Some(_), None) => {
            return Err(SessionError::Malformed(
                "ledger candidate review requires its reviewer identity".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(SessionError::Malformed(
                "ledger candidate settlement carries a reviewer without a review verdict".into(),
            ));
        }
    }
    if settlement.reason.len() > MAX_TOURNAMENT_OUTCOME {
        return Err(SessionError::Oversized(
            "ledger candidate settlement reason exceeds MAX_TOURNAMENT_OUTCOME".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- presentation bounds

/// Shape bounds of one `child_presentation_changed` row, shared by the
/// appender and the strict decoder (a hostile raw row must fail loudly on
/// read too). A same-state transition is refused: the appender treats it as
/// an idempotent no-op and never writes a row, so a durable `from == to`
/// row is corruption.
pub(crate) fn validate_child_presentation(
    child_id: &str,
    from: PresentationState,
    to: PresentationState,
    at_ms: i64,
) -> Result<(), SessionError> {
    if child_id.is_empty() || child_id.len() > MAX_PRESENTATION_CHILD_ID {
        return Err(SessionError::Malformed(format!(
            "ledger child_presentation_changed child_id must be 1..={MAX_PRESENTATION_CHILD_ID} bytes"
        )));
    }
    if !child_id.is_ascii()
        || child_id.contains('/')
        || child_id.contains('\\')
        || child_id.chars().any(|c| c.is_control())
    {
        return Err(SessionError::Malformed(
            "ledger child_presentation_changed child_id must be printable ASCII without '/' or '\\'"
                .into(),
        ));
    }
    if at_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger child_presentation_changed at_ms must be positive".into(),
        ));
    }
    if from == to {
        return Err(SessionError::Malformed(
            "ledger child_presentation_changed refuses a no-op transition (from == to): the \
             idempotent path writes no row"
                .into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- board bounds checks

pub(crate) fn check_board_id(value: u64, what: &str) -> Result<(), SessionError> {
    if value == 0 {
        return Err(SessionError::Malformed(format!(
            "ledger board {what} must be non-zero"
        )));
    }
    Ok(())
}

/// One board text field: non-empty, bounded, and free of control characters
/// except the layout characters a coordination message legitimately carries
/// (newline / carriage return / tab).
pub(crate) fn check_board_text(field: &str, value: &str, max: usize) -> Result<(), SessionError> {
    if value.is_empty() {
        return Err(SessionError::Malformed(format!(
            "ledger board {field} must be non-empty"
        )));
    }
    if value.len() > max {
        return Err(SessionError::Oversized(format!(
            "ledger board {field} of {} bytes exceeds {max}",
            value.len()
        )));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(SessionError::Malformed(format!(
            "ledger board {field} carries control characters"
        )));
    }
    Ok(())
}

/// Board note fields may be empty (an `ack` needs no prose) but carry the
/// same bound and control-character rule.
pub(crate) fn check_board_note(field: &str, value: &str, max: usize) -> Result<(), SessionError> {
    if value.len() > max {
        return Err(SessionError::Oversized(format!(
            "ledger board {field} of {} bytes exceeds {max}",
            value.len()
        )));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(SessionError::Malformed(format!(
            "ledger board {field} carries control characters"
        )));
    }
    Ok(())
}

/// Shape bounds of one `board_post` row, shared by the appender and the
/// strict decoder (a hostile raw row must fail loudly on read too).
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_board_post(
    board_id: u64,
    post_id: u64,
    author_child: Option<u64>,
    author_session: u64,
    subject: &str,
    body: &str,
    refs: &[String],
    revision: u64,
) -> Result<(), SessionError> {
    check_board_id(board_id, "board_id")?;
    check_board_id(post_id, "post_id")?;
    check_board_id(author_session, "author_session")?;
    if author_child == Some(0) {
        return Err(SessionError::Malformed(
            "ledger board author_child cannot be 0 (None means the root agent)".into(),
        ));
    }
    check_board_text("subject", subject, MAX_BOARD_SUBJECT_BYTES)?;
    check_board_text("body", body, MAX_BOARD_BODY_BYTES)?;
    if refs.len() > MAX_BOARD_REFS {
        return Err(SessionError::Oversized(format!(
            "ledger board post of {} refs exceeds MAX_BOARD_REFS",
            refs.len()
        )));
    }
    for r in refs {
        check_board_text("ref", r, MAX_BOARD_REF_BYTES)?;
    }
    if revision == 0 {
        return Err(SessionError::Malformed(
            "ledger board post revision must be >= 1".into(),
        ));
    }
    if post_id != revision {
        // The durable post identity IS the revision that created it:
        // revisions are never reused (a reset consumes one), so the id is
        // stable, unique per board and recoverable from the pinned stream.
        return Err(SessionError::Malformed(format!(
            "ledger board post id {post_id} does not equal its revision {revision}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_board_read(
    board_id: u64,
    child: u64,
    post_id: u64,
) -> Result<(), SessionError> {
    check_board_id(board_id, "board_id")?;
    check_board_id(child, "read child")?;
    check_board_id(post_id, "post_id")?;
    Ok(())
}

pub(crate) fn validate_board_receipt(
    board_id: u64,
    child: Option<u64>,
    post_id: u64,
    action: &str,
    note: &str,
) -> Result<(), SessionError> {
    check_board_id(board_id, "board_id")?;
    if child == Some(0) {
        return Err(SessionError::Malformed(
            "ledger board receipt child cannot be 0 (None means the root agent)".into(),
        ));
    }
    check_board_id(post_id, "post_id")?;
    if !matches!(
        action,
        BOARD_RECEIPT_ACK
            | BOARD_RECEIPT_TASK_UPDATE
            | BOARD_RECEIPT_BLOCKED
            | BOARD_RECEIPT_QUESTION
    ) {
        return Err(SessionError::Malformed(format!(
            "ledger board receipt action {action:?} is not ack|task_update|blocked|question"
        )));
    }
    check_board_note("receipt note", note, MAX_BOARD_RECEIPT_NOTE_BYTES)?;
    Ok(())
}

pub(crate) fn validate_board_reset(
    board_id: u64,
    previous_revision: u64,
    new_revision: u64,
) -> Result<(), SessionError> {
    check_board_id(board_id, "board_id")?;
    if previous_revision.checked_add(1) != Some(new_revision) {
        return Err(SessionError::Malformed(format!(
            "ledger board reset must bump the revision by exactly one \
             ({previous_revision} -> {new_revision})"
        )));
    }
    Ok(())
}

/// Shape bounds of one `completion_contract_set` row, shared by the appender
/// and the strict decoder (a hostile raw row must fail loudly on read too).
/// The default all-false contract is never durable: recording it would
/// create a row whose meaning is "no gate", which the default path already
/// expresses with no row at all.
pub(crate) fn validate_completion_contract_set(
    task_id: u64,
    revision: u64,
    contract: &CompletionContract,
) -> Result<(), SessionError> {
    if task_id == 0 {
        return Err(SessionError::Malformed(
            "ledger completion_contract_set task_id must be non-zero".into(),
        ));
    }
    if revision == 0 {
        return Err(SessionError::Malformed(
            "ledger completion_contract_set revision must be >= 1".into(),
        ));
    }
    if contract.is_default() {
        return Err(SessionError::Malformed(
            "ledger completion_contract_set refuses an all-false contract: the default behavior \
             is expressed by the ABSENCE of a row, never by a durable no-op row"
                .into(),
        ));
    }
    Ok(())
}

/// Shape of one stored TREE-snapshot digest: the versioned canonical
/// tree-manifest digest (`tm1:` + 64 hex, `faktor_fs::tree_manifest`) or the
/// legacy 64-char hex content-only digest (accepted so rows written before
/// the canonical manifest still decode). The two shapes can never compare
/// equal, so a legacy record fails equality loudly instead of silently
/// matching the canonical definition.
pub(crate) fn check_snapshot_digest(value: &str, what: &str) -> Result<(), SessionError> {
    let legacy =
        value.len() == MAX_RUN_BASE_DIGEST_BYTES && value.bytes().all(|b| b.is_ascii_hexdigit());
    if legacy || faktor_fs::tree_manifest::is_tree_manifest_digest(value) {
        Ok(())
    } else {
        Err(SessionError::Malformed(format!(
            "ledger {what} must be a canonical `tm1:<64-hex>` tree-manifest digest or a \
             64-char hex legacy digest"
        )))
    }
}

/// Shape bounds of one `completion_step_status` row, shared by the appender
/// and the strict decoder (a hostile raw row must fail loudly on read too).
pub(crate) fn validate_completion_step_status(
    task_id: u64,
    revision: u64,
    detail: &str,
    at_ms: i64,
    snapshot: Option<&str>,
) -> Result<(), SessionError> {
    if task_id == 0 {
        return Err(SessionError::Malformed(
            "ledger completion_step_status task_id must be non-zero".into(),
        ));
    }
    if revision == 0 {
        return Err(SessionError::Malformed(
            "ledger completion_step_status revision must be >= 1".into(),
        ));
    }
    if detail.len() > MAX_COMPLETION_STEP_DETAIL {
        return Err(SessionError::Oversized(format!(
            "ledger completion_step_status detail of {} bytes exceeds MAX_COMPLETION_STEP_DETAIL",
            detail.len()
        )));
    }
    if let Some(hash) = snapshot {
        let legacy = !hash.is_empty()
            && hash.len() <= MAX_VERIFICATION_TREE_HASH_BYTES
            && hash.bytes().all(|b| b.is_ascii_hexdigit());
        if !legacy && !faktor_fs::tree_manifest::is_tree_manifest_digest(hash) {
            return Err(SessionError::Malformed(
                "ledger completion_step_status snapshot must be a canonical `tm1:<64-hex>` \
                 tree-manifest digest or a non-empty legacy hex digest within \
                 MAX_VERIFICATION_TREE_HASH_BYTES"
                    .into(),
            ));
        }
    }
    if at_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger completion_step_status at_ms must be positive".into(),
        ));
    }
    Ok(())
}

/// Shape bounds of one `integration_recorded` row, shared by the appender
/// and the strict decoder (a hostile raw row must fail loudly on read too).
/// Bounded fields are explicit: the stored file/conflict/source lists are
/// samples with exact counts and content digests beside them.
pub(crate) fn validate_integration_record(
    record: &IntegrationRecordRow,
) -> Result<(), SessionError> {
    // Legacy 64-bit FNV identities never authorize a new integration row:
    // the typed refusal names the legacy class BEFORE any shape check, so a
    // labelled FNV value can never be masked by a generic hex-shape error.
    if let Some(legacy) = record.legacy_authority_digest() {
        return Err(SessionError::Malformed(legacy.to_string()));
    }
    let check_id = |value: &str, what: &str| -> Result<(), SessionError> {
        if value.is_empty() || value.len() > MAX_INTEGRATION_ID_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record {what} must be 1..={MAX_INTEGRATION_ID_BYTES} bytes"
            )));
        }
        Ok(())
    };
    let check_hex = |value: &str, what: &str| -> Result<(), SessionError> {
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record {what} must be the 64-char hex BLAKE3"
            )));
        }
        Ok(())
    };
    check_id(&record.run_id, "run_id")?;
    if record.task_id == 0 {
        return Err(SessionError::Malformed(
            "ledger integration_record task_id must be non-zero".into(),
        ));
    }
    if let Some(base) = &record.base_revision {
        check_id(base, "base_revision")?;
    }
    if let Some(base) = &record.base_snapshot {
        check_snapshot_digest(base, "base_snapshot")?;
    }
    // Explicit identity fields (hardening): each carries its own bound and
    // shape; the deprecated alias must AGREE with the explicit field when
    // both are present, so a tampered row can never name two base roots.
    if let Some(base) = &record.run_base_snapshot {
        check_snapshot_digest(base, "run_base_snapshot")?;
        if record
            .base_snapshot
            .as_deref()
            .is_some_and(|alias| alias != base)
        {
            return Err(SessionError::Malformed(
                "ledger integration_record base_snapshot and run_base_snapshot disagree".into(),
            ));
        }
    }
    if let Some(candidate) = &record.candidate_snapshot {
        check_snapshot_digest(candidate, "candidate_snapshot")?;
    }
    if let Some(landed) = &record.landed_snapshot {
        check_snapshot_digest(landed, "landed_snapshot")?;
        if !record.final_snapshot_hash.is_empty() && record.final_snapshot_hash != *landed {
            return Err(SessionError::Malformed(
                "ledger integration_record landed_snapshot and final_snapshot_hash disagree".into(),
            ));
        }
    }
    if let Some(digest) = &record.proof_basis_digest {
        if digest.is_empty() || digest.len() > MAX_INTEGRATION_ID_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record proof_basis_digest must be 1..={MAX_INTEGRATION_ID_BYTES} bytes"
            )));
        }
    }
    if let Some(txn) = &record.integration_txn_id {
        if txn.is_empty() || txn.len() > MAX_INTEGRATION_ID_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record integration_txn_id must be 1..={MAX_INTEGRATION_ID_BYTES} bytes"
            )));
        }
    }
    if record.final_root.is_empty() || record.final_root.len() > MAX_INTEGRATION_ROOT_BYTES {
        return Err(SessionError::Malformed(format!(
            "ledger integration_record final_root must be 1..={MAX_INTEGRATION_ROOT_BYTES} bytes"
        )));
    }
    // An EMPTY final hash is the record-first in-flight marker; a non-empty
    // one is the binding digest.
    if !record.final_snapshot_hash.is_empty() {
        check_snapshot_digest(&record.final_snapshot_hash, "final_snapshot_hash")?;
    }
    if record.integrated_files.len() > MAX_INTEGRATION_FILES {
        return Err(SessionError::Oversized(format!(
            "ledger integration_record stores {} integrated files (cap {MAX_INTEGRATION_FILES})",
            record.integrated_files.len()
        )));
    }
    for path in &record.integrated_files {
        if path.is_empty() || path.len() > MAX_INTEGRATION_PATH_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record file path must be 1..={MAX_INTEGRATION_PATH_BYTES} bytes"
            )));
        }
    }
    if record.integrated_files.len() as u64 > record.integrated_file_count {
        return Err(SessionError::Malformed(
            "ledger integration_record stores more file rows than its file count".into(),
        ));
    }
    if record.integrated_file_count == 0 {
        if !record.integrated_files.is_empty() || !record.integrated_files_digest.is_empty() {
            return Err(SessionError::Malformed(
                "ledger integration_record with zero integrated files must carry no file rows or digest"
                    .into(),
            ));
        }
    } else {
        check_hex(&record.integrated_files_digest, "integrated_files_digest")?;
    }
    if record.conflicts.len() > MAX_INTEGRATION_CONFLICTS {
        return Err(SessionError::Oversized(format!(
            "ledger integration_record stores {} conflict rows (cap {MAX_INTEGRATION_CONFLICTS})",
            record.conflicts.len()
        )));
    }
    for conflict in &record.conflicts {
        if conflict.is_empty() || conflict.len() > MAX_INTEGRATION_CONFLICT_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record conflict row must be 1..={MAX_INTEGRATION_CONFLICT_BYTES} bytes"
            )));
        }
    }
    if record.conflicts.len() as u64 > record.conflict_count {
        return Err(SessionError::Malformed(
            "ledger integration_record stores more conflict rows than its conflict count".into(),
        ));
    }
    // A clean finalized record must carry a final snapshot; an in-flight one
    // may not pretend to have applied files. Conflicts are legal in both:
    // they can be recorded before the remaining children apply.
    if record.sources.len() > MAX_INTEGRATION_SOURCES {
        return Err(SessionError::Oversized(format!(
            "ledger integration_record stores {} sources (cap {MAX_INTEGRATION_SOURCES})",
            record.sources.len()
        )));
    }
    for source in &record.sources {
        check_id(&source.child_id, "source child_id")?;
        check_id(&source.change_set_id, "source change_set_id")?;
        check_snapshot_digest(&source.candidate_root_hash, "source candidate_root_hash")?;
    }
    if record.source_count < record.sources.len() as u64 {
        return Err(SessionError::Malformed(
            "ledger integration_record source count is smaller than its stored sources".into(),
        ));
    }
    if record.source_count == 0 {
        if !record.sources_digest.is_empty() {
            return Err(SessionError::Malformed(
                "ledger integration_record with zero sources must carry no sources digest".into(),
            ));
        }
    } else if record.sources.len() as u64 != record.source_count {
        // The bounded-full case: the digest covers every source.
        check_hex(&record.sources_digest, "sources_digest")?;
    }
    if record.at_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger integration_record at_ms must be positive".into(),
        ));
    }
    Ok(())
}

/// Shape bounds of one `run_base` row, shared by the appender and the
/// strict decoder.
/// The verified-git artifact bound: a bounded manifest (entries × path
/// bytes), hex OIDs and a well-formed encoded remote ref.
pub(crate) const MAX_VERIFIED_GIT_MANIFEST_ENTRIES: usize = 200_000;
pub(crate) const MAX_VERIFIED_GIT_PATH_BYTES: usize = 4096;

pub(crate) fn is_hex_oid(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn validate_verified_git_artifact(
    artifact: &VerifiedGitArtifact,
) -> Result<(), SessionError> {
    if artifact.task_id == 0 || artifact.revision == 0 || artifact.verification_record == 0 {
        return Err(SessionError::Malformed(
            "ledger verified_git_artifact ids must be non-zero".into(),
        ));
    }
    if artifact.verified_root_digest.is_empty()
        || artifact.verified_root_digest.len() > 128
        || !artifact
            .verified_root_digest
            .bytes()
            .all(|b| b.is_ascii_graphic())
    {
        return Err(SessionError::Malformed(
            "ledger verified_git_artifact verified_root_digest must be 1..=128 printable bytes"
                .into(),
        ));
    }
    if artifact.verified_manifest.is_empty()
        || artifact.verified_manifest.len() > MAX_VERIFIED_GIT_MANIFEST_ENTRIES
    {
        return Err(SessionError::Oversized(
            "ledger verified_git_artifact manifest must be 1..=MAX_VERIFIED_GIT_MANIFEST_ENTRIES entries"
                .into(),
        ));
    }
    let mut previous: Option<&str> = None;
    for entry in &artifact.verified_manifest {
        if entry.path.is_empty() || entry.path.len() > MAX_VERIFIED_GIT_PATH_BYTES {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact manifest path must be 1..=4096 bytes".into(),
            ));
        }
        let path = std::path::Path::new(&entry.path);
        if path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact manifest path must be plain and relative".into(),
            ));
        }
        if let Some(prev) = previous {
            if prev >= entry.path.as_str() {
                return Err(SessionError::Malformed(
                    "ledger verified_git_artifact manifest paths must be strictly sorted".into(),
                ));
            }
        }
        previous = Some(&entry.path);
    }
    for oid in [
        artifact.git_tree_oid.as_deref(),
        artifact.commit_oid.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !is_hex_oid(oid) {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact OID must be 40/64 hex chars".into(),
            ));
        }
    }
    if let Some(r) = artifact.local_ref.as_deref() {
        if !r.starts_with("refs/") || r.len() > 512 || r.contains(' ') {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact local_ref must be a well-formed ref name".into(),
            ));
        }
    }
    if let Some(remote) = artifact.remote_ref.as_deref() {
        // Encoded `<remote>:<refname>@<oid>`; both halves must be
        // well-formed and the oid is the exact commit the push reconciled.
        let (remote_name, rest) = remote.split_once(':').ok_or_else(|| {
            SessionError::Malformed(
                "ledger verified_git_artifact remote_ref must encode <remote>:<ref>@<oid>".into(),
            )
        })?;
        if remote_name.is_empty()
            || remote_name.len() > 256
            || remote_name.contains(|c: char| c.is_whitespace())
        {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact remote name must be well-formed".into(),
            ));
        }
        let (refname, oid) = rest.rsplit_once('@').ok_or_else(|| {
            SessionError::Malformed(
                "ledger verified_git_artifact remote_ref must encode <remote>:<ref>@<oid>".into(),
            )
        })?;
        if !refname.starts_with("refs/") || refname.len() > 512 || refname.contains(' ') {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact remote ref name must be well-formed".into(),
            ));
        }
        if !is_hex_oid(oid) {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact remote_ref oid must be a git oid".into(),
            ));
        }
    }
    if artifact.updated_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger verified_git_artifact updated_ms must be positive".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_run_base_record(record: &RunBaseRecord) -> Result<(), SessionError> {
    if record.run_id.is_empty() || record.run_id.len() > MAX_INTEGRATION_ID_BYTES {
        return Err(SessionError::Malformed(
            "ledger run_base run_id must be 1..=MAX_INTEGRATION_ID_BYTES".into(),
        ));
    }
    if record.workspace_id == 0 || record.worktree_id == 0 {
        return Err(SessionError::Malformed(
            "ledger run_base workspace_id/worktree_id must be non-zero".into(),
        ));
    }
    check_snapshot_digest(&record.snapshot_hash, "snapshot_hash")?;
    if let Some(legacy) = record.legacy_manifest_digest() {
        return Err(SessionError::Malformed(legacy.to_string()));
    }
    let manifest_legacy = record.manifest_digest.len() == MAX_RUN_BASE_DIGEST_BYTES
        && record
            .manifest_digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit());
    if !manifest_legacy {
        return Err(SessionError::Malformed(
            "ledger run_base manifest_digest must be the 64-char hex BLAKE3".into(),
        ));
    }
    if record.root.is_empty() || record.root.len() > MAX_RUN_BASE_ROOT_BYTES {
        return Err(SessionError::Malformed(
            "ledger run_base root must be 1..=MAX_RUN_BASE_ROOT_BYTES".into(),
        ));
    }
    if record.created_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger run_base created_ms must be positive".into(),
        ));
    }
    Ok(())
}

/// Shape bounds of one `integration_txn` row, shared by the appender and
/// the strict decoder.
pub(crate) fn validate_integration_txn(row: &IntegrationTxnRow) -> Result<(), SessionError> {
    let check_hex = |value: &str, what: &str| -> Result<(), SessionError> {
        if value.len() != MAX_RUN_BASE_DIGEST_BYTES || !value.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(SessionError::Malformed(format!(
                "ledger integration_txn {what} must be the 64-char hex BLAKE3"
            )));
        }
        Ok(())
    };
    if row.run_id.is_empty() || row.run_id.len() > MAX_INTEGRATION_ID_BYTES {
        return Err(SessionError::Malformed(
            "ledger integration_txn run_id must be 1..=MAX_INTEGRATION_ID_BYTES".into(),
        ));
    }
    if row.task_id == 0 {
        return Err(SessionError::Malformed(
            "ledger integration_txn task_id must be non-zero".into(),
        ));
    }
    if row.owner_root.is_empty() || row.owner_root.len() > MAX_INTEGRATION_ROOT_BYTES {
        return Err(SessionError::Malformed(
            "ledger integration_txn owner_root must be 1..=MAX_INTEGRATION_ROOT_BYTES".into(),
        ));
    }
    if row.candidate_root.is_empty() || row.candidate_root.len() > MAX_INTEGRATION_ROOT_BYTES {
        return Err(SessionError::Malformed(
            "ledger integration_txn candidate_root must be 1..=MAX_INTEGRATION_ROOT_BYTES".into(),
        ));
    }
    check_snapshot_digest(&row.run_base_snapshot, "run_base_snapshot")?;
    check_snapshot_digest(
        &row.verified_candidate_snapshot,
        "verified_candidate_snapshot",
    )?;
    if !row.sources_digest.is_empty() {
        if let Some(legacy) =
            refuse_legacy_authority_digest("integration_txn sources_digest", &row.sources_digest)
                .err()
        {
            return Err(SessionError::Malformed(legacy.to_string()));
        }
        check_hex(&row.sources_digest, "sources_digest")?;
    }
    if row.paths.len() > MAX_INTEGRATION_TXN_PATHS {
        return Err(SessionError::Oversized(format!(
            "ledger integration_txn stores {} paths (cap {MAX_INTEGRATION_TXN_PATHS})",
            row.paths.len()
        )));
    }
    if row.path_count < row.paths.len() as u64 {
        return Err(SessionError::Malformed(
            "ledger integration_txn path count is smaller than its stored paths".into(),
        ));
    }
    if row.path_count == 0 && !row.paths.is_empty() {
        return Err(SessionError::Malformed(
            "ledger integration_txn with zero paths must carry no path rows".into(),
        ));
    }
    let mut previous: Option<&str> = None;
    for path in &row.paths {
        if path.path.is_empty() || path.path.len() > MAX_INTEGRATION_PATH_BYTES {
            return Err(SessionError::Malformed(
                "ledger integration_txn path must be 1..=MAX_INTEGRATION_PATH_BYTES".into(),
            ));
        }
        if previous.is_some_and(|p| p >= path.path.as_str()) {
            return Err(SessionError::Malformed(
                "ledger integration_txn paths must be strictly ascending".into(),
            ));
        }
        previous = Some(&path.path);
        // The canonical states and the rollback material must be internally
        // consistent on every NEW row: a regular base carries the CAS blob
        // of its exact payload digest, a symlink base its exact literal
        // target, an absent base no material at all. A legacy row
        // (`canonical == false`) is decoded additively and refused at
        // landing time, never validated as if it were authoritative.
        if path.canonical && !path.canonical_ready() {
            return Err(SessionError::Malformed(
                "ledger integration_txn path carries canonical states without their exact rollback material"
                    .into(),
            ));
        }
        if let Some(blob) = &path.rollback_blob {
            check_hex(blob, "path rollback_blob")?;
        }
        if let Some(target) = &path.rollback_link_target {
            if target.len() > faktor_fs::tree_manifest::MAX_TREE_MANIFEST_LINK_BYTES {
                return Err(SessionError::Oversized(
                    "ledger integration_txn symlink rollback target exceeds the manifest bound"
                        .into(),
                ));
            }
        }
    }
    if row.applied_count > row.path_count {
        return Err(SessionError::Malformed(
            "ledger integration_txn applied count exceeds its path count".into(),
        ));
    }
    if row.conflicts.len() > MAX_INTEGRATION_TXN_CONFLICTS {
        return Err(SessionError::Oversized(format!(
            "ledger integration_txn stores {} conflicts (cap {MAX_INTEGRATION_TXN_CONFLICTS})",
            row.conflicts.len()
        )));
    }
    for conflict in &row.conflicts {
        if conflict.is_empty() || conflict.len() > MAX_INTEGRATION_TXN_CONFLICT_BYTES {
            return Err(SessionError::Malformed(
                "ledger integration_txn conflict must be 1..=MAX_INTEGRATION_TXN_CONFLICT_BYTES"
                    .into(),
            ));
        }
    }
    if row.at_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger integration_txn at_ms must be positive".into(),
        ));
    }
    Ok(())
}

pub(crate) fn check_payload_bytes(payload: &LedgerPayload) -> Result<(), SessionError> {
    // Never `unwrap_or_default()` here: a defaulted value would be `Null`
    // (4 bytes) and silently BYPASS the bound this check exists to enforce.
    // The payload is an in-process typed value; a serialization failure is an
    // internal invariant break and must refuse the entry loudly.
    let value = serde_json::to_value(payload).map_err(|e| {
        SessionError::Internal(format!(
            "ledger payload of kind {} cannot be serialized for the bound check: {e}",
            entry_tag_of(payload)
        ))
    })?;
    let bytes = json_bytes(&value);
    if bytes > MAX_LEDGER_ENTRY_BYTES {
        return Err(SessionError::Oversized(format!(
            "ledger entry payload of {bytes} bytes exceeds MAX_LEDGER_ENTRY_BYTES"
        )));
    }
    Ok(())
}

pub(crate) fn malformed_row(what: &str, detail: &str) -> faktor_core::Error {
    SessionError::Malformed(format!("ledger {what} row is corrupt: {detail}")).into()
}

pub(crate) fn conflict_row(what: &str, detail: &str) -> faktor_core::Error {
    SessionError::Conflict(format!("ledger {what} row is inconsistent: {detail}")).into()
}

/// Shape bounds of one `edit_txn_prepared` row. Shared by the appender
/// (rejects BEFORE any byte is journaled) and the open-set reader (a
/// hostile raw-store row must fail there too).
pub(crate) fn validate_edit_txn_prepared(
    txn_id: u64,
    session: &str,
    files: &[EditTxnLedgerFile],
    strategy: &str,
) -> faktor_core::Result<()> {
    if txn_id == 0 {
        return Err(malformed_row(
            "edit_txn_prepared",
            "txn_id must be non-zero",
        ));
    }
    if session.is_empty() || session.len() > MAX_EDIT_TXN_SESSION_BYTES {
        return Err(malformed_row(
            "edit_txn_prepared",
            &format!("session must be 1..={MAX_EDIT_TXN_SESSION_BYTES} bytes"),
        ));
    }
    if files.is_empty() {
        return Err(malformed_row(
            "edit_txn_prepared",
            "at least one staged file is required",
        ));
    }
    if files.len() > MAX_EDIT_TXN_FILES {
        return Err(SessionError::Oversized(format!(
            "edit_txn_prepared of {} files exceeds MAX_EDIT_TXN_FILES",
            files.len()
        ))
        .into());
    }
    if !matches!(
        strategy,
        EDIT_TXN_STRATEGY_ROLL_FORWARD | EDIT_TXN_STRATEGY_ROLL_BACK
    ) {
        return Err(malformed_row(
            "edit_txn_prepared",
            &format!("strategy {strategy:?} is not roll_forward|roll_back"),
        ));
    }
    for f in files {
        check_text(&f.path, "edit txn file path")?;
        if faktor_core::hash::FileHash::from_hex(&f.base_digest).is_none() {
            return Err(malformed_row(
                "edit_txn_prepared",
                &format!(
                    "file {} has a base_digest that is not the 64-char hex BLAKE3",
                    f.path
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn check_edit_txn_path_list(list: &[String], what: &str) -> Result<(), SessionError> {
    if list.len() > MAX_EDIT_TXN_TERMINAL_PATHS {
        return Err(SessionError::Oversized(format!(
            "ledger edit txn {what} of {} paths exceeds MAX_EDIT_TXN_TERMINAL_PATHS",
            list.len()
        )));
    }
    for p in list {
        check_text(p, what)?;
    }
    Ok(())
}
