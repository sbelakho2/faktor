use std::process::ExitCode;

use faktor_supply_engineering_traps::{
    case_ids, run_case, run_suite, CaseReport, SuiteReport, MAX_CASES, SUITE_SCHEMA,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    Json,
    Text,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut output = Output::Json;
    let mut pretty = true;
    let mut case = None;
    let mut list = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => output = Output::Json,
            "--text" => output = Output::Text,
            "--pretty" => pretty = true,
            "--compact" => pretty = false,
            "--list" => list = true,
            "--case" => {
                index += 1;
                let id = args
                    .get(index)
                    .ok_or_else(|| "--case requires a case id".to_string())?;
                case = Some(id.clone());
            }
            "-h" | "--help" => {
                help();
                return Ok(ExitCode::SUCCESS);
            }
            other => return Err(format!("unknown argument {other:?}; use --help")),
        }
        index += 1;
    }

    if list {
        for id in case_ids() {
            println!("{id}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    let report = match case {
        Some(id) => {
            SuiteReport::from_cases(vec![run_case(&id).map_err(|error| error.to_string())?])
                .map_err(|error| error.to_string())?
        }
        None => run_suite().map_err(|error| error.to_string())?,
    };

    match output {
        Output::Json => {
            let text = report.to_json(pretty).map_err(|error| error.to_string())?;
            println!("{text}");
        }
        Output::Text => print_text(&report),
    }

    Ok(if report.summary.all_passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn print_text(report: &SuiteReport) {
    println!(
        "faktor supply/engineering traps {} ({} cases, max {MAX_CASES})",
        report.version,
        report.cases.len()
    );
    for case in &report.cases {
        print_case(case);
    }
    println!(
        "summary: {}/{} passed; solved={} rejected={} refused={} research_required={} unavailable={} escalated={}",
        report.summary.passed,
        report.summary.total,
        report.summary.solved,
        report.summary.rejected,
        report.summary.refused,
        report.summary.research_required,
        report.summary.unavailable,
        report.summary.escalated
    );
    if !report.summary.failed_cases.is_empty() {
        println!("failed: {}", report.summary.failed_cases.join(", "));
    }
}

fn print_case(case: &CaseReport) {
    let status = if case.passed() { "PASS" } else { "FAIL" };
    println!(
        "{status} {:>2}/8 {} [{}] {}",
        case.metrics.count(),
        case.id,
        case.expected.as_str(),
        case.title
    );
    println!("      shallow: {}", case.shallow_claim);
    println!(
        "      outcome: {:?}; missing={} boundaries={}",
        case.outcome.kind(),
        case.missing_info.len(),
        case.boundary_conditions.len()
    );
    println!("      experiment: {}", case.falsifiable_test.procedure);
}

fn help() {
    println!("faktor-supply-engineering-traps {SUITE_SCHEMA}");
    println!("Usage: faktor-supply-engineering-traps [--json|--text] [--pretty|--compact] [--case ID] [--list]");
    println!();
    println!("  --json       machine-readable report (default)");
    println!("  --text       human summary");
    println!("  --pretty     pretty JSON (default)");
    println!("  --compact    one-line JSON");
    println!("  --case ID    run exactly one case");
    println!("  --list       list the fixed case ids");
    println!("  -h, --help   show this help");
    println!();
    println!("Exit codes: 0 all cases pass, 1 a case fails, 2 CLI error.");
}
