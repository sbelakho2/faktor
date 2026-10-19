use crate::util;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Debug)]
enum TomlValue {
    Str(String),
    Array(Vec<TomlValue>),
    Other,
}

type Toml = BTreeMap<String, TomlValue>;

fn strip_comment(line: &str) -> String {
    let mut output = String::new();
    let mut in_string = false;
    let mut escaped = false;
    for ch in line.chars() {
        if in_string {
            output.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            output.push(ch);
        } else if ch == '#' {
            break;
        } else {
            output.push(ch);
        }
    }
    output
}

fn balanced_array(text: &str) -> bool {
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

fn parse_string(raw: &str) -> Result<(String, usize), String> {
    let mut chars = raw.char_indices();
    let Some((_, '"')) = chars.next() else {
        return Err("expected string".into());
    };
    let mut output = String::new();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => return Ok((output, index + 1)),
            '\\' => {
                let Some((_, escaped)) = chars.next() else {
                    return Err("unterminated escape".into());
                };
                match escaped {
                    'n' => output.push('\n'),
                    't' => output.push('\t'),
                    'r' => output.push('\r'),
                    '"' => output.push('"'),
                    '\\' => output.push('\\'),
                    other => {
                        output.push('\\');
                        output.push(other);
                    }
                }
            }
            other => output.push(other),
        }
    }
    Err("unterminated string".into())
}

fn parse_array(raw: &str) -> Result<Vec<TomlValue>, String> {
    let Some(inner) = raw
        .strip_prefix('[')
        .and_then(|text| text.strip_suffix(']'))
    else {
        return Err("malformed array".into());
    };
    let mut values = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        if rest.starts_with(',') {
            rest = rest[1..].trim();
            continue;
        }
        if rest.starts_with('"') {
            let (value, consumed) = parse_string(rest)?;
            values.push(TomlValue::Str(value));
            rest = rest[consumed..].trim();
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let token = rest[..end].trim();
            if !token.is_empty() {
                values.push(parse_scalar(token)?);
            }
            rest = rest[end..].trim();
        }
    }
    Ok(values)
}

fn parse_scalar(raw: &str) -> Result<TomlValue, String> {
    let raw = raw.trim();
    if raw.starts_with('"') {
        return parse_string(raw).map(|(value, _)| TomlValue::Str(value));
    }
    if raw.starts_with('[') {
        return parse_array(raw).map(TomlValue::Array);
    }
    match raw {
        "true" | "false" => Ok(TomlValue::Other),
        _ => raw
            .replace('_', "")
            .parse::<f64>()
            .map(|_| TomlValue::Other)
            .map_err(|_| format!("unsupported TOML value {raw:?}")),
    }
}

fn parse_toml(text: &str) -> Result<Toml, String> {
    let mut output = Toml::new();
    let mut section = String::new();
    let mut lines = text.lines();
    while let Some(raw_line) = lines.next() {
        let line = strip_comment(raw_line);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut value = value.trim().to_string();
        if value.starts_with('[') && !balanced_array(&value) {
            for next in lines.by_ref() {
                value.push('\n');
                value.push_str(&strip_comment(next));
                if balanced_array(&value) {
                    break;
                }
            }
        }
        let parsed = parse_scalar(&value)?;
        let full = if section.is_empty() {
            key.to_string()
        } else {
            format!("{section}.{key}")
        };
        output.insert(full, parsed);
    }
    Ok(output)
}

fn get<'a>(toml: &'a Toml, key: &str) -> Option<&'a TomlValue> {
    toml.get(key)
}

