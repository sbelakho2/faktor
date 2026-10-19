use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::path::Path;

struct NoDup(Value);

impl<'de> Deserialize<'de> for NoDup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(NoDupVisitor)
    }
}

struct NoDupVisitor;

impl<'de> Visitor<'de> for NoDupVisitor {
    type Value = NoDup;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(NoDup(Value::Bool(value)))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(NoDup(Value::Number(value.into())))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(NoDup(Value::Number(value.into())))
    }
    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(|number| NoDup(Value::Number(number)))
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(NoDup(Value::String(value.to_string())))
    }
    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(NoDup(Value::String(value)))
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(NoDup(Value::Null))
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(NoDup(Value::Null))
    }
    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element::<NoDup>()? {
            items.push(item.0);
        }
        Ok(NoDup(Value::Array(items)))
    }
    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut output = serde_json::Map::new();
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            let value = map.next_value::<NoDup>()?;
            output.insert(key, value.0);
        }
        Ok(NoDup(Value::Object(output)))
    }
}

pub fn repo_description(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("usage: faktor-ci-tools github repo-description FILE");
        return 2;
    };
    let text = match std::fs::read_to_string(Path::new(path)) {
        Ok(text) => text,
        Err(_) => {
            println!();
            return 1;
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            println!();
            return 1;
        }
    };
    println!(
        "{}",
        value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
    );
    0
}

fn status_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn status_verdict(args: &[String]) -> i32 {
    if args.len() < 3 {
        eprintln!("usage: faktor-ci-tools github status-verdict FILE COMMIT CONTEXT");
        return 2;
    }
    let (path, commit, context) = (&args[0], &args[1], &args[2]);
    let document = std::fs::read_to_string(Path::new(path))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let Some(document) = document.filter(Value::is_object) else {
        println!("violation|forge-status-unreadable");
        return 0;
    };
    if let Some(sha) = document.get("sha").and_then(Value::as_str) {
        if sha != commit {
            println!("violation|forge-status-sha-mismatch");
            return 0;
        }
    }
    let statuses = document
        .get("statuses")
        .and_then(Value::as_array)
        .map(|items| items.as_slice())
        .unwrap_or(&[]);
    let matching: Vec<&Value> = statuses
        .iter()
        .filter(|status| status.get("context").and_then(Value::as_str) == Some(context))
        .collect();
    let Some(latest) = matching.iter().max_by_key(|status| {
        status_text(&status.get("updated_at").cloned().unwrap_or(Value::Null))
    }) else {
        println!("violation|forge-status-absent");
        return 0;
    };
    let state = latest
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match state {
        "pending" => println!("violation|forge-status-pending"),
        "success" => println!("ok|forge-status-success"),
        "failure" | "error" => println!("violation|forge-status-not-success"),
        _ => println!("violation|forge-status-unknown"),
    }
    0
}

pub fn ci_cert_manifest(args: &[String]) -> i32 {
    if args.len() < 3 {
        eprintln!("usage: faktor-ci-tools github ci-cert-manifest FILE COMMIT TREE");
        return 2;
    }
    let (path, commit, tree) = (&args[0], &args[1], &args[2]);
    if !Path::new(path).is_file() {
        println!("absent|ci-certification-absent|no CI certification manifest at {path}");
        return 0;
    }
    let text = std::fs::read_to_string(Path::new(path)).unwrap_or_default();
    let parsed = serde_json::from_str::<NoDup>(&text)
        .map(|value| value.0)
        .and_then(|value| {
            if value.is_object() {
                Ok(value)
            } else {
                Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "not a JSON object",
                )))
            }
        });
    let manifest = match parsed {
        Ok(manifest) => manifest,
        Err(error) => {
            println!("malformed|ci-certification-unreadable|{error}");
            return 0;
        }
    };
    let schema = manifest.get("schema").and_then(Value::as_str);
    let workflow = manifest.get("workflow").and_then(Value::as_str);
    let status = manifest.get("status").and_then(Value::as_str);
    let file_commit = manifest.get("commit").and_then(Value::as_str);
    let file_tree = manifest.get("tree").and_then(Value::as_str);
    let pipeline = manifest
        .get("pipeline")
        .filter(|value| !value.is_null())
        .or_else(|| manifest.get("run_id").filter(|value| !value.is_null()))
        .map(status_text)
        .unwrap_or_default();
    if schema != Some("faktor-ci-certification/v2") {
        println!(
            "mismatch|ci-certification-schema|schema={:?}",
            schema.unwrap_or("")
        );
    } else if workflow != Some("trusted") {
        println!(
            "mismatch|ci-certification-workflow|workflow={:?}",
            workflow.unwrap_or("")
        );
    } else if !matches!(status, Some("pass") | Some("passed")) {
        println!(
            "mismatch|ci-certification-status|status={:?}",
            status.unwrap_or("")
        );
    } else if file_commit != Some(commit.as_str()) {
        println!(
            "mismatch|ci-certification-commit-mismatch|manifest={} shipped={commit}",
            file_commit.unwrap_or("")
        );
    } else if file_tree != Some(tree.as_str()) {
        println!(
            "mismatch|ci-certification-tree-mismatch|manifest={} shipped={tree}",
            file_tree.unwrap_or("")
        );
    } else {
        println!("match||pipeline={pipeline}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_keys_are_rejected() {
        let parsed = serde_json::from_str::<NoDup>(r#"{"a":1,"a":2}"#);
        assert!(parsed.is_err());
    }

    #[test]
    fn nested_duplicate_keys_are_rejected() {
        let parsed = serde_json::from_str::<NoDup>(r#"{"a":{"b":1,"b":2}}"#);
        assert!(parsed.is_err());
    }
}
