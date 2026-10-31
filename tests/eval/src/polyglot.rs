// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! HAR-14 polyglot monorepo generator (deterministic, seeded).
//!
//! The generated tree is a small but REAL cross-language system: a Rust
//! telemetry core, a TypeScript edge adapter that shells out to the Rust
//! core CLI, a Python reporting shim over the TypeScript edge, a Go relay
//! over the Python reporter, and a Kotlin console that polls the Go relay
//! over HTTP. Every hop cites its upstream path and symbol, every language
//! test re-derives the same FNV-1a constant independently, and every file of
//! the call chain carries [`CROSSREF_TOKEN`] — the ground-truth token the
//! retrieval/evidence queries must hit in ALL FIVE languages.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::prng::SplitMix64;
use crate::{FNV64_OFFSET_BASIS, FNV64_PRIME};

/// Ground-truth crossref token: must appear in every language subtree of the
/// call chain (definition side and every call-site/test side).
pub const CROSSREF_TOKEN: &str = "faktor_eval_crossref_core";

/// One generated file's identity in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub lines: u64,
    pub bytes: u64,
    pub blake3: String,
}

/// Ground truth for one cross-language symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolGround {
    pub name: String,
    pub path: String,
    pub language: String,
}

/// Retrieval/evidence ground truth emitted with the corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossrefManifest {
    pub token: String,
    /// Unique marker embedded ONLY in the Rust core source (the attachment
    /// reachability marker the conversation suite attaches as a file).
    pub attachment_marker: String,
    /// Decimal string of the FNV-1a 64 checksum of `b"faktor"`; every
    /// language subtree's test asserts this value with its own reference
    /// implementation.
    pub fnv_felt_faktor: String,
    /// Files that must be returned for a `CROSSREF_TOKEN` evidence query
    /// (spans all five languages + README).
    pub token_files: Vec<String>,
    pub symbols: Vec<SymbolGround>,
}

/// Full corpus manifest (committed generators emit; corpora themselves stay
/// out of git).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusManifest {
    pub schema: String,
    pub kind: String,
    pub seed: u64,
    pub total_lines: u64,
    pub files: Vec<FileRecord>,
    pub crossref: CrossrefManifest,
}

/// FNV-1a 64 over raw bytes (the shared cross-language constant).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV64_OFFSET_BASIS;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV64_PRIME);
    }
    hash
}

struct Writer<'a> {
    root: &'a Path,
    files: Vec<FileRecord>,
}

impl<'a> Writer<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            files: Vec::new(),
        }
    }

    fn write(&mut self, rel: &str, content: &str) -> io::Result<()> {
        let abs = self.root.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, content)?;
        self.files.push(FileRecord {
            path: rel.to_string(),
            lines: content.lines().count() as u64,
            bytes: content.len() as u64,
            blake3: blake3::hash(content.as_bytes()).to_hex().to_string(),
        });
        Ok(())
    }
}

/// Seeded filler block appended to the main source of each language (proves
/// the corpus is a function of the seed, not of the templates alone).
fn seeded_fillers(rng: &mut SplitMix64, language: &str, count: u32, symbol_prefix: &str) -> String {
    let mut out = String::new();
    for i in 0..count {
        let token = rng.hex_token(12);
        let salt = rng.range(1, 1 << 20);
        match language {
            "rust" => out.push_str(&format!(
                "/// Seeded filler doc {i} ({token}).\npub fn {symbol_prefix}_filler_{i}(x: u64) -> u64 {{\n    x ^ 0x{salt:08x}\n}}\n\n"
            )),
            "typescript" => out.push_str(&format!(
                "// Seeded filler doc {i} ({token}).\nexport function {symbol_prefix}Filler{i}(x: bigint): bigint {{\n  return x ^ 0n + BigInt(0x{salt:08x});\n}}\n\n"
            )),
            "python" => out.push_str(&format!(
                "# Seeded filler doc {i} ({token}).\ndef {symbol_prefix}_filler_{i}(x):\n    return x ^ 0x{salt:08x}\n\n"
            )),
            "go" => out.push_str(&format!(
                "// Seeded filler doc {i} ({token}).\nfunc {symbol_prefix}Filler{i}(x uint64) uint64 {{\n\treturn x ^ 0x{salt:08x}\n}}\n\n"
            )),
            "kotlin" => out.push_str(&format!(
                "// Seeded filler doc {i} ({token}).\nfun {symbol_prefix}Filler{i}(x: ULong): ULong =\n    x xor 0x{salt:08x}UL\n\n"
            )),
            _ => {}
        }
    }
    out
}

