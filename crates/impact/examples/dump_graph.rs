//! Serialize the selector projection: `cargo run -p faktor-impact --example dump_graph -- <root>`.
//! This is the bootstrap/verification path; the product generator invokes
//! `faktor impact graph --selector` (same builder, same document).

use std::path::PathBuf;

use faktor_impact::{GraphBuilder, Revision};

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let revision = Revision::read_head(&root);
    let mut builder = GraphBuilder::new(&root, revision).expect("workspace is readable");
    let graph = builder.build().expect("graph builds");
    let selector = graph.selector_projection().expect("projection seals");
    println!("{}", selector.to_json_compact().expect("graph serializes"));
}
