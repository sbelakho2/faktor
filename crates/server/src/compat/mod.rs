//! v7.5.6 / SDK compatibility glue (frozen migration surface).
//!
//! These handlers are the deliberately separate legacy surface (architecture
//! §16): `sdk` carries the SDK-shaped REST aliases, `v756` the frozen wire
//! contract. Compatibility code may depend on the native layer's shared
//! glue; the reverse dependency is forbidden and source-scan tested.

use std::sync::Arc;

use crate::api::AppState;
use crate::native::{PromptExecutionService, PromptReceipt, PromptRequest};

pub(crate) mod sdk;
pub(crate) mod v756;

pub(crate) use sdk::*;
pub(crate) use v756::*;

/// Translate one ordinary prompt DTO into the ONE product execution entry
/// ([`PromptExecutionService::prompt`]): the prompt becomes an in-session
/// run through the daemon's TaskExecutor (durable run + linkage rows,
/// detached recoverable drive), so legacy chat travels the same execution
/// authority as every other surface. Compatibility code only translates
/// DTO fields here — it never drives `AgentRuntime` itself.
///
/// Returns the executor's receipt (the durable run id + the true queued
/// state + the real session op id); the caller decides how its protocol
/// waits for the turn.
///
/// The compat surfaces impose NO mutation policy: the executor's daemon
/// default applies, and every mutating prompt executes in an isolated
/// candidate. The old per-surface direct-mutation forcing was deleted
/// (P0 isolation); no adapter may select a mutation target any more.
pub(crate) async fn submit_and_run(
    state: &AppState,
    session: faktor_core::id::SessionId,
    prompt: &str,
    files: &[String],
    model: Option<String>,
) -> Result<PromptReceipt, faktor_orchestrator::runtime::ExecError> {
    let service: Arc<PromptExecutionService> = PromptExecutionService::from_state(state);
    let request = PromptRequest {
        prompt: prompt.to_string(),
        files: files.to_vec(),
        model,
        ..Default::default()
    };
    service.prompt(session, request).await
}

/// The states that mean "a logical turn is occupying the session machine"
/// (the wait condition of `POST /session/{id}/message`). Re-exported from
/// the native prompt service so every adapter waits on ONE definition.
pub(crate) use crate::native::turn_machine_busy;
