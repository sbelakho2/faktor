//! `faktor-contracts` — frozen-contract generator and drift gate.
//!
//! Usage:
//!   faktor-contracts check  [--root DIR]   regenerate and compare; exit 1 on drift
//!   faktor-contracts write  [--root DIR]   write the canonical files
//!   faktor-contracts digest [--root DIR]   verify and print only the digest

use std::path::PathBuf;
use std::process::ExitCode;

use faktor_contracts::{check, repo_root, write};

fn usage() -> ExitCode {
    eprintln!(
        "usage: faktor-contracts <check|write|digest> [--root DIR]\n\
         \n\
         check   regenerate every frozen contract from the compiled vocabularies and\n\
         \x20       fail when docs/contracts differs (any rename/reorder is deliberate)\n\
         write   write the freshly generated canonical files under docs/contracts\n\
         digest  run check and print only the sha256 contract digest\n\
         \n\
         --root DIR overrides the repository root (tests use a scratch copy)."
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        return usage();
    };
    if matches!(command, "-h" | "--help" | "help") {
        usage();
        return ExitCode::SUCCESS;
    }
    let mut root: Option<PathBuf> = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("faktor-contracts: --root requires a directory");
                    return ExitCode::from(2);
                };
                root = Some(PathBuf::from(value));
                index += 2;
            }
            other => {
                eprintln!("faktor-contracts: unknown argument '{other}'");
                return usage();
            }
        }
    }
    let root = root.unwrap_or_else(repo_root);

    match command {
        "check" | "digest" => {
            let report = match check(&root) {
                Ok(report) => report,
                Err(err) => {
                    eprintln!("contracts: ERROR {err}");
                    return ExitCode::FAILURE;
                }
            };
            if report.ok {
                if command == "digest" {
                    println!("{}", report.digest);
                } else {
                    println!(
                        "contracts: PASS ({} frozen files match the compiled vocabularies, digest {})",
                        report.checked, report.digest
                    );
                }
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "contracts: FAIL ({} of {} frozen files diverged from the compiled vocabularies)",
                    report.mismatches.len(),
                    report.checked
                );
                for mismatch in &report.mismatches {
                    eprintln!("  {}", mismatch.detail);
                }
                eprintln!(
                    "contracts: run `cargo run -p faktor-contracts -- write` only for a \
                     deliberate contract change"
                );
                ExitCode::FAILURE
            }
        }
        "write" => match write(&root) {
            Ok(digest) => {
                println!("contracts: wrote {} (digest {digest})", root.display());
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("contracts: ERROR {err}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("faktor-contracts: unknown command '{other}'");
            usage()
        }
    }
}
