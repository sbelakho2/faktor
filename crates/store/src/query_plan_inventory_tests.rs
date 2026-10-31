// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! HAR-10 — a query-plan test for EVERY SQL statement in this crate.
//!
//! The sibling [`query_plan_tests`] pins exact plans for the audited hot
//! paths. This module is the completeness gate the programme row asks for:
//!
//! * **Enumeration** — a source scanner walks every production `.rs` file
//!   under `crates/store/src` (test-only `cfg(test)` regions and `*_tests.rs`
//!   files are stripped), extracts every SQL-bearing string literal, splits
//!   migration batches into single statements, and normalizes them into
//!   statement keys.
//! * **Drift check** — `query_plan_inventory.json` must list EXACTLY the
//!   extracted set in both directions. A new SQL literal that starts with a
//!   SQL keyword and is not registered fails
//!   `inventory_covers_every_sql_statement`; a registry entry with no source
//!   statement fails the same test as stale. Runtime-assembled statements
//!   (`conn.prepare(&sql)` after `push_str` fragments) are enumerated
//!   separately as dynamic sites; every site must resolve to a registered,
//!   planned statement (`plan_key`) or an explicit `reason`.
//! * **Plan check** — `every_statement_has_the_intended_plan` replays the
//!   inventory in SOURCE order (DDL is executed best-effort so statements
//!   that only exist mid-migration — e.g. the v44 `session_listing_backfill`
//!   TEMP table — still prepare at their source point) and for every case
//!   asserts: `EXPLAIN QUERY PLAN` succeeds, the plan matches the recorded
//!   lines exactly, plan-bearing classes carry at least one plan row (a
//!   missing plan fails the run), an indexed table is never fully scanned
//!   without an explicit `allow_scan` blessing, and typed-ledger range reads
//!   ride their typed index (`idx_ledger_entry_session_type_seq`, the
//!   `memory_v2` page indexes — same style as the `kind >= 'memory_v2:' AND
//!   kind < 'memory_v2;'` range predicates).
//!
//! Regenerate the recorded plans after a legitimate planner-affecting change:
//! `cargo test -p faktor-store record_query_plan_inventory -- --ignored`.
//! The recorder MERGES hand annotations (`allow_scan`, `reason`,
//! `override_rendered`, `vars`, `expand`, `require_index`) by statement
//! key/case, so a review only has to re-bless what actually changed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Registry schema (crates/store/query_plan_inventory.json)
// ---------------------------------------------------------------------------

const REGISTRY_SCHEMA: &str = "faktor-store-query-plans/v1";

#[derive(Debug, Deserialize, Serialize)]
struct Registry {
    schema: String,
    /// Every schema-ladder statement OCCURRENCE in source order
    /// `(first-seen order, statement key)` — duplicates included, because a
    /// migration batch may legitimately re-run idempotent DDL (`CREATE INDEX
    /// … IF NOT EXISTS` after a table rebuild) and the walk must reproduce
    /// the exact schema states production does.
    #[serde(default)]
    ladder: Vec<(usize, String)>,
    statements: Vec<StatementEntry>,
    dynamic_sites: Vec<DynamicSiteEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CaseRecord {
    /// The rendered statement that is EXPLAINed verbatim.
    sql: String,
    /// Exact `EXPLAIN QUERY PLAN` detail lines (may be empty only where the
    /// class rules allow it, e.g. `INSERT ... VALUES` or DDL).
    plan: Vec<String>,
    /// Per-case justification for a full scan of an indexed table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_scan: Option<String>,
    /// Per-case index the plan MUST use (structural intent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    require_index: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StatementEntry {
    /// Normalized statement key extracted from source.
    key: String,
    /// Source position (first occurrence) — the walk replays the inventory
    /// in this order so mid-migration statements prepare at their source
    /// point. For `source = representative` the order of `basis`.
    order: usize,
    /// `extracted` (drift-checked against the source scanner) or
    /// `representative` (hand-registered full text of a runtime-assembled
    /// statement; `basis` names the extracted statement whose source position
    /// orders it, and every representative must be referenced by a dynamic
    /// site).
    #[serde(default = "default_source", skip_serializing_if = "is_extracted")]
    source: String,
    /// For representatives: the extracted statement that anchors ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    basis: Option<String>,
    /// Whether the source literal is a `format!` template (holes/escapes).
    format: bool,
    /// `query` | `mutate` | `ddl` | `pragma` | `txn` | `fragment` | `non-sql`.
    class: String,
    /// Source files containing the literal (informational).
    #[serde(default)]
    files: Vec<String>,
    /// Hole values for `format!` templates not covered by source consts or
    /// the built-in representative rules.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    vars: BTreeMap<String, String>,
    /// Hole expansions (e.g. `{table}` over the purge table list): one case
    /// per value of the FIRST expanded hole.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    expand: BTreeMap<String, Vec<String>>,
    /// True when `cases[].sql` is hand-maintained (the raw source literal is
    /// a fragment that never runs standalone; the cases carry a representative
    /// full statement instead).
    #[serde(default)]
    override_rendered: bool,
    /// Entry-level scan justification shared by every case of this entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allow_scan: Option<String>,
    /// Why the literal is not a statement (`class = non-sql`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    cases: Vec<CaseRecord>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DynamicSiteEntry {
    /// `enclosing_fn | normalized first-argument expression`.
    key: String,
    file: String,
    /// Every statement this site may execute (a site with runtime branches
    /// maps to ALL branch statements); each must carry a plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    plan_keys: Vec<String>,
    /// Why no statement mapping exists (writer label pass-through, public
    /// passthrough SQL, migration batch driver, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// Raw argument text at the site (record-time literal resolution).
    #[serde(skip)]
    raw: String,
}

fn default_source() -> String {
    "extracted".to_string()
}

fn is_extracted(source: &str) -> bool {
    source == "extracted"
}

fn registry() -> Registry {
    let raw = include_str!("../query_plan_inventory.json");
    let parsed: Registry = serde_json::from_str(raw).expect("query_plan_inventory.json must parse");
    assert_eq!(
        parsed.schema, REGISTRY_SCHEMA,
        "query plan registry schema drift"
    );
    parsed
}

fn registry_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("query_plan_inventory.json")
}

// ---------------------------------------------------------------------------
// Source scanning
// ---------------------------------------------------------------------------

pub(crate) fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

pub(crate) fn collect_source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().collect(),
        Err(e) => panic!("cannot read {}: {e}", dir.display()),
    };
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() == "tests" {
                continue;
            }
            collect_source_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name.ends_with("_tests.rs") || file_name.starts_with("tests.rs") {
                continue;
            }
            out.push(path);
        }
    }
}

