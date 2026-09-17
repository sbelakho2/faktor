//! The durable billing-report maintenance schedule: periodically reports the
//! folded usage of the CURRENT period through the vendor adapter, with
//! record-before-send idempotency, jittered exponential backoff and the
//! BOUNDED downtime catch-up (missed periods re-planned oldest first up to a
//! configured cap; older periods marked skipped with a durable audit row).
//!
//! The schedule is cloud-owned ([`faktor_cloud::report_schedule`]): it
//! needs the durable [`faktor_cloud::BillingStore`], the vendor adapter and
//! the clock — all cloud types. This module re-exports it so every daemon
//! call site and test keeps one import surface.
pub use faktor_cloud::report_schedule::*;

#[cfg(test)]
#[path = "billing_report_tests.rs"]
mod billing_report_tests;
