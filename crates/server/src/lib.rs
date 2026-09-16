//! faktor-server — the HTTP/SSE surface of the daemon, speaking the
//! Faktor-native protocol. The UI connection is disposable: turns run
//! detached from any connection and resume from the journal.
//!
//! Auth: the frontend generates `FAKTOR_SERVER_PASSWORD` and passes it via env;
//! every endpoint requires it as `Authorization: Bearer <password>` or
//! `x-faktor-server-password: <password>`, plus the legacy per-start token as
//! a Bearer. The pre-cutover Basic compatibility arm is gone — see
//! [`auth`]'s migration note.

pub mod api;
pub mod auth;
pub mod native;
pub mod permission;

pub use api::{empty_evidence_store, serve, EvidenceStoreHandle, ServerDeps, ServerHandle};
pub use auth::{check_bearer, check_password, AuthToken, ServerPassword};
pub use permission::{ChannelPermissionRequester, PendingPermission};
