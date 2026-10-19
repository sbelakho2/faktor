use crate::util::{self, Result};
use std::cmp::Ordering;
use std::net::IpAddr;
use std::path::Path;
use std::str::FromStr;

const BEGIN_MARKER: &str = "// BEGIN GENERATED IANA TABLE";
const END_MARKER: &str = "// END GENERATED IANA TABLE";
const CSV_SOURCES: [(&str, bool); 2] = [
    ("iana-ipv4-special.csv", false),
    ("iana-ipv6-special.csv", true),
];
const TUNNELING_NAMES: [&str; 4] = ["ipv4-mapped", "ipv4-ipv6 translat", "6to4", "teredo"];

#[derive(Clone)]
struct Row {
    v6: bool,
    prefixlen: u8,
    packed: Vec<u8>,
    class: &'static str,
}

fn strip_annotations(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        output.push_str(&rest[..start]);
        if let Some(end) = rest[start..].find(']') {
            rest = &rest[start + end + 1..];
        } else {
            rest = "";
            break;
        }
    }
    output.push_str(rest);
    output.trim().to_string()
}

fn parse_truthy(cell: &str) -> bool {
    strip_annotations(cell).to_lowercase() == "true"
}

fn class_for(name: &str, globally_reachable: bool) -> &'static str {
    let folded = strip_annotations(name).to_lowercase();
    if folded.contains("loopback") {
        "Loopback"
    } else if folded.contains("private-use") || folded.contains("unique-local") {
        "Private"
    } else if folded.contains("link local") || folded.contains("link-local") {
        "LinkLocal"
    } else if folded.contains("shared address space") {
        "Cgnat"
    } else if folded.starts_with("documentation") {
        "Documentation"
    } else if folded.contains("benchmarking") {
        "Benchmark"
    } else if folded.contains("multicast") {
        "Multicast"
    } else if folded.contains("unspecified") || folded.contains("this host on this network") {
        "Unspecified"
    } else if folded.contains("this network")
        || TUNNELING_NAMES.iter().any(|needle| folded.contains(needle))
    {
        "Reserved"
    } else if globally_reachable {
        "Global"
    } else {
        "Reserved"
    }
}

fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' => in_quotes = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(ch),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn parse_network(block: &str, path: &str, line: usize) -> Result<(u8, Vec<u8>)> {
    let text = strip_annotations(block);
    let (address, prefix) = text
        .rsplit_once('/')
        .ok_or_else(|| format!("{path}: malformed address block {text:?} (row {line})"))?;
    let address = IpAddr::from_str(address).map_err(|error| {
        format!("{path}: malformed address block {text:?} (row {line}): {error}")
    })?;
    let prefixlen: u8 = prefix
        .parse()
        .map_err(|_| format!("{path}: malformed address block {text:?} (row {line})"))?;
    let bits = if address.is_ipv4() { 32 } else { 128 };
    if prefixlen as u32 > bits {
        return Err(format!(
            "{path}: malformed address block {text:?} (row {line}): prefix out of range"
        ));
    }
    let packed = match address {
        IpAddr::V4(address) => address.octets().to_vec(),
        IpAddr::V6(address) => address.octets().to_vec(),
    };
    let mut masked = packed.clone();
    for (index, byte) in masked.iter_mut().enumerate() {
        let remaining = prefixlen as i32 - (index as i32 * 8);
        if remaining <= 0 {
            *byte = 0;
        } else if remaining < 8 {
            *byte &= 0xffu8 << (8 - remaining);
        }
    }
    if masked != packed {
        return Err(format!(
            "{path}: malformed address block {text:?} (row {line}): host bits set"
        ));
    }
    Ok((prefixlen, packed))
}

fn load_rows(path: &str, is_v6: bool) -> Result<Vec<Row>> {
    let text = util::read_text(std::path::Path::new(path))?;
    let rows = parse_csv(&text);
    let Some(header) = rows.first() else {
        return Err(format!("{path}: empty CSV"));
    };
    let mut index = std::collections::HashMap::new();
    for (position, name) in header.iter().enumerate() {
        index.insert(name.trim().to_string(), position);
    }
    for required in ["Address Block", "Name", "Globally Reachable"] {
        if !index.contains_key(required) {
            return Err(format!("{path}: missing column {required:?}"));
        }
    }
    let mut output = Vec::new();
    for (row_number, row) in rows.iter().enumerate().skip(1) {
        let line = row_number + 1;
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let get = |name: &str| -> &str { row.get(index[name]).map(String::as_str).unwrap_or("") };
        let block_cell = get("Address Block");
        let name = get("Name");
        let reachable = parse_truthy(get("Globally Reachable"));
        for block in block_cell.split(',') {
            if strip_annotations(block).is_empty() {
                continue;
            }
            let (prefixlen, packed) = parse_network(block, path, line)?;
            output.push(Row {
                v6: is_v6,
                prefixlen,
                packed,
                class: class_for(name, reachable),
            });
        }
    }
    if output.is_empty() {
        return Err(format!("{path}: no rows"));
    }
    Ok(output)
}

fn render_row(row: &Row) -> String {
    let mut prefix: Vec<u8> = row.packed.clone();
    prefix.resize(16, 0);
    let prefix_text = prefix
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "    IanaRow {{ v6: {}, len: {}, prefix: [{}], class: AddressClass::{} }},",
        row.v6, row.prefixlen, prefix_text, row.class
    )
}

