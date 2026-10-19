use crate::util;
use serde_json::{json, Value};
use std::path::Path;

fn text(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(text)) => json!(text),
        Some(Value::Null) | None => Value::Null,
        Some(other) => other.clone(),
    }
}

pub fn run(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: faktor-ci-tools sbom METADATA_JSON NOW_MS");
        return 2;
    }
    let metadata = match util::read_json(Path::new(&args[0])) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("sbom: {error}");
            return 1;
        }
    };
    let generated_at_ms: i64 = args[1].parse().unwrap_or(0);
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let workspace_members = metadata
        .get("workspace_members")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let mut rows: Vec<(String, String, Value)> = packages
        .iter()
        .map(|package| {
            let name = package
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let version = package
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let row = json!({
                "name": text(package.get("name")),
                "version": text(package.get("version")),
                "license": text(package.get("license")),
                "source": text(package.get("source")),
            });
            (name, version, row)
        })
        .collect();
    rows.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let output = json!({
        "schema": "faktor-sbom/1",
        "generated_at_ms": generated_at_ms,
        "source": "cargo metadata --format-version 1 --locked",
        "package_count": packages.len(),
        "workspace_members": workspace_members,
        "packages": rows.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".into())
    );
    0
}
