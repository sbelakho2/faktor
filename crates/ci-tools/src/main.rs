mod capability;
mod fixtures;
mod flake;
mod github;
mod iana;
mod json_cmd;
mod lane_marker;
mod license;
mod manifest;
mod mock_woodpecker;
mod publish;
mod sbom;
mod util;
mod visual;
mod zip;

fn usage() {
    println!(
        "faktor-ci-tools — deterministic CI/certification helpers\n\n\
         Commands:\n\
           visual-baseline selftest | --platform P --file F [--class C]\n\
           capability-tests [--manifest PATH] | selftest\n\
           flake-tolerance run --lane ID [options] -- cargo test ... | selftest\n\
           lane-marker write --lane ID [options]\n\
           publish-status publish|selftest\n\
           iana-table [--check]\n\
           mock-woodpecker --port N --state-dir DIR\n\
           free-port\n\
           json MODE FILE [ARG...]\n\
           json-get FILE PATH [DEFAULT]\n\
           wo-body secret|secret-value|settings|cron ...\n\
           github repo-description|status-verdict|ci-cert-manifest ...\n\
           ci-manifest | manifest-problems|digest|check|has-problem ...\n\
           attestation-count | release-attested-count | keys-merge | strip-signature ...\n\
           fixture pipeline-json|detail-json|details-json|state|logs-json|repo-json|status-json ...\n\
           sbom METADATA_JSON NOW_MS\n\
           license-policy ROOT METADATA_JSON\n\
           make-zip OUT FILE..."
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        usage();
        std::process::exit(2);
    };
    let rest = &args[1..];
    let code = match command {
        "-h" | "--help" | "help" => {
            usage();
            0
        }
        "visual-baseline" | "check-visual-baseline" => visual::run(rest),
        "capability-tests" | "check-capability-tests-compiled" => capability::run(rest),
        "flake-tolerance" => flake::run_command(rest),
        "lane-marker" => lane_marker::run(rest),
        "publish-status" => publish::run(rest),
        "iana-table" | "generate-iana-network-table" => iana::run(rest),
        "mock-woodpecker" => mock_woodpecker::run(rest),
        "free-port" => mock_woodpecker::free_port(),
        "json" | "json-validate" => json_cmd::json(rest),
        "json-get" => json_cmd::json_get(rest),
        "wo-body" => json_cmd::wo_body(rest),
        "github" => match rest.first().map(String::as_str) {
            Some("repo-description") => github::repo_description(&rest[1..]),
            Some("status-verdict") => github::status_verdict(&rest[1..]),
            Some("ci-cert-manifest") => github::ci_cert_manifest(&rest[1..]),
            _ => {
                eprintln!(
                    "usage: faktor-ci-tools github repo-description|status-verdict|ci-cert-manifest ..."
                );
                2
            }
        },
        "ci-manifest" => manifest::ci_manifest(),
        "manifest-problems" => manifest::manifest_problems(rest),
        "manifest-digest" => manifest::manifest_digest(rest),
        "manifest-check" => manifest::manifest_check(rest),
        "manifest-has-problem" => manifest::manifest_has_problem(rest),
        "attestation-count" => manifest::attestation_count(rest),
        "release-attested-count" => manifest::release_attested_count(rest),
        "keys-merge" => manifest::keys_merge(rest),
        "strip-signature" => manifest::strip_signature(rest),
        "fixture" => match rest.first().map(String::as_str) {
            Some(subcommand) => fixtures::run(subcommand, &rest[1..]),
            None => {
                eprintln!("usage: faktor-ci-tools fixture COMMAND ...");
                2
            }
        },
        "sbom" => sbom::run(rest),
        "license-policy" => license::run(rest),
        "make-zip" => zip::run(rest),
        other => {
            eprintln!("faktor-ci-tools: unknown command '{other}'");
            usage();
            2
        }
    };
    std::process::exit(code);
}
