// Cross-layer production-wiring certification (`faktor-tests-production-wiring`).
//
// This crate exists to answer one question with tests instead of prose:
// **does the production daemon graph actually supply the identity, config
// and authority each subsystem needs — or do the subsystems only work when
// a test hand-constructs them?**
//
// The library IS the `faktor-cli` binary source compiled at this crate's
// root: `build.rs` copies `crates/cli/src/*.rs` into `OUT_DIR` (only the
// binary's leading `//!` doc block is turned into `//`, since rustc rejects
// inner docs in `include!`d files) and the `include!` below splices the copy
// in, so the harness calls the EXACT private `build_daemon_core` the
// executable runs — the same one-supervisor, one-store,
// one-commerce-service, one-orchestrator assembly — with no
// re-implementation. Because the copy lives at the crate root, every
// `crate::<module>` path inside the cli sources resolves to the same
// module, and type identity is exact rather than aliased.
//
// The included binary's own `#[cfg(test)] mod tests` is excluded: the
// library is built without `cfg(test)` (`[lib] test = false` in
// Cargo.toml) and integration tests link it as an ordinary dependency.
//
// Integration tests in `tests/` drive the returned `DaemonGraph` through
// the graph's real authorities (the agent's registered `source_market`
// tool, the daemon's `CommerceSourceService`, the daemon's `TaskExecutor`),
// substituting only documented external seams (loopback mock servers /
// fixture transports / fake GitHub) — never a hand-built subsystem.
#![allow(dead_code)]

include!(concat!(env!("OUT_DIR"), "/main.rs"));

pub mod wiring;
