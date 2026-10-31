// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Fixture-generation CLI for the eval programme (`scripts/run-evals.mjs`
//! drives this; the offline tests call the same library functions).
//!
//! Usage:
//!   eval_fixtures polyglot --root DIR [--seed N]
//!   eval_fixtures legacy --root DIR [--seed N] [--lines N]
//!
//! Prints the corpus manifest (JSON) on stdout. The generator OWNS `--root`
//! (an existing tree is replaced); callers point it at `target/eval-fixtures/…`
//! or a temp dir — generated corpora are never committed.

use std::path::PathBuf;
use std::process::ExitCode;

use faktor_tests_eval::{generate_legacy, generate_polyglot, LEGACY_TARGET_LINES};

fn arg_value(args: &[String], name: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == name)?;
    args.get(idx + 1).cloned()
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kind = args.first().map(String::as_str).unwrap_or("");
    let root = arg_value(&args, "--root").ok_or("missing --root DIR")?;
    let root = PathBuf::from(root);
    let seed: u64 = arg_value(&args, "--seed")
        .map(|v| v.parse().map_err(|_| "bad --seed".to_string()))
        .transpose()?
        .unwrap_or(42);
    let manifest = match kind {
        "polyglot" => generate_polyglot(&root, seed).map_err(|e| e.to_string())?,
        "legacy" => {
            let lines: u64 = arg_value(&args, "--lines")
                .map(|v| v.parse().map_err(|_| "bad --lines".to_string()))
                .transpose()?
                .unwrap_or(LEGACY_TARGET_LINES);
            generate_legacy(&root, seed, lines).map_err(|e| e.to_string())?
        }
        _ => {
            return Err(
                "usage: eval_fixtures polyglot|legacy --root DIR [--seed N] [--lines N]"
                    .to_string(),
            )
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&manifest).expect("manifest serializes")
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("eval_fixtures: {e}");
            ExitCode::FAILURE
        }
    }
}
