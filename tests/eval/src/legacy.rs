// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! HAR-14 ~1,000,000-line legacy repository generator (deterministic,
//! seeded; generated under `target/` or a temp dir — never committed).
//!
//! Shape: a flat "legacy" monolith of `modules/mod_NNNN/src/mod_NNNN.<ext>`
//! files (ext cycling Rust/Python/TypeScript/Go) plus two hub docs. Every
//! 40th module cites [`LEGACY_HUB_TOKEN`] and the hub anchor symbol, so
//! retrieval/evidence queries over a million-line corpus still have ground
//! truth. The total line count is EXACT: the caller's `target_lines` budget
//! is distributed over full modules plus one truncated final module.

use std::fs;
use std::io;
use std::path::Path;

use crate::polyglot::{CorpusManifest, CrossrefManifest, FileRecord, SymbolGround};
use crate::prng::SplitMix64;

/// Ground-truth hub token for the legacy corpus.
pub const LEGACY_HUB_TOKEN: &str = "faktor_eval_legacy_hub";
/// Default scale: one million lines.
pub const LEGACY_TARGET_LINES: u64 = 1_000_000;
/// Lines per full module file.
pub const LEGACY_LINES_PER_MODULE: u64 = 200;
/// Every Nth module file cites the hub (call-site ground truth at scale).
pub const LEGACY_HUB_STRIDE: u64 = 40;

const EXTS: [&str; 4] = ["rs", "py", "ts", "go"];

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

/// One seeded filler function in the module's language, exactly `lines`
/// tall (the last function of a truncated module may be cut short — a
/// realistic legacy file ending mid-block is fine for a lexical corpus).
fn filler_fn(language: &str, module: usize, n: usize, salt: u64, lines: usize) -> Vec<String> {
    let name = format!("seeded_mod{module:04}_fn{n:03}");
    let block = match language {
        "rs" => vec![
            format!("/// Seeded legacy filler ({salt:08x})."),
            format!("pub fn {name}(x: u64) -> u64 {{"),
            format!("    let a = x ^ 0x{:08x};", salt),
            format!("    let b = a.wrapping_mul({});", (salt % 97) + 2),
            "    let c = b ^ (x << 3);".to_string(),
            "    c.wrapping_add(1)".to_string(),
            "}".to_string(),
            String::new(),
        ],
        "py" => vec![
            format!("# Seeded legacy filler ({salt:08x})."),
            format!("def {name}(x):"),
            format!("    a = x ^ 0x{:08x}", salt),
            format!("    b = a * {}", (salt % 97) + 2),
            "    c = b ^ (x << 3)".to_string(),
            "    return c ^ 0xFFFFFFFFFFFFFFFF".to_string(),
            String::new(),
        ],
        "ts" => vec![
            format!("// Seeded legacy filler ({salt:08x})."),
            format!("export function {name}(x: number): number {{"),
            format!("  const a = x ^ 0x{:08x};", salt),
            format!("  const b = a * {};", (salt % 97) + 2),
            "  const c = b ^ (x << 3);".to_string(),
            "  return c >>> 0;".to_string(),
            "}".to_string(),
            String::new(),
        ],
        _ => vec![
            format!("// Seeded legacy filler ({salt:08x})."),
            format!("func {name}(x uint64) uint64 {{"),
            format!("\ta := x ^ 0x{:08x}", salt),
            format!("\tb := a * {}", (salt % 97) + 2),
            "\tc := b ^ (x << 3)".to_string(),
            "\treturn c".to_string(),
            "}".to_string(),
            String::new(),
        ],
    };
    block.into_iter().take(lines).collect()
}