/// Blank every `#[cfg(... test ...)]`-gated item (test-only regions inside
/// production files) while preserving byte offsets. `cfg(not(...test...))`
/// regions are PRODUCTION code and are kept.
pub(crate) fn strip_test_regions(src: &str) -> String {
    // Structure decisions (attribute extents, item bodies) are made on a
    // MASKED byte copy where string/comment contents are spaces: a brace
    // inside a doc comment must never end an item early. All indices are
    // BYTE offsets; blanks are applied byte-wise but through UTF-8 char
    // boundaries (a char touching a blanked range loses ALL its bytes) so
    // the result stays valid UTF-8 with identical byte length.
    let m = mask_literals_and_comments(src);
    let raw = src.as_bytes();
    let mut out = raw.to_vec();
    let n = out.len();
    let blank_range = |out: &mut Vec<u8>, from: usize, to: usize| {
        // Back up to the start of the char containing `from`.
        let mut start = from;
        while start > 0 && (raw[start] & 0xC0) == 0x80 {
            start -= 1;
        }
        let mut x = start;
        while x < to.min(n) {
            let len = match raw[x] {
                b if b < 0x80 => 1,
                b if b >> 5 == 0b110 => 2,
                b if b >> 4 == 0b1110 => 3,
                b if b >> 3 == 0b11110 => 4,
                _ => 1, // continuation byte reached mid-char: covered below
            };
            let end = (x + len).min(n);
            // Blank the whole char iff it intersects the range.
            if x < to && end > from {
                for k in x..end {
                    if raw[k] != b'\n' {
                        out[k] = b' ';
                    }
                }
            }
            x = end;
        }
    };
    let mut i = 0usize;
    while i + 4 < n {
        if m[i] == b'#' && m[i + 1] == b'[' {
            let mut j = i + 2;
            let mut depth = 1usize;
            while j < n {
                if m[j] == b'[' {
                    depth += 1;
                } else if m[j] == b']' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            if j >= n {
                break;
            }
            let attr = String::from_utf8_lossy(&m[i..=j]);
            let is_cfg = attr.starts_with("#[cfg(");
            let mentions_test = attr.contains("test");
            let negated = attr.contains("not(");
            if is_cfg && mentions_test && !negated {
                let mut k = j + 1;
                while k < n && m[k].is_ascii_whitespace() {
                    k += 1;
                }
                // Skip stacked attributes (`#[path = "..."]`) and `mod x;`.
                if k < n && m[k] == b'[' {
                    i = k;
                    continue;
                }
                let semi = (k..n).find(|&x| m[x] == b';');
                let open = (k..n).find(|&x| m[x] == b'{');
                let close = (k..n).find(|&x| m[x] == b'}');
                if let (Some(c), Some(o)) = (close, open) {
                    if c < o && close.is_none_or(|c| Some(c) < semi) {
                        // The attribute sits on a struct-literal FIELD or
                        // other position where the next brace is a CLOSER of
                        // an enclosing body: not an item header. Stripping
                        // here would delete unmatched closes and unbalance
                        // the source.
                        i = j + 1;
                        continue;
                    }
                }
                match (semi, open) {
                    (Some(s), Some(o)) if s < o => {
                        i = j + 1;
                        continue;
                    }
                    (Some(_), None) => {
                        i = j + 1;
                        continue;
                    }
                    (_, Some(open)) => {
                        let mut depth = 0usize;
                        let mut end = n;
                        for (x, &b) in m.iter().enumerate().skip(open) {
                            match b {
                                b'{' => depth += 1,
                                b'}' => {
                                    depth -= 1;
                                    if depth == 0 {
                                        end = x + 1;
                                        break;
                                    }
                                }
                                _ => {}
                            }
                        }
                        blank_range(&mut out, i, end);
                        i = end;
                        continue;
                    }
                    _ => {
                        i = j + 1;
                        continue;
                    }
                }
            }
        }
        i += 1;
    }
    String::from_utf8(out).expect("strip_test_regions preserves UTF-8")
}

/// Blank comments while preserving offsets so SQL examples in doc comments
/// can never be mistaken for statements, and `fn`/call scans cannot fire on
/// comment text. String contents are kept verbatim.
pub(crate) fn blank_comments(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = chars.clone();
    let mut i = 0usize;
    let mut quote: Option<char> = None;
    while i < n {
        let c = chars[i];
        if let Some(q) = quote {
            if c == '\\' && q == '"' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                quote = Some(c);
                i += 1;
            }
            '\'' => {
                if chars.get(i + 1) == Some(&'\\') {
                    let mut j = i + 2;
                    while j < n && chars[j] != '\'' {
                        j += 1;
                    }
                    if j < n {
                        i = j + 1;
                        continue;
                    }
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                    continue;
                }
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < n && chars[i] != '\n' {
                    out[i] = ' ';
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                let end = (i + 2..n).find(|&x| chars[x] == '*' && chars.get(x + 1) == Some(&'/'));
                let end = end.map(|x| x + 2).unwrap_or(n);
                for c in out[i..end].iter_mut() {
                    if *c != '\n' {
                        *c = ' ';
                    }
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    out.into_iter().collect()
}

#[derive(Debug, Clone)]
struct RawLiteral {
    text: String,
    format_mode: bool,
    pos: usize,
}

fn unescape_rust_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('0') => out.push('\0'),
            Some('\n') => {
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Extract every ordinary/raw string literal (comments already blanked by
/// the caller), flagging the ones that are `format!` templates.
fn scan_literals(src: &str) -> Vec<RawLiteral> {
    let b: Vec<u8> = src.as_bytes().to_vec();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < n {
        if b[i] == b'r' || b[i] == b'b' {
            let mut k = i + 1;
            if k < n && b[k] == b'r' {
                k += 1;
            }
            let mut hashes = 0usize;
            while k < n && b[k] == b'#' {
                hashes += 1;
                k += 1;
            }
            if k < n && b[k] == b'"' {
                let needle: Vec<u8> = {
                    let mut v = vec![b'"'];
                    v.extend(std::iter::repeat_n(b'#', hashes));
                    v
                };
                let start = k + 1;
                let mut end = start;
                while end + needle.len() <= n && b[end..end + needle.len()] != needle[..] {
                    end += 1;
                }
                let end = end.min(n);
                let text = String::from_utf8_lossy(&b[start..end]).into_owned();
                out.push(RawLiteral {
                    text,
                    format_mode: false,
                    pos: i,
                });
                i = end + needle.len();
                continue;
            }
        }
        if b[i] == b'"' {
            let mut j = i + 1;
            while j < n {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == b'"' {
                    break;
                }
                j += 1;
            }
            let text = String::from_utf8_lossy(&b[i + 1..j.min(n)]).into_owned();
            out.push(RawLiteral {
                text: unescape_rust_string(&text),
                format_mode: is_format_arg(src, i),
                pos: i,
            });
            i = (j + 1).min(n);
            continue;
        }
        i += 1;
    }
    out
}

/// True when the literal is the first argument of `format!(`.
fn is_format_arg(src: &str, lit_start: usize) -> bool {
    let bytes = src.as_bytes();
    let mut i = lit_start;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'(' {
        return false;
    }
    i -= 1;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'!' {
        return false;
    }
    i -= 1;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if !src[..i].ends_with("format") {
        return false;
    }
    let before = i - "format".len();
    before == 0 || !(bytes[before - 1].is_ascii_alphanumeric() || bytes[before - 1] == b'_')
}

const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "REPLACE",
    "CREATE",
    "ALTER",
    "DROP",
    "PRAGMA",
    "WITH",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
    "ANALYZE",
    "VACUUM",
    "EXPLAIN",
];

fn sql_leading_keyword(text: &str) -> Option<&'static str> {
    let mut rest = text.trim_start();
    if let Some(stripped) = rest.strip_prefix("--") {
        let idx = stripped.find('\n')?;
        rest = stripped[idx + 1..].trim_start();
    }
    let first = rest
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    SQL_KEYWORDS
        .iter()
        .copied()
        .find(|kw| kw.eq_ignore_ascii_case(first))
}

/// Remove `--`/`/* */` comments outside quotes, collapse whitespace, trim.
fn normalize_statement(text: &str) -> String {
    let stripped = strip_sql_comments(text);
    let mut out = String::with_capacity(stripped.len());
    let mut last_space = false;
    for c in stripped.chars() {
        if c.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim().to_string()
}

fn strip_sql_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(n);
    let mut i = 0usize;
    let mut quote: Option<char> = None;
    while i < n {
        let c = chars[i];
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                if i + 1 < n && chars[i + 1] == q {
                    out.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        if c == '-' && i + 1 < n && chars[i + 1] == '-' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Split a batch into single statements (quote/comment aware).
fn split_statements(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        if let Some(q) = quote {
            current.push(c);
            if c == q {
                if i + 1 < n && chars[i + 1] == q {
                    current.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                current.push(c);
                i += 1;
            }
            '-' if i + 1 < n && chars[i + 1] == '-' => {
                while i < n && chars[i] != '\n' {
                    current.push(chars[i]);
                    i += 1;
                }
            }
            '/' if i + 1 < n && chars[i + 1] == '*' => {
                while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                    current.push(chars[i]);
                    i += 1;
                }
                current.push_str("*/");
                i = (i + 2).min(n);
            }
            ';' => {
                out.push(std::mem::take(&mut current));
                i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }
    out.push(current);
    out.into_iter()
        .map(|s| normalize_statement(&s))
        .filter(|s| !s.is_empty())
        .collect()
}

// ------------------------------------------------------------- fn spans

#[derive(Debug, Clone)]
pub(crate) struct FnSpan {
    pub(crate) name: String,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Mask string/char literal contents and comments to spaces (byte offsets
/// and newlines preserved) so structural scans can never trip over a brace
/// or a `fn` word inside a literal (a runaway span once swallowed whole
/// functions).
pub(crate) fn mask_literals_and_comments(src: &str) -> Vec<u8> {
    let mut out = src.as_bytes().to_vec();
    let n = out.len();
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for i in from..to.min(out.len()) {
            if out[i] != b'\n' {
                out[i] = b' ';
            }
        }
    };
    let mut i = 0usize;
    while i < n {
        if out[i] == b'/' && i + 1 < n && out[i + 1] == b'/' {
            let start = i;
            while i < n && out[i] != b'\n' {
                i += 1;
            }
            blank(&mut out, start, i);
            continue;
        }
        if out[i] == b'/' && i + 1 < n && out[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < n && !(out[i] == b'*' && out[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            blank(&mut out, start, i);
            continue;
        }
        if out[i] == b'r' || out[i] == b'b' {
            let mut k = i + 1;
            if k < n && out[k] == b'r' {
                k += 1;
            }
            let mut hashes = 0usize;
            while k < n && out[k] == b'#' {
                hashes += 1;
                k += 1;
            }
            if k < n && out[k] == b'"' {
                let needle: Vec<u8> = {
                    let mut v = vec![b'"'];
                    v.extend(std::iter::repeat_n(b'#', hashes));
                    v
                };
                let start = k + 1;
                let mut end = start;
                while end + needle.len() <= n && out[end..end + needle.len()] != needle[..] {
                    end += 1;
                }
                let end = (end + needle.len()).min(n);
                blank(&mut out, i, end);
                i = end;
                continue;
            }
        }
        if out[i] == b'"' {
            let start = i;
            i += 1;
            while i < n {
                if out[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if out[i] == b'"' {
                    i += 1;
                    break;
                }
                if out[i] == b'\n' {
                    break;
                }
                i += 1;
            }
            let i2 = i.min(n);
            blank(&mut out, start, i2);
            i = i2;
            continue;
        }
        if out[i] == b'\'' {
            if i + 1 < n && out[i + 1] == b'\\' {
                let start = i;
                let mut j = i + 2;
                while j < n && out[j] != b'\'' {
                    j += 1;
                }
                if j < n {
                    i = j + 1;
                    blank(&mut out, start, i);
                    continue;
                }
            } else if i + 2 < n && out[i + 2] == b'\'' {
                blank(&mut out, i, i + 3);
                i += 3;
                continue;
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    out
}

/// Function spans over the MASKED source (byte offsets): `fn <ident>` and
/// the matching braces of its body, blind to literals and comments.
pub(crate) fn fn_spans(src: &str) -> Vec<FnSpan> {
    let m = mask_literals_and_comments(src);
    let n = m.len();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i + 2 < n {
        if m[i] == b'f' && m[i + 1] == b'n' {
            let before_ok = i == 0 || !ident(m[i - 1]);
            let after = i + 2;
            let after_ok = after >= n || !ident(m[after]);
            if before_ok && after_ok {
                let mut k = after;
                while k < n && m[k].is_ascii_whitespace() {
                    k += 1;
                }
                let name_start = k;
                while k < n && ident(m[k]) {
                    k += 1;
                }
                let name = String::from_utf8_lossy(&m[name_start..k]).into_owned();
                let mut j = k;
                while j < n && m[j] != b'{' && m[j] != b';' {
                    j += 1;
                }
                if j < n && m[j] == b'{' {
                    let mut depth = 0usize;
                    let mut end = n;
                    for (x, &b) in m.iter().enumerate().skip(j) {
                        match b {
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = x + 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    spans.push(FnSpan {
                        name,
                        start: i,
                        end,
                    });
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    spans
}

fn enclosing_fn(spans: &[FnSpan], pos: usize) -> Option<&str> {
    spans
        .iter()
        .find(|s| pos >= s.start && pos < s.end)
        .map(|s| s.name.as_str())
}

// ------------------------------------------------- dynamic SQL sites

fn normalize_expr(expr: &str) -> String {
    expr.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Find SQL execution sites whose first argument is NOT a plain literal:
/// those statements are assembled at runtime and need an explicit plan
/// mapping. Literal-argument sites are covered by the statement inventory.
fn scan_dynamic_sites(src: &str, file: &str) -> Vec<(String, DynamicSiteEntry)> {
    const METHODS: &[&str] = &[
        "prepare_cached",
        "execute_batch",
        "query_map",
        "query_row",
        "prepare",
        "execute",
    ];
    let spans = fn_spans(src);
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < n {
        if bytes[i] != b'.' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < n && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        let method = &src[i + 1..j];
        if !METHODS.contains(&method) {
            i = j;
            continue;
        }
        let mut k = j;
        while k < n && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if k >= n || bytes[k] != b'(' {
            i = j;
            continue;
        }
        let Some(arg) = parse_first_arg(src, k) else {
            i = j;
            continue;
        };
        let arg_trimmed = arg.trim();
        if arg_trimmed.starts_with('"')
            || arg_trimmed.starts_with("r\"")
            || arg_trimmed.starts_with("r#")
        {
            // Literal first argument: the statement itself is enumerated by
            // the statement inventory (writer labels are not SQL).
            i = j;
            continue;
        }
        // Statement-style calls (`stmt.execute(params![..])`): the SQL was
        // bound at `prepare(...)`, whose own site is enumerated instead.
        if arg_trimmed.starts_with("params!")
            || arg_trimmed.starts_with("rusqlite::params")
            || arg_trimmed.starts_with("params_from_iter")
            || arg_trimmed.starts_with('[')
            || arg_trimmed.starts_with("&[")
        {
            i = j;
            continue;
        }
        let Some(func) = enclosing_fn(&spans, i) else {
            i = j;
            continue;
        };
        let expr = normalize_expr(arg_trimmed);
        let key = format!("{func} || {expr}");
        out.push((
            key.clone(),
            DynamicSiteEntry {
                key,
                file: file.to_string(),
                plan_keys: Vec::new(),
                reason: None,
                raw: arg.clone(),
            },
        ));
        i = j;
    }
    out
}

/// Parse the first argument of a call starting at `open` = index of `(`.
fn parse_first_arg(src: &str, open: usize) -> Option<String> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut depth = 0usize;
    let i = open;
    let start = open + 1;
    let mut k = open + 1;
    let mut quote: Option<char> = None;
    while k < n {
        let c = chars[k];
        if let Some(q) = quote {
            if c == '\\' && q == '"' {
                k += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            k += 1;
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                k += 1;
            }
            '(' | '[' => {
                depth += 1;
                k += 1;
            }
            '}' if depth == 0 => return None,
            '{' if depth == 0 => return None,
            ')' | ']' => {
                if depth == 0 {
                    return Some(chars[start..k].iter().collect());
                }
                depth -= 1;
                k += 1;
                if depth == 0 && c == ')' {
                    return Some(chars[start..k].iter().collect());
                }
            }
            ',' if depth == 0 => {
                return Some(chars[start..k].iter().collect());
            }
            _ => k += 1,
        }
    }
    let _ = i;
    None
}

/// Map `let var = <SQL literal | format!("…") | String::from("…")>` to the
/// statement keys of the initializer, so a site using `&var` can resolve to
/// its statement automatically.
fn scan_let_bindings(src: &str, spans: &[FnSpan], ex: &mut Extraction) {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let skip_ws = |from: usize| -> usize {
        let mut x = from;
        while x < n && bytes[x].is_ascii_whitespace() {
            x += 1;
        }
        x
    };
    let mut i = 0usize;
    while i + 3 < n {
        let is_let = bytes[i] == b'l'
            && bytes[i + 1] == b'e'
            && bytes[i + 2] == b't'
            && (i == 0 || !is_ident(bytes[i - 1]))
            && !is_ident(bytes[i + 3]);
        if !is_let {
            i += 1;
            continue;
        }
        let mut k = skip_ws(i + 3);
        // optional `mut`
        if k + 4 <= n
            && bytes[k] == b'm'
            && bytes[k + 1] == b'u'
            && bytes[k + 2] == b't'
            && !is_ident(bytes[k + 3])
        {
            k = skip_ws(k + 4);
        }
        let name_start = k;
        while k < n && is_ident(bytes[k]) {
            k += 1;
        }
        let name = &src[name_start..k];
        if name.is_empty() {
            i = name_start.max(i + 1);
            continue;
        }
        // Optional type annotation up to the assignment `=`.
        k = skip_ws(k);
        if k < n && bytes[k] == b':' {
            while k < n && bytes[k] != b'=' && bytes[k] != b';' && bytes[k] != b'\n' {
                k += 1;
            }
        }
        if k >= n || bytes[k] != b'=' {
            i = (name_start + name.len()).max(i + 1);
            continue;
        }
        k = skip_ws(k + 1);
        let Some(func) = enclosing_fn(spans, i).map(str::to_string) else {
            i += 1;
            continue;
        };
        // Initializer must be one of the SQL template shapes.
        let init_window = if src[k..].starts_with("format!") {
            let open = k + "format!".len();
            let open = skip_ws(open);
            if open >= n || bytes[open] != b'(' {
                i += 1;
                continue;
            }
            open + 1
        } else if src[k..].starts_with("String::from") {
            let open = k + "String::from".len();
            let open = skip_ws(open);
            if open >= n || bytes[open] != b'(' {
                i += 1;
                continue;
            }
            open + 1
        } else if bytes.get(k) == Some(&b'"')
            || src[k..].starts_with("r\"")
            || src[k..].starts_with("r#")
        {
            k
        } else {
            i += 1;
            continue;
        };
        let end = spans
            .iter()
            .find(|s| s.start <= i && i < s.end)
            .map(|s| s.end)
            .unwrap_or(n);
        let window_end = end.min(n);
        if init_window >= window_end {
            i += 1;
            continue;
        }
        for lit in scan_literals(&src[init_window..window_end])
            .into_iter()
            .take(1)
        {
            if sql_leading_keyword(&lit.text).is_none() {
                continue;
            }
            for statement in split_statements(&lit.text) {
                if sql_leading_keyword(&statement).is_none() {
                    continue;
                }
                ex.var_statements
                    .entry(format!("{func}|{name}"))
                    .or_default()
                    .insert(statement);
            }
        }
        i = (name_start + name.len()).max(i + 1);
    }
}

// ------------------------------------------------------- format rendering

/// Every `const NAME: … = "…"` string constant in production source, so
/// format holes like `{OUTCOME_ROW_COLUMNS}` render from the SAME text the
/// runtime interpolates (drift-proof: change the const, the registry must
/// be re-recorded).
fn source_string_consts(src: &str) -> BTreeMap<String, String> {
    let src = blank_comments(src);
    let bytes = src.as_bytes();
    let n = bytes.len();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = BTreeMap::new();
    let mut i = 0usize;
    while i + 5 < n {
        let is_const = bytes[i] == b'c'
            && bytes[i + 1] == b'o'
            && bytes[i + 2] == b'n'
            && bytes[i + 3] == b's'
            && bytes[i + 4] == b't'
            && (i == 0 || !is_ident(bytes[i - 1]))
            && (i + 5 >= n || !is_ident(bytes[i + 5]));
        if !is_const {
            i += 1;
            continue;
        }
        let mut k = i + 5;
        while k < n && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        let name_start = k;
        while k < n && is_ident(bytes[k]) {
            k += 1;
        }
        if k == name_start {
            i += 1;
            continue;
        }
        let name = src[name_start..k].to_string();
        while k < n && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if k >= n || bytes[k] != b':' {
            i = name_start + 1;
            continue;
        }
        while k < n && bytes[k] != b'=' && bytes[k] != b';' && bytes[k] != b'\n' {
            k += 1;
        }
        if k >= n || bytes[k] != b'=' {
            i = name_start + 1;
            continue;
        }
        let mut v = k + 1;
        while v < n && bytes[v].is_ascii_whitespace() {
            v += 1;
        }
        let starts_literal = bytes.get(v) == Some(&b'"')
            || bytes.get(v) == Some(&b'r')
                && (bytes.get(v + 1) == Some(&b'"') || bytes.get(v + 1) == Some(&b'#'));
        if !starts_literal {
            i = name_start + 1;
            continue;
        }
        if let Some(lit) = scan_literals(&src[v..]).first() {
            out.entry(name)
                .or_insert_with(|| normalize_statement(&lit.text));
        }
        i = v + 1;
    }
    out
}

fn builtin_hole(hole: &str) -> Option<String> {
    Some(match hole {
        "placeholders" | "ph" | "sid_ph" | "task_ph" | "placeholder" => "?".to_string(),
        "sid"
        | "below_seq"
        | "rows"
        | "target"
        | "message_id"
        | "limit_placeholder"
        | "MAX_ORCHESTRATOR_FACT_SCAN_ROWS"
        | "padding"
        | "limit"
        | "count"
        | "seq" => "1".to_string(),
        "data_expr" => "m.data".to_string(),
        "filter" | "column_list" => String::new(),
        _ => return None,
    })
}

/// Render a statement key into executable SQL: `format!` holes are filled
/// from JSON `vars`, source `&str` consts, expansions or the built-in
/// representative values.
fn render_case(
    entry: &StatementEntry,
    hole: Option<&str>,
    value: Option<&str>,
    consts: &BTreeMap<String, String>,
) -> Result<String, String> {
    if !entry.format {
        return Ok(entry.key.clone());
    }
    let mut out = String::with_capacity(entry.key.len());
    let chars: Vec<char> = entry.key.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        if c == '{' && i + 1 < n && chars[i + 1] == '{' {
            out.push('{');
            i += 2;
            continue;
        }
        if c == '}' && i + 1 < n && chars[i + 1] == '}' {
            out.push('}');
            i += 2;
            continue;
        }
        if c == '{' {
            let close = (i..n)
                .find(|&x| chars[x] == '}')
                .ok_or_else(|| format!("unterminated hole in {}", entry.key))?;
            let name: String = chars[i + 1..close].iter().collect();
            if let Some(expanded) = hole {
                if name == expanded {
                    out.push_str(value.unwrap_or("?"));
                    i = close + 1;
                    continue;
                }
            }
            if name.is_empty() {
                if !out.ends_with('?') {
                    out.push('?');
                }
                i = close + 1;
                continue;
            }
            if let Some(v) = entry.vars.get(&name) {
                out.push_str(v);
            } else if let Some(v) = consts.get(&name) {
                out.push_str(v);
            } else if let Some(v) = builtin_hole(&name) {
                out.push_str(&v);
            } else {
                return Err(format!("unknown format hole `{{{name}}}` in {}", entry.key));
            }
            i = close + 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Ok(normalize_statement(&out))
}

// ------------------------------------------------------- EQP + schema facts

fn explain_plan(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut stmt = match conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")) {
        Ok(stmt) => stmt,
        Err(e) => return Err(format!("prepare failed: {e}")),
    };
    let nulls = vec![rusqlite::types::Value::Null; stmt.parameter_count()];
    let mut rows = match stmt.query(rusqlite::params_from_iter(nulls.iter())) {
        Ok(rows) => rows,
        Err(e) => return Err(format!("step failed: {e}")),
    };
    let mut out = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => match row.get::<_, String>(3) {
                Ok(detail) => out.push(detail),
                Err(e) => return Err(format!("detail column unreadable: {e}")),
            },
            Ok(None) => return Ok(out),
            Err(e) => return Err(format!("step failed: {e}")),
        }
    }
}

fn schema_tables(conn: &rusqlite::Connection) -> BTreeSet<String> {
    catalog_strings(conn, "SELECT name FROM sqlite_master WHERE type = 'table'")
}

fn catalog_strings(conn: &rusqlite::Connection, query: &str) -> BTreeSet<String> {
    let mut stmt = conn.prepare(query).expect("catalog query");
    let mut rows = stmt.query([]).expect("catalog rows");
    let mut out = BTreeSet::new();
    while let Some(row) = rows.next().expect("catalog step") {
        out.insert(row.get::<_, String>(0).expect("catalog value"));
    }
    out
}

fn indexed_tables(conn: &rusqlite::Connection) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare("SELECT tbl_name FROM sqlite_master WHERE type = 'index'")
        .expect("index catalog query");
    let mut rows = stmt.query([]).expect("index catalog rows");
    let mut out = BTreeSet::new();
    while let Some(row) = rows.next().expect("index catalog step") {
        out.insert(row.get::<_, String>(0).expect("index table"));
    }
    out
}

/// Lowercased relations (tables + aliases) a statement references, for SCAN
/// attribution. Alias mappings are `alias->table`.
fn referenced_relations(sql: &str) -> BTreeSet<String> {
    let upper = sql.to_ascii_lowercase();
    const NOISE: &[&str] = &[
        "where", "order", "limit", "set", "values", "on", "group", "having", "union", "select",
        "join", "left", "right", "inner", "outer", "and", "or", "as", "using", "cross", "from",
        "into", "table", "indexed", "by",
    ];
    let mut out = BTreeSet::new();
    for kw in ["from ", "join ", "into ", "update "] {
        let mut search = 0usize;
        while let Some(idx) = upper[search..].find(kw) {
            let start = search + idx + kw.len();
            search = start;
            let rest = &upper[start..];
            let word: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if word.is_empty() || SQL_KEYWORDS.contains(&word.as_str()) || word == "temp" {
                continue;
            }
            out.insert(word.clone());
            let after = rest[word.len()..].trim_start();
            let alias: String = if let Some(stripped) = after.strip_prefix("as ") {
                stripped
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect()
            } else if after.starts_with('(') {
                String::new()
            } else {
                after
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect()
            };
            if !alias.is_empty() && alias != word && !NOISE.contains(&alias.as_str()) {
                out.insert(format!("{alias}->{word}"));
            }
        }
    }
    out
}

/// Resolve `SCAN <token>` to a schema table (token may be an alias).
/// `SCAN … USING [COVERING] INDEX …` is index-backed, never a full scan.
fn scan_table_of(plan_line: &str, relations: &BTreeSet<String>) -> Option<String> {
    let rest = plan_line.strip_prefix("SCAN ")?;
    if rest.contains("USING") && rest.contains("INDEX") {
        return None;
    }
    let token = rest
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|c| c == '(' || c == ')' || c == ',')
        .to_ascii_lowercase();
    if token.is_empty() || token.starts_with('(') || token == "constant" {
        return None;
    }
    if relations.contains(&token) {
        return Some(token);
    }
    relations
        .iter()
        .find_map(|r| r.strip_prefix(&format!("{token}->")).map(str::to_string))
}

// --------------------------------------------------------------- extraction

#[derive(Debug, Default)]
struct Extraction {
    /// statement key -> source files
    statements: BTreeMap<String, BTreeSet<String>>,
    /// statement key -> first source position (walk order)
    first_order: BTreeMap<String, usize>,
    /// statement key -> format flag
    format: BTreeMap<String, bool>,
    /// site key -> site record
    sites: BTreeMap<String, DynamicSiteEntry>,
    /// fn -> statement keys (for site auto-resolution)
    fn_statements: BTreeMap<String, BTreeSet<String>>,
    /// `fn|variable` -> statement keys of its `let var = <sql>` initializer
    var_statements: BTreeMap<String, BTreeSet<String>>,
    /// Every `schema.rs` statement OCCURRENCE in source order
    /// `(order, key)` — duplicates preserved (see `Registry::ladder`).
    ladder_seq: Vec<(usize, String)>,
    /// total statements seen (monotone order counter)
    seen: usize,
}

fn extract() -> Extraction {
    let mut files = Vec::new();
    collect_source_files(&src_dir(), &mut files);
    let mut ex = Extraction::default();
    for path in &files {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let stripped = blank_comments(&strip_test_regions(&raw));
        let rel = path
            .strip_prefix(src_dir())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let spans = fn_spans(&stripped);
        for lit in scan_literals(&stripped) {
            if sql_leading_keyword(&lit.text).is_none() {
                continue;
            }
            let fns: BTreeSet<String> = enclosing_fn(&spans, lit.pos)
                .map(|f| f.to_string())
                .into_iter()
                .collect();
            for statement in split_statements(&lit.text) {
                if sql_leading_keyword(&statement).is_none() {
                    continue;
                }
                ex.seen += 1;
                ex.statements
                    .entry(statement.clone())
                    .or_default()
                    .insert(rel.clone());
                ex.format
                    .entry(statement.clone())
                    .or_insert(lit.format_mode);
                ex.first_order.entry(statement.clone()).or_insert(ex.seen);
                if rel == "schema.rs" {
                    ex.ladder_seq.push((ex.seen, statement.clone()));
                }
                for f in &fns {
                    ex.fn_statements
                        .entry(f.clone())
                        .or_default()
                        .insert(statement.clone());
                }
            }
        }
        for (key, site) in scan_dynamic_sites(&stripped, &rel) {
            ex.sites.insert(key, site);
        }
        scan_let_bindings(&stripped, &spans, &mut ex);
    }
    ex
}

// ------------------------------------------------------------------ tests

/// HAR-10 drift check: the registry lists EXACTLY every SQL statement the
/// source scanner extracts — no unlisted new statement, no stale entry —
/// and every class/reason/rendering rule holds.
#[test]
fn inventory_covers_every_sql_statement() {
    let ex = extract();
    let reg = registry();
    assert_eq!(
        reg.statements.len(),
        reg.statements
            .iter()
            .map(|e| &e.key)
            .collect::<BTreeSet<_>>()
            .len(),
        "duplicate statement keys in the registry"
    );
    let registered: BTreeMap<&str, &StatementEntry> =
        reg.statements.iter().map(|e| (e.key.as_str(), e)).collect();

    let mut missing: Vec<_> = ex
        .statements
        .keys()
        .filter(|k| !registered.contains_key(k.as_str()))
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "NEW SQL statements are unlisted — run record_query_plan_inventory, \
         review the diff and commit the registry:\n{}",
        missing
            .iter()
            .take(20)
            .map(|k| format!("  {k}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let mut stale: Vec<&str> = registered
        .keys()
        .copied()
        .filter(|k| {
            let entry = registered[*k];
            entry.source == "extracted" && !ex.statements.contains_key(*k)
        })
        .collect();
    stale.sort();
    assert!(
        stale.is_empty(),
        "registry entries with no source statement (stale):\n{}",
        stale
            .iter()
            .take(20)
            .map(|k| format!("  {k}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // The ladder occurrence sequence (duplicates included) must match the
    // source scanner exactly — the walk replays it verbatim.
    assert_eq!(
        reg.ladder, ex.ladder_seq,
        "schema ladder occurrence drift — re-record the inventory"
    );

    // Every representative must be referenced by at least one dynamic site
    // and must not shadow an extracted statement.
    let used_in_sites: BTreeSet<&str> = reg
        .dynamic_sites
        .iter()
        .flat_map(|s| s.plan_keys.iter().map(String::as_str))
        .collect();
    for entry in reg
        .statements
        .iter()
        .filter(|e| e.source == "representative")
    {
        assert!(
            !ex.statements.contains_key(&entry.key),
            "representative {} collides with an extracted statement",
            entry.key
        );
        assert!(
            used_in_sites.contains(entry.key.as_str()),
            "stale representative {} (referenced by no dynamic site)",
            entry.key
        );
        let basis = entry
            .basis
            .as_deref()
            .unwrap_or_else(|| panic!("representative {} needs a basis", entry.key));
        let basis_order = *ex.first_order.get(basis).unwrap_or_else(|| {
            panic!("representative {} basis `{basis}` not extracted", entry.key)
        });
        assert_eq!(
            entry.order, basis_order,
            "representative {} order must follow its basis {basis}",
            entry.key
        );
    }

    for entry in &reg.statements {
        assert!(
            matches!(
                entry.class.as_str(),
                "query" | "mutate" | "ddl" | "pragma" | "txn" | "fragment" | "non-sql"
            ),
            "unknown class `{}` for {}",
            entry.class,
            entry.key
        );
        if entry.source == "extracted" {
            let source_format = *ex
                .format
                .get(&entry.key)
                .expect("registry key present in extraction");
            assert_eq!(
                entry.format, source_format,
                "format flag drift for {}",
                entry.key
            );
            let source_order = *ex
                .first_order
                .get(&entry.key)
                .expect("registry key present in extraction");
            assert_eq!(
                entry.order, source_order,
                "source order drift for {} — re-record the inventory",
                entry.key
            );
        }
        match entry.class.as_str() {
            "non-sql" => {
                assert!(
                    entry.reason.is_some(),
                    "non-sql entry needs a reason: {}",
                    entry.key
                );
                assert!(
                    entry.cases.is_empty(),
                    "non-sql entry must not carry cases: {}",
                    entry.key
                );
            }
            "fragment" => {
                assert!(
                    entry.override_rendered,
                    "fragment entries must carry hand-maintained rendered cases: {}",
                    entry.key
                );
                assert!(
                    !entry.cases.is_empty(),
                    "fragment entry missing cases: {}",
                    entry.key
                );
            }
            _ => {
                assert!(
                    !entry.cases.is_empty(),
                    "statement has no rendered cases: {}",
                    entry.key
                );
            }
        }
    }
}

/// HAR-10: every dynamic (runtime-assembled) SQL execution site resolves to
/// a registered, planned statement or an explicit reason.
#[test]
fn every_dynamic_site_resolves_to_a_planned_statement() {
    let ex = extract();
    let reg = registry();
    let registered: BTreeMap<&str, &DynamicSiteEntry> = reg
        .dynamic_sites
        .iter()
        .map(|e| (e.key.as_str(), e))
        .collect();

    let mut missing: Vec<_> = ex
        .sites
        .keys()
        .filter(|k| !registered.contains_key(k.as_str()))
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "NEW dynamic SQL sites are unlisted:\n{}",
        missing
            .iter()
            .take(20)
            .map(|k| format!("  {k}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut stale: Vec<&str> = registered
        .keys()
        .copied()
        .filter(|k| !ex.sites.contains_key(*k))
        .collect();
    stale.sort();
    assert!(
        stale.is_empty(),
        "stale dynamic sites:\n{}",
        stale
            .iter()
            .take(20)
            .map(|k| format!("  {k}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Planned statements: every mapped target must carry cases.
    let planned: BTreeSet<&str> = reg
        .statements
        .iter()
        .filter(|e| e.class != "non-sql" && !e.cases.is_empty())
        .map(|e| e.key.as_str())
        .collect();
    for site in &reg.dynamic_sites {
        let has_reason = site.reason.as_deref().is_some_and(|r| !r.trim().is_empty());
        assert!(
            has_reason == site.plan_keys.is_empty(),
            "dynamic site {} must carry exactly one of: a non-empty reason, \
             or non-empty plan_keys",
            site.key
        );
        if has_reason {
            continue;
        }
        for plan_key in &site.plan_keys {
            assert!(
                planned.contains(plan_key.as_str()),
                "dynamic site {} plans against a missing/unplanned statement {plan_key}",
                site.key
            );
        }
    }
}

/// Fresh in-memory connection representing a FROM-SCRATCH database: DDL
/// entries replay in source order (exactly the migration ladder a first
/// open performs), so every statement prepares against the schema state
/// that exists at its source point — TEMP tables of mid-migration steps
/// included — while head-only state can never mask a broken statement.
fn walk_conn() -> rusqlite::Connection {
    rusqlite::Connection::open_in_memory().expect("in-memory inventory walk connection")
}

/// The statement a DDL entry executes at its source point.
fn entry_exec_sql(entry: &StatementEntry) -> &str {
    entry
        .cases
        .first()
        .map(|c| c.sql.as_str())
        .unwrap_or(&entry.key)
}

/// Execute one DDL entry at its source point; the error text is the typed
/// drift report (a fresh replay must succeed).
fn execute_ddl(conn: &rusqlite::Connection, entry: &StatementEntry) -> Result<(), String> {
    let sql = entry_exec_sql(entry);
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("DDL SOURCE-POINT FAILURE [{}] prepare: {e}", entry.key))?;
    let nulls = vec![rusqlite::types::Value::Null; stmt.parameter_count()];
    stmt.execute(rusqlite::params_from_iter(nulls.iter()))
        .map_err(|e| format!("DDL SOURCE-POINT FAILURE [{}] execute: {e}", entry.key))?;
    Ok(())
}

/// Shared walk over a fresh connection:
///
/// 1. **Ladder phase** - replay `reg.ladder` (every `schema.rs` statement
///    occurrence, duplicates included): each statement's cases are EXPLAINed
///    at its first occurrence, and DDL executes at EVERY occurrence,
///    reproducing what the production migration loop does (re-running
///    idempotent DDL after a table rebuild re-creates its indexes).
/// 2. **Runtime phase** - every non-ladder statement is EXPLAINed against
///    the head schema the ladder produced.
fn walk_inventory<F>(reg: &Registry, check: &mut F) -> Vec<String>
where
    F: FnMut(&StatementEntry, &CaseRecord, &rusqlite::Connection) -> Vec<String>,
{
    let conn = walk_conn();
    let by_key: BTreeMap<&str, &StatementEntry> =
        reg.statements.iter().map(|e| (e.key.as_str(), e)).collect();
    let mut failures = Vec::new();
    let mut eqp_done: BTreeSet<&str> = BTreeSet::new();
    let mut ladder_keys: BTreeSet<&str> = BTreeSet::new();
    for (_, key) in &reg.ladder {
        let Some(entry) = by_key.get(key.as_str()) else {
            failures.push(format!("ladder step references unknown statement: {key}"));
            continue;
        };
        ladder_keys.insert(entry.key.as_str());
        if entry.class == "non-sql" {
            continue;
        }
        if eqp_done.insert(entry.key.as_str()) {
            for case in &entry.cases {
                failures.extend(check(entry, case, &conn));
            }
        }
        if entry.class == "ddl" {
            if let Err(e) = execute_ddl(&conn, entry) {
                failures.push(e);
            }
        }
    }
    let mut runtime: Vec<&StatementEntry> = reg
        .statements
        .iter()
        .filter(|e| !ladder_keys.contains(e.key.as_str()))
        .collect();
    runtime.sort_by_key(|e| (e.order, e.key.as_str()));
    for entry in runtime {
        if entry.class == "non-sql" {
            continue;
        }
        for case in &entry.cases {
            failures.extend(check(entry, case, &conn));
        }
        if entry.class == "ddl" {
            if let Err(e) = execute_ddl(&conn, entry) {
                failures.push(e);
            }
        }
    }
    failures
}

/// Build one registry entry for an extracted key: merge hand annotations,
/// render the representative case SQL and EXPLAIN every case (recording the
/// golden plan). DDL execution stays with the caller - the ladder phase
/// must execute duplicates.
#[allow(clippy::too_many_arguments)]
fn build_entry(
    key: &str,
    ex: &Extraction,
    prev_statements: &BTreeMap<&str, &StatementEntry>,
    consts: &BTreeMap<String, String>,
    conn: &rusqlite::Connection,
    needs_review: &mut Vec<String>,
) -> StatementEntry {
    let files: Vec<String> = ex.statements[key].iter().cloned().collect();
    let format = *ex.format.get(key).expect("format flag");
    let order = *ex.first_order.get(key).expect("order");
    let prev = prev_statements.get(key).copied();
    let mut entry = StatementEntry {
        key: key.to_string(),
        order,
        source: default_source(),
        basis: None,
        format,
        class: prev
            .map(|p| p.class.clone())
            .unwrap_or_else(|| default_class(key)),
        files,
        vars: prev.map(|p| p.vars.clone()).unwrap_or_default(),
        expand: prev.map(|p| p.expand.clone()).unwrap_or_default(),
        override_rendered: prev.is_some_and(|p| p.override_rendered),
        allow_scan: prev.and_then(|p| p.allow_scan.clone()),
        reason: prev.and_then(|p| p.reason.clone()),
        cases: Vec::new(),
    };
    if entry.class == "non-sql" {
        return entry;
    }
    let rendered: Result<Vec<String>, String> = if entry.override_rendered {
        Ok(prev
            .expect("override entries must exist")
            .cases
            .iter()
            .map(|c| c.sql.clone())
            .collect())
    } else if let Some((hole, values)) = entry.expand.iter().next() {
        let hole = hole.clone();
        let mut out = Vec::new();
        let mut err = None;
        for value in values {
            match render_case(&entry, Some(&hole), Some(value), consts) {
                Ok(sql) => out.push(sql),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        match err {
            Some(e) => Err(e),
            None => Ok(out),
        }
    } else {
        render_case(&entry, None, None, consts).map(|sql| vec![sql])
    };
    let rendered = match rendered {
        Ok(rendered) => rendered,
        Err(e) => {
            needs_review.push(format!(
                "render failed [{}] {e} - annotate as non-sql (with reason) or \
                 supply vars/override rendering",
                entry.key
            ));
            return entry;
        }
    };
    let prev_cases: BTreeMap<&str, &CaseRecord> = prev
        .map(|p| p.cases.iter().map(|c| (c.sql.as_str(), c)).collect())
        .unwrap_or_default();
    for sql in rendered {
        let prev_case = prev_cases.get(sql.as_str()).copied();
        match explain_plan(conn, &sql) {
            Ok(plan) => {
                entry.cases.push(CaseRecord {
                    sql,
                    plan,
                    allow_scan: prev_case.and_then(|c| c.allow_scan.clone()),
                    require_index: prev_case.and_then(|c| c.require_index.clone()),
                });
            }
            Err(e) => {
                needs_review.push(format!(
                    "EQP FAILED [{}] class={} :: {e}\n    {}",
                    entry.class, entry.key, sql
                ));
                entry.cases.push(CaseRecord {
                    sql,
                    plan: Vec::new(),
                    allow_scan: None,
                    require_index: None,
                });
            }
        }
    }
    entry
}

/// HAR-10 plan gate: every case prepares, plan-bearing classes carry a plan,
/// the recorded plan matches EXPLAIN QUERY PLAN exactly, indexed tables are
/// never scanned without a blessing, and typed-ledger range reads ride their
/// typed index.
#[test]
fn every_statement_has_the_intended_plan() {
    let reg = registry();
    let failures = walk_inventory(&reg, &mut |entry, case, conn| {
        let mut out = Vec::new();
        match explain_plan(conn, &case.sql) {
            Err(e) => {
                out.push(format!(
                    "PREPARE FAILED [{}]: {e}\n    {}",
                    entry.key, case.sql
                ));
            }
            Ok(plan) => {
                if plan != case.plan {
                    out.push(format!(
                        "PLAN DRIFT [{}]\n    sql: {}\n    expected: {:?}\n    actual:   {:?}",
                        entry.key, case.sql, case.plan, plan
                    ));
                }
                let plan_bearing_required = match entry.class.as_str() {
                    "query" => true,
                    "mutate" => !case.sql.to_ascii_uppercase().starts_with("INSERT"),
                    _ => false,
                };
                if plan_bearing_required && plan.is_empty() {
                    out.push(format!(
                        "MISSING PLAN [{}]: {} produced no EXPLAIN QUERY PLAN rows",
                        entry.key, case.sql
                    ));
                }
                let bless = case.allow_scan.as_ref().or(entry.allow_scan.as_ref());
                if bless.is_none() {
                    let relations = referenced_relations(&case.sql);
                    let tables = schema_tables(conn);
                    let indexed = indexed_tables(conn);
                    for line in &plan {
                        if let Some(table) = scan_table_of(line, &relations) {
                            if tables.contains(&table) && indexed.contains(&table) {
                                out.push(format!(
                                    "UNBLESSED SCAN [{}] scans indexed table `{table}` \
                                     (add allow_scan with a reason): {}",
                                    entry.key, case.sql
                                ));
                            }
                        }
                    }
                }
                let require = case
                    .require_index
                    .as_deref()
                    .or_else(|| required_index_for(&entry.class, &entry.key));
                if let Some(index) = require {
                    if !plan.iter().any(|line| line.contains(index)) {
                        out.push(format!(
                            "WRONG ACCESS PATH [{}]: plan must use `{index}`\n    plan: {:?}",
                            entry.key, plan
                        ));
                    }
                }
            }
        }
        out
    });
    assert!(
        failures.is_empty(),
        "{} statement plan failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Structural intent for typed-ledger range reads (programme style:
/// `kind >= 'memory_v2:' AND kind < 'memory_v2;'` rides the v2 page index,
/// a typed `entry_type = ?` + `seq` range rides
/// `idx_ledger_entry_session_type_seq`). Schema DDL is exempt: a
/// `CREATE INDEX … WHERE <predicate>` statement contains the predicate but
/// has no plan.
fn required_index_for(class: &str, key: &str) -> Option<&'static str> {
    if class == "ddl" || class == "txn" || class == "pragma" {
        return None;
    }
    if key.contains("kind >= 'memory_v2:'") && key.contains("memory_v2;") {
        if key.contains("workspace_memory_fact") {
            return Some("idx_workspace_memory_fact_v2_page");
        }
        return Some("idx_memory_fact_v2_page");
    }
    if key.contains("FROM ledger_entry")
        && key.contains("entry_type =")
        && (key.contains("seq >") || key.contains("seq <"))
    {
        return Some("idx_ledger_entry_session_type_seq");
    }
    None
}

fn read_source_flat() -> String {
    let mut files = Vec::new();
    collect_source_files(&src_dir(), &mut files);
    let mut out = String::new();
    for path in files {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            out.push_str(&raw);
        }
    }
    out
}

// ------------------------------------------------------------------ recorder

/// Regenerates `query_plan_inventory.json` from the current source + schema
/// head, MERGING hand annotations (class/reason/vars/expand/
/// override_rendered/allow_scan/require_index) for statements and cases that
/// still exist. Review the diff: new SCANs, lost blessings and plan changes
/// must be re-justified.
#[test]
#[ignore = "[perf] manual inventory regeneration; run to re-record plans (query-plan inventory)"]
fn record_query_plan_inventory() {
    let ex = extract();
    let previous: Registry = match std::fs::read_to_string(registry_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            panic!("existing registry unreadable ({e}); fix or delete it first")
        }),
        Err(_) => Registry {
            schema: REGISTRY_SCHEMA.to_string(),
            ladder: Vec::new(),
            statements: Vec::new(),
            dynamic_sites: Vec::new(),
        },
    };
    let prev_statements: BTreeMap<&str, &StatementEntry> = previous
        .statements
        .iter()
        .map(|e| (e.key.as_str(), e))
        .collect();
    let prev_sites: BTreeMap<&str, &DynamicSiteEntry> = previous
        .dynamic_sites
        .iter()
        .map(|e| (e.key.as_str(), e))
        .collect();

    let conn = walk_conn();
    let consts = source_string_consts(&read_source_flat());

    let mut needs_review = Vec::new();

    // One entry per unique statement: cases are built (and EXPLAINed) at
    // the entry's FIRST ladder occurrence, while DDL executes at EVERY
    // ladder occurrence — the production migration loop re-runs idempotent
    // DDL after table rebuilds (`CREATE INDEX … IF NOT EXISTS`), and
    // skipping the duplicates would plan against a schema production never
    // has. Runtime statements then record against the head schema.
    let mut built: BTreeMap<String, StatementEntry> = BTreeMap::new();
    let ddl_step = |entry: &StatementEntry, review: &mut Vec<String>| {
        if entry.class == "ddl" {
            if let Err(e) = execute_ddl(&conn, entry) {
                review.push(e);
            }
        }
    };
    for (_, key) in &ex.ladder_seq {
        if let Some(entry) = built.get(key) {
            let entry = entry.clone();
            ddl_step(&entry, &mut needs_review);
        } else {
            let entry = build_entry(
                key,
                &ex,
                &prev_statements,
                &consts,
                &conn,
                &mut needs_review,
            );
            ddl_step(&entry, &mut needs_review);
            built.insert(key.clone(), entry);
        }
    }
    for key in ex.statements.keys() {
        if built.contains_key(key) {
            continue;
        }
        let entry = build_entry(
            key,
            &ex,
            &prev_statements,
            &consts,
            &conn,
            &mut needs_review,
        );
        ddl_step(&entry, &mut needs_review);
        built.insert(key.clone(), entry);
    }
    let mut statements: Vec<StatementEntry> = built.into_values().collect();

    // Hand-registered representative statements (full text of a
    // runtime-assembled statement) are preserved and re-recorded: their
    // basis orders them in the walk, their hand-maintained cases are
    // re-EXPLAINed against the current schema.
    for prev_entry in &previous.statements {
        if prev_entry.source != "representative" {
            continue;
        }
        let mut entry = prev_entry.clone();
        let basis_ok = match entry.basis.as_deref() {
            Some(basis) => match ex.first_order.get(basis) {
                Some(order) => {
                    entry.order = *order;
                    true
                }
                None => false,
            },
            None => false,
        };
        if !basis_ok {
            needs_review.push(format!(
                "representative {} needs a `basis` naming an extracted statement",
                entry.key
            ));
            statements.push(entry);
            continue;
        }
        if ex.statements.contains_key(&entry.key) {
            needs_review.push(format!(
                "representative {} collides with an extracted statement; drop \
                 `source: representative`",
                entry.key
            ));
        }
        let mut cases = Vec::new();
        for case in &entry.cases {
            let mut case = case.clone();
            match explain_plan(&conn, &case.sql) {
                Ok(plan) => case.plan = plan,
                Err(e) => needs_review.push(format!(
                    "representative EQP FAILED [{}] :: {e}\n    {}",
                    entry.key, case.sql
                )),
            }
            cases.push(case);
        }
        if cases.is_empty() {
            needs_review.push(format!(
                "representative {} has no cases (give it the full statement)",
                entry.key
            ));
        }
        entry.cases = cases;
        statements.push(entry);
    }
    statements.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.key.cmp(&b.key)));

    let mut dynamic_sites = Vec::new();
    for (key, site) in &ex.sites {
        let prev = prev_sites.get(key.as_str()).copied();
        let mut entry = site.clone();
        if let Some(prev) = prev {
            entry.plan_keys = prev.plan_keys.clone();
            entry.reason = prev.reason.clone();
        }
        let annotated = entry
            .reason
            .as_deref()
            .is_some_and(|r| !r.trim().is_empty())
            || !entry.plan_keys.is_empty();
        if !annotated {
            let func = key.split(" || ").next().unwrap_or_default();
            let expr = key
                .split_once(" || ")
                .map(|(_, e)| e)
                .unwrap_or_default()
                .to_string();
            // A statement is a mapping candidate only when it is actually
            // planned (class + cases exist in the recorded registry).
            let planned = |k: &String| -> bool {
                match prev_statements.get(k.as_str()) {
                    Some(p) => p.class != "non-sql" && !p.cases.is_empty(),
                    None => default_class(k) != "non-sql",
                }
            };
            // (a) a SQL literal inside the RAW expression (`&format!("…")`).
            let lit_candidates: BTreeSet<String> = scan_literals(&entry.raw)
                .into_iter()
                .filter(|lit| sql_leading_keyword(&lit.text).is_some())
                .flat_map(|lit| split_statements(&lit.text))
                .filter(|k| ex.statements.contains_key(k))
                .filter(&planned)
                .collect();
            // (b) the initializer of the referenced variable (`&insert_sql`).
            let var = expr
                .trim_start_matches('&')
                .trim_start_matches('(')
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default();
            let var_candidates: BTreeSet<String> = ex
                .var_statements
                .get(&format!("{func}|{var}"))
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(&planned)
                .collect();
            // (c) an associated/plain const named in the expression
            // (`Self::VERIFICATION_RECORD_SCOPED_SELECT`).
            let const_name = expr.rsplit("::").next().unwrap_or_default();
            let const_candidates: BTreeSet<String> = consts
                .get(const_name)
                .map(|value| {
                    split_statements(value)
                        .into_iter()
                        .filter(|k| ex.statements.contains_key(k))
                        .filter(&planned)
                        .collect()
                })
                .unwrap_or_default();
            // (d) every planned statement declared in the same fn (covers
            // runtime branch sites like `prepare(sql)` with two branches).
            let fn_candidates: BTreeSet<String> = ex
                .fn_statements
                .get(func)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(&planned)
                .collect();
            let chosen = [
                lit_candidates,
                var_candidates,
                const_candidates,
                fn_candidates,
            ]
            .into_iter()
            .find(|c| !c.is_empty());
            match chosen {
                Some(chosen) => {
                    entry.plan_keys = chosen.into_iter().collect::<Vec<_>>();
                }
                None => needs_review.push(format!(
                    "dynamic site needs a plan_key or reason: {key} — no planned \
                     candidates in raw expression / var `{var}` / const \
                     `{const_name}` / fn `{func}`"
                )),
            }
        }
        dynamic_sites.push(entry);
    }

    let registry = Registry {
        schema: REGISTRY_SCHEMA.to_string(),
        ladder: ex.ladder_seq.clone(),
        statements,
        dynamic_sites,
    };
    let json = serde_json::to_string_pretty(&registry).expect("serialize registry");
    std::fs::write(registry_path(), format!("{json}\n")).expect("write registry");
    println!(
        "recorded {} statements, {} dynamic sites",
        registry.statements.len(),
        registry.dynamic_sites.len()
    );
    if !needs_review.is_empty() {
        println!("NEEDS REVIEW ({}):", needs_review.len());
        for line in &needs_review {
            println!("  {line}");
        }
    }
    assert!(
        needs_review.is_empty(),
        "{} entries need annotation before the inventory can gate",
        needs_review.len()
    );
}

fn default_class(key: &str) -> String {
    let upper = key.to_ascii_uppercase();
    let first = upper.split_whitespace().next().unwrap_or("");
    match first {
        "SELECT" | "WITH" | "EXPLAIN" => "query".to_string(),
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" => "mutate".to_string(),
        "CREATE" | "ALTER" | "DROP" => "ddl".to_string(),
        "PRAGMA" => "pragma".to_string(),
        "BEGIN" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => "txn".to_string(),
        "ANALYZE" | "VACUUM" => "ddl".to_string(),
        _ => "query".to_string(),
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_finds_the_expected_scope() {
        let ex = extract();
        assert!(
            ex.statements.len() > 200,
            "extraction found only {} statements; the scanner is broken",
            ex.statements.len()
        );
        assert!(
            ex.sites.len() > 10,
            "extraction found only {} dynamic sites",
            ex.sites.len()
        );
        // Test-only SQL must never be enumerated.
        for key in ex.statements.keys() {
            assert!(
                !key.contains("hash-race") && !key.contains("attachment_boom"),
                "test-only statement leaked into inventory: {key}"
            );
        }
    }

    #[test]
    fn strip_test_regions_removes_inline_test_modules() {
        let src = "fn real() {}\n#[cfg(test)]\nmod tests { fn t() { let _ = \"SELECT 1\"; } }\n";
        let stripped = strip_test_regions(src);
        assert!(
            !stripped.contains("SELECT 1"),
            "test region must be blanked"
        );
        assert!(stripped.contains("fn real()"));
        let src2 = "#[cfg(not(any(test, feature = \"fault-injection\")))]\nstruct X;\n";
        assert!(strip_test_regions(src2).contains("struct X"));
    }

    #[test]
    fn blank_comments_keeps_strings_and_offsets() {
        let src = "// conn.execute(\"SELECT 1\")\nlet a = \"x // y\";\n/* block */let b = 1;";
        let out = blank_comments(src);
        assert!(!out.contains("conn.execute"));
        assert!(out.contains("\"x // y\""));
        assert_eq!(out.len(), src.len());
    }

    #[test]
    fn split_statements_respects_quotes_and_comments() {
        let sql = "INSERT INTO t VALUES ('a;b'); -- c;\nDELETE FROM t; /* x; */ UPDATE t SET a=1;";
        let parts = split_statements(sql);
        assert_eq!(parts.len(), 3, "parts: {parts:?}");
        assert!(parts[0].starts_with("INSERT INTO t VALUES ('a;b')"));
        assert!(parts[1].starts_with("DELETE FROM t"));
        assert!(parts[2].starts_with("UPDATE t SET a=1"));
    }

    #[test]
    fn format_holes_render_representative_statements() {
        let mut entry = StatementEntry {
            key: "SELECT * FROM x WHERE id IN ({placeholders}) AND k = ?{}".to_string(),
            order: 1,
            source: default_source(),
            basis: None,
            format: true,
            class: "query".to_string(),
            files: vec!["x.rs".into()],
            vars: BTreeMap::new(),
            expand: BTreeMap::new(),
            override_rendered: false,
            allow_scan: None,
            reason: None,
            cases: Vec::new(),
        };
        let consts = BTreeMap::new();
        let rendered = render_case(&entry, None, None, &consts).unwrap();
        assert_eq!(rendered, "SELECT * FROM x WHERE id IN (?) AND k = ?");
        entry
            .expand
            .insert("table".to_string(), vec!["t1".into(), "t2".into()]);
        entry.key = "DELETE FROM {table} WHERE session_id = ?1".to_string();
        let a = render_case(&entry, Some("table"), Some("t1"), &consts).unwrap();
        let b = render_case(&entry, Some("table"), Some("t2"), &consts).unwrap();
        assert_eq!(a, "DELETE FROM t1 WHERE session_id = ?1");
        assert_eq!(b, "DELETE FROM t2 WHERE session_id = ?1");
    }

    #[test]
    fn scan_rules_reject_unblessed_scans_and_detect_aliases() {
        let relations =
            referenced_relations("SELECT * FROM message m JOIN part p ON 1=1 WHERE m.id = p.id");
        assert!(relations.contains("message"));
        assert!(relations.contains("m->message"));
        assert!(scan_table_of("SCAN m", &relations).as_deref() == Some("message"));
        assert!(scan_table_of("SCAN CONSTANT ROW", &relations).is_none());
        assert!(scan_table_of("SCAN (subquery-1)", &relations).is_none());
        let relations = referenced_relations("DELETE FROM event WHERE seq = 1");
        assert!(
            scan_table_of(
                "SCAN event USING INDEX idx_event_session_seq (seq=?)",
                &relations
            )
            .is_none(),
            "index-backed scans are not full scans"
        );
        assert!(
            scan_table_of("SCAN event", &relations).as_deref() == Some("event"),
            "plain scans still attribute to the table"
        );
    }
}
