//! Daemon configuration (faktor-plus.json). Provider keys are referenced by
//! environment variable name — the runtime never stores secrets.

use std::path::Path;
use std::sync::Arc;

use faktor_core::model::{
    BillingOrigin, MicroUsdPerMillionTokens, ModelCapabilities, PriceQuote, RoutingMode,
};
use faktor_orchestrator::runtime::task_executor::MutationMode;
use faktor_provider::catalog::{BillingOriginProvider, PricingOverrides, QualityOverrideProvider};
use faktor_provider::egress::HttpTransport;
use faktor_provider::Provider;
use faktor_sandbox::{NetworkGate, SandboxGuarantee, SandboxPolicy};
use faktor_security::secret::SecretValue;

mod core;
pub use core::*;
mod completion;
pub use completion::*;
mod tasks;
pub use tasks::*;
mod efficiency;
pub use efficiency::*;
mod cloud;
pub use cloud::*;
mod scm;
pub use scm::*;
mod updater;
pub use updater::*;
mod billing;
pub use billing::*;
mod workers;
pub use workers::*;
mod worker_plane;
pub use worker_plane::*;
mod worker_node;
pub use worker_node::*;
mod enterprise;
pub use enterprise::*;
mod embedding;
pub use embedding::*;
mod verification;
pub use verification::*;
mod sandbox;
pub use sandbox::*;
mod mcp;
pub use mcp::*;
mod provider;
pub use provider::*;
mod commerce;
pub use commerce::*;

#[cfg(test)]
mod tests;
