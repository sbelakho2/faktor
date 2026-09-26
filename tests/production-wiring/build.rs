//! Copies the `faktor-cli` binary source into `OUT_DIR` so it can be spliced
//! into this crate at the CRATE ROOT (via `include!`) with the exact same
//! module topology it has in the binary. Copying preserves the included
//! file's directory for `mod <name>;` resolution, so `crates/cli/src/*.rs`
//! land in `OUT_DIR/*.rs` and every `mod` declaration resolves to its copy.
//!
//! The ONLY transformation is on `main.rs`: its leading `//!` inner doc
//! block is converted to ordinary `//` comments, because rustc rejects inner
//! doc comments in `include!`d files even at the top of the crate. No code
//! byte is otherwise changed, and `rerun-if-changed` keeps the copy in sync
//! with the real sources on every build.

use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let cli_src = manifest.join("../../crates/cli/src");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("out dir"));
    println!("cargo:rerun-if-changed={}", cli_src.display());

    let mut copied = 0usize;
    for entry in std::fs::read_dir(&cli_src).expect("cli src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path.file_name().expect("file name");
        let text = std::fs::read_to_string(&path).expect("read cli source");
        let text = absolutize_includes(&text, &cli_src);
        let rendered = if name == "main.rs" {
            strip_leading_inner_docs(&text)
        } else {
            text
        };
        std::fs::write(out_dir.join(name), rendered).expect("write cli source copy");
        copied += 1;
    }
    assert!(
        copied > 0,
        "no cli sources copied from {}",
        cli_src.display()
    );
}

/// Rewrite relative `include_str!`/`include_bytes!` paths to absolute paths
/// into the REAL `crates/cli/src` tree: the copies live in `OUT_DIR`, so the
/// original relative paths (e.g. `../../server/src/api.rs`, reachable from
/// the real source directory) would otherwise resolve against `OUT_DIR` and
/// fail whenever a `#[cfg(test)]` module containing them is compiled.
fn absolutize_includes(text: &str, cli_src: &Path) -> String {
    let mut rendered = text.to_string();
    for macro_name in ["include_str!", "include_bytes!"] {
        let mut search_from = 0usize;
        while let Some(rel) = rendered[search_from..].find(macro_name) {
            let macro_at = search_from + rel;
            let after = macro_at + macro_name.len();
            let Some(open_rel) = rendered[after..].find('"') else {
                break;
            };
            let path_start = after + open_rel + 1;
            let Some(close_rel) = rendered[path_start..].find('"') else {
                break;
            };
            let path_end = path_start + close_rel;
            let path_text = rendered[path_start..path_end].to_string();
            if path_text.starts_with('/') {
                search_from = path_end;
            } else {
                let absolute = cli_src.join(&path_text);
                let absolute = absolute.canonicalize().unwrap_or(absolute);
                let absolute = absolute.to_string_lossy().into_owned();
                rendered.replace_range(path_start..path_end, &absolute);
                search_from = path_start + absolute.len();
            }
        }
    }
    rendered
}

/// Convert the leading `//!` inner-doc run into `//` comments (the rest of
/// the file is untouched, including any later inner attributes inside
/// modules).
fn strip_leading_inner_docs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_leading_docs = true;
    for line in text.split_inclusive('\n') {
        if in_leading_docs && line.starts_with("//!") {
            out.push_str("//");
            out.push_str(&line[3..]);
        } else {
            in_leading_docs = false;
            out.push_str(line);
        }
    }
    out
}
