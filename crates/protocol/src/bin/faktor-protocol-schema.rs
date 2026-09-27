//! Emits the canonical protocol schema artifact (audit 25).
//!
//! Usage:
//!   faktor-protocol-schema                 # artifact JSON on stdout
//!   faktor-protocol-schema --out PATH      # write PATH
//!   faktor-protocol-schema --check PATH    # compare PATH to the emitter
//!
//! `--check` is the artifact half of the protocol codegen drift gate: it
//! exits 1 when the checked-in artifact differs from what this binary emits.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out: Option<PathBuf> = None;
    let mut check: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--out" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    eprintln!("faktor-protocol-schema: --out needs a path");
                    return ExitCode::from(2);
                };
                out = Some(PathBuf::from(path));
            }
            "--check" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    eprintln!("faktor-protocol-schema: --check needs a path");
                    return ExitCode::from(2);
                };
                check = Some(PathBuf::from(path));
            }
            other => {
                eprintln!("faktor-protocol-schema: unknown argument {other}");
                return ExitCode::from(2);
            }
        }
        index += 1;
    }

    let text = faktor_protocol::schema::canonical_json();

    if let Some(path) = check {
        let existing = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                eprintln!(
                    "faktor-protocol-schema: cannot read {}: {e}",
                    path.display()
                );
                return ExitCode::from(1);
            }
        };
        if existing != text {
            eprintln!(
                "faktor-protocol-schema: {} drifted from the emitter \
                 (regenerate with `faktor-protocol-schema --out {}`)",
                path.display(),
                path.display()
            );
            return ExitCode::from(1);
        }
        println!(
            "faktor-protocol-schema: {} matches the emitter",
            path.display()
        );
        return ExitCode::SUCCESS;
    }

    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "faktor-protocol-schema: cannot create {}: {e}",
                    parent.display()
                );
                return ExitCode::from(1);
            }
        }
        if let Err(e) = std::fs::write(&path, &text) {
            eprintln!(
                "faktor-protocol-schema: cannot write {}: {e}",
                path.display()
            );
            return ExitCode::from(1);
        }
        println!("faktor-protocol-schema: wrote {}", path.display());
        return ExitCode::SUCCESS;
    }

    print!("{text}");
    ExitCode::SUCCESS
}