fn strings(toml: &Toml, key: &str) -> Vec<String> {
    match get(toml, key) {
        Some(TomlValue::Array(values)) => values
            .iter()
            .filter_map(|value| match value {
                TomlValue::Str(value) => Some(value.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn string(toml: &Toml, key: &str, fallback: &str) -> String {
    match get(toml, key) {
        Some(TomlValue::Str(value)) => value.clone(),
        _ => fallback.to_string(),
    }
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
}

#[derive(Debug)]
enum Node {
    Lic(String),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
}

fn tokenize(expression: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = expression.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if matches!(ch, '(' | ')' | '/') {
            tokens.push(ch.to_string());
            continue;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '+' | '-') {
            let mut end = index + ch.len_utf8();
            while let Some((next_index, next)) = chars.peek().copied() {
                if next.is_ascii_alphanumeric() || matches!(next, '.' | '+' | '-') {
                    end = next_index + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let token = &expression[index..end];
            tokens.push(if token.chars().all(char::is_alphabetic) {
                token.to_uppercase()
            } else {
                token.to_string()
            });
            continue;
        }
        return None;
    }
    Some(tokens)
}

struct Parser {
    tokens: Vec<String>,
    index: usize,
}

impl Parser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.index).map(String::as_str)
    }
    fn take(&mut self) -> Option<String> {
        let value = self.tokens.get(self.index).cloned();
        self.index += 1;
        value
    }
    fn primary(&mut self) -> Result<Node, String> {
        let token = self.take().ok_or_else(|| "unexpected end".to_string())?;
        let mut node = if token == "(" {
            let node = self.expr()?;
            if self.take().as_deref() != Some(")") {
                return Err("unbalanced parentheses".into());
            }
            node
        } else if matches!(token.as_str(), ")" | "AND" | "OR" | "WITH" | "/") {
            return Err(format!("unexpected token {token:?}"));
        } else {
            Node::Lic(token)
        };
        if self.peek() == Some("WITH") {
            self.take();
            let exception = self.take().ok_or_else(|| "malformed WITH".to_string())?;
            let Node::Lic(license) = &node else {
                return Err("malformed WITH".into());
            };
            if matches!(exception.as_str(), ")" | "AND" | "OR" | "WITH" | "/") {
                return Err("malformed WITH".into());
            }
            node = Node::Lic(format!("{license} WITH {exception}"));
        }
        Ok(node)
    }
    fn term(&mut self) -> Result<Node, String> {
        let mut node = self.primary()?;
        while self.peek() == Some("AND") {
            self.take();
            let right = self.primary()?;
            node = Node::And(Box::new(node), Box::new(right));
        }
        Ok(node)
    }
    fn expr(&mut self) -> Result<Node, String> {
        let mut node = self.term()?;
        while matches!(self.peek(), Some("OR") | Some("/")) {
            self.take();
            let right = self.term()?;
            node = Node::Or(Box::new(node), Box::new(right));
        }
        Ok(node)
    }
}

fn parse_expression(expression: &str) -> Result<Node, String> {
    let tokens = tokenize(expression)
        .ok_or_else(|| format!("unparseable SPDX expression {expression:?}"))?;
    let mut parser = Parser { tokens, index: 0 };
    let node = parser.expr()?;
    if parser.index != parser.tokens.len() {
        return Err(format!("unparseable SPDX expression {expression:?}"));
    }
    Ok(node)
}

fn evaluate(node: &Node, allow: &BTreeSet<String>) -> (bool, BTreeSet<String>) {
    match node {
        Node::Lic(license) => {
            let normalized = normalize(license);
            if allow.contains(&normalized) {
                (true, BTreeSet::new())
            } else {
                let mut missing = BTreeSet::new();
                missing.insert(normalized);
                (false, missing)
            }
        }
        Node::Or(left, right) => {
            let (left_ok, left_missing) = evaluate(left, allow);
            let (right_ok, right_missing) = evaluate(right, allow);
            if left_ok || right_ok {
                (true, BTreeSet::new())
            } else {
                (false, left_missing.union(&right_missing).cloned().collect())
            }
        }
        Node::And(left, right) => {
            let (left_ok, left_missing) = evaluate(left, allow);
            let (right_ok, right_missing) = evaluate(right, allow);
            if left_ok && right_ok {
                (true, BTreeSet::new())
            } else {
                (false, left_missing.union(&right_missing).cloned().collect())
            }
        }
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|value| value != 0.0),
    }
}

fn source_text(package: &Value) -> String {
    package
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

pub fn run(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("usage: faktor-ci-tools license-policy ROOT METADATA_JSON");
        return 2;
    }
    let root = Path::new(&args[0]);
    let metadata_path = Path::new(&args[1]);
    let deny_text = match std::fs::read_to_string(root.join("deny.toml")) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("check-licenses: deny.toml: {error}");
            return 1;
        }
    };
    let deny = match parse_toml(&deny_text) {
        Ok(deny) => deny,
        Err(error) => {
            eprintln!("check-licenses: deny.toml: {error}");
            return 1;
        }
    };
    let metadata = match util::read_json(metadata_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("check-licenses: {error}");
            return 1;
        }
    };
    let mut violations = Vec::new();
    let mut allow = BTreeSet::new();
    for entry in strings(&deny, "licenses.allow") {
        allow.insert(normalize(&entry));
    }
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let workspace: Vec<&Value> = packages
        .iter()
        .filter(|package| package.get("source").is_none_or(Value::is_null))
        .collect();
    let external: Vec<&Value> = packages
        .iter()
        .filter(|package| !package.get("source").is_none_or(Value::is_null))
        .collect();
    for package in &workspace {
        let name = package.get("name").and_then(Value::as_str).unwrap_or("?");
        let license = package.get("license").and_then(Value::as_str).unwrap_or("");
        if normalize(license) != "APACHE-2.0" {
            violations.push(format!(
                "workspace member {name} must resolve to Apache-2.0 (license.workspace = true); found {license:?}"
            ));
        }
        if let Some(license_file) = package
            .get("license_file")
            .filter(|value| truthy(Some(value)))
        {
            violations.push(format!(
                "workspace member {name} declares license-file {license_file:?}; use the workspace SPDX license"
            ));
        }
    }
    for package in &external {
        let name = package.get("name").and_then(Value::as_str).unwrap_or("?");
        let version = package
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let license = package.get("license").and_then(Value::as_str).unwrap_or("");
        if license.is_empty() {
            violations.push(format!(
                "{name} {version}: no declared license (license-file={:?})",
                package.get("license_file")
            ));
            continue;
        }
        match parse_expression(license) {
            Ok(node) => {
                let (ok, missing) = evaluate(&node, &allow);
                if !ok {
                    violations.push(format!(
                        "{name} {version}: license {license:?} is not allowed (unmet: {})",
                        missing.into_iter().collect::<Vec<_>>().join(", ")
                    ));
                }
            }
            Err(error) => violations.push(format!("{name} {version}: {error}")),
        }
    }
    let unknown_registry = string(&deny, "sources.unknown-registry", "warn");
    let unknown_git = string(&deny, "sources.unknown-git", "warn");
    if unknown_registry != "deny" {
        violations.push("deny.toml [sources] unknown-registry must be \"deny\"".into());
    }
    if unknown_git != "deny" {
        violations.push("deny.toml [sources] unknown-git must be \"deny\"".into());
    }
    let allowed_registries: BTreeSet<String> = strings(&deny, "sources.allow-registry")
        .into_iter()
        .map(|url| url.trim_end_matches('/').to_string())
        .collect();
    let allowed_git = strings(&deny, "sources.allow-git");
    for package in &external {
        let name = package.get("name").and_then(Value::as_str).unwrap_or("?");
        let version = package
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let source = source_text(package);
        if let Some(url) = source.strip_prefix("registry+") {
            let url = url.trim_end_matches('/');
            if !allowed_registries.contains(url) {
                violations.push(format!("{name} {version}: unknown registry {url:?}"));
            }
        } else if let Some(url) = source.strip_prefix("git+") {
            let allowed = allowed_git.iter().any(|entry| {
                let entry = entry.trim_end_matches('/');
                url == entry
                    || url.starts_with(&format!("{entry}/"))
                    || url.starts_with(&format!("{entry}?"))
            });
            if !allowed {
                violations.push(format!(
                    "{name} {version}: git source {url:?} is not allowlisted"
                ));
            }
        } else if !source.is_empty() {
            violations.push(format!("{name} {version}: unknown source kind {source:?}"));
        }
    }
    for package in &workspace {
        let name = package.get("name").and_then(Value::as_str).unwrap_or("?");
        if let Some(dependencies) = package.get("dependencies").and_then(Value::as_array) {
            for dependency in dependencies {
                if dependency.get("req").and_then(Value::as_str) == Some("*")
                    && truthy(dependency.get("source"))
                {
                    let dep_name = dependency
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    violations.push(format!(
                        "{name} depends on {dep_name} with a wildcard requirement (*)"
                    ));
                }
            }
        }
    }
    for required in ["LICENSE", "NOTICE", "deny.toml"] {
        let path = root.join(required);
        if !path.is_file() || path.metadata().map(|meta| meta.len() == 0).unwrap_or(true) {
            violations.push(format!(
                "required root artifact {required:?} is missing or empty"
            ));
        }
    }
    if !violations.is_empty() {
        eprintln!("check-licenses: POLICY VIOLATIONS:");
        for violation in &violations {
            eprintln!("  - {violation}");
        }
        return 1;
    }
    println!(
        "check-licenses: {} workspace members (Apache-2.0) and {} external packages satisfy the deny.toml licenses/bans/sources policy",
        workspace.len(),
        external.len()
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiline_allow_list() {
        let parsed =
            parse_toml("[licenses]\nallow = [\n  \"MIT\",\n  \"Apache-2.0\",\n]\n").unwrap();
        assert_eq!(
            strings(&parsed, "licenses.allow"),
            vec!["MIT", "Apache-2.0"]
        );
    }

    #[test]
    fn evaluates_spdx_or_and_with() {
        let allow: BTreeSet<String> = ["MIT".to_string(), "APACHE-2.0".to_string()]
            .into_iter()
            .collect();
        let node = parse_expression("MIT OR GPL-3.0").unwrap();
        assert!(evaluate(&node, &allow).0);
        let node = parse_expression("MIT AND GPL-3.0").unwrap();
        assert!(!evaluate(&node, &allow).0);
        let allow: BTreeSet<String> = ["APACHE-2.0 WITH LLVM-EXCEPTION".to_string()]
            .into_iter()
            .collect();
        let node = parse_expression("Apache-2.0 WITH LLVM-exception").unwrap();
        assert!(evaluate(&node, &allow).0);
    }
}
