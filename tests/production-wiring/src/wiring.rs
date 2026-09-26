//! The public harness the integration tests use to reach the production
//! daemon-core construction path and the production seams inside it.
//!
//! Nothing here constructs an authority of its own: [`build_production_graph`]
//! runs the executable's own `build_daemon`, which opens the durable store +
//! CAS, constructs the ONE process supervisor over that CAS, and runs the
//! private `build_daemon_core` (steps 4-21 of the daemon construction
//! order). The helper accessors only hand back what that graph built. Tests
//! add fake external adapters through the PRODUCTION registration functions
//! ([`register_production_connectors`] is the literal
//! `tools_market::register_commerce_connectors`), so a fake can never widen
//! what the daemon itself wired.

use std::path::Path;
use std::sync::Arc;

use crate::{
    build_daemon, build_daemon_with_acquisition_planner, build_daemon_with_mcp_and_chunks,
};

pub use crate::config::{CommerceCfg, Config};
pub use crate::graph::{DaemonGraph, SemanticCfg};
pub use crate::tools_market::{
    commerce_seams, register_commerce_connectors, CommerceRegistration, CommerceSeams,
};

/// Build the production daemon graph exactly as the executable does through
/// [`crate::build_daemon`] (the `faktor run`/`doctor`/command entry).
pub fn build_production_graph(data_dir: &Path, config: Config) -> Result<DaemonGraph, String> {
    build_daemon(data_dir, Some(config))
}

/// [`build_production_graph`] with an explicit acquisition-planner seam: the
/// daemon's own `build_daemon_core` assembly runs, and the injected planner
/// (a spy) is the ONE the daemon's commerce service invokes. `None` is the
/// production default. This is the seam that proves
/// `AcquisitionPlanning::plan()` really runs on the daemon path.
pub fn build_production_graph_with_planner(
    data_dir: &Path,
    config: Config,
    planner: Option<Arc<dyn faktor_commerce::service::AcquisitionPlanning>>,
) -> Result<DaemonGraph, String> {
    build_daemon_with_acquisition_planner(data_dir, Some(config), planner)
}

/// Build the production daemon graph through the `serve`-family entry
/// (`build_daemon_with_mcp_and_chunks`, whose only extra step is spawning
/// configured MCP servers on the same supervisor before the core runs).
pub async fn build_production_graph_with_mcp(
    data_dir: &Path,
    config: Config,
) -> Result<DaemonGraph, String> {
    build_daemon_with_mcp_and_chunks(data_dir, Some(config), None).await
}

/// The daemon's ONE commerce service when `[commerce]` was enabled at build
/// time (`None` is the disabled-parity graph).
pub fn commerce_service(
    graph: &DaemonGraph,
) -> Option<Arc<faktor_commerce::service::CommerceSourceService>> {
    graph.commerce.clone()
}

/// The tool the daemon's own registry holds under `name` (lazy tools
/// included): the exact `Arc` the runtime would execute, with the exact
/// service/identity/config the daemon injected at construction.
pub fn daemon_tool(graph: &DaemonGraph, name: &str) -> Arc<faktor_agent::Tool> {
    graph
        .agent
        .deps()
        .tools
        .get(name)
        .unwrap_or_else(|| panic!("the daemon registry must hold tool {name:?}"))
}

/// Register connectors through the production registration path over the
/// daemon-built service. `seams` is the production [`CommerceSeams`] struct;
/// tests substitute the transport and/or credential provider while the
/// registration, identity injection, per-source policy and registry wiring
/// stay production code.
pub fn register_production_connectors(
    service: &faktor_commerce::service::CommerceSourceService,
    cfg: &CommerceCfg,
    seams: &CommerceSeams,
) -> CommerceRegistration {
    register_commerce_connectors(service, cfg, seams)
        .unwrap_or_else(|e| panic!("production connector registration refused: {e}"))
}
