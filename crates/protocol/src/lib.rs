//! faktor-protocol — Faktor-owned protocol types.
//!
//! The daemon's native surface (`docs/native-protocol.md`) and the shared
//! conversation/projection shapes live in [`native`]; API error envelopes
//! live in [`error`]. There is no foreign wire-compatibility contract.

pub mod error;
pub mod native;

pub use error::ApiError;
pub use native::*;