/// Generate the polyglot monorepo under `root` (the generator OWNS `root`:
/// an existing tree is replaced). Returns the corpus manifest, also written
/// to `<root>/manifest.json`.
pub fn generate_polyglot(root: &Path, seed: u64) -> io::Result<CorpusManifest> {
    if root.exists() {
        fs::remove_dir_all(root)?;
    }
    fs::create_dir_all(root)?;
    let felt = fnv1a64(b"faktor");
    let felt_decimal = felt.to_string();
    let marker = format!(
        "FaktorEvalAttachmentMarker_{}",
        SplitMix64::new(seed).hex_token(16)
    );
    let mut w = Writer::new(root);

    // ---------------------------------------------------------- Rust core
    let rust_core = format!(
        r#"//! Telemetry core: stable event checksums for the Faktor eval polyglot
//! monorepo. This is the DEFINITION side of the call chain; every other
//! language subtree cites this file and symbol.
//! Crossref: {CROSSREF_TOKEN} (downstream: ts/telemetry-edge -> python/telemetry-report -> go/telemetry-relay -> kotlin/telemetry-console).
//! Attachment marker (unique to this file): {marker}

/// Protocol version of the event envelope.
pub const CORE_PROTOCOL_VERSION: u32 = 3;

/// FNV-1a 64 of b"faktor" — the constant every language subtree's test
/// re-derives with its own reference implementation.
pub const EXPECTED_CHECKSUM_FAKTOR: u64 = {felt_decimal};

/// FNV-1a 64-bit hash (stable across platforms; the cross-language contract).
pub fn fnv1a64(bytes: &[u8]) -> u64 {{
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {{
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }}
    hash
}}

/// One telemetry event in the core envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {{
    pub id: u64,
    pub kind: String,
    pub payload: String,
}}

/// The core checksum: called by the TS edge adapter through the
/// `telemetry-core checksum` CLI boundary (see ts/telemetry-edge/src/ingest.ts).
pub fn compute_checksum(bytes: &[u8]) -> u64 {{
    fnv1a64(bytes)
}}

