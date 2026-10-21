// Real-workspace task corpus for the agent-intelligence measurement.
//
// Each task materializes a small but real git workspace, runs a hidden test
// suite as the EXTERNAL verification authority (never the agent's own claim),
// and asks for a complete engineering change in one prompt. The baseline and
// improved pipelines execute the same task bytes; only the pipeline version
// differs.
//
// Categories: bug_fix, failing_test, cross_file_refactor, migration,
// security_fix.

const rustManifest = (name) => `[package]
name = "${name}"
version = "0.1.0"
edition = "2021"
`;

export const tasks = [
  {
    id: "rust_bug_wrap",
    category: "bug_fix",
    language: "rust",
    prompt:
      "Fix wrap in src/wrap.rs. It must collapse runs of whitespace, drop leading and trailing whitespace, and never return empty lines. Keep the public API and the line-packing behavior at the width boundary.",
    testCmd: "cargo test --offline --quiet",
    files: {
      "Cargo.toml": rustManifest("textkit"),
      "src/lib.rs": `pub mod wrap;
pub use wrap::{wrap, wrap_words};
`,
      "src/wrap.rs": `pub fn wrap_words(words: &[&str], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in words {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current);
            current = (*word).to_string();
        }
    }
    lines.push(current);
    lines
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let words: Vec<&str> = text.split(' ').collect();
    wrap_words(&words, width)
}
`,
      "tests/wrap.rs": `use textkit::wrap;

#[test]
fn wraps_on_the_boundary() {
    assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
}
`,
    },
    hidden: {
      "tests/hidden_eval.rs": `use textkit::wrap;

#[test]
fn repeated_and_trailing_whitespace_never_yield_blank_lines() {
    assert_eq!(wrap("alpha  beta ", 11), vec!["alpha beta"]);
    assert_eq!(wrap("  lead", 20), vec!["lead"]);
    assert_eq!(wrap("trail  ", 20), vec!["trail"]);
}

#[test]
fn exact_width_still_packs() {
    assert_eq!(wrap("aaa bbb", 7), vec!["aaa bbb"]);
    assert_eq!(wrap("aaa bbb", 6), vec!["aaa", "bbb"]);
}
`,
    },
  },
  {
    id: "js_bug_ttl",
    category: "bug_fix",
    language: "js",
    prompt:
      "Fix parseTtl in src/parse.js: an explicit TTL of 0 seconds must be honored, not replaced by the default. Negative and non-numeric values still throw; null and empty input still use the default.",
    testCmd: "node --test",
    files: {
      "package.json": `{
  "name": "ttlkit",
  "version": "0.1.0",
  "type": "module",
  "scripts": { "test": "node --test" }
}
`,
      "src/parse.js": `export const DEFAULT_TTL = 60;

export function parseTtl(raw) {
  if (raw == null || raw === "") return DEFAULT_TTL;
  const n = Number(raw);
  if (!Number.isFinite(n) || n < 0) throw new Error("invalid ttl");
  return n || DEFAULT_TTL;
}
`,
      "src/config.js": `import { parseTtl } from "./parse.js";

export function cacheConfig(env) {
  return { ttl: parseTtl(env.TTL), scope: env.SCOPE || "local" };
}
`,
      "test/config.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { parseTtl, DEFAULT_TTL } from "../src/parse.js";
import { cacheConfig } from "../src/config.js";

test("default and positive ttl", () => {
  assert.equal(parseTtl(undefined), DEFAULT_TTL);
  assert.equal(parseTtl("15"), 15);
  assert.equal(cacheConfig({ TTL: "30" }).ttl, 30);
});
`,
    },
    hidden: {
      "test/hidden_eval.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { parseTtl, DEFAULT_TTL } from "../src/parse.js";

test("zero ttl is honored", () => {
  assert.equal(parseTtl("0"), 0);
  assert.equal(parseTtl(0), 0);
});

test("invalid input still refuses and empty still defaults", () => {
  assert.throws(() => parseTtl("-1"));
  assert.throws(() => parseTtl("abc"));
  assert.equal(parseTtl(""), DEFAULT_TTL);
});
`,
    },
  },
  {
    id: "rust_failing_stack",
    category: "failing_test",
    language: "rust",
    prompt:
      "The test suite fails. Fix the implementation in src/lib.rs so the existing tests pass; do not change the tests.",
    testCmd: "cargo test --offline --quiet",
    files: {
      "Cargo.toml": rustManifest("stackkit"),
      "src/lib.rs": `#[derive(Default)]
pub struct Stack<T> {
    items: Vec<T>,
}

impl<T> Stack<T> {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn push(&mut self, value: T) {
        self.items.push(value);
    }

    pub fn pop(&mut self) -> Option<T> {
        Some(self.items.pop().unwrap())
    }

    pub fn peek(&self) -> Option<&T> {
        Some(self.items.last().unwrap())
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
`,
      "tests/stack.rs": `use stackkit::Stack;

#[test]
fn empty_stack_peek_is_none() {
    let stack: Stack<i32> = Stack::new();
    assert_eq!(stack.peek(), None);
}

#[test]
fn push_and_peek() {
    let mut stack = Stack::new();
    stack.push(7);
    assert_eq!(stack.peek(), Some(&7));
    assert_eq!(stack.len(), 1);
}
`,
    },
    hidden: {
      "tests/hidden_eval.rs": `use stackkit::Stack;

#[test]
fn empty_pop_and_len_are_honest() {
    let mut stack: Stack<i32> = Stack::new();
    assert_eq!(stack.pop(), None);
    assert_eq!(stack.peek(), None);
    assert_eq!(stack.len(), 0);
    assert!(stack.is_empty());
}

#[test]
fn lifo_order_survives() {
    let mut stack = Stack::new();
    stack.push(1);
    stack.push(2);
    assert_eq!(stack.pop(), Some(2));
    assert_eq!(stack.pop(), Some(1));
    assert_eq!(stack.pop(), None);
}
`,
    },
  },
  {
    id: "js_failing_slug",
    category: "failing_test",
    language: "js",
    prompt:
      "The test suite fails. Fix src/slug.js so accented Latin text produces ASCII slugs; do not change the tests.",
    testCmd: "node --test",
    files: {
      "package.json": `{
  "name": "slugkit",
  "version": "0.1.0",
  "type": "module",
  "scripts": { "test": "node --test" }
}
`,
      "src/slug.js": `export function slugify(input) {
  return String(input)
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}
`,
      "test/slug.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { slugify } from "../src/slug.js";

test("ascii titles", () => {
  assert.equal(slugify("Hello, World!"), "hello-world");
});

test("accented latin titles", () => {
  assert.equal(slugify("Café déjà vu"), "cafe-deja-vu");
});
`,
    },
    hidden: {
      "test/hidden_eval.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { slugify } from "../src/slug.js";

test("diacritics fold to ascii", () => {
  assert.equal(slugify("Crème Brûlée"), "creme-brulee");
  assert.equal(slugify("Málaga über"), "malaga-uber");
  assert.equal(slugify("  Hello, World!  "), "hello-world");
});
`,
    },
  },
  {
    id: "rust_refactor_rename",
    category: "cross_file_refactor",
    language: "rust",
    prompt:
      "Rename the public helper compute_total to total_cents across the crate, including exports, callers and tests. No occurrence of the old name may remain. Behavior must not change.",
    testCmd: "cargo test --offline --quiet",
    files: {
      "Cargo.toml": rustManifest("invoicing"),
      "src/lib.rs": `pub mod cart;
pub mod money;
pub mod report;

pub use cart::cart_total;
pub use money::compute_total;
`,
      "src/money.rs": `pub fn compute_total(cents: &[i64]) -> i64 {
    cents.iter().sum()
}
`,
      "src/cart.rs": `use crate::money::compute_total;

pub fn cart_total(items: &[i64]) -> i64 {
    compute_total(items)
}
`,
      "src/report.rs": `use crate::money::compute_total;

pub fn report_total(items: &[i64]) -> String {
    format!("total={}", compute_total(items))
}
`,
      "tests/cart.rs": `use invoicing::{cart_total, compute_total};

#[test]
fn totals_add_up() {
    assert_eq!(compute_total(&[10, 20]), 30);
    assert_eq!(cart_total(&[1, 2, 3]), 6);
}
`,
    },
    hidden: {
      "tests/hidden_eval.rs": `#[test]
fn new_public_name_is_used_everywhere() {
    assert_eq!(invoicing::total_cents(&[4, 5]), 9);
    assert_eq!(invoicing::cart_total(&[4, 5]), 9);
    assert_eq!(invoicing::report::report_total(&[4, 5]), "total=9");
    for source in [
        include_str!("../src/lib.rs"),
        include_str!("../src/money.rs"),
        include_str!("../src/cart.rs"),
        include_str!("../src/report.rs"),
        include_str!("../tests/cart.rs"),
    ] {
        assert!(!source.contains("compute_total"), "old name remains: {source}");
    }
}
`,
    },
  },
  {
    id: "js_refactor_extract",
    category: "cross_file_refactor",
    language: "js",
    prompt:
      "Extract the duplicated formatPrice helper into a new module src/format.js and import it from src/cart.js and src/invoice.js. Keep output byte-identical and leave no duplicate definitions.",
    testCmd: "node --test",
    files: {
      "package.json": `{
  "name": "pricekit",
  "version": "0.1.0",
  "type": "module",
  "scripts": { "test": "node --test" }
}
`,
      "src/cart.js": `function formatPrice(cents) {
  return "$" + (cents / 100).toFixed(2);
}

export function cartLine(name, cents) {
  return name + ": " + formatPrice(cents);
}
`,
      "src/invoice.js": `function formatPrice(cents) {
  return "$" + (cents / 100).toFixed(2);
}

export function invoiceLine(name, cents) {
  return name.toUpperCase() + " " + formatPrice(cents);
}
`,
      "test/price.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { cartLine } from "../src/cart.js";
import { invoiceLine } from "../src/invoice.js";

test("existing output", () => {
  assert.equal(cartLine("book", 1234), "book: $12.34");
  assert.equal(invoiceLine("book", 1234), "BOOK $12.34");
});
`,
    },
    hidden: {
      "test/hidden_eval.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { formatPrice } from "../src/format.js";
import { cartLine } from "../src/cart.js";
import { invoiceLine } from "../src/invoice.js";

test("one shared formatter", () => {
  assert.equal(formatPrice(1234), "$12.34");
  assert.equal(formatPrice(5), "$0.05");
  assert.equal(cartLine("book", 500), "book: $5.00");
  assert.equal(invoiceLine("book", 500), "BOOK $5.00");
});

test("no duplicate formatter definition remains", () => {
  for (const path of ["src/cart.js", "src/invoice.js"]) {
    const source = readFileSync(new URL("../" + path, import.meta.url), "utf8");
    assert.ok(!source.includes("function formatPrice"), path + " still defines formatPrice");
  }
});
`,
    },
  },
  {
    id: "rust_migration_config",
    category: "migration",
    language: "rust",
    prompt:
      'Migrate the config schema to nested {"server":{"host","port"}} while continuing to accept the legacy flat {"host","port"} shape. Keep the public Config::from_json, host() and port() API and reject missing fields.',
    testCmd: "cargo test --offline --quiet",
    files: {
      "Cargo.toml": `[package]
name = "settings"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
`,
      "src/lib.rs": `use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub host: String,
    pub port: u16,
}

impl Config {
    pub fn from_json(raw: &str) -> Result<Self, String> {
        serde_json::from_str(raw).map_err(|e| e.to_string())
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}
`,
      "tests/config.rs": `use settings::Config;

#[test]
fn legacy_flat_shape_still_parses() {
    let config = Config::from_json(r#"{"host":"localhost","port":8080}"#).unwrap();
    assert_eq!(config.host(), "localhost");
    assert_eq!(config.port(), 8080);
}
`,
    },
    hidden: {
      "tests/hidden_eval.rs": `use settings::Config;

#[test]
fn nested_shape_parses() {
    let config = Config::from_json(r#"{"server":{"host":"0.0.0.0","port":443}}"#).unwrap();
    assert_eq!(config.host(), "0.0.0.0");
    assert_eq!(config.port(), 443);
}

#[test]
fn legacy_shape_still_parses() {
    let config = Config::from_json(r#"{"host":"legacy","port":9000}"#).unwrap();
    assert_eq!(config.host(), "legacy");
    assert_eq!(config.port(), 9000);
}

#[test]
fn missing_fields_are_rejected() {
    assert!(Config::from_json(r#"{"server":{"host":"x"}}"#).is_err());
    assert!(Config::from_json(r#"{"port":1}"#).is_err());
}
`,
    },
  },
  {
    id: "js_migration_promise",
    category: "migration",
    language: "js",
    prompt:
      "Migrate loadSettings in src/settings.js from callback style to a Promise-returning API with a single path argument. Update every caller (src/app.js, src/cli.js, test/settings.test.js). Keep the parsed settings shape and error behavior.",
    testCmd: "node --test",
    files: {
      "package.json": `{
  "name": "settingskit",
  "version": "0.1.0",
  "type": "module",
  "scripts": { "test": "node --test" }
}
`,
      "src/settings.js": `import { readFile } from "node:fs";

export function loadSettings(path, callback) {
  readFile(path, "utf8", (error, raw) => {
    if (error) return callback(error);
    try {
      callback(null, JSON.parse(raw));
    } catch (parseError) {
      callback(parseError);
    }
  });
}
`,
      "src/app.js": `import { loadSettings } from "./settings.js";

export function describeSettings(path, callback) {
  loadSettings(path, (error, settings) => {
    if (error) return callback(error);
    callback(null, settings.name + " v" + settings.version);
  });
}
`,
      "src/cli.js": `import { loadSettings } from "./settings.js";

export function printSettings(path, out) {
  loadSettings(path, (error, settings) => {
    if (error) throw error;
    out(settings.name + " v" + settings.version);
  });
}
`,
      "test/settings.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadSettings } from "../src/settings.js";
import { describeSettings } from "../src/app.js";

function fixture() {
  const dir = mkdtempSync(join(tmpdir(), "settings-"));
  const path = join(dir, "settings.json");
  writeFileSync(path, JSON.stringify({ name: "demo", version: "1.2.3" }));
  return path;
}

test("loads settings through the callback", () => {
  const path = fixture();
  loadSettings(path, (error, settings) => {
    assert.equal(error, null);
    assert.equal(settings.name, "demo");
  });
});

test("describes settings through the callback", () => {
  const path = fixture();
  describeSettings(path, (error, description) => {
    assert.equal(error, null);
    assert.equal(description, "demo v1.2.3");
  });
});
`,
    },
    hidden: {
      "test/hidden_eval.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadSettings } from "../src/settings.js";
import { describeSettings } from "../src/app.js";

function fixture() {
  const dir = mkdtempSync(join(tmpdir(), "settings-hidden-"));
  const path = join(dir, "settings.json");
  writeFileSync(path, JSON.stringify({ name: "demo", version: "9.9.9" }));
  return path;
}

test("loadSettings returns a Promise from one argument", async () => {
  const promise = loadSettings(fixture());
  assert.equal(typeof promise.then, "function");
  const settings = await promise;
  assert.equal(settings.name, "demo");
  assert.equal(settings.version, "9.9.9");
});

test("callers use the promise API", async () => {
  assert.equal(await describeSettings(fixture()), "demo v9.9.9");
  await assert.rejects(loadSettings("/definitely/missing/settings.json"));
});
`,
    },
  },
  {
    id: "rust_security_traversal",
    category: "security_fix",
    language: "rust",
    prompt:
      "Harden resolve_asset in src/lib.rs against path traversal and symlink escape. Requests must resolve to an existing file inside the canonical workspace root; absolute paths, .. escapes and symlinks pointing outside must be rejected. Legitimate nested paths must keep working.",
    testCmd: "cargo test --offline --quiet",
    files: {
      "Cargo.toml": rustManifest("assetd"),
      "src/lib.rs": `use std::path::{Path, PathBuf};

pub fn resolve_asset(root: &Path, request: &str) -> Result<PathBuf, String> {
    let candidate = root.join(request);
    candidate
        .canonicalize()
        .map_err(|error| format!("asset not found: {error}"))
}
`,
      "tests/assets.rs": `use std::fs;
use std::path::Path;

use assetd::resolve_asset;

#[test]
fn nested_file_resolves() {
    let dir = tempfile_dir();
    let root = dir.join("root");
    fs::create_dir_all(root.join("css")).unwrap();
    fs::write(root.join("css/app.css"), "body{}").unwrap();
    let resolved = resolve_asset(&root, "css/app.css").unwrap();
    assert!(resolved.ends_with("css/app.css"));
}

fn tempfile_dir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "assetd-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&base).unwrap();
    let _ = Path::new(&base);
    base
}
`,
    },
    hidden: {
      "tests/hidden_eval.rs": `use std::fs;

use assetd::resolve_asset;

fn sandbox() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "assetd-hidden-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(base.join("root/public")).unwrap();
    fs::write(base.join("root/public/ok.txt"), "ok").unwrap();
    fs::write(base.join("secret.txt"), "secret").unwrap();
    base
}

#[test]
fn traversal_and_absolute_requests_are_refused() {
    let base = sandbox();
    let root = base.join("root");
    assert!(resolve_asset(&root, "../secret.txt").is_err());
    assert!(resolve_asset(&root, "public/../../secret.txt").is_err());
    assert!(resolve_asset(&root, "/etc/passwd").is_err());
}

#[cfg(unix)]
#[test]
fn symlink_escape_is_refused() {
    let base = sandbox();
    let root = base.join("root");
    std::os::unix::fs::symlink(base.join("secret.txt"), root.join("link.txt")).unwrap();
    assert!(resolve_asset(&root, "link.txt").is_err());
    assert_eq!(fs::read_to_string(resolve_asset(&root, "public/ok.txt").unwrap()).unwrap(), "ok");
}
`,
    },
  },
  {
    id: "js_security_exec",
    category: "security_fix",
    language: "js",
    prompt:
      "Fix the command injection in runReport (src/report.js). Only the formats json, csv and html may run, and the format must never be interpreted by a shell. Unknown formats must be rejected. Existing valid behavior stays.",
    testCmd: "node --test",
    files: {
      "package.json": `{
  "name": "reportkit",
  "version": "0.1.0",
  "type": "module",
  "scripts": { "test": "node --test" }
}
`,
      "scripts/render.js": `const format = process.argv[2];
const supported = new Set(["json", "csv", "html"]);
if (!supported.has(format)) {
  console.error("unsupported format");
  process.exit(2);
}
process.stdout.write("rendered:" + format + "\\n");
`,
      "src/report.js": `import { execSync } from "node:child_process";

export function runReport(format) {
  return execSync("node scripts/render.js " + format, { encoding: "utf8" });
}
`,
      "test/report.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { runReport } from "../src/report.js";

test("renders a known format", () => {
  assert.match(runReport("json"), /rendered:json/);
});
`,
    },
    hidden: {
      "test/hidden_eval.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, rmSync } from "node:fs";
import { runReport } from "../src/report.js";

test("valid formats still render", () => {
  assert.match(runReport("csv"), /rendered:csv/);
  assert.match(runReport("html"), /rendered:html/);
});

test("shell metacharacters never execute", () => {
  const marker = "/tmp/faktor-intel-injection-" + process.pid;
  rmSync(marker, { force: true });
  assert.throws(() => runReport("json; touch " + marker));
  assert.equal(existsSync(marker), false);
  rmSync(marker, { force: true });
});
`,
    },
  },
  {
    id: "rust_bug_bisect",
    category: "bug_fix",
    language: "rust",
    prompt:
      "Fix lower_bound in src/lib.rs: it must return the first index whose value is >= target, or values.len() when every value is smaller. Do not change the signature and add no dependencies.",
    testCmd: "cargo test --offline --quiet",
    files: {
      "Cargo.toml": rustManifest("bisectkit"),
      "src/lib.rs": `pub fn lower_bound(values: &[i32], target: i32) -> usize {
    let mut low = 0usize;
    let mut high = values.len();
    while low < high {
        let mid = (low + high) / 2;
        if values[mid] < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if low == values.len() && !values.is_empty() {
        low - 1
    } else {
        low
    }
}
`,
      "tests/bounds.rs": `use bisectkit::lower_bound;

#[test]
fn inside_the_range() {
    assert_eq!(lower_bound(&[1, 3, 5], 3), 1);
    assert_eq!(lower_bound(&[1, 3, 5], 4), 2);
}
`,
    },
    hidden: {
      "tests/hidden_eval.rs": `use bisectkit::lower_bound;

#[test]
fn boundaries() {
    assert_eq!(lower_bound(&[], 7), 0);
    assert_eq!(lower_bound(&[1, 2, 3], 0), 0);
    assert_eq!(lower_bound(&[1, 2, 2, 2, 9], 2), 1);
    assert_eq!(lower_bound(&[1, 2, 3], 3), 2);
    assert_eq!(lower_bound(&[1, 2, 3], 4), 3);
    assert_eq!(lower_bound(&[1, 2, 3], 100), 3);
}
`,
    },
  },
  {
    id: "js_refactor_callsites",
    category: "cross_file_refactor",
    language: "js",
    prompt:
      "Rename formatDate to formatTimestamp in src/format.js and update every call site, including the CLI in bin/cli.js and the tests. No old name may remain; output must stay identical.",
    testCmd: "node --test",
    files: {
      "package.json": `{
  "name": "auditkit",
  "version": "0.1.0",
  "type": "module",
  "scripts": { "test": "node --test" }
}
`,
      "src/format.js": `export function formatDate(value) {
  return new Date(value).toISOString();
}
`,
      "src/report.js": `import { formatDate } from "./format.js";

export function reportLine(at) {
  return "report " + formatDate(at);
}
`,
      "src/audit.js": `import { formatDate } from "./format.js";

export function auditLine(at) {
  return "audit " + formatDate(at);
}
`,
      "bin/cli.js": `import { formatDate } from "../src/format.js";

const at = process.argv[2] || "2020-01-02T03:04:05Z";
process.stdout.write(formatDate(at) + "\\n");
`,
      "test/report.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { formatDate } from "../src/format.js";
import { reportLine } from "../src/report.js";
import { auditLine } from "../src/audit.js";

test("formats and reports", () => {
  assert.equal(formatDate("2020-01-02T03:04:05Z"), "2020-01-02T03:04:05.000Z");
  assert.equal(reportLine("2020-01-02T03:04:05Z"), "report 2020-01-02T03:04:05.000Z");
  assert.equal(auditLine("2020-01-02T03:04:05Z"), "audit 2020-01-02T03:04:05.000Z");
});
`,
    },
    hidden: {
      "test/hidden_eval.test.js": `import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { formatTimestamp } from "../src/format.js";
import { reportLine } from "../src/report.js";
import { auditLine } from "../src/audit.js";

test("new name is exported and used", () => {
  assert.equal(formatTimestamp("2020-01-02T03:04:05Z"), "2020-01-02T03:04:05.000Z");
  assert.equal(reportLine("2020-01-02T03:04:05Z"), "report 2020-01-02T03:04:05.000Z");
  assert.equal(auditLine("2020-01-02T03:04:05Z"), "audit 2020-01-02T03:04:05.000Z");
});

test("the CLI uses the new name", () => {
  const out = execFileSync(process.execPath, ["bin/cli.js", "2020-01-02T03:04:05Z"], {
    encoding: "utf8",
  });
  assert.equal(out.trim(), "2020-01-02T03:04:05.000Z");
});

test("old name is gone from every source", () => {
  for (const path of [
    "src/format.js",
    "src/report.js",
    "src/audit.js",
    "bin/cli.js",
    "test/report.test.js",
  ]) {
    const source = readFileSync(new URL("../" + path, import.meta.url), "utf8");
    assert.ok(!source.includes("formatDate"), path + " still mentions formatDate");
  }
});
`,
    },
  },
];

export function taskById(id) {
  const task = tasks.find((task) => task.id === id);
  if (!task) throw new Error("unknown task " + id);
  return task;
}
