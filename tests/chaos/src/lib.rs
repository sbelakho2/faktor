//! Shared-harness audit rows HAR-01 (local chaos proxy between the daemon
//! and a provider) and HAR-04 (loom/shuttle-style interleaving) — see
//! `docs/specs/test-programme-2026-10-18/findings.md` for the row
//! definitions. This crate is a TEST HARNESS ONLY: nothing here is ever
//! selected by a production build (workspace member `tests/chaos`).
//!
//! Design:
//!
//! - [`proxy`] — a pure std/tokio TCP chaos proxy that forwards to a real
//!   provider endpoint or a local mock and injects one deterministic fault
//!   class per run (connection kill mid-SSE, byte truncation, SSE frame
//!   reorder, delay burst, RST after N bytes, gzip-truncated body). Fault
//!   parameters come from a fixed seed/flag table, so the same fault
//!   sequence replays byte-for-byte on every run.
//! - [`mock`] — an in-repo, offline OpenAI-compatible HTTP/SSE mock
//!   provider (no new heavy deps: raw tokio TCP + hand-rolled HTTP/1.1).
//! - [`daemon`] — spawns the REAL `faktor-cli` binary into a temp data
//!   dir, waits for the frozen startup line, drives the native HTTP API
//!   with bounded deadlines, counts budget-ledger rows in the store DB
//!   (read-only) and audits the process table for orphans.
//! - [`interleaving`] — a deterministic step scheduler that enumerates
//!   every interleaving of small concurrent programs (no external loom
//!   dependency).
//!
//! Coverage honesty for the interleaving row lives in the HAR-04 module
//! docs (`har04.rs`): operations the real state machines expose only as
//! indivisible transactions are driven through the store's deterministic
//! crash seams at enumerated durability points instead.
// TEST-SEPARATION: oracle=independent-reference isolation=separate-process,isolated-data-dir

#![cfg_attr(
    not(test),
    allow(dead_code, unused_imports, unused_variables, unused_mut)
)] // test-harness crate: the lib view exists only for clippy

pub mod daemon;
pub mod interleaving;
pub mod mock;
pub mod proxy;

#[cfg(test)]
mod har01;
#[cfg(test)]
mod har04;
