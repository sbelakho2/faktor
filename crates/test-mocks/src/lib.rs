// TEST-SEPARATION: oracle=property isolation=pure-unit
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Command;

const MCP_SOURCE: &str = concat!(
    include_str!("mocks/minijson.rs"),
    "\n",
    include_str!("mocks/mcp_mock.rs")
);
const LSP_SOURCE: &str = concat!(
    include_str!("mocks/minijson.rs"),
    "\n",
    include_str!("mocks/lsp_mock.rs")
);

fn hash(source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

fn ensure(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("faktor-test-mocks");
    std::fs::create_dir_all(&dir).expect("mock build dir");
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let output = dir.join(format!("{name}-{:016x}{suffix}", hash(source)));
    if output.is_file() {
        return output;
    }
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let source_path = dir.join(format!("{name}-{:016x}-{unique}.rs", hash(source)));
    std::fs::write(&source_path, source).expect("mock source write");
    let temp = dir.join(format!("{name}-{:016x}-{unique}.tmp{suffix}", hash(source)));
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let compiled = Command::new(rustc)
        .arg("--edition")
        .arg("2021")
        .arg("-C")
        .arg("opt-level=1")
        .arg("-o")
        .arg(&temp)
        .arg(&source_path)
        .output()
        .expect("run rustc for test mock");
    if !compiled.status.success() {
        panic!(
            "rustc failed for test mock {name}: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
    }
    match std::fs::rename(&temp, &output) {
        Ok(()) => output,
        Err(_) if output.is_file() => {
            let _ = std::fs::remove_file(&temp);
            output
        }
        Err(error) => panic!("publish test mock {name}: {error}"),
    }
}

pub fn mcp_bin() -> PathBuf {
    ensure("mcp-mock", MCP_SOURCE)
}

pub fn lsp_bin() -> PathBuf {
    ensure("lsp-mock", LSP_SOURCE)
}

#[cfg(test)]
mod tests {
    use super::hash;

    #[test]
    fn source_hash_is_stable_and_distinguishes_mock_sources() {
        assert_eq!(hash("mock source a"), hash("mock source a"));
        assert_ne!(hash("mock source a"), hash("mock source b"));
    }
}