fn render_section(root: &Path) -> Result<String> {
    let data_dir = root.join("crates/security/data");
    let mut rows = Vec::new();
    for (filename, is_v6) in CSV_SOURCES {
        let path = data_dir.join(filename);
        rows.extend(load_rows(&path.display().to_string(), is_v6)?);
    }
    rows.sort_by(|left, right| {
        left.v6
            .cmp(&right.v6)
            .then_with(|| right.prefixlen.cmp(&left.prefixlen))
            .then_with(|| {
                if left.packed == right.packed {
                    Ordering::Equal
                } else {
                    left.packed[..].cmp(&right.packed[..])
                }
            })
    });
    let v4 = rows.iter().filter(|row| !row.v6).count();
    let v6 = rows.iter().filter(|row| row.v6).count();
    if v4 < 20 || v6 < 20 {
        return Err("refusing to emit a suspiciously small IANA table".into());
    }
    let mut lines = vec![
        BEGIN_MARKER.to_string(),
        "#[rustfmt::skip]".to_string(),
        "pub(crate) static IANA_SPECIAL_TABLE: &[IanaRow] = &[".to_string(),
    ];
    lines.extend(rows.iter().map(render_row));
    lines.push("];".to_string());
    lines.push(END_MARKER.to_string());
    Ok(lines.join("\n"))
}

fn committed_section(text: &str) -> Option<&str> {
    let start = text.find(BEGIN_MARKER)?;
    let end = text.find(END_MARKER)?;
    if end < start {
        return None;
    }
    Some(&text[start..end + END_MARKER.len()])
}

pub fn run(args: &[String]) -> i32 {
    let check = args.iter().any(|arg| arg == "--check");
    let unknown: Vec<&String> = args.iter().filter(|arg| *arg != "--check").collect();
    if !unknown.is_empty() {
        eprintln!(
            "unknown argument(s): {}",
            unknown
                .iter()
                .map(|arg| arg.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
        return 2;
    }
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("generate-iana-network-table: {error}");
            return 1;
        }
    };
    let expected = match render_section(&root) {
        Ok(expected) => expected,
        Err(error) => {
            eprintln!("generate-iana-network-table: {error}");
            return 1;
        }
    };
    let network = root.join("crates/security/src/network.rs");
    let text = match util::read_text(&network) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("generate-iana-network-table: {error}");
            return 1;
        }
    };
    let committed = committed_section(&text);
    if check {
        let Some(committed) = committed else {
            eprintln!(
                "STALE: {} has no generated IANA table markers; run scripts/generate-iana-network-table",
                network.display()
            );
            return 1;
        };
        if committed != expected {
            let expected_lines: Vec<&str> = expected.lines().collect();
            let committed_lines: Vec<&str> = committed.lines().collect();
            for index in 0..expected_lines.len().max(committed_lines.len()) {
                let want = expected_lines.get(index).copied().unwrap_or("<missing>");
                let got = committed_lines.get(index).copied().unwrap_or("<missing>");
                if want != got {
                    eprintln!(
                        "STALE: the committed IANA table differs from the pinned CSVs at generated line {}:\n  expected: {want}\n  committed: {got}\nRun scripts/generate-iana-network-table",
                        index + 1
                    );
                    break;
                }
            }
            return 1;
        }
        println!("generate-iana-network-table: OK (table matches the pinned CSVs)");
        return 0;
    }
    if committed.is_none() {
        eprintln!(
            "{}: generated table markers not found; add `{BEGIN_MARKER}` / `{END_MARKER}` first",
            network.display()
        );
        return 1;
    }
    let start = text.find(BEGIN_MARKER).unwrap();
    let end = text.find(END_MARKER).unwrap() + END_MARKER.len();
    let updated = format!("{}{}{}", &text[..start], expected, &text[end..]);
    if let Err(error) = std::fs::write(&network, updated) {
        eprintln!(
            "generate-iana-network-table: {}: {error}",
            network.display()
        );
        return 1;
    }
    println!("generate-iana-network-table: wrote {}", network.display());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_derivation_is_stable() {
        assert_eq!(class_for("Loopback Address", true), "Loopback");
        assert_eq!(class_for("Private-Use", true), "Private");
        assert_eq!(class_for("IPv4-mapped Address", true), "Reserved");
        assert_eq!(class_for("Some Prefix", true), "Global");
        assert_eq!(class_for("Some Prefix", false), "Reserved");
    }

    #[test]
    fn annotations_are_stripped() {
        assert_eq!(strip_annotations("False [1]"), "False");
        assert_eq!(strip_annotations("192.0.2.0/24 [2]"), "192.0.2.0/24");
    }

    #[test]
    fn strict_network_rejects_host_bits() {
        assert!(parse_network("192.0.2.0/24", "x", 1).is_ok());
        assert!(parse_network("192.0.2.1/24", "x", 1).is_err());
        assert!(parse_network("2001:db8::/32", "x", 1).is_ok());
    }

    #[test]
    fn csv_quotes_and_commas() {
        let rows = parse_csv("a,b\n\"x,y\",\"q\"\"z\"\n");
        assert_eq!(rows[0], vec!["a", "b"]);
        assert_eq!(rows[1], vec!["x,y", "q\"z"]);
    }
}
