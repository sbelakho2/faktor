//! Deterministic crash seams for fault certification.
//!
//! Compiled only for the crate's own tests or with the `fault-injection`
//! cargo feature; the production profile compiles this module out entirely
//! (the seam vocabulary has no representation there). Living in its own
//! module keeps the seam payload out of the size-ceilinged `runtime.rs`.

/// Deterministic crash seams (adversarial tests only): execution returns
/// [`crate::runtime::ExecError::InjectedCrashSeam`] at the FIRST matching
/// point, leaving every durable row exactly as a real crash would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashSeam {
    /// After the child session + registry rows exist, before its first
    /// drive is submitted.
    BeforeDrive,
    /// After a child reached its terminal state and its durable rows were
    /// settled, before the parent continuation admits further work.
    AfterChildTerminal,
    /// Controlled merge: fires after the durable in-flight merge record was
    /// written, before any file apply (approve_and_merge crash window).
    AfterMergeRecord,
    /// Controlled merge: fires once `after` files of the apply loop were
    /// processed (any outcome), leaving a partially applied merge that a
    /// replay must reconcile through the CAS.
    MergeApply { after: usize },
    /// Wave A3: fires right after the run's work-item → child assignment
    /// rows committed, BEFORE any child spawn. Re-open must resume with the
    /// SAME child ids (the assignment rows are durable; nothing was minted
    /// at spawn).
    AfterAssignmentsPersisted,
    /// Settlement: fires after the immutable run base was recorded, BEFORE
    /// any child spawn (the run is re-openable from the durable base row).
    AfterRunBaseRecorded,
    /// Settlement: fires after the child change sets were staged and the
    /// candidate was composed, before any verification runs.
    AfterCandidatePrepared,
    /// Settlement: fires after a passing verification record + fact landed,
    /// before the landing transaction's first durable row.
    AfterPreparedVerification,
    /// Settlement: fires after the landing transaction's record-first row
    /// committed, before the first owner write.
    AfterIntegrationTxnRecord,
    /// Settlement: fires once `after` owner paths of the landing apply loop
    /// were processed (any outcome), leaving a recoverable partial landing.
    IntegrationApply { after: usize },
    /// Settlement: fires while a rollback loop is running.
    DuringRollback,
    /// Settlement: fires after the landed owner digest was computed, before
    /// the overall integration record is finalized.
    AfterFinalOwnerSnapshot,
    /// Settlement: fires before `complete_verified_task` is consumed.
    BeforeTaskCompletion,
}