/// Generate the legacy corpus under `root` (the generator OWNS `root`) with
/// EXACTLY `target_lines` total lines across all files. Returns the corpus
/// manifest, also written to `<root>/manifest.json`.
pub fn generate_legacy(root: &Path, seed: u64, target_lines: u64) -> io::Result<CorpusManifest> {
    assert!(target_lines >= LEGACY_LINES_PER_MODULE, "target too small");
    if root.exists() {
        fs::remove_dir_all(root)?;
    }
    fs::create_dir_all(root)?;
    let mut rng = SplitMix64::new(seed);
    let mut w = Writer::new(root);

    // Hub docs count toward the exact line budget.
    let hub_doc = format!(
        "# Legacy hub (generated corpus, seed {seed})\n\n\
         Anchor symbol: `legacy_hub_anchor` (modules/mod0000).\n\
         Crossref token: {LEGACY_HUB_TOKEN} — cited by every {LEGACY_HUB_STRIDE}th\n\
         module; retrieval/evidence ground truth at 1M-line scale.\n"
    );
    w.write("HUB.md", &hub_doc)?;
    let readme = format!(
        "# Faktor eval legacy 1M fixture (generated — do not edit)\n\n\
         Deterministic seeded corpus (seed {seed}, {target_lines} lines exactly)\n\
         for HAR-14 bounded indexing, entry-bound snapshot/watcher behavior and\n\
         scale retrieval queries.\n"
    );
    w.write("README.md", &readme)?;
    let docs_lines: u64 = w.files.iter().map(|f| f.lines).sum();
    let module_budget = target_lines - docs_lines;

    let mut modules_written: u64 = 0;
    let mut m: u64 = 0;
    while modules_written < module_budget {
        let want = (module_budget - modules_written).min(LEGACY_LINES_PER_MODULE) as usize;
        let language = EXTS[(m as usize) % EXTS.len()];
        let mut lines: Vec<String> = Vec::with_capacity(want);
        let cites_hub = m.is_multiple_of(LEGACY_HUB_STRIDE);
        let header = if cites_hub {
            vec![
                format!("// Legacy module mod{m:04} (generated corpus, seed {seed})."),
                format!("// Language: {language}."),
                format!("// Crossref: {LEGACY_HUB_TOKEN} -> modules/mod0000 `legacy_hub_anchor`."),
                String::new(),
            ]
        } else {
            vec![
                format!("// Legacy module mod{m:04} (generated corpus, seed {seed})."),
                format!("// Language: {language}."),
                String::new(),
            ]
        };
        for line in header {
            if lines.len() < want {
                lines.push(line);
            }
        }
        // The anchor symbol lives in mod0000 (manifest ground truth).
        if m == 0 {
            let anchor = vec![
                "/// Legacy hub anchor: referenced by every hub-citing module.".to_string(),
                "pub fn legacy_hub_anchor(x: u64) -> u64 {".to_string(),
                "    x.wrapping_mul(0x9e3779b97f4a7c15)".to_string(),
                "}".to_string(),
                String::new(),
            ];
            for line in anchor {
                if lines.len() < want {
                    lines.push(line);
                }
            }
        }
        let mut n = 0usize;
        while lines.len() < want {
            let salt = rng.next_u64() & 0xffff_ffff;
            let chunk = filler_fn(language, m as usize, n, salt, want - lines.len());
            lines.extend(chunk);
            n += 1;
        }
        let content = lines.join("\n") + "\n";
        modules_written += lines.len() as u64;
        let rel = format!("modules/mod{m:04}/src/mod{m:04}.{language}");
        w.write(&rel, &content)?;
        m += 1;
    }

    let mut files = w.files;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let total_lines: u64 = files.iter().map(|f| f.lines).sum();
    debug_assert_eq!(total_lines, target_lines);
    let token_files: Vec<String> = files
        .iter()
        .filter(|f| {
            fs::read_to_string(root.join(&f.path))
                .map(|c| c.contains(LEGACY_HUB_TOKEN))
                .unwrap_or(false)
        })
        .map(|f| f.path.clone())
        .collect();
    let manifest = CorpusManifest {
        schema: "faktor-eval-corpus/v1".to_string(),
        kind: "legacy-1m".to_string(),
        seed,
        total_lines,
        files,
        crossref: CrossrefManifest {
            token: LEGACY_HUB_TOKEN.to_string(),
            // The legacy corpus carries no attachment marker; a sentinel
            // keeps the manifest shape uniform across kinds.
            attachment_marker: String::new(),
            fnv_felt_faktor: String::new(),
            token_files,
            symbols: vec![SymbolGround {
                name: "legacy_hub_anchor".to_string(),
                path: "modules/mod0000/src/mod0000.rs".to_string(),
                language: "rust".to_string(),
            }],
        },
    };
    fs::write(
        root.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).expect("manifest serializes"),
    )?;
    Ok(manifest)
}