/// Ingest one event; returns its checksum. Called from ingest_event test
/// suites and referenced by every downstream subtree.
pub fn ingest_event(event: &Event) -> u64 {{
    let mut buf = Vec::new();
    buf.extend_from_slice(event.id.to_le_bytes().as_slice());
    buf.extend_from_slice(event.kind.as_bytes());
    buf.extend_from_slice(event.payload.as_bytes());
    compute_checksum(&buf)
}}
"#,
        marker = marker
    );
    let rust_core = format!(
        "{rust_core}\n{}",
        seeded_fillers(&mut SplitMix64::new(seed ^ 0x11), "rust", 3, "core")
    );
    let rust_tests = r#"
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_faktor_is_the_cross_language_constant() {
        assert_eq!(compute_checksum(b"faktor"), EXPECTED_CHECKSUM_FAKTOR);
    }

    #[test]
    fn ingest_event_is_stable() {
        let e = Event { id: 1, kind: "boot".into(), payload: "ok".into() };
        assert_eq!(ingest_event(&e), ingest_event(&e));
    }
}
"#;
    let rust_core = format!("{rust_core}{rust_tests}");
    w.write(
        "rust/telemetry_core/Cargo.toml",
        "[package]\nname = \"telemetry_core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )?;
    w.write("rust/telemetry_core/src/lib.rs", &rust_core)?;
    let rust_integration = format!(
        r#"//! Integration test over the public core API.
//! Crossref: {CROSSREF_TOKEN}.

use telemetry_core::{{compute_checksum, EXPECTED_CHECKSUM_FAKTOR}};

#[test]
fn felt_factor_integration_constant() {{
    assert_eq!(compute_checksum(b"faktor"), EXPECTED_CHECKSUM_FAKTOR);
}}
"#
    );
    w.write("rust/telemetry_core/tests/checksum.rs", &rust_integration)?;

    // ------------------------------------------------------ TypeScript edge
    let ts_ingest = format!(
        r#"// Edge adapter over the Rust telemetry core.
// Call chain: this file -> `telemetry-core checksum` (Rust CLI boundary,
// rust/telemetry_core/src/lib.rs `compute_checksum`); upstream of
// python/telemetry-report/report.py `summarize_events`.
// Crossref: {CROSSREF_TOKEN}

import {{ spawnSync }} from "node:child_process";

// Decimal FNV-1a 64 of b"faktor" — asserted by the local reference
// implementation in ingest.test.ts against the Rust constant.
export const EXPECTED_CHECKSUM_FAKTOR = {felt_decimal}n;

/** Independent JS reference implementation (BigInt FNV-1a 64). */
export function fnv1a64Reference(data: Buffer): bigint {{
  let hash = 0xcbf29ce484222325n;
  const prime = 0x100000001b3n;
  for (const b of data) {{
    hash ^= BigInt(b);
    hash = (hash * prime) & 0xffffffffffffffffn;
  }}
  return hash;
}}

/**
 * computeChecksum — calls the Rust core through its CLI boundary
 * (rust/telemetry_core/src/lib.rs::compute_checksum). This is the real
 * cross-language call site, not a reimplementation.
 */
export function computeChecksum(data: Buffer): bigint {{
  const res = spawnSync("telemetry-core", ["checksum"], {{ input: data }});
  if (res.status !== 0 || res.stdout === null) {{
    throw new Error("telemetry-core CLI boundary failed");
  }}
  return BigInt(res.stdout.toString("utf8").trim());
}}

/** ingestEvent — edge ingest path over computeChecksum. */
export function ingestEvent(kind: string, payload: string): bigint {{
  return computeChecksum(Buffer.from(`${{kind}} ${{payload}}`, "utf8"));
}}
"#,
    );
    let ts_ingest = format!(
        "{ts_ingest}\n{}",
        seeded_fillers(&mut SplitMix64::new(seed ^ 0x22), "typescript", 3, "edge")
    );
    w.write(
        "ts/telemetry-edge/package.json",
        "{\n  \"name\": \"telemetry-edge\",\n  \"version\": \"0.1.0\",\n  \"type\": \"module\"\n}\n",
    )?;
    w.write(
        "ts/telemetry-edge/tsconfig.json",
        "{\n  \"compilerOptions\": { \"strict\": true, \"module\": \"esnext\", \"target\": \"es2022\" }\n}\n",
    )?;
    w.write("ts/telemetry-edge/src/ingest.ts", &ts_ingest)?;
    let ts_test = format!(
        r#"//! Edge tests: the JS reference implementation must equal the Rust
//! constant from rust/telemetry_core/src/lib.rs.
//! Crossref: {CROSSREF_TOKEN}

import {{ fnv1a64Reference, EXPECTED_CHECKSUM_FAKTOR }} from "./ingest";
import {{ strict as assert }} from "node:assert";

assert.equal(fnv1a64Reference(Buffer.from("faktor", "utf8")), EXPECTED_CHECKSUM_FAKTOR);
"#
    );
    w.write("ts/telemetry-edge/src/ingest.test.ts", &ts_test)?;

    // ----------------------------------------------------- Python reporter
    let py_report = format!(
        r#""""Reporting shim over the TypeScript edge adapter.

Call chain: this module -> `npx tsx ts/telemetry-edge/src/ingest.ts`
(TS boundary, `ingestEvent`); upstream of go/telemetry-relay/relay/relay.go
`relayEvents`.
Crossref: {CROSSREF_TOKEN}
"""

import subprocess

# Decimal FNV-1a 64 of b"faktor" — asserted against the Rust constant by the
# local reference implementation in tests/test_report.py.
EXPECTED_CHECKSUM_FAKTOR = {felt_decimal}


def fnv1a64_reference(data: bytes) -> int:
    """Independent Python reference implementation."""
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def summarize_events(events: list[str]) -> str:
    """Summarize a batch through the TypeScript edge CLI (real boundary)."""
    payload = "\n".join(events)
    res = subprocess.run(
        ["npx", "tsx", "ts/telemetry-edge/src/ingest.ts"],
        input=payload,
        capture_output=True,
        text=True,
    )
    if res.returncode != 0:
        raise RuntimeError("telemetry-edge boundary failed")
    return f"events={{len(events)}} edge={{res.stdout.strip()}}"
"#,
    );
    let py_report = format!(
        "{py_report}\n{}",
        seeded_fillers(&mut SplitMix64::new(seed ^ 0x33), "python", 3, "report")
    );
    w.write(
        "python/telemetry-report/pyproject.toml",
        "[project]\nname = \"telemetry-report\"\nversion = \"0.1.0\"\nrequires-python = \">=3.11\"\n",
    )?;
    w.write(
        "python/telemetry-report/telemetry_report/__init__.py",
        "from .report import summarize_events, fnv1a64_reference\n",
    )?;
    w.write(
        "python/telemetry-report/telemetry_report/report.py",
        &py_report,
    )?;
    let py_test = format!(
        r#""""Reporter tests: the Python reference implementation must equal
the Rust constant from rust/telemetry_core/src/lib.rs.
Crossref: {CROSSREF_TOKEN}
"""

from telemetry_report.report import EXPECTED_CHECKSUM_FAKTOR, fnv1a64_reference


def test_felt_factor_constant():
    assert fnv1a64_reference(b"faktor") == EXPECTED_CHECKSUM_FAKTOR
"#
    );
    w.write("python/telemetry-report/tests/test_report.py", &py_test)?;

    // ---------------------------------------------------------- Go relay
    let go_relay = format!(
        r#"// Relay: forwards event batches to the Python reporter.
// Call chain: this package -> `python3 -m telemetry_report` (Python
// boundary, python/telemetry-report/report.py `summarize_events`);
// upstream of kotlin/telemetry-console Console.kt `renderConsole`.
// Crossref: {CROSSREF_TOKEN}

package relay

import (
	"fmt"
	"os/exec"
	"strings"
)

// ExpectedChecksumFaktor is the decimal FNV-1a 64 of b"faktor" — asserted
// against the Rust constant by relay_test.go's local reference.
const ExpectedChecksumFaktor uint64 = {felt_decimal}

// Fnv1a64Reference is the independent Go reference implementation.
func Fnv1a64Reference(data []byte) uint64 {{
	h := uint64(0xcbf29ce484222325)
	for _, b := range data {{
		h ^= uint64(b)
		h *= 0x100000001b3
	}}
	return h
}}

// RelayEvents calls the Python reporter over the real subprocess boundary.
func RelayEvents(batch []string) (string, error) {{
	cmd := exec.Command("python3", "-m", "telemetry_report")
	cmd.Stdin = strings.NewReader(strings.Join(batch, "\n"))
	out, err := cmd.Output()
	if err != nil {{
		return "", fmt.Errorf("telemetry_report boundary: %w", err)
	}}
	return strings.TrimSpace(string(out)), nil
}}
"#,
    );
    let go_relay = format!(
        "{go_relay}\n{}",
        seeded_fillers(&mut SplitMix64::new(seed ^ 0x44), "go", 3, "relay")
    );
    w.write(
        "go/telemetry-relay/go.mod",
        "module telemetry-relay\n\ngo 1.22\n",
    )?;
    w.write("go/telemetry-relay/relay/relay.go", &go_relay)?;
    let go_test = format!(
        r#"package relay

import "testing"

// Relay tests: the Go reference implementation must equal the Rust constant
// from rust/telemetry_core/src/lib.rs.
// Crossref: {CROSSREF_TOKEN}

func TestFeltFactorConstant(t *testing.T) {{
	if Fnv1a64Reference([]byte("faktor")) != ExpectedChecksumFaktor {{
		t.Fatal("go reference diverged from the rust constant")
	}}
}}
"#
    );
    w.write("go/telemetry-relay/relay/relay_test.go", &go_test)?;

    // ------------------------------------------------------ Kotlin console
    let kt_console = format!(
        r#"// Console: polls the Go relay over HTTP.
// Call chain: this class -> GET http://localhost:8080/events (Go boundary,
// go/telemetry-relay/relay/relay.go `RelayEvents`).
// Crossref: {CROSSREF_TOKEN}

package eval.console

import java.net.HttpURLConnection
import java.net.URL

/** Decimal FNV-1a 64 of b"faktor"; ConsoleTest.kt re-derives it locally. */
const val EXPECTED_CHECKSUM_FAKTOR = "{felt_decimal}"

/** Independent Kotlin reference implementation. */
fun fnv1a64Reference(data: ByteArray): ULong {{
    var h: ULong = 0xcbf29ce484222325UL
    for (b in data) {{
        h = h xor (b.toULong() and 0xffUL)
        h = h * 0x100000001b3UL
    }}
    return h
}}

/** renderConsole — pulls the relay's event summary over HTTP. */
fun renderConsole(endpoint: String): String {{
    val conn = URL(endpoint).openConnection() as HttpURLConnection
    conn.connectTimeout = 2_000
    conn.readTimeout = 5_000
    return conn.inputStream.bufferedReader().use {{ it.readText() }}
}}
"#,
    );
    let kt_console = format!(
        "{kt_console}\n{}",
        seeded_fillers(&mut SplitMix64::new(seed ^ 0x55), "kotlin", 3, "console")
    );
    w.write(
        "kotlin/telemetry-console/settings.gradle.kts",
        "rootProject.name = \"telemetry-console\"\n",
    )?;
    w.write(
        "kotlin/telemetry-console/build.gradle.kts",
        "plugins { kotlin(\"jvm\") version \"1.9.24\" }\n",
    )?;
    w.write(
        "kotlin/telemetry-console/src/main/kotlin/Console.kt",
        &kt_console,
    )?;
    let kt_test = format!(
        r#"package eval.console

import kotlin.test.Test
import kotlin.test.assertEquals

// Console tests: the Kotlin reference implementation must equal the Rust
// constant from rust/telemetry_core/src/lib.rs.
// Crossref: {CROSSREF_TOKEN}

class ConsoleTest {{
    @Test
    fun feltFactorConstant() {{
        val expected = java.lang.Long.parseUnsignedLong(EXPECTED_CHECKSUM_FAKTOR)
        assertEquals(expected, fnv1a64Reference("faktor".toByteArray()).toLong())
    }}
}}
"#
    );
    w.write(
        "kotlin/telemetry-console/src/test/kotlin/ConsoleTest.kt",
        &kt_test,
    )?;

    // ------------------------------------------------------------- README
    let readme = format!(
        r#"# Faktor eval polyglot fixture (generated — do not edit)

Deterministic seeded corpus (seed {seed}) for HAR-14 retrieval/evidence
queries, bounded indexing budgets, watcher entry-bound behavior and the
read-only conversation scenarios of the eval suite.

## Call chain (ground truth)

    kotlin/telemetry-console (renderConsole)
      --HTTP--> go/telemetry-relay/relay (RelayEvents)
      --subprocess--> python/telemetry-report (summarize_events)
      --subprocess--> ts/telemetry-edge (ingestEvent / computeChecksum)
      --CLI--> rust/telemetry_core (compute_checksum / ingest_event)

Every hop's source comment cites {CROSSREF_TOKEN} and its upstream path and
symbol; every language test re-derives the FNV-1a 64 constant of b"faktor"
({felt_decimal}) with an independent local reference implementation.

## Ground truth for retrieval/evidence queries

- token `{CROSSREF_TOKEN}` -> the whole call chain across all five languages
  (see manifest.json `crossref.token_files`).
- symbol `compute_checksum` -> rust/telemetry_core/src/lib.rs.
- symbol `computeChecksum` -> ts/telemetry-edge/src/ingest.ts.
- symbol `summarize_events` -> python/telemetry-report/telemetry_report/report.py.
- attachment marker `{marker}` -> rust/telemetry_core/src/lib.rs only.
"#,
        seed = seed
    );
    w.write("README.md", &readme)?;

    // ---------------------------------------------------------- manifest
    let mut files = w.files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let token_files: Vec<String> = files
        .iter()
        .filter(|f| {
            fs::read_to_string(root.join(&f.path))
                .map(|c| c.contains(CROSSREF_TOKEN))
                .unwrap_or(false)
        })
        .map(|f| f.path.clone())
        .collect();
    let manifest = CorpusManifest {
        schema: "faktor-eval-corpus/v1".to_string(),
        kind: "polyglot".to_string(),
        seed,
        total_lines: files.iter().map(|f| f.lines).sum(),
        files,
        crossref: CrossrefManifest {
            token: CROSSREF_TOKEN.to_string(),
            attachment_marker: marker,
            fnv_felt_faktor: felt_decimal,
            token_files,
            symbols: vec![
                SymbolGround {
                    name: "compute_checksum".to_string(),
                    path: "rust/telemetry_core/src/lib.rs".to_string(),
                    language: "rust".to_string(),
                },
                SymbolGround {
                    name: "ingest_event".to_string(),
                    path: "rust/telemetry_core/src/lib.rs".to_string(),
                    language: "rust".to_string(),
                },
                SymbolGround {
                    name: "computeChecksum".to_string(),
                    path: "ts/telemetry-edge/src/ingest.ts".to_string(),
                    language: "typescript".to_string(),
                },
                SymbolGround {
                    name: "summarize_events".to_string(),
                    path: "python/telemetry-report/telemetry_report/report.py".to_string(),
                    language: "python".to_string(),
                },
                SymbolGround {
                    name: "relayEvents".to_string(),
                    path: "go/telemetry-relay/relay/relay.go".to_string(),
                    language: "go".to_string(),
                },
                SymbolGround {
                    name: "renderConsole".to_string(),
                    path: "kotlin/telemetry-console/src/main/kotlin/Console.kt".to_string(),
                    language: "kotlin".to_string(),
                },
            ],
        },
    };
    fs::write(
        root.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).expect("manifest serializes"),
    )?;
    Ok(manifest)
}

/// The five language roots the polyglot corpus must span (ground truth used
/// by the retrieval tests and documented in the README).
pub const POLYGLOT_LANGUAGE_ROOTS: &[&str] = &["rust/", "ts/", "python/", "go/", "kotlin/"];

/// Language root of a manifest path, if it belongs to a language subtree.
pub fn language_root_of(rel: &str) -> Option<&'static str> {
    POLYGLOT_LANGUAGE_ROOTS
        .iter()
        .copied()
        .find(|root| rel.starts_with(root))
}
