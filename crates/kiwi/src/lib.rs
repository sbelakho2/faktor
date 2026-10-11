//! @authority-collapse: authority-bearing
//!
//! faktor-kiwi — the OPTIONAL external (Kiwi-shaped) verification provider.
//!
//! Kiwi is external INDEPENDENT verification (`docs/specs/autonomous-engineering.md`):
//! Faktor submits the engineering objective, acceptance criteria, constraints,
//! invariants, risks and proof obligations, and Kiwi returns evidence or a
//! counterexample per claim. Faktor never tells Kiwi what results to expect
//! beyond the required behavior, and Kiwi is never required for ordinary
//! operation: the crate is DISABLED by default and an unconfigured daemon
//! simply has no external provider (the native baseline degrades honestly).
//!
//! Two hard invariants are structural:
//!
//! 1. **Submission carries the contract, never expected results.** The bounded
//!    [`KiwiJob`] payload is built from the frozen
//!    [`faktor_core::EngineeringContract`] types; prior evaluation state,
//!    confidence and evidence refs are STRIPPED before submission, and the
//!    wire has no field for an expected outcome. Proof obligations are stated
//!    as required behavior.
//! 2. **Ingestion is typed and bounded.** The response is parsed into
//!    [`KiwiReply`] / [`KiwiEvidence`] with strict per-field and per-count
//!    bounds; claim ids must be obligation ids the job actually submitted
//!    (unknown or duplicate ids are typed refusals, never accepted data).
//!
//! The client executes through the daemon's ONE checked egress
//! [`faktor_provider::egress::HttpTransport`] — the same seam every provider
//! adapter uses — under a bounded [`faktor_provider::egress::ResponseBudget`],
//! the configured provider watchdog and caller cancellation. It never
//! constructs a private HTTP client.

pub mod client;
pub mod wire;

pub use client::{
    KiwiProviderConfig, KiwiVerificationProvider, DEFAULT_KIWI_OPERATIONS, DEFAULT_KIWI_TIMEOUT_MS,
    KIWI_PROVIDER_ID, MAX_KIWI_AUTH_ENV_BYTES, MAX_KIWI_ENDPOINT_BYTES, MAX_KIWI_RESPONSE_BYTES,
    MAX_KIWI_TIMEOUT_MS,
};
pub use wire::{
    job_id_for, KiwiEvidence, KiwiJob, KiwiReply, KIWI_SCHEMA_VERSION,
    MAX_KIWI_ACCEPTANCE_CRITERIA, MAX_KIWI_CLAIMS, MAX_KIWI_CONTRACT_ENTRIES,
    MAX_KIWI_EVIDENCE_PER_CLAIM, MAX_KIWI_ID_BYTES, MAX_KIWI_JOB_TEXT_BYTES, MAX_KIWI_TEXT_BYTES,
};
