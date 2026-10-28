//! `daemon`: daemon construction (`builder`), graph wiring
//! (`wiring`) and the serving lifecycle (`serve`).

use super::*;

pub mod builder;
pub(crate) mod foundation_settings;
pub mod semantic;
pub mod serve;
pub(crate) mod serve_lock;
pub mod wiring;

pub use builder::*;
pub(crate) use semantic::SemanticSources;
pub(crate) use serve::*;
pub(crate) use wiring::*;
