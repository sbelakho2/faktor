//! Static source-authority certification (audit 31/107-109).
//!
//! Twelve structural invariants are locked by scanning the repository's
//! *production* Rust sources (`crates/*/src`, test modules and out-of-line
//! `#[cfg(test)] mod` bodies excluded):
//!
//! 1. **Child spawning** — `std::process::Command` /
//!    `tokio::process::Command` machinery exists ONLY in
//!    `crates/terminal` (the process supervisor) and `crates/pty` (the
//!    interactive-terminal platform launcher). Every other production crate
//!    must route children through the supervisor; the scans exit non-zero
//!    listing offenders with an empty default allowlist. An INDIRECT
//!    `ResolvedCommand`/`.lower*` -> process-`Command` conversion (lowering a
//!    command spec and then spawning it directly) is detected by a
//!    co-location scan even when no spawn marker names the type directly.
//! 2. **Outbound HTTP** — `reqwest::Client` construction and `.execute`
//!    calls exist ONLY inside the checked transport
//!    (`crates/provider/src/egress.rs`); every adapter send goes through
//!    the `HttpTransport` seam.
//! 3. **Durable atomic writes** — the forbidden patterns are checked
//!    INDEPENDENTLY (no fsync marker required): any `std::fs::rename` /
//!    `fs::rename` / `tokio::fs::rename` / `NamedTempFile::persist`, and any
//!    write-family call targeting a temp path, must either live in
//!    `crates/fs/src/atomic.rs` or match a small exact line allowlist.
//!    Pre-existing grandfathered sequences (the CAS store, `faktor-fs`'s
//!    internal stream copy, the git worktree metadata save) are allowlisted
//!    **line-by-line** by exact content, so a NEW sequence anywhere is
//!    still listed loudly. The index generation publish and the CLI startup
//!    backup finalize were the last inline rename writers; both now route
//!    through `crates/fs/src/atomic.rs` and have NO allowlist entry — a
//!    regression there is a red scan, never a review nit.
//! 4. **ONE semantic-provider registry authority** (audits 48-54/58/59/83) —
//!    production code constructs `SemanticProviderRegistry::new` ONLY in
//!    the agent crate's fallback constructor and the CLI graph builder;
//!    the native server introspection surface can never construct a
//!    parallel registry (it inspects `deps.semantic` only).
//! 5. **ONE durable evidence authority** — `DurableEvidenceAuthority::for_store`
//!    exists exactly ONCE in production (`crates/agent/src/runtime.rs`,
//!    the runtime whose allocation the daemon graph clones and the native
//!    server receives); no CLI/server site may construct a parallel
//!    authority over the same rows.
//! 6. **Constructor-site authority** — `DurableBudgetLedger::new` and
//!    `ProcessSupervisor::new` additionally have documented production
//!    sites with exact per-file occurrence counts: a new (or stale)
//!    construction site anywhere is a red test, never a review nit.
//! 7. **Retired product-name authority** — the retired product name and its
//!    version tokens appear ONLY in the historical attribution directory
//!    (`ui/LICENSES/`). The scan covers every regular file (source AND
//!    artifact: stale bundles, VSIX archives, jars count) outside the
//!    standard build/cache skip trees; the token literals are assembled at
//!    runtime so the scanner source cannot exempt itself.
//! 8. **No secret-shaped product settings** — the IDE apps' settings
//!    manifests (`apps/**/package.json` contributed properties,
//!    `apps/**/plugin.xml` name/key attributes, `*.schema.json` /
//!    `settings.json` schemas) must never declare a key shaped like a
//!    credential (`*Token`, `*Secret`, `*ApiKey`, `*Credential`, `*Password`,
//!    provider keys such as `openaiKey`). Credentials belong in the OS/IDE
//!    secret store (`vscode.SecretStorage` / JetBrains `PasswordSafe`). The
//!    allowlist is TIGHT — exact (manifest, key) pairs, each with a written
//!    justification, asserted load-bearing and non-stale — and a planted
//!    `faktor.someApiKey` setting fails the scan.
//! 9. **ONE egress address-classification authority** — `AddressClass`,
//!    `classify_ip`, `EgressAddressPolicy` and `vet_resolved_answers` are
//!    defined ONCE in `crates/security/src/network.rs` (generated from the
//!    pinned IANA special-purpose CSVs). Browser/provider production code
//!    inside `crates/browser/src` and `crates/provider/src` may `pub use`
//!    them but never re-define a classifier, class enum or answer-vetting
//!    function locally; a planted `fn classify_v4` in either crate is a
//!    red scan.
//! 10. **No plaintext secret-shaped struct fields** — a production struct
//!     field named `password`, `*_token`, `private_key*` or `client_secret`
//!     with type `String`/`Option<String>` is refused; the value must be a
//!     redacting wrapper (`faktor_security::secret::SecretValue` or a
//!     dedicated newtype). The ONLY exception is a WIRE DTO that mirrors an
//!     external payload shape, carries the documented
//!     `SECRET-FIELD-GATE-WIRE-DTO:` annotation and is listed, with a written
//!     justification, in `SECRET_WIRE_DTO_ALLOWLIST`; every such DTO must
//!     convert immediately and can never store the secret.
//! 11. **Budgeted response-body reads** — every production egress consumer
//!     (`crates/provider` streaming, the wire adapters, SCM, cloud OIDC,
//!     updater, semantic, commerce connectors) must read response bodies
//!     through the budget-aware helpers (`CheckedResponse` /
//!     `ResponseBudget` / `BudgetedBody`), never through a raw
//!     `reqwest::Response`. A direct `.json()` read, `.bytes()`/`.text()`/
//!     `.chunk(`/`bytes_stream()` read, a `.body_mut()` grab,
//!     `.copy_to(`/`.copy_to_bytes(`/`read_to_end(` stream copy, or the
//!     un-budgeted `resp.json().await` shape is a red test: unbounded reads
//!     are exactly the hang/RAM surface the response budget exists to
//!     close.
//! 12. **No resolve-then-std::fs workspace mutation** — a production
//!     `.resolve(…)` binding that is then passed to a `std::fs`/`fs`
//!     mutation call is refused: resolution is a point-in-time check, so a
//!     parent directory swapped for an outside symlink afterwards redirects
//!     the mutation outside the workspace. Deletions go through
//!     `WorkspaceHandle::remove_file` (anchored, no-follow), content writes
//!     through `write_atomic`/`crate::atomic`.
//!
//! Scanning methodology: per file, comments and string literals are masked
//! out and every `#[cfg(...)]`-gated item that can never compile in a
//! non-test build (`#[cfg(test)]`, `#[cfg(all(test, unix))]`, …) is
//! removed with brace-matched ranges, so markers in tests, docs or
//! examples can never certify production code. Out-of-line
//! `#[cfg(test)] mod` bodies (`tests.rs`, `*_tests.rs`) are skipped by the
//! production scans. The machinery is itself adversarially tested against
//! synthetic sources.

#[cfg(test)]
mod scans {
    use std::path::Path;

    // ------------------------------------------------------------------
    // source lexing / cfg(test) stripping
    // ------------------------------------------------------------------

    /// Byte mask: `true` = semantic code (comments and string/char
    /// literals masked out, newlines preserved as irrelevant).
    fn code_mask(src: &str) -> Vec<bool> {
        let b = src.as_bytes();
        let n = b.len();
        let mut code = vec![true; n];
        let mut mask = |from: usize, to: usize| {
            let to = to.min(n);
            code[from..to].fill(false);
        };
        let mut i = 0usize;
        while i < n {
            if b[i] == b'/' && i + 1 < n && b[i + 1] == b'/' {
                let mut j = i;
                while j < n && b[j] != b'\n' {
                    j += 1;
                }
                mask(i, j);
                i = j;
            } else if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
                let mut j = i + 2;
                while j < n && !(b[j] == b'*' && j + 1 < n && b[j + 1] == b'/') {
                    j += 1;
                }
                if j < n {
                    j += 2;
                }
                mask(i, j);
                i = j;
            } else if (b[i] == b'r' || (b[i] == b'b' && i + 1 < n && b[i + 1] == b'r')) && {
                let mut k = i + if b[i] == b'b' { 2 } else { 1 };
                while k < n && b[k] == b'#' {
                    k += 1;
                }
                k < n && b[k] == b'"'
            } {
                // raw string r"…", r#"…"#, br#"…"#: ends at '"' + same
                // number of '#'. An unterminated raw string masks to EOF
                // (the file is not valid Rust anyway; being conservative
                // never certifies production code).
                let prefix = if b[i] == b'b' { 2 } else { 1 };
                let mut k = i + prefix;
                while k < n && b[k] == b'#' {
                    k += 1;
                }
                let hashes = k - (i + prefix);
                let mut j = k + 1; // skip the opening quote
                while j < n {
                    if b[j] == b'"' {
                        let mut h = 0usize;
                        while j + 1 + h < n && h < hashes && b[j + 1 + h] == b'#' {
                            h += 1;
                        }
                        if h == hashes {
                            j += 1 + hashes;
                            break;
                        }
                    }
                    j += 1;
                }
                mask(i, j);
                i = j;
            } else if b[i] == b'"' {
                let mut j = i + 1;
                while j < n {
                    if b[j] == b'\\' && j + 1 < n {
                        j += 2;
                    } else if b[j] == b'"' {
                        j += 1;
                        break;
                    } else {
                        j += 1;
                    }
                }
                mask(i, j);
                i = j;
            } else if b[i] == b'\'' {
                // char literal (masked) vs lifetime (kept): only mask when
                // a closing quote exists nearby.
                let mut j = i + 1;
                let mut closed = false;
                while j < n && j <= i + 12 {
                    if b[j] == b'\\' && j + 1 < n {
                        j += 2;
                        continue;
                    }
                    if b[j] == b'\'' {
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                if closed {
                    mask(i, j + 1);
                    i = j + 1;
                } else {
                    i += 1;
                }
            } else {
                i += 1;
            }
        }
        code
    }

    /// Three-valued evaluation of a `cfg(…)` predicate.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Tv {
        T,
        F,
        U,
    }

    fn cfg_eval(body: &str, test: bool, unknown: bool) -> Option<Tv> {
        let bytes = body.as_bytes();
        let n = bytes.len();
        let mut pos = 0usize;
        fn ident_end(bytes: &[u8], mut p: usize) -> usize {
            while p < bytes.len() && (bytes[p].is_ascii_alphanumeric() || bytes[p] == b'_') {
                p += 1;
            }
            p
        }
        fn parse(bytes: &[u8], pos: &mut usize, test: bool, unknown: bool) -> Option<Tv> {
            if bytes[*pos..].starts_with(b"not(") {
                *pos += 4;
                let v = parse(bytes, pos, test, unknown)?;
                if *pos >= bytes.len() || bytes[*pos] != b')' {
                    return None;
                }
                *pos += 1;
                return Some(match v {
                    Tv::T => Tv::F,
                    Tv::F => Tv::T,
                    Tv::U => Tv::U,
                });
            }
            for op in [&b"all("[..], &b"any("[..]] {
                if bytes[*pos..].starts_with(op) {
                    *pos += 4;
                    let mut vals = Vec::new();
                    loop {
                        vals.push(parse(bytes, pos, test, unknown)?);
                        if *pos >= bytes.len() {
                            return None;
                        }
                        if bytes[*pos] == b',' {
                            *pos += 1;
                        } else if bytes[*pos] == b')' {
                            *pos += 1;
                            break;
                        } else {
                            return None;
                        }
                    }
                    let is_all = op == &b"all("[..];
                    let mut saw_t = false;
                    let mut saw_f = false;
                    let mut saw_u = false;
                    for v in &vals {
                        match v {
                            Tv::T => saw_t = true,
                            Tv::F => saw_f = true,
                            Tv::U => saw_u = true,
                        }
                    }
                    return Some(if is_all {
                        if saw_f {
                            Tv::F
                        } else if saw_u {
                            Tv::U
                        } else {
                            Tv::T
                        }
                    } else if saw_t {
                        Tv::T
                    } else if saw_u {
                        Tv::U
                    } else {
                        Tv::F
                    });
                }
            }
            let id_end = ident_end(bytes, *pos);
            if id_end == *pos {
                return None;
            }
            let ident = std::str::from_utf8(&bytes[*pos..id_end]).ok()?;
            *pos = id_end;
            if *pos < bytes.len() && bytes[*pos] == b'=' {
                *pos += 1;
                if *pos >= bytes.len() || bytes[*pos] != b'"' {
                    return None;
                }
                *pos += 1;
                while *pos < bytes.len() && bytes[*pos] != b'"' {
                    *pos += 1;
                }
                if *pos >= bytes.len() {
                    return None;
                }
                *pos += 1;
            }
            if ident == "test" {
                return Some(if test { Tv::T } else { Tv::F });
            }
            Some(if unknown { Tv::T } else { Tv::F })
        }
        let v = parse(bytes, &mut pos, test, unknown)?;
        (pos == n).then_some(v)
    }

    /// Does the predicate mention the `test` key at all?
    fn cfg_mentions_test(body: &str) -> bool {
        let b = body.as_bytes();
        let mut i = 0usize;
        while i < b.len() {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j > i {
                let ident = &b[i..j];
                if ident == b"test" {
                    return true;
                }
                i = j;
            } else {
                i += 1;
            }
        }
        false
    }

    /// A `cfg(…)` item is test-gated when it mentions `test` and can NEVER
    /// be present in a non-test build (evaluated with the unknown keys at
    /// both extremes, since `not()` can flip either way).
    fn is_test_gated(attr_body: &str) -> bool {
        if !cfg_mentions_test(attr_body) {
            return false;
        }
        let compact: String = attr_body.chars().filter(|c| !c.is_whitespace()).collect();
        cfg_eval(&compact, false, true) == Some(Tv::F)
            && cfg_eval(&compact, false, false) == Some(Tv::F)
    }

    /// Kept (production) byte ranges: everything outside test-gated items.
    fn kept_ranges(src: &str, code: &[bool]) -> Vec<(usize, usize)> {
        let b = src.as_bytes();
        let n = b.len();
        let mut drops: Vec<(usize, usize)> = Vec::new();
        let mut i = 0usize;
        while i + 6 <= n {
            if b[i..].starts_with(b"#[cfg(") && code[i..i + 6].iter().all(|c| *c) {
                // find the matching ")]"
                let mut end = None;
                let mut j = i + 6;
                while j + 2 <= n {
                    if b[j] == b')' && b[j + 1] == b']' && code[j] && code[j + 1] {
                        end = Some(j + 2);
                        break;
                    }
                    j += 1;
                }
                let Some(after_attr) = end else {
                    break;
                };
                let body = std::str::from_utf8(&b[i + 6..after_attr - 2]).unwrap_or("");
                if !is_test_gated(body) {
                    i = after_attr;
                    continue;
                }
                // skip any further attributes attached to the same item
                let mut k = after_attr;
                loop {
                    while k < n && !code[k] {
                        k += 1;
                    }
                    if k < n && b[k..].starts_with(b"#[") {
                        let mut depth = 0usize;
                        let mut m = k;
                        while m < n {
                            if code[m] {
                                if b[m] == b'[' {
                                    depth += 1;
                                } else if b[m] == b']' {
                                    depth -= 1;
                                    if depth == 0 {
                                        m += 1;
                                        break;
                                    }
                                }
                            }
                            m += 1;
                        }
                        k = m;
                    } else {
                        break;
                    }
                }
                // locate the item: '{' block or ';' statement at depth 0
                let mut depth = 0usize;
                let mut item_end = None;
                while k < n {
                    if !code[k] {
                        k += 1;
                        continue;
                    }
                    match b[k] {
                        b'(' | b'[' => depth += 1,
                        b')' | b']' => depth = depth.saturating_sub(1),
                        b'{' if depth == 0 => {
                            let mut d = 0usize;
                            let mut m = k;
                            while m < n {
                                if code[m] {
                                    match b[m] {
                                        b'{' => d += 1,
                                        b'}' => {
                                            d -= 1;
                                            if d == 0 {
                                                item_end = Some(m + 1);
                                                break;
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                m += 1;
                            }
                            break;
                        }
                        b';' if depth == 0 => {
                            item_end = Some(k + 1);
                            break;
                        }
                        _ => {}
                    }
                    k += 1;
                }
                match item_end {
                    Some(e) => {
                        drops.push((i, e));
                        i = e;
                    }
                    None => i = after_attr,
                }
            } else {
                i += 1;
            }
        }
        drops.sort_unstable();
        let mut kept = Vec::new();
        let mut pos = 0usize;
        for (a, z) in drops {
            if a > pos {
                kept.push((pos, a));
            }
            if z > pos {
                pos = z;
            }
        }
        if pos < n {
            kept.push((pos, n));
        }
        kept
    }

    struct File<'a> {
        rel: String,
        src: &'a str,
        code: Vec<bool>,
        kept: Vec<(usize, usize)>,
    }

    fn load(rel: &str) -> Option<File<'_>> {
        let root = repo_root();
        let path = root.join(rel);
        let src = std::fs::read_to_string(&path).ok()?;
        let code = code_mask(&src);
        let kept = kept_ranges(&src, &code);
        Some(File {
            rel: rel.to_string(),
            src: leak(&src),
            code,
            kept,
        })
    }

    /// Leak helper keeps `File` borrow-free; scans run once per test.
    fn leak(s: &str) -> &'static str {
        Box::leak(s.to_string().into_boxed_str())
    }

    fn repo_root() -> std::path::PathBuf {
        // Runtime-robust: a stale test binary from a deleted checkout
        // (shared target dir, matching fingerprints) embeds that checkout's
        // CARGO_MANIFEST_DIR. Try the build-time root, then cwd and exe
        // ancestors; use the FIRST that really contains crates/. Fail
        // loudly with every path tried — never a silent skip.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Some(root) = manifest.parent().and_then(|p| p.parent()) {
            candidates.push(root.to_path_buf());
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.extend(cwd.ancestors().map(|a| a.to_path_buf()));
        }
        if let Ok(exe) = std::env::current_exe() {
            candidates.extend(exe.ancestors().map(|a| a.to_path_buf()));
        }
        let root = candidates
            .iter()
            .find(|base| base.join("crates").is_dir())
            .unwrap_or_else(|| {
                panic!(
                    "repository root not found from {}; tried: {candidates:?}",
                    manifest.display()
                )
            });
        root.to_path_buf()
    }

    /// Repository-relative paths are compared against `/`-joined constants
    /// and prefixes. `Path::display()` uses `\` on Windows, which silently
    /// defeated the terminal/pty exclusion, the out-of-line-test exemption
    /// and every allowlist/site comparison there (2026-09 Windows-runner
    /// failure: the supervisor's own spawns and `egress.rs` itself were
    /// scanned). Every rel that enters a matcher or a comparison is
    /// normalized here; on unix this is exactly the identity.
    fn normalize_rel(rel: &str) -> String {
        rel.replace('\\', "/")
    }

    /// Every `crates/<crate>/src/**/*.rs` file (test dirs excluded).
    fn walk_crate_sources() -> Vec<String> {
        let root = repo_root().join("crates");
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let ft = match entry.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if ft.is_dir() {
                    if matches!(name.as_ref(), "tests" | "examples" | "benches" | "target")
                        || name.starts_with('.')
                    {
                        continue;
                    }
                    stack.push(path);
                } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let rel = path
                        .strip_prefix(repo_root())
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    out.push(normalize_rel(&rel));
                }
            }
        }
        out.sort();
        out
    }

    /// True when a production scan must treat `rel` as test-only code. The
    /// rule is deliberately STRONGER than the historical name check: a
    /// test-ish name alone must never exempt production code.
    ///
    /// 1. A file under a `/tests/` path component is test code by Cargo's
    ///    integration-test layout.
    /// 2. A file named `tests.rs` or ending in `_tests.rs` is test code only
    ///    when a sibling `*.rs` in the same directory actually INCLUDES it
    ///    as a test module: a test-gated `#[cfg(...)]` attribute attached to
    ///    `mod <stem>;` or to `#[path = "<file>"]` (the via-`#[path]` shape
    ///    `shadow_tests.rs` uses). The include is verified on disk, so a
    ///    production-compiled `*_tests.rs` stays scanned.
    ///
    /// Windows separators never matter: `rel` is normalized before any
    /// comparison and `repo_root().join` accepts `/` on every platform.
    fn is_test_file(rel: &str) -> bool {
        let rel = normalize_rel(rel);
        if rel.contains("/tests/") {
            return true;
        }
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        if name != "tests.rs" && !name.ends_with("_tests.rs") {
            return false;
        }
        let path = repo_root().join(&rel);
        let Some(dir) = path.parent() else {
            return false;
        };
        let stem = name.strip_suffix(".rs").unwrap_or(name);
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let sibling = entry.path();
            if sibling == path || sibling.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&sibling) else {
                continue;
            };
            if declares_test_include(&src, name, stem) {
                return true;
            }
        }
        false
    }

    /// Byte offsets of `needle` in `src` at un-masked (code) positions, so a
    /// mention inside a comment or string literal can never count.
    fn code_needle_offsets(src: &str, needle: &str) -> Vec<usize> {
        let code = code_mask(src);
        let mut out = Vec::new();
        let mut pos = 0usize;
        while let Some(rel) = src[pos..].find(needle) {
            let at = pos + rel;
            if code[at..at + needle.len()].iter().all(|c| *c) {
                out.push(at);
            }
            pos = at + needle.len();
        }
        out
    }

    /// The contiguous `#[...]` attribute run immediately before `at`
    /// (single-line and multi-line attributes, whitespace between them).
    fn preceding_attributes(src: &str, at: usize) -> String {
        let mut end = src[..at].trim_end().len();
        let mut out = String::new();
        loop {
            let s = &src[..end];
            if s.as_bytes().last().is_none_or(|b| *b != b']') {
                break;
            }
            let bytes = s.as_bytes();
            let mut depth = 0i32;
            let mut i = s.len();
            let mut start = None;
            while i > 0 {
                i -= 1;
                match bytes[i] {
                    b']' => depth += 1,
                    b'[' => {
                        depth -= 1;
                        if depth == 0 {
                            start = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(start) = start else { break };
            if start == 0 || bytes[start - 1] != b'#' {
                break;
            }
            out = format!("{}{}", &src[start - 1..end], out);
            end = src[..start - 1].trim_end().len();
        }
        out
    }

    /// True when the attribute run carries a `#[cfg(...)]` that can never be
    /// present in a non-test build (the same predicate the cfg stripper
    /// uses).
    fn has_test_gated_cfg(attrs: &str) -> bool {
        let mut rest = attrs;
        while let Some(at) = rest.find("#[cfg(") {
            let body_start = at + "#[cfg(".len();
            let Some(end) = rest[body_start..].find(")]") else {
                return false;
            };
            let body: String = rest[body_start..body_start + end]
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            if is_test_gated(&body) {
                return true;
            }
            rest = &rest[body_start + end + 2..];
        }
        false
    }

    /// Does `src` include `file_name`/`stem` as a test module (see
    /// [`is_test_file`])? Both declaration shapes are recognized:
    /// `mod <stem>;` (code-masked, so a comment/string mention never
    /// counts) and `#[path = "<file>"]` (which lives inside the attribute's
    /// string, so it is matched raw) — each only when the immediately
    /// preceding attribute run is test-gated.
    fn declares_test_include(src: &str, file_name: &str, stem: &str) -> bool {
        for at in code_needle_offsets(src, &format!("mod {stem};")) {
            if has_test_gated_cfg(&preceding_attributes(src, at)) {
                return true;
            }
        }
        let path_attr = format!("#[path = \"{file_name}\"]");
        let mut pos = 0usize;
        while let Some(rel) = src[pos..].find(&path_attr) {
            let at = pos + rel;
            if has_test_gated_cfg(&preceding_attributes(src, at)) {
                return true;
            }
            pos = at + path_attr.len();
        }
        false
    }

    /// Positions of `marker` on code lines inside kept (production)
    /// ranges.
    fn find_markers(f: &File<'_>, markers: &[&str]) -> Vec<(usize, String)> {
        let mut hits = Vec::new();
        for &m in markers {
            let mb = m.as_bytes();
            let mut pos = 0usize;
            while let Some(rel) = f.src[pos..].find(m) {
                let at = pos + rel;
                let in_kept = f.kept.iter().any(|(a, z)| at >= *a && at + mb.len() <= *z);
                let in_code = f.code[at..at + mb.len()].iter().all(|c| *c);
                if in_kept && in_code {
                    let line = line_of(f.src, at);
                    hits.push((line, trim_line(f.src, at)));
                }
                pos = at + mb.len();
            }
        }
        hits.sort_unstable();
        hits.dedup();
        hits
    }

    fn line_of(src: &str, at: usize) -> usize {
        src.as_bytes()[..at].iter().filter(|b| **b == b'\n').count() + 1
    }

    /// Byte offsets of `marker` on code lines inside kept (production)
    /// ranges (the offset analogue of [`find_markers`], for scans that need
    /// to inspect the enclosing expression).
    fn find_marker_offsets(f: &File<'_>, marker: &str) -> Vec<usize> {
        let mb = marker.as_bytes();
        let mut out = Vec::new();
        let mut pos = 0usize;
        while let Some(rel) = f.src[pos..].find(marker) {
            let at = pos + rel;
            let in_kept = f.kept.iter().any(|(a, z)| at >= *a && at + mb.len() <= *z);
            let in_code = f.code[at..at + mb.len()].iter().all(|c| *c);
            if in_kept && in_code {
                out.push(at);
            }
            pos = at + mb.len();
        }
        out
    }

    /// True when `.await` appears within `window` bytes after `at` — the
    /// shape of the manager's async read wrappers (a synchronous store read
    /// on a Tokio worker has no await in its own expression).
    fn awaited_within(f: &File<'_>, at: usize, window: usize) -> bool {
        let end = (at + window).min(f.src.len());
        f.src[at..end].contains(".await")
    }

    /// The audit-13 tripwire: production `crates/agent` code must never run
    /// a bounded read synchronously. Offenders are the explicitly rejected
    /// shapes — `store().provider_call_prefix_rows`, `store().cost_task_row`,
    /// `store().get_task`, and any `messages_backwards_bounded` call whose
    /// expression is not awaited (the sync `SessionHandle` read) — while the
    /// `SessionManager` async wrappers (`provider_prefix_history`,
    /// `budget_view`, `task`, awaited `messages_backwards_bounded`) pass.
    fn agent_sync_store_read_offenders(f: &File<'_>) -> Vec<String> {
        let mut offenders = Vec::new();
        for marker in [
            "provider_call_prefix_rows",
            "cost_task_row",
            ".store().get_task",
        ] {
            for (line, text) in find_markers(f, &[marker]) {
                offenders.push(format!(
                    "{}:{line}: {text}  [synchronous store read on the turn path; \
                     submit it through the SessionManager's bounded read pool]",
                    f.rel
                ));
            }
        }
        for at in find_marker_offsets(f, "messages_backwards_bounded") {
            if !awaited_within(f, at, 400) {
                offenders.push(format!(
                    "{}:{}: un-awaited messages_backwards_bounded (synchronous \
                     SessionHandle read; use the awaited SessionManager wrapper)",
                    f.rel,
                    line_of(f.src, at)
                ));
            }
        }
        offenders
    }

    fn trim_line(src: &str, at: usize) -> String {
        let line = line_of(src, at);
        src.lines()
            .nth(line - 1)
            .map(|l| l.trim().to_string())
            .unwrap_or_default()
    }

    fn assert_no_offenders(scan: &str, offenders: &[String], scanned: usize, floor: usize) {
        assert!(scanned >= floor, "{scan}: scan walked nothing: {scanned}");
        assert!(
            offenders.is_empty(),
            "{scan} — source authority violations:\n  {}\n",
            offenders.join("\n  ")
        );
    }

    /// The construct/spawn markers the bootstrap tightness proof counts:
    /// [`SPAWN_MARKERS`] minus `CommandExt`. `release.rs` names
    /// `std::os::unix::process::CommandExt` only to call `exec()`, which
    /// replaces the process and spawns nothing, so `CommandExt` is not a
    /// child-owning shape in this file; every other process-`Command`
    /// marker still fires on production text.
    const BOOTSTRAP_CONSTRUCT_MARKERS: &[&str] = &[
        "std::process::Command",
        "tokio::process::Command",
        "process::Stdio",
        "Command::new",
        "Command::spawn",
    ];

    /// The exact production construct/spawn site(s) the file-level
    /// [`SPAWN_BOOTSTRAP_EXEMPT`] may hold — ONE line, the
    /// hash-then-exec of the digest-verified artifact in `launch()`: on
    /// unix `execve` replaces this process, on non-unix the `spawn` is
    /// owned by the bounded `wait_bounded` poll under
    /// `RELEASE_FORWARD_CEILING_MS`. Test-only children are excluded from
    /// production text by [`kept_ranges`] exactly as in every other
    /// production scan, so the `wait_bounded` unit-test fixtures
    /// (`std::process::Command::new("sh")` inside `#[cfg(test)] mod tests`,
    /// consumed by a bounded wait or killed+reaped) are invisible here —
    /// while any NEW production construct/spawn line fires. The sanctioned
    /// site is pinned by exact text, not a bare count, so the exemption
    /// cannot absorb a second launch path or an offloaded process runtime;
    /// a missing site is a stale-exemption error, never a silent pass.
    const BOOTSTRAP_ALLOWED_SPAWN_SITES: &[&str] =
        &["let mut command = std::process::Command::new(&target.binary);"];

    /// Offenders for the bootstrap tightness proof: production
    /// construct/spawn sites not on [`BOOTSTRAP_ALLOWED_SPAWN_SITES`], plus
    /// a stale-entry error when an allowed site vanished. Separate from
    /// [`production_spawn_offenders`] because that scan silences the whole
    /// exempt file; this function is the exemption's teeth.
    fn bootstrap_spawn_offenders(f: &File<'_>) -> Vec<String> {
        let sites = find_markers(f, BOOTSTRAP_CONSTRUCT_MARKERS);
        let mut offenders: Vec<String> = sites
            .iter()
            .filter(|(_, text)| !BOOTSTRAP_ALLOWED_SPAWN_SITES.contains(&text.as_str()))
            .map(|(line, text)| {
                format!(
                    "{}:{line}: {text}  [unowned bootstrap construct/spawn; the exemption \
                     covers exactly one verified exec]",
                    f.rel
                )
            })
            .collect();
        for allowed in BOOTSTRAP_ALLOWED_SPAWN_SITES {
            if !sites.iter().any(|(_, text)| text == allowed) {
                offenders.push(format!(
                    "{}: sanctioned bootstrap spawn site is missing: {allowed:?} — \
                     the file-level exemption is stale and must be re-audited",
                    f.rel
                ));
            }
        }
        offenders
    }

    /// Tightness proof for [`SPAWN_BOOTSTRAP_EXEMPT`]: the updater's
    /// bootstrap launcher may hold exactly ONE production spawn site — the
    /// hash-then-exec of the verified artifact — so the file-level
    /// exemption can never hide a growing process runtime. `#[cfg(test)]`
    /// children are excluded by the same production extraction the other
    /// scans use; they are never shipped and own no production child.
    #[test]
    fn bootstrap_launcher_exemption_covers_exactly_one_spawn_site() {
        let f = load("crates/updater/src/release.rs").expect("release.rs readable");
        let production_sites = find_markers(&f, BOOTSTRAP_CONSTRUCT_MARKERS);
        assert_eq!(
            production_sites.len(),
            1,
            "crates/updater/src/release.rs production text must hold exactly one \
             construct/spawn site (the verified exec; test-gated children are \
             excluded like every other production scan): {production_sites:?}"
        );
        let offenders = bootstrap_spawn_offenders(&f);
        assert!(
            offenders.is_empty(),
            "crates/updater/src/release.rs — bootstrap exemption tightness violations:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// Planted-violation proof for the bootstrap tightness proof: an
    /// unowned production spawn added to the exempt file MUST fail even
    /// though [`SPAWN_BOOTSTRAP_EXEMPT`] silences the generic spawn scan,
    /// while the sanctioned exec line alone passes, a test-gated child
    /// stays invisible, and a vanished exec is a stale-exemption error.
    #[test]
    fn bootstrap_tightness_fires_on_a_planted_unowned_spawn() {
        let sanctioned = "let mut command = std::process::Command::new(&target.binary);";
        let planted = format!(
            "fn launch(target: &ReleaseTarget) {{\n    {sanctioned}\n}}\n\
             fn smuggled() {{ let _ = std::process::Command::new(\"/bin/sh\"); }}\n"
        );
        let f = synthetic_file("crates/updater/src/release.rs", &planted);
        let offenders = bootstrap_spawn_offenders(&f);
        assert_eq!(
            offenders.len(),
            1,
            "exactly the planted unowned line must fire: {offenders:?}"
        );
        assert!(
            offenders[0].contains("smuggled") && offenders[0].contains("Command::new"),
            "the offender must name the planted unowned spawn: {offenders:?}"
        );
        // The sanctioned site alone never fires (no false positive).
        let f = synthetic_file(
            "crates/updater/src/release.rs",
            &format!("fn launch(target: &ReleaseTarget) {{\n    {sanctioned}\n}}\n"),
        );
        assert!(
            bootstrap_spawn_offenders(&f).is_empty(),
            "the documented exec is the one allowed site"
        );
        // A test-gated child (the wait_bounded fixtures) is NOT production.
        let f = synthetic_file(
            "crates/updater/src/release.rs",
            &format!(
                "fn launch(target: &ReleaseTarget) {{\n    {sanctioned}\n}}\n\
                 #[cfg(test)]\nmod tests {{\n    \
                 fn t() {{ let _ = std::process::Command::new(\"sh\"); }}\n}}\n"
            ),
        );
        assert!(
            bootstrap_spawn_offenders(&f).is_empty(),
            "test-gated children are excluded like every other production scan"
        );
        // A missing sanctioned site is a stale exemption, not a pass.
        let f = synthetic_file(
            "crates/updater/src/release.rs",
            "fn launch(_target: &ReleaseTarget) {}\n",
        );
        let offenders = bootstrap_spawn_offenders(&f);
        assert!(
            offenders.iter().any(|o| o.contains("stale")),
            "a vanished verified exec must not pass silently: {offenders:?}"
        );
    }

    // ------------------------------------------------------------------
    // scan 1: production child spawning
    // ------------------------------------------------------------------

    /// Files whose plain `use std::process::Command` import is the
    /// documented environment authority, never a spawn site.
    const SPAWN_NAME_IMPORT_ALLOWLIST: &[&str] = &["crates/core/src/command.rs"];

    /// The ONE bootstrap launcher: it digest-verifies the release artifact
    /// and execve()s it BEFORE any daemon (and therefore any supervisor)
    /// exists, so the exec IS the launch contract and cannot be supervised.
    /// The tightness proof
    /// [`bootstrap_launcher_exemption_covers_exactly_one_spawn_site`]
    /// asserts the file's production text holds exactly ONE construct/spawn
    /// site — pinned by exact line, with a planted-violation proof — so the
    /// exemption can never silently grow (test-gated children are excluded,
    /// as in every production scan).
    const SPAWN_BOOTSTRAP_EXEMPT: &[&str] = &["crates/updater/src/release.rs"];

    const SPAWN_MARKERS: &[&str] = &[
        "std::process::Command",
        "tokio::process::Command",
        "process::Stdio",
        "Command::new",
        "Command::spawn",
        "CommandExt",
    ];

    /// Production spawn offenders of one file. `rel` may arrive in Windows
    /// (`\`) or POSIX (`/`) spelling and is normalized before EVERY
    /// exemption/allowlist comparison. Exempt: the supervisor and pty
    /// launcher crates (the single child owners), out-of-line test modules
    /// (see [`is_test_file`]), and the one documented `use
    /// std::process::Command` name import.
    fn production_spawn_offenders(rel: &str, f: &File<'_>) -> Vec<String> {
        let rel = normalize_rel(rel);
        if rel.starts_with("crates/terminal/") || rel.starts_with("crates/pty/") {
            return Vec::new(); // the supervisor crate + the pty launcher wrapper
        }
        if is_test_file(&rel) {
            return Vec::new(); // out-of-line test bodies are test-only by declaration
        }
        if SPAWN_BOOTSTRAP_EXEMPT.contains(&rel.as_str()) {
            return Vec::new(); // verified-artifact bootstrap exec (see above)
        }
        find_markers(f, SPAWN_MARKERS)
            .into_iter()
            .filter(|(_, text)| {
                !(text.contains("use std::process::Command")
                    && SPAWN_NAME_IMPORT_ALLOWLIST.contains(&rel.as_str()))
            })
            .map(|(line, text)| format!("{rel}:{line}: {text}"))
            .collect()
    }

    /// `std::process::Command` / `tokio::process::Command` spawn machinery
    /// may exist in exactly two production homes: `crates/terminal` (the
    /// process supervisor, the single owner of children) and `crates/pty`
    /// (the interactive-terminal platform launcher wrapper). One additional
    /// file may NAME `std::process::Command` without constructing or
    /// spawning one: `crates/core/src/command.rs`, the environment authority
    /// whose `EnvSpec::apply` configures a caller-provided `Command` — only
    /// the `use` line is excused, every construction/spawn marker there
    /// still fires. Platform launcher wrappers and tests are the ONLY listed
    /// exceptions — the default allowlist is empty, so any other production
    /// crate that starts spawning is listed loudly and fails the build.
    #[test]
    fn no_production_child_spawn_outside_terminal_and_pty_launcher() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            let Some(f) = load(&rel) else {
                continue;
            };
            offenders.extend(production_spawn_offenders(&rel, &f));
            scanned += 1;
        }
        assert_no_offenders(
            "spawn scan: child spawn machinery outside crates/terminal and crates/pty \
             (every child must be routed through the ProcessSupervisor or the pty launcher)",
            &offenders,
            scanned,
            100,
        );
    }

    /// The INDIRECT spawn shape: a production file that LOWERS a
    /// `CommandSpec`/`ResolvedCommand` (or names the resolved type) and
    /// ALSO contains process-`Command` construction/spawn markers. The
    /// direct scan catches the literal markers; this scan additionally
    /// keeps flagging the conversion when a lowered program value flows
    /// into a spawn the marker list would otherwise describe as legit.
    fn indirect_resolved_spawn_offenders(f: &File<'_>) -> Vec<String> {
        let lowered = find_markers(f, &["ResolvedCommand", ".lower(", ".lower_with("]);
        if lowered.is_empty() {
            return Vec::new();
        }
        find_markers(
            f,
            &[
                "std::process::Command",
                "tokio::process::Command",
                "process::Command",
                "Command::new",
                "Command::spawn",
                "CommandExt",
            ],
        )
        .into_iter()
        .filter(|(_, text)| {
            !(text.contains("use std::process::Command")
                && SPAWN_NAME_IMPORT_ALLOWLIST.contains(&f.rel.as_str()))
        })
        .map(|(line, text)| {
            format!(
                "{}:{line}: {text}  [indirect ResolvedCommand -> process spawn]",
                f.rel
            )
        })
        .collect()
    }

    #[test]
    fn no_indirect_resolved_command_spawn_outside_terminal_and_pty() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            if rel.starts_with("crates/terminal/")
                || rel.starts_with("crates/pty/")
                || is_test_file(&rel)
            {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            offenders.extend(indirect_resolved_spawn_offenders(&f));
            scanned += 1;
        }
        assert_no_offenders(
            "indirect-spawn scan: a file lowers a ResolvedCommand and then constructs/spawns \
             a process Command (route the resolved program+argv through the ProcessSupervisor)",
            &offenders,
            scanned,
            100,
        );
    }

    // ------------------------------------------------------------------
    // scan 2: production reqwest client egress
    // ------------------------------------------------------------------

    /// A raw `reqwest::Client` (construction or `.execute`) may exist ONLY
    /// inside the checked transport `crates/provider/src/egress.rs`.
    /// Adapter production code never names a client: every send goes
    /// through the `HttpTransport` seam, so the request-time destination
    /// gate applies to every provider. The default allowlist is empty.
    /// Out-of-line test modules (see [`is_test_file`]) are test-only by
    /// declaration; `rel` is normalized for the same reason as scan 1.
    ///
    /// Matchers:
    /// * qualified markers — `reqwest::Client`, `Client::builder`,
    ///   `.execute(`;
    /// * an import that brings `Client` into scope UNQUALIFIED
    ///   (`use reqwest::*;`, `use reqwest::Client;`,
    ///   `use reqwest::{Client, ...}`) is itself a client marker: it means
    ///   the bare `Client::new(...)`/`Client::builder(...)` spellings
    ///   construct a raw reqwest client without ever writing
    ///   `reqwest::Client`, so the wildcard form can no longer escape the
    ///   scan. Bare markers are word-boundary checked (`MyClient::new(`
    ///   never matches) and `::`-prefixed qualified spellings are left to
    ///   the qualified markers.
    const REQWEST_CLIENT_MARKERS: &[&str] = &["reqwest::Client", "Client::builder", ".execute("];

    /// `true` when this (trimmed) production line imports `Client` from
    /// `reqwest` WITHOUT a `reqwest::` path qualifier: a glob import, a
    /// direct `use reqwest::Client`, a `pub use` or a braced import whose
    /// member list contains `Client` (optionally aliased).
    fn reqwest_client_imported_unqualified(line: &str) -> bool {
        // `pub use reqwest::...` contains `use reqwest::...` verbatim, so a
        // single split covers both.
        let Some((_, after)) = line.split_once("use reqwest::") else {
            return false;
        };
        let after = after.trim();
        if after.starts_with('*') || after.starts_with("::*") {
            return true; // the glob re-exports `Client` (and friends)
        }
        if after.starts_with("Client")
            && after["Client".len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
        {
            return true;
        }
        match line.split_once('{') {
            Some((_, members)) => members
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|token| token == "Client"),
            None => false,
        }
    }

    /// Scan-2 offenders of one production file: raw reqwest client
    /// construction/execute, including the bare spellings a
    /// `use reqwest::*;` / `use reqwest::Client;` import makes possible.
    fn reqwest_client_offenders(f: &File<'_>) -> Vec<String> {
        let mut offenders = Vec::new();
        for (line, text) in find_markers(f, REQWEST_CLIENT_MARKERS) {
            offenders.push(format!("{}:{line}: {text}", f.rel));
        }
        let imports_client = find_markers(f, &["use reqwest::"])
            .iter()
            .any(|(_, text)| reqwest_client_imported_unqualified(text));
        if imports_client {
            for marker in ["Client::new(", "Client::builder("] {
                for at in find_marker_offsets(f, marker) {
                    // Word boundary: `MyClient::new(` is a different type,
                    // and `reqwest::Client::new(` is already flagged above.
                    let before = at
                        .checked_sub(1)
                        .map(|i| f.src.as_bytes()[i])
                        .filter(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b':');
                    if before.is_none() {
                        let line = line_of(f.src, at);
                        offenders.push(format!(
                            "{}:{line}: {}  [bare Client from a reqwest import]",
                            f.rel,
                            trim_line(f.src, at)
                        ));
                    }
                }
            }
        }
        offenders.sort_unstable();
        offenders.dedup();
        offenders
    }

    #[test]
    fn no_production_reqwest_client_outside_the_checked_transport() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            if rel == "crates/provider/src/egress.rs" || is_test_file(&rel) {
                continue; // the ONE checked transport: PolicyCheckedHttpTransport
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            let has_reqwest = !find_markers(&f, &["reqwest"]).is_empty();
            if !has_reqwest {
                continue; // no reqwest in production here: nothing to gate
            }
            offenders.extend(reqwest_client_offenders(&f));
            scanned += 1;
        }
        assert_no_offenders(
            "reqwest scan: raw client construction/execute outside \
             crates/provider/src/egress.rs (every adapter send must go through the \
             HttpTransport seam)",
            &offenders,
            scanned,
            4,
        );
    }

    // ------------------------------------------------------------------
    // scan 2b: ONE egress address-classification authority
    // ------------------------------------------------------------------

    /// The address-class authority (`AddressClass`, `classify_ip`,
    /// `EgressAddressPolicy`, `vet_resolved_answers`) is defined ONCE in
    /// `crates/security/src/network.rs`. The provider resolver and the
    /// browser broker MUST consume it: a locally re-defined classifier,
    /// class enum or answer-vetting function is a red test — that
    /// duplication is exactly what let the two tables drift and let the
    /// browser skip literal-IP vetting. A `pub use` re-export is fine; a
    /// definition is not.
    const EGRESS_AUTHORITY_MARKERS: &[&str] = &[
        "fn classify_v4",
        "fn classify_v6",
        "fn classify_ip",
        "enum AddressClass",
        "enum IpClass",
        "fn embedded_v4",
        "fn class_permitted",
        "fn vet_resolved_answers",
    ];

    fn egress_authority_redefinition_offenders(f: &File<'_>) -> Vec<String> {
        find_markers(f, EGRESS_AUTHORITY_MARKERS)
            .into_iter()
            .map(|(line, text)| {
                format!(
                    "{}:{line}: {text}  [address classification lives ONCE in \
                     faktor_security::network; consume it instead of redefining it]",
                    f.rel
                )
            })
            .collect()
    }

    #[test]
    fn browser_and_provider_never_redefine_the_egress_address_authority() {
        // The authority itself must exist with its generated pinned table, so
        // this scan cannot pass by everyone deleting classification.
        let authority = std::fs::read_to_string(repo_root().join("crates/security/src/network.rs"))
            .expect("the egress address authority crates/security/src/network.rs must exist");
        for marker in [
            "pub fn classify_ip",
            "pub fn vet_resolved_answers",
            "pub enum AddressClass",
            "pub struct EgressAddressPolicy",
            "// BEGIN GENERATED IANA TABLE",
        ] {
            assert!(
                authority.contains(marker),
                "faktor_security::network is missing {marker:?}"
            );
        }
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            let consumer =
                rel.starts_with("crates/browser/") || rel.starts_with("crates/provider/");
            if !consumer || is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            scanned += 1;
            offenders.extend(egress_authority_redefinition_offenders(&f));
        }
        assert_no_offenders(
            "egress address authority scan: browser/provider must consume \
             faktor_security::network (no local classify_v4/classify_v6/AddressClass/\
             IpClass/vet_resolved_answers definitions)",
            &offenders,
            scanned,
            8,
        );
        // Negative proof: a planted local classifier FIRES, and re-exporting
        // the authority is the sanctioned shape that does not.
        let planted = synthetic_file(
            "crates/browser/src/evil.rs",
            "fn classify_v4(v4: u8) -> u8 { v4 }\n",
        );
        assert!(
            !egress_authority_redefinition_offenders(&planted).is_empty(),
            "a planted local classifier must fire the scan"
        );
        let planted = synthetic_file(
            "crates/provider/src/resolver.rs",
            "pub use faktor_security::network::{AddressClass, EgressAddressPolicy};\n",
        );
        assert!(
            egress_authority_redefinition_offenders(&planted).is_empty(),
            "re-exporting the authority is the sanctioned shape"
        );
    }

    // ------------------------------------------------------------------
    // scan 3: hand-rolled temp-write / rename sequences
    // ------------------------------------------------------------------

    /// The forbidden patterns are checked INDEPENDENTLY — no fsync marker
    /// is required, and the historic temp+rename+sync triangle is NOT the
    /// detection shape. Any production `std::fs::rename` / `fs::rename` /
    /// `tokio::fs::rename` / `NamedTempFile::persist`, and any write-family
    /// call whose target names a temp path (`tmp`/`NamedTempFile` on the
    /// same line), is an offender unless its exact line content is
    /// allowlisted. The one sanctioned home is `crates/fs/src/atomic.rs`;
    /// the grandfathered sequences below are documented per exact line, so
    /// any NEW sequence (including inside a grandfathered file) is listed
    /// loudly. Default allowlist: empty.
    const ATOMIC_ANCHOR: &str = "crates/fs/src/atomic.rs";

    const ATOMIC_RENAME_MARKERS: &[&str] = &[
        "std::fs::rename",
        "fs::rename",
        "tokio::fs::rename",
        "NamedTempFile::persist",
    ];

    const ATOMIC_WRITE_MARKERS: &[&str] = &[
        "fs::write",
        "std::fs::write",
        "tokio::fs::write",
        "File::create",
        "OpenOptions",
        "write_all",
    ];

    /// Calls that actually MUTATE the file an `OpenOptions` targets. The
    /// builder itself is not a write: access-mode flags (`.read(true)`,
    /// `.write(true)`) only grant rights. The CAS streaming put reopens its
    /// already-fsynced temp read+WRITE so `FlushFileBuffers` can run on
    /// Windows (commit b18eec4) — that line performs no mutation and is a
    /// durability flush, not an atomic-write sequence, so the 2026-09 false
    /// positive is fixed here: an `OpenOptions` line that names a temp path
    /// is an offender only when one of these mutation calls is on the same
    /// line. Every other write marker mutates by itself.
    const ATOMIC_MUTATION_MARKERS: &[&str] = &[
        "write_all",
        "write_fmt",
        "fs::write",
        "std::fs::write",
        "tokio::fs::write",
        "File::create",
        "create(true)",
        "create_new(true)",
        "truncate(true)",
        "set_len(",
    ];

    const ATOMIC_ALLOWLIST: &[(&str, &str)] = &[
        // crates/cas: content-addressed store writer (frozen layer below
        // crates/fs; its own documented temp+fsync+rename durability
        // contract, self-contained and seam-tested).
        (
            "crates/cas/src/lib.rs",
            "let file = fs::File::create(&tmp)?;",
        ),
        (
            "crates/cas/src/lib.rs",
            "let mut f = fs::File::create(&tmp)?;",
        ),
        ("crates/cas/src/lib.rs", "match fs::rename(tmp, path) {"),
        // crates/fs/src/lib.rs: the fs crate's own stream-copy writer
        // (copy_open_file) — same-crate internal helper that reuses the
        // atomic module's temp naming and fsync_parent.
        (
            "crates/fs/src/lib.rs",
            "let mut out = fs::File::create(&tmp)",
        ),
        (
            "crates/fs/src/lib.rs",
            "fs::rename(&tmp, target).map_err(|e| {",
        ),
        // crates/git: worktree metadata save (spec §33) — best-effort
        // .git-internal writer with its own unique-temp discipline.
        ("crates/git/src/lib.rs", "std::fs::rename(&tmp, &path)?;"),
        // crates/git: the stale-lease reconcile claim — GIT-INTERNAL
        // metadata (the mutation lease under the common git dir), renamed
        // to a unique tombstone (pid + uuid) so exactly one stealer wins;
        // it never touches workspace content and is followed by the
        // tombstone removal.
        (
            "crates/git/src/guard.rs",
            "match std::fs::rename(&path, &tombstone) {",
        ),
        // crates/fs: entry-state transactional landing (canonical
        // kind/mode/literal-target triple). The landing primitive stages
        // its own uniquely-named temp beside the destination, fsyncs it,
        // renames and fsyncs the parent through `crate::atomic::fsync_parent`
        // — the generic in-memory content-replace helper cannot express the
        // symlink-aware replacement this primitive owns.
        (
            "crates/fs/src/entry_state.rs",
            "let mut file = fs::File::create(tmp).map_err(|e| io_failure(\"create\", tmp, e))?;",
        ),
        (
            "crates/fs/src/entry_state.rs",
            "fs::rename(&tmp, dst).map_err(|e| {",
        ),
    ];

    /// The temp-write half of scan 3, independent of any fsync: a
    /// write-family call whose LINE also targets a temp path. The temp
    /// check reads the raw line (not the code mask), because the canonical
    /// shape is a string-literal path such as `".thing.tmp"`. An
    /// `OpenOptions` builder alone is not a write (see
    /// [`ATOMIC_MUTATION_MARKERS`]): it fires only with an actual mutation
    /// call on the same line, so a pure durability reopen of an existing
    /// temp is accepted while a create/truncate/write of one still fires.
    fn atomic_temp_write_hits(f: &File<'_>) -> Vec<(usize, String)> {
        let mut hits = Vec::new();
        for (line, text) in find_markers(f, ATOMIC_WRITE_MARKERS) {
            let raw = f.src.lines().nth(line - 1).unwrap_or_default();
            if !(raw.contains("tmp") || raw.contains("NamedTempFile")) {
                continue;
            }
            if text.contains("OpenOptions")
                && !ATOMIC_MUTATION_MARKERS
                    .iter()
                    .any(|mutation| raw.contains(mutation))
            {
                continue;
            }
            hits.push((line, text));
        }
        // `NamedTempFile::persist` in its associated/literal form AND the
        // instance form (`line ... NamedTempFile ... .persist(`) are both
        // independent forbidden patterns.
        for (line, text) in find_markers(f, &["NamedTempFile"]) {
            let raw = f.src.lines().nth(line - 1).unwrap_or_default();
            if raw.contains(".persist(") {
                hits.push((line, text));
            }
        }
        hits.sort_unstable();
        hits.dedup();
        hits
    }

    /// Every scan-3 offender of one production file: rename/persist
    /// patterns plus temp-path writes, minus the exact-line allowlist.
    fn atomic_write_offenders(f: &File<'_>) -> Vec<String> {
        let mut hits = find_markers(f, ATOMIC_RENAME_MARKERS);
        hits.extend(atomic_temp_write_hits(f));
        hits.sort_unstable();
        hits.dedup();
        let mut offenders = Vec::new();
        for (line, text) in hits {
            let allowlisted = ATOMIC_ALLOWLIST
                .iter()
                .any(|(p, t)| *p == f.rel && *t == text);
            if !allowlisted {
                offenders.push(format!("{}:{line}: {text}", f.rel));
            }
        }
        offenders
    }

    #[test]
    fn no_hand_rolled_atomic_write_outside_fs_atomic_and_the_exact_allowlist() {
        let mut offenders: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            if rel == ATOMIC_ANCHOR || is_test_file(&rel) {
                continue; // the one sanctioned home; out-of-line test bodies
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            offenders.extend(atomic_write_offenders(&f));
            scanned += 1;
        }
        assert_no_offenders(
            "atomic-write scan: a rename / NamedTempFile persist / temp-path write \
             exists outside crates/fs/src/atomic.rs (route file-content replacement \
             through faktor_fs::atomic)",
            &offenders,
            scanned,
            3,
        );
    }

    // ------------------------------------------------------------------
    // scan 3b: no resolve-then-std::fs mutation of workspace paths (P1-C)
    // ------------------------------------------------------------------

    /// P1-C: production code must NEVER resolve a workspace-relative path
    /// with `WorkspaceHandle::resolve` (or any `.resolve(`) and then mutate
    /// that resolved PATH STRING with `std::fs`/`fs`. Resolution is a
    /// point-in-time check: between it and the mutation syscall a parent
    /// directory can be swapped for an outside symlink, so the mutation
    /// follows the swap and touches an external file. Workspace mutations
    /// go through the anchored handle (`WorkspaceHandle::remove_file`,
    /// `write_atomic`, `RootedDir::*`), which walks from the fd opened at
    /// open time and refuses links.
    ///
    /// Detection shape: either (a) a mutation call whose own argument
    /// expression contains `.resolve(` (`std::fs::remove_file(ws.resolve(rel)?)`),
    /// or (b) a `let <name> = … .resolve(…)` binding whose `<name>` appears
    /// as an argument of a mutation call within the next 700 bytes
    /// (`let resolved = ws.resolve(rel)?; std::fs::remove_file(&resolved)`).
    /// A resolve used only for a capability gate/read (`sandbox_gate`,
    /// `metadata`) is not a mutation and does not fire.
    const FS_MUTATION_MARKERS: &[&str] = &[
        "std::fs::remove_file",
        "std::fs::remove_dir_all",
        "std::fs::remove_dir",
        "std::fs::write",
        "std::fs::rename",
        "std::fs::copy",
        "std::fs::create_dir_all",
        "std::fs::create_dir",
        "std::fs::hard_link",
        "std::fs::symlink",
        "std::fs::set_permissions",
        "std::fs::File::create",
        "fs::remove_file",
        "fs::remove_dir_all",
        "fs::remove_dir",
        "fs::write",
        "fs::rename",
        "fs::copy",
        "fs::create_dir_all",
        "fs::create_dir",
        "fs::hard_link",
        "fs::symlink",
        "fs::set_permissions",
    ];

    /// The identifier bound by the nearest preceding `let <name> = …`
    /// statement (bounded window; `None` when the resolve is not bound).
    fn let_binding_before(src: &str, at: usize) -> Option<String> {
        let start = at.saturating_sub(240);
        let window = &src[start..at];
        let boundary = window.rfind([';', '{', '}']).map(|i| i + 1).unwrap_or(0);
        let stmt = &window[boundary..];
        let let_at = stmt.find("let ")?;
        let name: String = stmt[let_at + 4..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        (!name.is_empty()).then_some(name)
    }

    /// The argument text of the call whose marker ends at `from` (first `(`
    /// to its matching `)`), bounded; empty when no call parens follow.
    fn call_args_after(src: &str, from: usize) -> String {
        let b = src.as_bytes();
        let end = (from + 512).min(src.len());
        let mut i = from;
        while i < end && b[i] != b'(' {
            if b[i] == b';' || b[i] == b'}' {
                return String::new();
            }
            i += 1;
        }
        if i >= end {
            return String::new();
        }
        let start = i + 1;
        let mut depth = 1i32;
        let mut j = start;
        while j < src.len() {
            match b[j] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return src[start..j].to_string();
                    }
                }
                _ => {}
            }
            j += 1;
        }
        String::new()
    }

    fn resolve_then_fs_mutation_offenders(f: &File<'_>) -> Vec<String> {
        let mut hits: Vec<(usize, String)> = Vec::new();
        // (a) nested: the mutation call's own argument resolves the path.
        for marker in FS_MUTATION_MARKERS {
            for at in find_marker_offsets(f, marker) {
                let args = call_args_after(f.src, at + marker.len());
                if args.contains(".resolve(") {
                    hits.push((line_of(f.src, at), trim_line(f.src, at)));
                }
            }
        }
        // (b) bound: `let resolved = ….resolve(rel)?; … fs::<mutation>(&resolved)`.
        for at in find_marker_offsets(f, ".resolve(") {
            let Some(binding) = let_binding_before(f.src, at) else {
                continue;
            };
            let window_end = (at + 700).min(f.src.len());
            for marker in FS_MUTATION_MARKERS {
                let mut pos = at;
                while let Some(rel) = f.src[pos..window_end].find(marker) {
                    let mat = pos + rel;
                    if f.code[mat..mat + marker.len()].iter().all(|c| *c) {
                        let args = call_args_after(f.src, mat + marker.len());
                        let uses_binding = args
                            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .any(|w| w == binding);
                        if uses_binding {
                            hits.push((line_of(f.src, mat), trim_line(f.src, mat)));
                        }
                    }
                    pos = mat + marker.len();
                }
            }
        }
        hits.sort_unstable();
        hits.dedup();
        hits.into_iter()
            .map(|(line, text)| format!("{}:{line}: {text}", f.rel))
            .collect()
    }

    /// P1-C static rule: production code must not resolve a workspace path
    /// and then mutate it by path. The only sanctioned route for a workspace
    /// file deletion is `WorkspaceHandle::remove_file` (anchored, no-follow);
    /// content writes stay on `write_atomic`/`crate::atomic`.
    #[test]
    fn no_resolve_then_std_fs_mutation_of_workspace_paths() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            if is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            offenders.extend(resolve_then_fs_mutation_offenders(&f));
            scanned += 1;
        }
        assert_no_offenders(
            "resolve-then-mutate scan: production code resolves a workspace path and \
             then mutates it with std::fs (use WorkspaceHandle::remove_file / write_atomic; \
             the resolved path loses authority and can be raced by a symlink swap)",
            &offenders,
            scanned,
            100,
        );
    }

    /// Negative proofs: the planted two-step and the nested direct form
    /// MUST fire; a gate-only resolve, a read after resolve, and a
    /// test-gated pattern must not.
    #[test]
    fn resolve_then_mutation_scan_fires_on_the_planted_two_step() {
        let planted = "fn del(ws: &WorkspaceHandle, rel: &Path) -> Result<(), Error> {\n    \
                       let resolved = ws.resolve(rel)?;\n    \
                       std::fs::remove_file(&resolved).map_err(|e| Error::internal(format!(\"{e}\")))?;\n    \
                       Ok(())\n}\n";
        let f = synthetic_file("crates/snapshot/src/lib.rs", planted);
        let offenders = resolve_then_fs_mutation_offenders(&f);
        assert_eq!(
            offenders.len(),
            1,
            "exactly the planted line fires: {offenders:?}"
        );
        assert!(
            offenders[0].contains("std::fs::remove_file") && offenders[0].contains("resolved"),
            "{offenders:?}"
        );

        let nested = "fn del(ws: &WorkspaceHandle, rel: &Path) -> std::io::Result<()> {\n    \
                      std::fs::remove_file(ws.resolve(rel)?)\n}\n";
        let f = synthetic_file("crates/agent/src/runtime.rs", nested);
        assert!(
            !resolve_then_fs_mutation_offenders(&f).is_empty(),
            "the nested direct form must fire"
        );

        // A resolve used only for a capability gate (and a handle write) is
        // the sanctioned shape.
        let gate_only = "fn gate(ws: &WorkspaceHandle, rel: &Path) -> Result<(), Error> {\n    \
                         let resolved = ws.resolve(rel)?;\n    \
                         sandbox_gate(&resolved.clone(), \"write_file\")?;\n    \
                         ws.write_atomic(rel, b\"x\")?;\n    Ok(())\n}\n";
        let f = synthetic_file("crates/cli/src/tools.rs", gate_only);
        assert!(
            resolve_then_fs_mutation_offenders(&f).is_empty(),
            "a gate-only resolve is not a mutation"
        );

        // A read after resolve (`metadata`) is not a mutation.
        let read_only = "fn stat(ws: &WorkspaceHandle, rel: &Path) -> std::io::Result<()> {\n    \
                         let resolved = ws.resolve(rel)?;\n    \
                         let _ = std::fs::metadata(&resolved)?;\n    Ok(())\n}\n";
        let f = synthetic_file("crates/fs/src/lib.rs", read_only);
        assert!(
            resolve_then_fs_mutation_offenders(&f).is_empty(),
            "metadata is a read, not a mutation"
        );

        // A test-gated pattern is not production text.
        let test_gated = "fn ok() {}\n#[cfg(test)]\nmod tests {\n    \
                          fn t(ws: &WorkspaceHandle, rel: &Path) {\n        \
                          let resolved = ws.resolve(rel).unwrap();\n        \
                          std::fs::remove_file(&resolved).unwrap();\n    }\n}\n";
        let f = synthetic_file("crates/snapshot/src/lib.rs", test_gated);
        assert!(
            resolve_then_fs_mutation_offenders(&f).is_empty(),
            "#[cfg(test)] bodies are excluded like every production scan"
        );
    }

    // ------------------------------------------------------------------
    // scan 4: daemon-authority constructor sites (documented, exact counts)
    // ------------------------------------------------------------------

    /// Each daemon-lifetime constructor has exactly the DOCUMENTED
    /// production sites, with exact per-file occurrence counts:
    ///
    /// - `DurableEvidenceAuthority::for_store`: ONE site — the agent
    ///   runtime, whose allocation the daemon graph clones into
    ///   `DaemonGraph::evidence` and the native server receives. No
    ///   CLI/server site may build a parallel authority over the same rows.
    /// - `DurableBudgetLedger::new`: the CLI graph's ONE ledger, the
    ///   per-task/per-session view constructions in the session crate, the
    ///   orchestrator's two cap-seeding sites, and the server crate's
    ///   `ServerDeps::new` embedded/test seam (the daemon assembles through
    ///   `new_with` over the graph's ledger).
    /// - `SemanticProviderRegistry::new`: the agent fallback constructor
    ///   (embedded/test hosts) and the CLI graph builder; the native
    ///   introspection surface inspects `deps.semantic` only.
    /// - `ProcessSupervisor::new`: the two daemon entries
    ///   (`build_daemon`, `build_daemon_with_mcp_inner`), each handing the
    ///   ONE supervisor into the core builder, plus the local Acquire login
    ///   command's short-lived supervisor (its own CAS).
    /// - `ProcessSupervisor::try_shared`: the documented STANDALONE entries
    ///   (index `open`, hooks `try_new`, verify executor/inventory, the
    ///   doctor probe) — fallible typed constructors, never a panic; the
    ///   daemon graph injects its supervisor instead.
    ///
    /// A new (or stale) site anywhere is a red test, never a review nit.
    /// Out-of-line `#[cfg(test)] mod` bodies are skipped (test-only by
    /// declaration).
    const CONSTRUCTOR_SITES: &[(&str, &[(&str, usize)])] = &[
        (
            "DurableEvidenceAuthority::for_store",
            &[("crates/agent/src/runtime.rs", 1)],
        ),
        (
            "DurableBudgetLedger::new",
            &[
                ("crates/cli/src/main.rs", 1),
                ("crates/orchestrator/src/task_executor.rs", 2),
                ("crates/server/src/api.rs", 1),
                ("crates/session/src/manager.rs", 1),
                ("crates/session/src/task.rs", 2),
            ],
        ),
        (
            "SemanticProviderRegistry::new",
            &[
                ("crates/agent/src/lib.rs", 1),
                ("crates/cli/src/graph.rs", 1),
            ],
        ),
        (
            "ProcessSupervisor::new",
            &[
                ("crates/cli/src/main.rs", 2),
                // Faktor Acquire `commerce login` (docs/acquire.md §14): the
                // LOCAL admin command opens the headed dedicated profile
                // browser through a short-lived supervisor over its own CAS.
                // Never a model tool, never the daemon's model environment.
                ("crates/cli/src/tools_market.rs", 1),
            ],
        ),
        // The STANDALONE process authority (`try_shared`) has exactly these
        // production sites. Daemon-owned subsystems (index/hooks/verify)
        // take the daemon's ONE supervisor by injection; each `try_shared`
        // site below is a documented standalone entry point (a fallible
        // constructor that returns a typed error, never a panic), and the
        // doctor site is the read-only diagnostic probe. The normal graph
        // (`build_daemon*`) contains none.
        (
            "ProcessSupervisor::try_shared",
            &[
                ("crates/hooks/src/lib.rs", 1),
                ("crates/index/src/service.rs", 1),
                ("crates/verify/src/exec.rs", 1),
                ("crates/verify/src/inventory.rs", 1),
                ("crates/cli/src/main.rs", 1),
            ],
        ),
    ];

    /// Every production file that constructs `marker`, with the exact
    /// marker hits (line + text).
    fn marker_sites(marker: &str) -> Vec<(String, Vec<(usize, String)>)> {
        let mut out = Vec::new();
        for rel in walk_crate_sources() {
            if is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            let hits = find_markers(&f, &[marker]);
            if !hits.is_empty() {
                out.push((rel, hits));
            }
        }
        out
    }

    #[test]
    fn daemon_authority_constructors_have_exactly_the_documented_sites() {
        for (marker, documented) in CONSTRUCTOR_SITES.iter().copied() {
            let seen = marker_sites(marker);
            // Every documented site must exist with EXACTLY the documented
            // count (a stale allowlist entry is a red test).
            for (file, want) in documented.iter().copied() {
                let found = seen
                    .iter()
                    .find(|(rel, _)| rel.as_str() == file)
                    .map(|(_, hits)| hits.len())
                    .unwrap_or(0);
                assert_eq!(
                    found, want,
                    "{marker}: documented production site {file} must occur exactly \
                     {want} time(s), found {found}"
                );
            }
            // No undocumented production site may construct the authority.
            let mut offenders = Vec::new();
            for (rel, hits) in &seen {
                if documented.iter().any(|(file, _)| *file == rel.as_str()) {
                    continue;
                }
                for (line, text) in hits {
                    offenders.push(format!("{rel}:{line}: {text}"));
                }
            }
            assert!(
                offenders.is_empty(),
                "{marker} outside its documented production sites:\n  {}\n",
                offenders.join("\n  ")
            );
        }
        // The evidence authority is the strictest case (exactly one site):
        // assert the marker really is scanned, so the allowlist can never
        // pass vacuously.
        assert_eq!(
            marker_sites("DurableEvidenceAuthority::for_store")
                .iter()
                .map(|(_, hits)| hits.len())
                .sum::<usize>(),
            1,
            "production DurableEvidenceAuthority::for_store occurrences must be exactly 1"
        );
    }

    // ------------------------------------------------------------------
    // scan 5: no synchronous store reads in the production agent runtime
    // ------------------------------------------------------------------

    /// Audit 13: the production `crates/agent` turn path reads through the
    /// `SessionManager`'s bounded async read pool, never synchronously on a
    /// Tokio worker. The explicitly rejected shapes are
    /// `store().provider_call_prefix_rows`, `store().cost_task_row`,
    /// `store().get_task` and a sync (un-awaited)
    /// `messages_backwards_bounded`; the manager async wrappers are the
    /// allowance, and their ADOPTION is asserted too (an empty allowlist
    /// scan would pass vacuously). Test modules, comments and strings are
    /// stripped by the shared machinery.
    #[test]
    fn no_synchronous_store_reads_in_the_production_agent_runtime() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        let mut adopted = 0usize;
        for rel in walk_crate_sources() {
            if !rel.starts_with("crates/agent/") {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            scanned += 1;
            offenders.extend(agent_sync_store_read_offenders(&f));
            // The async wrappers are really adopted by the production agent
            // (history, budget, prefix at minimum): the allowance is
            // demonstrated, not assumed.
            if !find_markers(
                &f,
                &[
                    "messages_backwards_bounded",
                    "budget_view(",
                    "provider_prefix_history(",
                ],
            )
            .is_empty()
            {
                adopted += 1;
            }
        }
        assert_no_offenders(
            "sync-read scan: the production agent runtime must submit every bounded read \
             (history/budget/task/prefix/verification/memory) through the SessionManager's \
             async read pool",
            &offenders,
            scanned,
            5,
        );
        assert!(
            adopted >= 1,
            "the async read wrappers must be adopted by the production agent runtime \
             (otherwise this scan certifies nothing)"
        );
    }

    // ------------------------------------------------------------------
    // scan 6: retired product-name tokens (source + artifact)
    // ------------------------------------------------------------------

    /// The retired product-name tokens. Each is assembled at runtime from
    /// split literals so THIS scanner's own source cannot exempt itself: the
    /// only permitted location anywhere in the tree is the historical
    /// attribution directory (`ui/LICENSES/`).
    fn retired_tokens() -> Vec<String> {
        vec![
            concat!("ki", "lo").to_string(),
            concat!("v7", "56").to_string(),
            concat!("v7", ".5.6").to_string(),
        ]
    }

    const RETIRED_TOKEN_SKIP_DIRS: &[&str] = &[
        ".git",
        "target",
        "node_modules",
        "build",
        ".gradle",
        // Agent Manager / agent-tool state (never repository source): it
        // contains full checkouts of other worktrees, including historical
        // revisions, and must never be mistaken for the delivered tree.
        // Composed at runtime (like `retired_tokens`) so this scanner's own
        // source can never carry the retired token it forbids.
        concat!(".", "ki", "lo"),
    ];

    /// The ONE permitted location: the retained historical attribution for
    /// the removed vendored UI code. Reported by the test summary.
    const RETIRED_TOKEN_ATTRIBUTION_PREFIX: &str = "ui/LICENSES/";

    fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || haystack.len() < needle.len() {
            return false;
        }
        haystack.windows(needle.len()).any(|window| {
            window
                .iter()
                .zip(needle.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        })
    }

    /// Stream a file in bounded chunks (1 MiB + token-length overlap) so a
    /// huge artifact is scanned without being materialized in RAM.
    fn file_contains_any_token(path: &Path, tokens: &[Vec<u8>]) -> bool {
        use std::io::Read;
        let overlap = tokens.iter().map(|token| token.len()).max().unwrap_or(1);
        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };
        let chunk_size = 1024 * 1024;
        let mut carry: Vec<u8> = Vec::new();
        let mut buffer = vec![0u8; chunk_size];
        loop {
            let Ok(read) = file.read(&mut buffer) else {
                return false;
            };
            if read == 0 {
                return false;
            }
            let mut window = Vec::with_capacity(carry.len() + read);
            window.extend_from_slice(&carry);
            window.extend_from_slice(&buffer[..read]);
            if tokens
                .iter()
                .any(|token| contains_ascii_case_insensitive(&window, token))
            {
                return true;
            }
            let keep = overlap.saturating_sub(1).min(window.len());
            carry = window[window.len() - keep..].to_vec();
        }
    }

    /// Every regular file under the repo root except the skip trees, as a
    /// `/`-normalized repository-relative path.
    fn walk_repo_files() -> Vec<String> {
        let root = repo_root();
        let mut out = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy().to_string();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    if RETIRED_TOKEN_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push(entry.path());
                } else if file_type.is_file() {
                    // OS core dumps are crash artifacts, never source: a
                    // crashed process can leave retired tokens in heap bytes
                    // (e.g. environment/paths), so they must not fail the
                    // branding scan. They are git-ignored and deleted by
                    // hygiene tooling; skipping here keeps the scan about
                    // delivered source only.
                    if name == "core" || name.starts_with("core.") {
                        continue;
                    }
                    let rel = entry
                        .path()
                        .strip_prefix(&root)
                        .unwrap_or(&entry.path())
                        .display()
                        .to_string();
                    out.push(normalize_rel(&rel));
                }
            }
        }
        out.sort();
        out
    }

    /// Retired-token offenders of one repository-relative path (empty when
    /// the path is the attribution location).
    fn retired_token_offenders(rel: &str) -> Vec<String> {
        let rel = normalize_rel(rel);
        if rel.starts_with(RETIRED_TOKEN_ATTRIBUTION_PREFIX) {
            return Vec::new();
        }
        let tokens: Vec<Vec<u8>> = retired_tokens()
            .into_iter()
            .map(|token| token.into_bytes())
            .collect();
        let path = repo_root().join(&rel);
        if !file_contains_any_token(&path, &tokens) {
            return Vec::new();
        }
        vec![format!("{rel}: retired product-name token")]
    }

    /// The retired product name (and its version tokens) must appear ONLY in
    /// the historical attribution directory. This is the source AND artifact
    /// scan: every regular file outside the skip trees is read as bytes (a
    /// stale compiled bridge, bundle, VSIX or jar counts), and the scan is
    /// asserted non-vacuous against a synthetic offender which is itself
    /// assembled at runtime.
    #[test]
    fn retired_product_name_tokens_appear_only_in_the_attribution_location() {
        // The attribution exception must be real, not vacuous.
        let attribution = repo_root().join("ui/LICENSES");
        assert!(
            attribution.join("NOTICE.md").is_file(),
            "the historical attribution (ui/LICENSES/NOTICE.md) must exist; \
             it is the ONE permitted location for the retired product name"
        );
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        let mut attribution_files = 0usize;
        for rel in walk_repo_files() {
            if rel.starts_with(RETIRED_TOKEN_ATTRIBUTION_PREFIX) {
                attribution_files += 1;
                continue;
            }
            offenders.extend(retired_token_offenders(&rel));
            scanned += 1;
        }
        assert_no_offenders(
            "retired-token scan: the retired product name (and its version tokens) may appear \
             ONLY under ui/LICENSES/; delete, rename or re-author the source (never re-vendor it)",
            &offenders,
            scanned,
            100,
        );
        assert!(
            attribution_files >= 1,
            "the attribution location must exist and be walked"
        );
        // The scanner is not vacuous: a synthetic file carrying the retired
        // token at runtime is flagged (composed here so this source stays
        // clean).
        let synthetic = repo_root()
            .join("target/certification")
            .join(format!("retired-token-selfcheck-{}", std::process::id()));
        if std::fs::create_dir_all(synthetic.parent().unwrap_or(Path::new("."))).is_ok() {
            let probe = retired_tokens()
                .into_iter()
                .next()
                .expect("at least one token");
            if std::fs::write(&synthetic, format!("prefix {probe} suffix")).is_ok() {
                let rel = synthetic
                    .strip_prefix(repo_root())
                    .unwrap_or(&synthetic)
                    .display()
                    .to_string();
                let hits = retired_token_offenders(&normalize_rel(&rel));
                assert!(
                    !hits.is_empty(),
                    "a synthetic file carrying the retired token must be flagged"
                );
                let _ = std::fs::remove_file(&synthetic);
            }
        }
    }

    // ------------------------------------------------------------------
    // scan 8: secret-shaped product settings (apps)
    // ------------------------------------------------------------------

    /// Skip trees for the settings scan: build outputs, caches, vendored code
    /// and test resources are not product settings declarations.
    const SETTING_MANIFEST_SKIP_DIRS: &[&str] =
        &["node_modules", "build", "out", ".gradle", "target", ".git"];

    /// Files that DECLARE product settings. A lockfile, bundle, jar or visual
    /// baseline is not a settings schema and is deliberately not scanned
    /// (dependency names may legitimately contain "token" substrings).
    fn is_setting_manifest(name: &str) -> bool {
        name == "package.json"
            || name == "plugin.xml"
            || name == "settings.json"
            || name.ends_with(".schema.json")
    }

    /// Every settings manifest under `apps/`, `/`-normalized.
    fn walk_setting_manifests() -> Vec<String> {
        let root = repo_root().join("apps");
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy().to_string();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    if SETTING_MANIFEST_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push(entry.path());
                } else if file_type.is_file() && is_setting_manifest(&name) {
                    let rel = entry
                        .path()
                        .strip_prefix(repo_root())
                        .unwrap_or(&entry.path())
                        .display()
                        .to_string();
                    out.push(normalize_rel(&rel));
                }
            }
        }
        out.sort();
        out
    }

    /// The RAW secret-shape matcher: separators and case never matter.
    /// Families: `*Token`, `*Secret`, `*ApiKey`, `*Credential`, `*Password`,
    /// the common key-material fragments, and a provider name combined with
    /// `key` (e.g. `openaiKey`). Non-secret path/size shapes (`tokenBudget`,
    /// `credentialsPath`, `defaultProvider`) deliberately pass: the scan
    /// targets credential VALUES, not words containing their stem.
    fn is_secret_shaped_setting_key(key: &str) -> bool {
        let normalized: String = key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if normalized.is_empty() {
            return false;
        }
        const SECRET_SUFFIXES: &[&str] = &["token", "secret", "apikey", "credential", "password"];
        if SECRET_SUFFIXES
            .iter()
            .any(|suffix| normalized.ends_with(suffix))
        {
            return true;
        }
        const SECRET_FRAGMENTS: &[&str] = &[
            "clientsecret",
            "privatekey",
            "accesskey",
            "providerkey",
            "providertoken",
            "providersecret",
            "providercredential",
            "llmkey",
        ];
        if SECRET_FRAGMENTS
            .iter()
            .any(|fragment| normalized.contains(fragment))
        {
            return true;
        }
        const PROVIDER_NAMES: &[&str] = &[
            "openai",
            "anthropic",
            "gemini",
            "googleai",
            "azure",
            "bedrock",
            "vertex",
            "mistral",
            "cohere",
            "groq",
            "openrouter",
            "deepseek",
            "xai",
            "huggingface",
            "replicate",
            "together",
        ];
        normalized.ends_with("key") && PROVIDER_NAMES.iter().any(|name| normalized.contains(name))
    }

    /// The ONLY permitted exceptions: exact (manifest, key) pairs, each with a
    /// written justification. Keep this list TINY; every entry is asserted
    /// load-bearing (the key really is declared there and really matches the
    /// raw matcher) and non-stale by the tests below.
    const SECRET_SETTING_ALLOWLIST: &[(&str, &str, &str)] = &[(
        "apps/vscode/package.json",
        "faktor.controlToken",
        "DEPRECATED plaintext migration shim: declared only so VS Code surfaces the deprecation \
         message; the client REFUSES to send it for cloud calls, and the one-shot migration prompt \
         stores the value in SecretStorage and deletes this setting (the local daemon password path \
         is separate and unchanged). Removed from this list when the declaration is dropped.",
    )];

    fn secret_setting_allowlisted(rel: &str, key: &str) -> bool {
        SECRET_SETTING_ALLOWLIST
            .iter()
            .any(|(file, allowed, _)| *file == rel && *allowed == key)
    }

    /// Minimal JSON value tree; only object member names are retained.
    enum JsonValue {
        Object(Vec<(String, JsonValue)>),
        Array(Vec<JsonValue>),
        Scalar,
    }

    fn skip_json_ws(bytes: &[u8], pos: &mut usize) {
        while *pos < bytes.len() && matches!(bytes[*pos], b' ' | b'\t' | b'\n' | b'\r') {
            *pos += 1;
        }
    }

    fn parse_json_string(bytes: &[u8], pos: &mut usize) -> Option<String> {
        if bytes.get(*pos) != Some(&b'"') {
            return None;
        }
        *pos += 1;
        let mut out = String::new();
        while *pos < bytes.len() {
            match bytes[*pos] {
                b'"' => {
                    *pos += 1;
                    return Some(out);
                }
                b'\\' => {
                    *pos += 1;
                    let escape = *bytes.get(*pos)?;
                    *pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = bytes.get(*pos..*pos + 4)?;
                            let text = std::str::from_utf8(hex).ok()?;
                            let code = u32::from_str_radix(text, 16).ok()?;
                            out.push(char::from_u32(code)?);
                            *pos += 4;
                        }
                        _ => return None,
                    }
                }
                _ => {
                    let rest = std::str::from_utf8(&bytes[*pos..]).ok()?;
                    let ch = rest.chars().next()?;
                    out.push(ch);
                    *pos += ch.len_utf8();
                }
            }
        }
        None
    }

    fn parse_json_value(bytes: &[u8], pos: &mut usize) -> Option<JsonValue> {
        skip_json_ws(bytes, pos);
        match *bytes.get(*pos)? {
            b'{' => {
                *pos += 1;
                let mut members = Vec::new();
                skip_json_ws(bytes, pos);
                if bytes.get(*pos) == Some(&b'}') {
                    *pos += 1;
                    return Some(JsonValue::Object(members));
                }
                loop {
                    skip_json_ws(bytes, pos);
                    let key = parse_json_string(bytes, pos)?;
                    skip_json_ws(bytes, pos);
                    if bytes.get(*pos) != Some(&b':') {
                        return None;
                    }
                    *pos += 1;
                    let value = parse_json_value(bytes, pos)?;
                    members.push((key, value));
                    skip_json_ws(bytes, pos);
                    match bytes.get(*pos) {
                        Some(b',') => *pos += 1,
                        Some(b'}') => {
                            *pos += 1;
                            return Some(JsonValue::Object(members));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                *pos += 1;
                let mut items = Vec::new();
                skip_json_ws(bytes, pos);
                if bytes.get(*pos) == Some(&b']') {
                    *pos += 1;
                    return Some(JsonValue::Array(items));
                }
                loop {
                    items.push(parse_json_value(bytes, pos)?);
                    skip_json_ws(bytes, pos);
                    match bytes.get(*pos) {
                        Some(b',') => *pos += 1,
                        Some(b']') => {
                            *pos += 1;
                            return Some(JsonValue::Array(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => {
                parse_json_string(bytes, pos)?;
                Some(JsonValue::Scalar)
            }
            _ => {
                // number / true / false / null: consume the token.
                let start = *pos;
                while *pos < bytes.len() && !matches!(bytes[*pos], b',' | b'}' | b']') {
                    *pos += 1;
                }
                if *pos == start {
                    return None;
                }
                Some(JsonValue::Scalar)
            }
        }
    }

    /// Strict whole-document parse; trailing garbage is a parse failure.
    fn parse_json(text: &str) -> Option<JsonValue> {
        let bytes = text.as_bytes();
        let mut pos = 0usize;
        let value = parse_json_value(bytes, &mut pos)?;
        skip_json_ws(bytes, &mut pos);
        if pos != bytes.len() {
            return None;
        }
        Some(value)
    }

    fn json_member<'a>(value: &'a JsonValue, key: &str) -> Option<&'a JsonValue> {
        match value {
            JsonValue::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, entry)| entry),
            _ => None,
        }
    }

    fn collect_properties_keys(value: &JsonValue, out: &mut Vec<String>) {
        let Some(JsonValue::Object(properties)) = json_member(value, "properties") else {
            return;
        };
        for (key, _) in properties {
            out.push(key.clone());
        }
    }

    /// The setting keys a VS Code manifest contributes: `contributes.
    /// configuration(.properties)` in both the object and the array form.
    fn json_setting_keys(root: &JsonValue) -> Vec<String> {
        let mut out = Vec::new();
        let Some(contributes) = json_member(root, "contributes") else {
            return out;
        };
        let Some(configuration) = json_member(contributes, "configuration") else {
            return out;
        };
        match configuration {
            JsonValue::Object(_) => collect_properties_keys(configuration, &mut out),
            JsonValue::Array(items) => {
                for item in items {
                    collect_properties_keys(item, &mut out);
                }
            }
            JsonValue::Scalar => {}
        }
        out
    }

    /// `name=` / `key=` attribute values of an XML settings manifest
    /// (plugin.xml `option name="..."`, setting/config elements).
    fn xml_setting_attribute_values(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for prefix in ["name=\"", "key=\"", "name='", "key='"] {
            let quote = prefix.chars().last().unwrap_or('"');
            let mut pos = 0usize;
            while let Some(relative) = text[pos..].find(prefix) {
                let start = pos + relative + prefix.len();
                let Some(end) = text[start..].find(quote) else {
                    break;
                };
                out.push(text[start..start + end].to_string());
                pos = start + end + 1;
            }
        }
        out
    }

    /// Secret-shaped settings of one manifest. An unparseable JSON manifest is
    /// an OFFENDER (fail-closed): a malformed/hostile manifest must never pass
    /// silently just because the scan could not read it.
    fn secret_setting_offenders(rel: &str) -> Vec<String> {
        let rel = normalize_rel(rel);
        let path = repo_root().join(&rel);
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let keys = if name == "plugin.xml" {
            xml_setting_attribute_values(&text)
        } else {
            match parse_json(&text) {
                Some(root) => json_setting_keys(&root),
                None => {
                    return vec![format!(
                        "{rel}: settings manifest is not parseable JSON (fail-closed: a malformed \
                         manifest may hide a secret-shaped setting)"
                    )];
                }
            }
        };
        keys.into_iter()
            .filter(|key| is_secret_shaped_setting_key(key))
            .filter(|key| !secret_setting_allowlisted(&rel, key))
            .map(|key| {
                format!(
                    "{rel}: secret-shaped product setting {key:?} — credentials must live in the \
                     OS/IDE secret store (vscode.SecretStorage / JetBrains PasswordSafe), never in \
                     a product setting"
                )
            })
            .collect()
    }

    /// The real-tree invariant: every settings manifest under `apps/` declares
    /// no secret-shaped key outside the documented allowlist.
    #[test]
    fn apps_never_declare_secret_shaped_product_settings() {
        let manifests = walk_setting_manifests();
        let mut offenders = Vec::new();
        for rel in &manifests {
            offenders.extend(secret_setting_offenders(rel));
        }
        assert_no_offenders(
            "secret-shaped product settings: settings manifests (apps/**/package.json, plugin.xml, \
             *.schema.json, settings.json) must never declare *Token/*Secret/*ApiKey/*Credential/\
             *Password/provider keys; credentials belong in the OS/IDE secret store",
            &offenders,
            manifests.len(),
            2,
        );
        assert!(
            manifests
                .iter()
                .any(|rel| rel == "apps/vscode/package.json"),
            "the VS Code settings manifest must be walked (walk: {manifests:?})"
        );
        assert!(
            manifests.iter().any(|rel| rel.ends_with("plugin.xml")),
            "the JetBrains settings manifest must be walked (walk: {manifests:?})"
        );
    }

    /// The allowlist stays tight: every entry is exact, justified, declared in
    /// the real manifest, matches the raw matcher (load-bearing) and does not
    /// leak into other keys.
    #[test]
    fn secret_setting_allowlist_entries_are_documented_and_load_bearing() {
        assert!(
            SECRET_SETTING_ALLOWLIST.len() <= 2,
            "the secret-settings allowlist must stay tiny ({} entries)",
            SECRET_SETTING_ALLOWLIST.len()
        );
        for (file, key, justification) in SECRET_SETTING_ALLOWLIST {
            assert!(
                justification.len() >= 40,
                "allowlist entry {file} {key} needs a real written justification"
            );
            assert!(
                is_secret_shaped_setting_key(key),
                "allowlist entry {file} {key} must be load-bearing (it must match the raw matcher)"
            );
            let path = repo_root().join(file);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("allowlisted manifest missing: {file}"));
            let declared = if file.ends_with("plugin.xml") {
                xml_setting_attribute_values(&text)
            } else {
                parse_json(&text)
                    .map(|root| json_setting_keys(&root))
                    .unwrap_or_default()
            };
            assert!(
                declared.iter().any(|entry| entry == key),
                "allowlist entry {file} {key} is stale: the key is not declared there"
            );
            assert!(
                secret_setting_offenders(file)
                    .iter()
                    .all(|offender| !offender.contains(&format!("{key:?}"))),
                "the allowlisted key must be the ONLY reason it does not appear as an offender"
            );
            assert!(
                !secret_setting_allowlisted(file, "faktor.someApiKey"),
                "the allowlist may never widen to a different key in {file}"
            );
        }
    }

    /// Planted-failure proof: a planted `faktor.someApiKey` setting (object and
    /// array manifest forms, plus a plugin.xml attribute) is flagged, and a
    /// safe control manifest is not.
    #[test]
    fn planted_api_key_setting_fails_the_scan() {
        let dir = repo_root()
            .join("target/certification")
            .join(format!("secret-setting-selfcheck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("planted settings dir");
        // Assembled at runtime so this scanner's own source stays clean.
        let key = ["faktor", "some", "Api", "Key"].join(".");
        let rel_of = |path: &Path| {
            normalize_rel(
                &path
                    .strip_prefix(repo_root())
                    .unwrap_or(path)
                    .display()
                    .to_string(),
            )
        };

        // Object-form VS Code manifest.
        let object_form = dir.join("object/package.json");
        std::fs::create_dir_all(object_form.parent().expect("parent")).expect("planted dir");
        std::fs::write(
            &object_form,
            format!(
                "{{\"name\":\"planted\",\"contributes\":{{\"configuration\":{{\"properties\":\
                 {{\"{key}\":{{\"type\":\"string\"}}}}}}}}}}"
            ),
        )
        .expect("planted object manifest");
        let offenders = secret_setting_offenders(&rel_of(&object_form));
        assert_eq!(
            offenders.len(),
            1,
            "the planted {key} setting must fail the scan: {offenders:?}"
        );
        assert!(
            offenders[0].contains(&key),
            "the offender must name the planted key: {}",
            offenders[0]
        );

        // Array-form VS Code manifest.
        let array_form = dir.join("array/package.json");
        std::fs::create_dir_all(array_form.parent().expect("parent")).expect("planted dir");
        std::fs::write(
            &array_form,
            format!(
                "{{\"name\":\"planted\",\"contributes\":{{\"configuration\":[{{\"title\":\"f\",\
                 \"properties\":{{\"{key}\":{{\"type\":\"string\"}}}}}}]}}}}"
            ),
        )
        .expect("planted array manifest");
        assert_eq!(
            secret_setting_offenders(&rel_of(&array_form)).len(),
            1,
            "the array manifest form must be scanned"
        );

        // plugin.xml attribute form.
        let xml_form = dir.join("xml/plugin.xml");
        std::fs::create_dir_all(xml_form.parent().expect("parent")).expect("planted dir");
        std::fs::write(
            &xml_form,
            format!("<idea-plugin><component><option name=\"{key}\" value=\"x\"/></component></idea-plugin>"),
        )
        .expect("planted plugin.xml");
        assert_eq!(
            secret_setting_offenders(&rel_of(&xml_form)).len(),
            1,
            "the plugin.xml attribute form must be scanned"
        );

        // Safe control: non-secret settings and non-configuration members pass.
        let safe = dir.join("safe/package.json");
        std::fs::create_dir_all(safe.parent().expect("parent")).expect("planted dir");
        std::fs::write(
            &safe,
            "{\"name\":\"safe\",\"contributes\":{\"configuration\":{\"properties\":{\
             \"faktor.binaryPath\":{\"type\":\"string\"},\"faktor.controlPlaneEndpoint\":{\"type\":\"string\"},\
             \"faktor.mutationMode\":{\"type\":\"string\"}}}}}",
        )
        .expect("safe manifest");
        assert!(
            secret_setting_offenders(&rel_of(&safe)).is_empty(),
            "non-secret settings must pass"
        );

        // A malformed manifest is an offender (fail-closed), never a silent pass.
        let malformed = dir.join("malformed/package.json");
        std::fs::create_dir_all(malformed.parent().expect("parent")).expect("planted dir");
        std::fs::write(&malformed, "{\"contributes\": {\"configuration\": ")
            .expect("malformed manifest");
        assert_eq!(
            secret_setting_offenders(&rel_of(&malformed)).len(),
            1,
            "an unparseable manifest must fail closed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The raw matcher's adversarial matrix: every required family fires and
    /// the documented non-secret shapes pass.
    #[test]
    fn secret_setting_matcher_flags_every_required_family() {
        for key in [
            "faktor.controlToken",
            "some_api_key",
            "accessSecret",
            "llmCredential",
            "daemonPassword",
            "openaiKey",
            "anthropicApiKey",
            "provider_token",
            "clientSecret",
            "privateKey",
            "accessKey",
            "FAKTOR_APIKEY",
        ] {
            assert!(
                is_secret_shaped_setting_key(key),
                "{key:?} must be flagged as secret-shaped"
            );
        }
        for key in [
            "faktor.binaryPath",
            "faktor.dataDir",
            "faktor.installRoot",
            "faktor.extraArgs",
            "faktor.defaultProvider",
            "faktor.defaultModel",
            "faktor.mutationMode",
            "faktor.budgetTokens",
            "faktor.controlPlaneEndpoint",
            "faktor.controlPlaneOrganization",
            "tokenBudget",
            "credentialsPath",
            "secretiveNotes",
            "apiKeyRotationDays",
            "",
        ] {
            assert!(
                !is_secret_shaped_setting_key(key),
                "{key:?} is not a credential and must pass"
            );
        }
    }

    #[test]
    fn cfg_stripper_removes_every_test_module_shape() {
        // Synthetic files: markers in test modules, raw strings, docs,
        // cfg(all(test, unix)) blocks must never survive as production.
        let src = r##"
//! docs with #[cfg(test)] and std::process::Command mentions
use std::process::Command as C;
fn production_spawn() { let _c = Command::new("x"); }
#[cfg(all(test, unix))]
mod unix_tests {
    fn spawn() { let _ = std::process::Command::new("t"); }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() { let _ = std::process::Command::new("t"); }
    const RAW: &str = r#"std::process::Command fake"#;
}
#[cfg(test)]
use std::sync::OnceLock;
#[cfg(test)]
static SEAM: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
fn after() { let _ = C::new("y"); }
"##;
        let code = code_mask(src);
        let kept = kept_ranges(src, &code);
        let mut prod = String::new();
        for (a, z) in kept {
            prod.push_str(&src[a..z]);
        }
        assert!(prod.contains("fn production_spawn"));
        assert!(prod.contains("fn after"));
        assert!(
            !prod.contains("mod unix_tests") && !prod.contains("mod tests"),
            "test modules leaked into production text:\n{prod}"
        );
        assert!(
            !prod.contains("std::process::Command::new(\"t\")"),
            "markers in tests leaked"
        );
        assert!(!prod.contains("OnceLock"), "cfg(test) items leaked");
        // Raw-string and doc-comment mentions are masked (not code).
        assert!(code_mask(r##"let s = r#"std::process::Command"#;"##)
            .iter()
            .any(|c| !c));
    }

    #[test]
    fn cfg_stripper_keeps_cfg_platform_blocks() {
        // cfg(unix)/cfg(not(unix))/cfg(any(test, unix)) production shapes
        // must NOT be stripped, and pure `cfg(any(test, unix))` items that
        // also exist on unix production are kept (conservative direction).
        let src = r##"
#[cfg(unix)]
fn unix_alive() { let _ = std::process::Command::new("ps"); }
#[cfg(not(unix))]
fn win_alive() { let _ = std::process::Command::new("tasklist"); }
#[cfg(any(test, unix))]
fn both() {}
#[cfg(not(test))]
fn prod_only() {}
"##;
        let code = code_mask(src);
        let kept = kept_ranges(src, &code);
        let prod: String = kept.iter().map(|(a, z)| &src[*a..*z]).collect();
        for needle in ["unix_alive", "win_alive", "both", "prod_only"] {
            assert!(prod.contains(needle), "{needle} must survive");
        }
    }

    #[test]
    fn scan_floors_guard_against_an_empty_walk() {
        // The scans assert a minimum file count; prove the walk finds the
        // real tree (a silently skipped tree would pass vacuously).
        let files = walk_crate_sources();
        assert!(files.len() >= 100, "walk too small: {}", files.len());
        assert!(files.contains(&"crates/security/src/lib.rs".to_string()));
        assert!(files.contains(&"crates/provider/src/egress.rs".to_string()));
        assert!(
            files
                .iter()
                .all(|f| !f.starts_with("crates/") || f.contains("/src/")),
            "only crate src trees may certify production code"
        );
    }

    #[test]
    fn fixed_residual_sites_carry_zero_atomic_allowlist_entries() {
        // The index generation publish and the CLI backup finalize route
        // through crates/fs/src/atomic.rs: ANY allowlist entry naming them
        // would silently re-open the hand-rolled rename residual. The scan
        // is also asserted non-vacuous (it really sees new renames in those
        // files).
        //
        // The ONLY documented grandfathers left are the CAS store, the fs
        // crate's internal stream copy, the git worktree metadata save, the
        // git stale-lease reconcile claim (git-internal metadata, never
        // workspace content) and the fs crate's canonical entry-state
        // landing primitive (symlink/mode-aware, parent-fsync via
        // crate::atomic); a new file appearing here is a red test, never a
        // review nit.
        const DOCUMENTED_GRANDFATHERS: &[&str] = &[
            "crates/cas/src/lib.rs",
            "crates/fs/src/lib.rs",
            "crates/fs/src/entry_state.rs",
            "crates/git/src/lib.rs",
            "crates/git/src/guard.rs",
        ];
        for (rel, text) in ATOMIC_ALLOWLIST {
            assert!(
                DOCUMENTED_GRANDFATHERS.contains(rel),
                "undocumented atomic-write allowlist entry {rel}: {text:?}"
            );
            assert_ne!(
                *rel, "crates/index/src/service.rs",
                "index publish must not be allowlisted again: {text:?}"
            );
            assert_ne!(
                *rel, "crates/cli/src/main.rs",
                "backup finalize must not be allowlisted again: {text:?}"
            );
        }
        let f = synthetic_file(
            "crates/index/src/service.rs",
            "fn p() { fs::rename(&scratch, &gen_path).unwrap(); }\n",
        );
        assert!(
            !atomic_write_offenders(&f).is_empty(),
            "a hand-rolled publish rename in the index service must still fire"
        );
        let f = synthetic_file(
            "crates/cli/src/main.rs",
            "fn p() { std::fs::rename(&tmp, &dest).unwrap(); }\n",
        );
        assert!(
            !atomic_write_offenders(&f).is_empty(),
            "a hand-rolled backup finalize rename in the CLI must still fire"
        );
    }

    #[test]
    fn allowlist_lines_that_no_longer_exist_are_dead_entries() {
        // Grandfathered allowlist entries must keep matching real lines:
        // a dead entry means the sequence was removed or moved, and the
        // entry should be cleaned up or the move audited.
        for (rel, text) in ATOMIC_ALLOWLIST {
            let root = repo_root();
            let src = std::fs::read_to_string(root.join(rel))
                .unwrap_or_else(|_| panic!("allowlisted file missing: {rel}"));
            let found = src.lines().any(|l| l.trim() == *text);
            assert!(
                found,
                "stale atomic-write allowlist entry {rel}: {text:?} — the grandfathered \
                 sequence moved or was removed; update the allowlist"
            );
        }
    }

    /// Synthetic-file negative tests: prove each scan FIRES on a violation
    /// (the machinery is not vacuously green).
    fn synthetic_file(rel: &str, src: &str) -> File<'static> {
        let code = code_mask(src);
        let kept = kept_ranges(src, &code);
        File {
            rel: rel.to_string(),
            src: leak(src),
            code,
            kept,
        }
    }

    #[test]
    fn spawn_scan_fires_on_a_violating_crate() {
        const MARKERS: &[&str] = &[
            "std::process::Command",
            "tokio::process::Command",
            "process::Stdio",
            "Command::new",
            "Command::spawn",
            "CommandExt",
        ];
        let f = synthetic_file(
            "crates/git/src/lib.rs",
            "fn run() { let c = std::process::Command::new(\"git\"); }\n",
        );
        let hits = find_markers(&f, MARKERS);
        assert!(!hits.is_empty(), "a production spawn must be flagged");
        assert!(
            !production_spawn_offenders("crates/git/src/lib.rs", &f).is_empty(),
            "the production scan must fail on a real spawn"
        );
        // The sanctioned homes are the only silent files.
        for allowed in ["crates/terminal/src/lib.rs", "crates/pty/src/unix.rs"] {
            let f = synthetic_file(
                allowed,
                "fn run() { let _ = std::process::Command::new(\"x\"); }\n",
            );
            assert!(
                !find_markers(&f, MARKERS).is_empty(),
                "marker detection must still work"
            );
            assert!(
                production_spawn_offenders(allowed, &f).is_empty(),
                "{allowed} is a sanctioned home"
            );
        }
        // A spawn buried in a #[cfg(test)] module must NOT fire.
        let f = synthetic_file(
            "crates/git/src/lib.rs",
            "fn ok() {}\n#[cfg(test)] mod tests {\n  fn t() { let _ = std::process::Command::new(\"x\"); }\n}\n",
        );
        assert!(find_markers(&f, MARKERS).is_empty());
    }

    #[test]
    fn separator_normalization_makes_windows_rels_scan_identically() {
        // Windows runners produce `crates\x\src\lib.rs` rels; every
        // exemption and the production violation must behave exactly as
        // with `/` (the 2026-09 Windows-runner failure).
        let f = synthetic_file(
            "crates/x/src/lib.rs",
            "fn run() { let c = std::process::Command::new(\"git\"); }\n",
        );
        let offenders = production_spawn_offenders("crates\\x\\src\\lib.rs", &f);
        assert!(
            offenders
                .iter()
                .any(|o| o.contains("crates/x/src/lib.rs") && o.contains("std::process::Command")),
            "a real production spawn must still fire through a Windows rel: {offenders:?}"
        );
        for exempt in ["crates\\terminal\\src\\lib.rs", "crates\\pty\\src\\unix.rs"] {
            let f = synthetic_file(
                exempt,
                "fn run() { let _ = std::process::Command::new(\"x\"); }\n",
            );
            assert!(
                production_spawn_offenders(exempt, &f).is_empty(),
                "{exempt} must stay exempt under Windows separators"
            );
        }
        // The via-#[path] out-of-line test files are recognized under the
        // Windows spelling too (their real cfg(test) sibling includes are
        // verified on disk).
        assert!(is_test_file("crates\\orchestrator\\src\\shadow_tests.rs"));
        assert!(is_test_file("crates\\acp\\src\\tests.rs"));
        assert!(!is_test_file("crates\\orchestrator\\src\\shadow.rs"));
        assert!(!is_test_file("crates\\git\\src\\plain.rs"));
        // A merely test-NAMED file whose include is not test-gated stays
        // scanned (a name alone never exempts production code).
        let f = synthetic_file(
            "crates/git/src/thing_tests.rs",
            "fn run() { let _ = std::process::Command::new(\"git\"); }\n",
        );
        assert!(
            !production_spawn_offenders("crates\\git\\src\\thing_tests.rs", &f).is_empty(),
            "a production-compiled *_tests.rs file must stay scanned"
        );
        assert_eq!(normalize_rel("crates/x/src/lib.rs"), "crates/x/src/lib.rs");
    }

    #[test]
    fn reqwest_scan_fires_on_a_violating_adapter() {
        let f = synthetic_file(
            "crates/openai/src/lib.rs",
            "use reqwest::Client;\npub fn send() { let c = Client::new(); let _ = c.execute(r); }\n",
        );
        let has_reqwest = !find_markers(&f, &["reqwest"]).is_empty();
        assert!(has_reqwest);
        let hits = find_markers(&f, REQWEST_CLIENT_MARKERS);
        assert!(
            hits.iter()
                .any(|(_, t)| t.contains("Client::new") || t.contains(".execute("))
                || !reqwest_client_offenders(&f).is_empty(),
            "raw client code in an adapter must be flagged: {hits:?}"
        );
        // The bare-import escape: `use reqwest::Client;` + `Client::new()`
        // with NO `.execute(`/`reqwest::Client` spelling anywhere.
        let f = synthetic_file(
            "crates/openai/src/lib.rs",
            "use reqwest::Client;\nfn send() -> Client { Client::new() }\n",
        );
        let offenders = reqwest_client_offenders(&f);
        assert_eq!(
            offenders.len(),
            2,
            "the import line and the bare construction both fire: {offenders:?}"
        );
        assert!(
            offenders.iter().any(|o| o.contains("bare Client")),
            "the bare construction must be named: {offenders:?}"
        );
        // The WILDCARD-import escape (the audit blind spot): `use reqwest::*;`
        // plus a bare `Client::new()` must fire.
        let f = synthetic_file(
            "crates/openai/src/lib.rs",
            "use reqwest::*;\nfn send() { let c = Client::new(); let _ = c.get(\"u\"); }\n",
        );
        let offenders = reqwest_client_offenders(&f);
        assert_eq!(
            offenders.len(),
            1,
            "`use reqwest::*; Client::new()` must fire: {offenders:?}"
        );
        assert!(offenders[0].contains("bare Client"), "{offenders:?}");
        // A braced import that brings Client in unqualified fires too.
        let f = synthetic_file(
            "crates/openai/src/lib.rs",
            "use reqwest::{Client, RequestBuilder};\nfn b() -> Client { Client::builder().build().unwrap() }\n",
        );
        assert!(
            reqwest_client_offenders(&f)
                .iter()
                .any(|o| o.contains("bare Client")),
            "a braced `Client` import plus bare construction must fire"
        );
        // Compliant shapes pass: a glob import that never constructs a
        // client, a same-named local type (`MyClient`), and a non-reqwest
        // `Client`.
        for compliant in [
            "use reqwest::*;\nfn f() { let _ = reqwest::Url::parse(\"https://x\"); }\n",
            "use reqwest::*;\nfn f() { let c = MyClient::new(); let _ = c; }\n",
            "use other::*;\nfn f() { let c = Client::new(); let _ = c; }\n",
        ] {
            let f = synthetic_file("crates/openai/src/lib.rs", compliant);
            assert!(
                reqwest_client_offenders(&f).is_empty(),
                "compliant shape must pass: {compliant:?} -> {:?}",
                reqwest_client_offenders(&f)
            );
        }
        // A file whose production text never mentions reqwest is not gated.
        let f = synthetic_file(
            "crates/store/src/lib.rs",
            "fn q() { conn.execute(\"SELECT 1\", []).unwrap(); }\n",
        );
        assert!(find_markers(&f, &["reqwest"]).is_empty());
    }

    #[test]
    fn atomic_scan_fires_on_the_write_then_rename_regression_fixture() {
        // The audit regression fixture: a temp-path write followed by a
        // rename, with NO fsync call anywhere. The fsync is not the
        // detection signal; both patterns are independently forbidden.
        let fixture = "fn bad(){std::fs::write(\".thing.tmp\",b\"x\"); \
                       std::fs::rename(\".thing.tmp\",\"thing\");}\n";
        let f = synthetic_file("crates/git/src/lib.rs", fixture);
        let offenders = atomic_write_offenders(&f);
        assert!(
            !offenders.is_empty(),
            "the write-then-rename fixture must fail the scan"
        );
        assert!(
            offenders.iter().any(|o| o.contains("std::fs::rename")),
            "the rename must be listed: {offenders:?}"
        );
        // The temp-path write alone fires too (no rename needed).
        let f = synthetic_file(
            "crates/agent/src/lib.rs",
            "fn w() { std::fs::write(\"cache.tmp\", b\"x\").unwrap(); }\n",
        );
        assert!(
            !atomic_write_offenders(&f).is_empty(),
            "a temp-path write with no rename/fsync must be listed"
        );
        // A rename with no temp path is independently forbidden.
        let f = synthetic_file(
            "crates/agent/src/lib.rs",
            "fn m() { std::fs::rename(\"a\", \"b\").unwrap(); }\n",
        );
        assert!(
            !atomic_write_offenders(&f).is_empty(),
            "a rename with no temp path must be listed independently"
        );
        // NamedTempFile::persist (instance form) is independently forbidden.
        let f = synthetic_file(
            "crates/agent/src/lib.rs",
            "fn p() { NamedTempFile::new()?.persist(\"out\").unwrap(); }\n",
        );
        assert!(
            !atomic_write_offenders(&f).is_empty(),
            "NamedTempFile persist must be listed independently"
        );
        // False-positive regression (the CAS streaming put, commit b18eec4):
        // reopening an fsynced temp read+WRITE performs NO mutation — the
        // write access exists solely for the Windows FlushFileBuffers. The
        // access-mode flags are not a write, so the line must be accepted
        // WITHOUT any allowlist entry.
        for src in [
            "fn s(tmp: &std::path::Path) -> std::io::Result<()> { \
             let f = fs::OpenOptions::new().read(true).write(true).open(&tmp)?; \
             f.sync_all() }\n",
            "fn s(tmp: &std::path::Path) -> std::io::Result<()> { \
             fs::OpenOptions::new().write(true).open(&tmp)?.sync_all() }\n",
        ] {
            let f = synthetic_file("crates/cas/src/lib.rs", src);
            assert!(
                atomic_write_offenders(&f).is_empty(),
                "an OpenOptions reopen with no mutation call is a durability \
                 flush, never an atomic-write sequence: {:?}",
                atomic_write_offenders(&f)
            );
        }
        // The mutation still fires: an OpenOptions sequence that CREATES the
        // temp is an offender (and the mutation requirement never weakens
        // the plain write/rename/persist patterns above).
        let f = synthetic_file(
            "crates/agent/src/lib.rs",
            "fn c(tmp: &std::path::Path) -> std::io::Result<()> { \
             let f = fs::OpenOptions::new().write(true).create(true).open(&tmp)?; \
             Ok(()) }\n",
        );
        assert!(
            !atomic_write_offenders(&f).is_empty(),
            "create(true) on a temp path must be listed"
        );
        // A plain non-temp write is not an atomic-write sequence.
        let f = synthetic_file(
            "crates/agent/src/lib.rs",
            "fn w() { std::fs::write(\"cache.bin\", b\"x\").unwrap(); }\n",
        );
        assert!(atomic_write_offenders(&f).is_empty());
        // The exact allowlist excuses the grandfathered line only.
        let f = synthetic_file("crates/cas/src/lib.rs", "match fs::rename(tmp, path) {\n");
        assert!(
            atomic_write_offenders(&f).is_empty(),
            "the exact allowlisted line must pass"
        );
    }

    #[test]
    fn sync_read_scan_fires_on_sync_reads_and_allows_the_async_wrappers() {
        // A sync SessionHandle-style read (no await) fires.
        let f = synthetic_file(
            "crates/agent/src/runtime.rs",
            "fn t(h: &H) { let _ = h.messages_backwards_bounded(None, 4, 64).unwrap(); }\n",
        );
        let hits = agent_sync_store_read_offenders(&f);
        assert!(
            hits.iter()
                .any(|h| h.contains("messages_backwards_bounded")),
            "an un-awaited window read must be flagged: {hits:?}"
        );
        // The manager async wrapper (awaited) passes.
        let f = synthetic_file(
            "crates/agent/src/runtime.rs",
            "async fn t(m: &M) { let _ = m.messages_backwards_bounded(1, None, 4, 64).await.unwrap(); }\n",
        );
        assert!(
            agent_sync_store_read_offenders(&f).is_empty(),
            "the awaited manager wrapper must pass"
        );
        // Direct sync store reads fire for every rejected shape.
        for (src, needle) in [
            (
                "fn t() { let _ = self.deps.session.store().get_task(s, t); }\n",
                "get_task",
            ),
            (
                "fn t() { let _ = self.deps.session.store().cost_task_row(s, t); }\n",
                "cost_task_row",
            ),
            (
                "fn t() { let _ = self.deps.session.store().provider_call_prefix_rows(s); }\n",
                "provider_call_prefix_rows",
            ),
        ] {
            let f = synthetic_file("crates/agent/src/runtime.rs", src);
            let hits = agent_sync_store_read_offenders(&f);
            assert!(
                hits.iter().any(|h| h.contains(needle)),
                "{needle} must be flagged: {hits:?}"
            );
        }
        // A read buried in a #[cfg(test)] module must NOT fire.
        let f = synthetic_file(
            "crates/agent/src/runtime.rs",
            "#[cfg(test)] mod tests {\n  fn t() { let _ = h.messages_backwards_bounded(None, 1, 1); }\n}\n",
        );
        assert!(agent_sync_store_read_offenders(&f).is_empty());
    }

    #[test]
    fn constructor_marker_scan_ignores_test_gated_mentions() {
        for marker in [
            "DurableEvidenceAuthority::for_store",
            "DurableBudgetLedger::new",
            "SemanticProviderRegistry::new",
            "ProcessSupervisor::new",
        ] {
            let f = synthetic_file(
                "crates/agent/src/runtime.rs",
                &format!("fn production() {{ let _ = {marker}(a, b); }}\n"),
            );
            assert!(
                !find_markers(&f, &[marker]).is_empty(),
                "{marker} must be detected in production text"
            );
            let f = synthetic_file(
                "crates/agent/src/runtime.rs",
                &format!("#[cfg(test)] mod tests {{\n  fn t() {{ let _ = {marker}(a, b); }}\n}}\n"),
            );
            assert!(
                find_markers(&f, &[marker]).is_empty(),
                "{marker} in a test module must never certify or violate production"
            );
        }
    }

    #[test]
    fn indirect_spawn_scan_fires_on_lower_then_spawn_and_passes_the_supervisor_seam() {
        let f = synthetic_file(
            "crates/cli/src/tools.rs",
            "fn run(spec: CommandSpec) -> std::io::Result<()> {\n  let resolved = spec.lower()?;\n  let mut c = std::process::Command::new(resolved.program);\n  c.args(resolved.args);\n  c.spawn()?;\n  Ok(())\n}\n",
        );
        let hits = indirect_resolved_spawn_offenders(&f);
        assert!(
            !hits.is_empty(),
            "lower-then-spawn must be flagged: {hits:?}"
        );
        // Lowering and handing program/argv to the supervisor seam (no
        // process-Command anywhere) is the sanctioned shape.
        let f = synthetic_file(
            "crates/cli/src/tools.rs",
            "fn run(spec: CommandSpec, sup: &ProcessSupervisor) {\n  let resolved = spec.lower()?;\n  let _ = sup.spawn(SpawnConfig { cmd: resolved.program, args: resolved.args });\n}\n",
        );
        assert!(indirect_resolved_spawn_offenders(&f).is_empty());
        // Lowering alone (no spawn marker) never fires.
        let f = synthetic_file(
            "crates/git/src/lib.rs",
            "fn lower(spec: CommandSpec) { let _ = spec.lower(); }\n",
        );
        assert!(indirect_resolved_spawn_offenders(&f).is_empty());
        // The helper detects the shape anywhere; the scan TEST excludes the
        // terminal/pty homes (the supervisor owns its process layer).
        let f = synthetic_file(
            "crates/terminal/src/lib.rs",
            "fn spawn(cmd: ResolvedCommand) { let _ = std::process::Command::new(cmd.program); }\n",
        );
        assert!(!indirect_resolved_spawn_offenders(&f).is_empty());
    }

    // ------------------------------------------------------------------
    // scan: authority-path lock poison discipline
    // ------------------------------------------------------------------

    /// Production files whose locks sit on the authority path (fs registries,
    /// session ownership registries, permissions-adjacent caches, MCP/LSP
    /// process ownership, search/router caches, PTY rings, hooks, git lock
    /// maps). Raw `.lock().unwrap()` / `.lock().expect(..)` is forbidden on
    /// every one: each lock carries a classified disposition
    /// (cache/ring => rebuild; authority/policy => typed refusal;
    /// ownership/process => reconcile against the durable authority).
    const AUTHORITY_LOCK_FILES: &[&str] = &[
        "crates/fs/src/lib.rs",
        "crates/fs/src/atomic.rs",
        "crates/fs/src/platform/unix.rs",
        "crates/fs/src/platform/windows.rs",
        "crates/session/src/manager.rs",
        "crates/session/src/ops.rs",
        "crates/session/src/process.rs",
        "crates/session/src/artifacts.rs",
        "crates/mcp/src/lib.rs",
        "crates/lsp/src/lib.rs",
        "crates/search/src/lib.rs",
        "crates/router/src/lib.rs",
        "crates/router/src/outcomes.rs",
        "crates/pty/src/ring.rs",
        "crates/pty/src/unix.rs",
        "crates/pty/src/windows.rs",
        "crates/hooks/src/lib.rs",
        "crates/git/src/lib.rs",
        "crates/orchestrator/src/runtime.rs",
        "crates/orchestrator/src/task_executor.rs",
        "crates/orchestrator/src/merge.rs",
        "crates/server/src/permission.rs",
    ];

    /// The TIGHT allowlist: `(rel, exact trimmed line prefix)` pairs that may
    /// remain raw. Deliberately EMPTY — every scanned site is classified.
    const AUTHORITY_LOCK_ALLOWLIST: &[(&str, &str)] = &[];

    /// Raw `unwrap`/`expect` offenders at one file's `.lock()` call sites,
    /// restricted to production (kept) code. `.unwrap_or_else(...)` /
    /// `.unwrap_or(...)` never match: the needle requires the exact call
    /// paren. Multi-line chains (`.lock()` newline `.expect(..)`) are seen
    /// through comments/whitespace.
    fn authority_lock_offenders(rel: &str, f: &File<'_>) -> Vec<String> {
        let mut offenders = Vec::new();
        for at in find_marker_offsets(f, ".lock()") {
            let mut i = at + ".lock()".len();
            let n = f.src.len();
            while i < n && (!f.code[i] || f.src.as_bytes()[i].is_ascii_whitespace()) {
                i += 1;
            }
            for needle in [".unwrap(", ".expect("] {
                if f.src[i..].starts_with(needle) {
                    let line = line_of(f.src, at);
                    let text = trim_line(f.src, at);
                    if AUTHORITY_LOCK_ALLOWLIST
                        .iter()
                        .any(|(r, l)| *r == rel && text.starts_with(l))
                    {
                        continue;
                    }
                    offenders.push(format!(
                        "{rel}:{line}: {text}  [raw {} on an authority-path lock; classify \
                         the site (cache/ring => rebuild, authority/policy => typed \
                         refusal, ownership/process => reconcile)]",
                        needle.trim_start_matches('.').trim_end_matches('(')
                    ));
                }
            }
        }
        offenders.sort();
        offenders.dedup();
        offenders
    }

    #[test]
    fn authority_path_locks_never_raw_unwrap_or_expect() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in AUTHORITY_LOCK_FILES {
            let f = load(rel).unwrap_or_else(|| panic!("authority lock file missing: {rel}"));
            scanned += 1;
            offenders.extend(authority_lock_offenders(rel, &f));
        }
        assert_no_offenders(
            "authority lock poison discipline",
            &offenders,
            scanned,
            AUTHORITY_LOCK_FILES.len(),
        );
    }

    #[test]
    fn authority_lock_scan_detects_raw_and_exempts_classified_shapes() {
        let raw = synthetic_file(
            "crates/fs/src/atomic.rs",
            "fn f(lock: &Mutex<()>) { let _ = lock.lock().unwrap(); }\n",
        );
        assert_eq!(
            authority_lock_offenders("crates/fs/src/atomic.rs", &raw).len(),
            1
        );
        let expect = synthetic_file(
            "crates/fs/src/atomic.rs",
            "fn f(lock: &Mutex<()>) {\n  let _ =\n    lock\n      .lock()\n      .expect(\"poisoned\");\n}\n",
        );
        assert_eq!(
            authority_lock_offenders("crates/fs/src/atomic.rs", &expect).len(),
            1
        );
        let recovered = synthetic_file(
            "crates/fs/src/atomic.rs",
            "fn f(lock: &Mutex<()>) { let _ = lock.lock().unwrap_or_else(|p| p.into_inner()); }\n",
        );
        assert!(authority_lock_offenders("crates/fs/src/atomic.rs", &recovered).is_empty());
        // A raw site inside a #[cfg(test)] module is test code.
        let test_only = synthetic_file(
            "crates/fs/src/atomic.rs",
            "#[cfg(test)] mod tests {\n  fn t(lock: &Mutex<()>) { let _ = lock.lock().unwrap(); }\n}\n",
        );
        assert!(authority_lock_offenders("crates/fs/src/atomic.rs", &test_only).is_empty());
    }

    // ------------------------------------------------------------------
    // scan: canonical BLAKE3 authority digests (no 64-bit FNV folds)
    // ------------------------------------------------------------------

    /// Production files whose identities AUTHORIZE work (verification,
    /// proof reuse, integration, change-set/base-map identity, semantic
    /// fact identity). A 64-bit FNV fold, `stable_list_digest` or
    /// `stable_content_digest` must never appear here: those values could
    /// authorize a reuse or alias two identities under a truncated hash.
    const AUTHORITY_DIGEST_FILES: &[&str] = &[
        "crates/core/src/authority.rs",
        "crates/core/src/state.rs",
        "crates/verify/src/criteria.rs",
        "crates/verify/src/lib.rs",
        "crates/session/src/task.rs",
        "crates/session/src/ledger.rs",
        "crates/orchestrator/src/merge.rs",
        "crates/orchestrator/src/task_executor.rs",
        "crates/orchestrator/src/shadow.rs",
        "crates/memory/src/lib.rs",
        // Included so any NEW FNV fold in the tournament criterion identity
        // is a red scan; the two grandfathered lines are allowlisted below.
        "crates/orchestrator/src/tournament.rs",
    ];

    /// `(rel, exact trimmed line prefix)` allowlist. Deliberately TINY and
    /// justified:
    ///
    /// - `session/src/task.rs`: the pre-v22 `CriterionId::legacy` decoder,
    ///   REQUIRED to keep legacy durable rows readable. It never mints a new
    ///   id (the modern constructor is BLAKE3) and every new authority
    ///   decision refuses the legacy class typed
    ///   (`LegacyAuthorityDigest`).
    /// - `orchestrator/src/tournament.rs`: the pre-existing FNV criterion
    ///   id, outside the authorized change set of this migration; the scan
    ///   keeps it from spreading to any other line.
    const AUTHORITY_DIGEST_ALLOWLIST: &[(&str, &str)] = &[
        (
            "crates/session/src/task.rs",
            "const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;",
        ),
        (
            "crates/session/src/task.rs",
            "const PRIME: u64 = 0x0000_0100_0000_01b3;",
        ),
        (
            "crates/orchestrator/src/tournament.rs",
            "let mut hash: u64 = 0xcbf2_9ce4_8422_2325;",
        ),
        (
            "crates/orchestrator/src/tournament.rs",
            "hash = hash.wrapping_mul(0x0000_0100_0000_01b3);",
        ),
    ];

    const AUTHORITY_DIGEST_MARKERS: &[&str] = &[
        "stable_list_digest",
        "stable_content_digest",
        "fnv1a",
        "0xcbf2_9ce4_8422_2325",
        "0x0000_0100_0000_01b3",
    ];

    /// FNV/stable-list offenders at one authority file's production (kept,
    /// code-masked) positions, with the justified legacy-decode allowlist.
    fn authority_digest_offenders(rel: &str, f: &File<'_>) -> Vec<String> {
        let rel = normalize_rel(rel);
        find_markers(f, AUTHORITY_DIGEST_MARKERS)
            .into_iter()
            .filter(|(_, text)| {
                !AUTHORITY_DIGEST_ALLOWLIST
                    .iter()
                    .any(|(file, prefix)| *file == rel.as_str() && text.starts_with(prefix))
            })
            .map(|(line, text)| format!("{rel}:{line}: {text}"))
            .collect()
    }

    #[test]
    fn authority_files_never_fold_fnv_or_stable_list_digests() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in AUTHORITY_DIGEST_FILES {
            let f = load(rel).unwrap_or_else(|| panic!("authority digest file missing: {rel}"));
            scanned += 1;
            offenders.extend(authority_digest_offenders(rel, &f));
        }
        assert_no_offenders(
            "canonical BLAKE3 authority digests",
            &offenders,
            scanned,
            AUTHORITY_DIGEST_FILES.len(),
        );
    }

    #[test]
    fn authority_digest_scan_detects_fnv_and_stable_list_markers() {
        let fnv = synthetic_file(
            "crates/memory/src/lib.rs",
            "fn f() { let x = 0xcbf2_9ce4_8422_2325u64; let _ = x; }\n",
        );
        assert_eq!(
            authority_digest_offenders("crates/memory/src/lib.rs", &fnv).len(),
            1
        );
        let allowlisted = synthetic_file(
            "crates/session/src/task.rs",
            "        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;\n",
        );
        assert!(
            authority_digest_offenders("crates/session/src/task.rs", &allowlisted).is_empty(),
            "the justified legacy-decode line is allowlisted"
        );
        let list = synthetic_file(
            "crates/verify/src/criteria.rs",
            "fn f(items: &[String]) -> String { stable_list_digest(items) }\n",
        );
        assert_eq!(
            authority_digest_offenders("crates/verify/src/criteria.rs", &list).len(),
            1
        );
        let content = synthetic_file(
            "crates/orchestrator/src/merge.rs",
            "fn f(items: &[String]) -> String { stable_content_digest(items) }\n",
        );
        assert_eq!(
            authority_digest_offenders("crates/orchestrator/src/merge.rs", &content).len(),
            1
        );
        // A doc/string mention can never count as an implementation.
        let doc = synthetic_file(
            "crates/memory/src/lib.rs",
            "//! stable_list_digest is retired here\nconst X: &str = \"fnv1a\";\n",
        );
        assert!(authority_digest_offenders("crates/memory/src/lib.rs", &doc).is_empty());
    }

    // ------------------------------------------------------------------
    // scan 9: Faktor Acquire commerce-path static authority
    // (`docs/acquire.md` §2/§3/§16)
    //
    //  * the commerce-path crates, and every workspace crate their NORMAL
    //    dependency closure reaches, must contain no model reasoning
    //    runtime, no model router and no model adapter;
    //  * no commerce-path production source may build an HTTP client or
    //    spawn any child process (HTTP is an injected checked transport,
    //    Chromium is a supervised child in `faktor-browser`);
    //  * no literal Chromium/Chrome binary may be spawned from a
    //    `Command::new` anywhere in production;
    //  * no floating-point type may appear on a price-bearing commerce
    //    source (the `visit_f64`/`visit_f32` rejection guards are the one
    //    allowed mention).
    // ------------------------------------------------------------------

    /// The commerce-path crates whose transitive NORMAL dependency closure
    /// is certified model-free (spec §2/§3).
    const COMMERCE_PATH_CRATES: &[&str] = &[
        "faktor-acquire",
        "faktor-commerce",
        "faktor-commerce-connectors",
        "faktor-browser",
    ];

    /// Model execution, routing and adapter crates that must never be
    /// reachable (directly or transitively) from a commerce path (spec §2).
    const COMMERCE_FORBIDDEN_DEPS: &[&str] = &[
        "faktor-agent",
        "faktor-router",
        "faktor-gateway",
        "faktor-ollama",
        "faktor-deepseek",
        "faktor-openai",
        "faktor-anthropic",
        "faktor-google",
    ];

    /// The cli file that carries the `source_market` gateway and the money
    /// decimal serialization: it is a commerce path too.
    const COMMERCE_TOOL_GATEWAY: &str = "crates/cli/src/tools_market.rs";

    /// Quoted strings inside a manifest fragment.
    fn manifest_quoted_strings(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = src;
        while let Some(start) = rest.find('"') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('"') else {
                break;
            };
            out.push(after[..end].to_string());
            rest = &after[end + 1..];
        }
        out
    }

    /// Comment-stripped manifest line body (Cargo.toml `#` comments).
    fn manifest_body(raw: &str) -> &str {
        raw.split('#').next().unwrap_or(raw)
    }

    /// The `members = [ ... ]` array of the workspace root manifest.
    fn workspace_members(src: &str) -> Vec<String> {
        let Some(at) = src.find("members") else {
            return Vec::new();
        };
        let rest = &src[at..];
        let Some(open) = rest.find('[') else {
            return Vec::new();
        };
        let body = &rest[open + 1..];
        let Some(close) = body.find(']') else {
            return Vec::new();
        };
        manifest_quoted_strings(&body[..close])
    }

    /// The `[package] name = "..."` of one crate manifest.
    fn manifest_package_name(src: &str) -> Option<String> {
        let mut in_package = false;
        for raw in src.lines() {
            let line = manifest_body(raw).trim();
            if line.starts_with('[') {
                in_package = line == "[package]";
                continue;
            }
            if !in_package {
                continue;
            }
            if let Some(rest) = line.strip_prefix("name") {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    return manifest_quoted_strings(rest).into_iter().next();
                }
            }
        }
        None
    }

    /// One dependency edge parsed from a crate manifest.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ManifestDep {
        /// The dependency target: the `package = "..."` rename when
        /// present, else the dependency key (for a `workspace = true` entry
        /// this is the workspace ALIAS, resolved against the root
        /// `[workspace.dependencies]` table before the closure walk).
        target: String,
        /// The `path = "..."` value when the dependency is path-based.
        path: Option<String>,
        /// True when the rename was explicit (`package = "..."`), so a
        /// path lookup must not override it.
        renamed: bool,
        /// True when the entry came from a `dev-dependencies` table.
        dev: bool,
        /// True when the entry inherits from `[workspace.dependencies]`
        /// (`alias.workspace = true` or `{ workspace = true }`): the alias
        /// MUST resolve in the root table (an unresolvable alias is a hard
        /// scan failure, never a silently skipped edge).
        workspace: bool,
    }

    /// Apply one property of a TABLE-form dependency body line
    /// (`[dependencies.foo]` followed by `path = "..."` / `package = "..."`
    /// / `workspace = true`).
    fn apply_dep_property_line(dep: &mut ManifestDep, name: &str, value: &str) {
        let value = value.trim();
        if name == "workspace" {
            dep.workspace = value == "true";
            return;
        }
        if let Some(text) = manifest_quoted_strings(value).into_iter().next() {
            match name {
                "package" => {
                    dep.target = text;
                    dep.renamed = true;
                }
                "path" => dep.path = Some(text),
                _ => {}
            }
        }
    }

    /// Apply one `name = value` property of an INLINE dependency entry
    /// (`{ package = "...", path = "...", workspace = true }`).
    fn apply_dep_property(dep: &mut ManifestDep, name: &str, value: &str) {
        let inline = value.trim().trim_matches(|c| c == '{' || c == '}');
        for entry in inline.split(',') {
            let entry = entry.trim().trim_matches(|c| c == '{' || c == '}').trim();
            let Some((entry_name, entry_value)) = entry.split_once('=') else {
                continue;
            };
            if entry_name.trim() != name {
                continue;
            }
            let entry_value = entry_value.trim();
            if name == "workspace" {
                dep.workspace = entry_value == "true";
                continue;
            }
            if let Some(text) = manifest_quoted_strings(entry_value).into_iter().next() {
                match name {
                    "package" => {
                        dep.target = text;
                        dep.renamed = true;
                    }
                    "path" => dep.path = Some(text),
                    _ => {}
                }
            }
        }
    }

    /// The dependency class of one section path: the index of the FIRST
    /// segment in `{dependencies, build-dependencies, dev-dependencies}`
    /// that forms a well-formed dependency table (`[<class>]` or
    /// `[target.<cfg>.<class>]`). Everything else (`[package]` keys, a
    /// `[dependencies]` mention deeper in an unrelated section) is not a
    /// dependency table.
    fn dependency_class(section: &[String]) -> Option<usize> {
        let at = section.iter().position(|part| {
            matches!(
                part.as_str(),
                "dependencies" | "build-dependencies" | "dev-dependencies"
            )
        })?;
        if at == 0 || (at == 2 && section[0] == "target") {
            Some(at)
        } else {
            None
        }
    }

    /// Parse every dependency-table entry of one crate manifest, including
    /// the TABLE form (`[dependencies.foo]` /
    /// `[target.'cfg(unix)'.dependencies.foo]`, whose `path` / `package` /
    /// `workspace` properties follow as lines) and the dotted workspace
    /// form (`foo.workspace = true`). A comment mention or an unrelated
    /// table entry never counts.
    fn manifest_deps(src: &str) -> Vec<ManifestDep> {
        let mut out: Vec<ManifestDep> = Vec::new();
        let mut section: Vec<String> = Vec::new();
        let mut table_dep: Option<usize> = None;
        for raw in src.lines() {
            let line = manifest_body(raw).trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                section = line
                    .trim_matches(|c| c == '[' || c == ']')
                    .split('.')
                    .map(|part| part.trim().trim_matches('"').trim_matches('\'').to_string())
                    .collect();
                table_dep = None;
                if let Some(at) = dependency_class(&section) {
                    if section.len() > at + 1 {
                        out.push(ManifestDep {
                            target: section[at + 1..].join("."),
                            path: None,
                            renamed: false,
                            dev: section[at] == "dev-dependencies",
                            workspace: false,
                        });
                        table_dep = Some(out.len() - 1);
                    }
                }
                continue;
            }
            let Some(at) = dependency_class(&section) else {
                continue;
            };
            let Some(eq) = line.find('=') else {
                continue;
            };
            let key_raw = line[..eq].trim().trim_matches('"');
            let value = &line[eq + 1..];
            if let Some(index) = table_dep {
                apply_dep_property_line(&mut out[index], key_raw, value);
                continue;
            }
            let (key, workspace) = match key_raw.split_once('.') {
                Some((key, suffix)) if suffix.trim() == "workspace" => (key.trim(), true),
                _ => (key_raw, false),
            };
            let mut dep = ManifestDep {
                target: key.to_string(),
                path: None,
                renamed: false,
                dev: section[at] == "dev-dependencies",
                workspace,
            };
            apply_dep_property(&mut dep, "package", value);
            apply_dep_property(&mut dep, "path", value);
            if !workspace {
                apply_dep_property(&mut dep, "workspace", value);
            }
            out.push(dep);
        }
        out
    }

    /// `alias -> (package_rename, path)` of the root
    /// `[workspace.dependencies]` table. A member's `alias.workspace = true`
    /// entry resolves through this table, so a workspace `package = "..."`
    /// rename can never hide a dependency's real target from the scan.
    fn workspace_dependencies(
        src: &str,
    ) -> std::collections::BTreeMap<String, (Option<String>, Option<String>)> {
        let mut out = std::collections::BTreeMap::new();
        let mut in_table = false;
        for raw in src.lines() {
            let line = manifest_body(raw).trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                in_table =
                    line.trim_matches(|c| c == '[' || c == ']').trim() == "workspace.dependencies";
                continue;
            }
            if !in_table {
                continue;
            }
            let Some(eq) = line.find('=') else {
                continue;
            };
            let alias = line[..eq].trim().trim_matches('"').to_string();
            if alias.is_empty() {
                continue;
            }
            let mut dep = ManifestDep {
                target: alias.clone(),
                path: None,
                renamed: false,
                dev: false,
                workspace: false,
            };
            apply_dep_property(&mut dep, "package", &line[eq + 1..]);
            apply_dep_property(&mut dep, "path", &line[eq + 1..]);
            let package = dep.renamed.then(|| dep.target.clone());
            out.insert(alias, (package, dep.path));
        }
        out
    }

    /// One resolved dependency edge: the package name the scanner must
    /// check, and (when a `path`/workspace-table path exists) the manifest
    /// dir whose OWN dependencies must be walked — inside OR outside the
    /// workspace member list (a non-member path dependency is not exempt).
    struct ResolvedDep {
        name: String,
        dir: Option<std::path::PathBuf>,
    }

    /// Resolve one parsed dependency edge to its real package name and
    /// manifest dir. Workspace-inherited entries resolve through the root
    /// `[workspace.dependencies]` table (module path relative to the ROOT);
    /// a direct `path` is relative to the depending manifest's dir.
    fn resolve_manifest_dep(
        root: &Path,
        manifest_dir: &Path,
        dep: &ManifestDep,
        workspace_deps: &std::collections::BTreeMap<String, (Option<String>, Option<String>)>,
    ) -> ResolvedDep {
        let mut name = dep.target.clone();
        let mut path = dep.path.clone();
        let mut workspace_path = false;
        if dep.workspace {
            let Some((package, table_path)) = workspace_deps.get(&dep.target) else {
                panic!(
                    "{}: `{}` inherits from [workspace.dependencies] but the root table \
                     does not define it; the scan cannot verify the edge",
                    manifest_dir.display(),
                    dep.target
                );
            };
            if let Some(package) = package {
                name = package.clone();
            }
            if path.is_none() {
                path = table_path.clone();
                workspace_path = true;
            }
        }
        let dir = path.map(|path| {
            if workspace_path {
                root.join(path)
            } else {
                manifest_dir.join(path)
            }
        });
        ResolvedDep { name, dir }
    }

    /// The commerce-path dependency closure: production-graph offender
    /// chains that reach a forbidden model crate, dev-dependency offender
    /// chains that reach one without an explicit classification, the number
    /// of visited crates and the visited crate set.
    struct CommerceClosure {
        offenders: Vec<String>,
        dev_offenders: Vec<String>,
        /// Every resolved `(declaring crate, dev-dependency)` edge, so the
        /// classification allowlist can be asserted load-bearing.
        dev_edges: Vec<(String, String)>,
        visited: usize,
        crates: std::collections::BTreeSet<String>,
    }

    /// Walk the NORMAL dependency closure of [`COMMERCE_PATH_CRATES`] from
    /// `root`, following workspace members AND non-member `path`
    /// dependencies, resolving workspace aliases/renames, and classifying
    /// every dev-dependency of every visited package. Fails LOUD on an edge
    /// it cannot resolve (missing workspace alias, unreadable path
    /// manifest, rename mismatch): an unverifiable edge is never silently
    /// skipped, and neither are dev-dependencies.
    fn commerce_dependency_closure_at(root: &Path) -> CommerceClosure {
        let root_manifest = root.join("Cargo.toml");
        let root_src = std::fs::read_to_string(&root_manifest)
            .unwrap_or_else(|e| panic!("{} unreadable: {e}", root_manifest.display()));
        let members = workspace_members(&root_src);
        assert!(!members.is_empty(), "workspace members list parsed");
        let workspace_deps = workspace_dependencies(&root_src);
        let mut packages: std::collections::BTreeMap<String, std::path::PathBuf> =
            std::collections::BTreeMap::new();
        for member in &members {
            let dir = root.join(member);
            let manifest = dir.join("Cargo.toml");
            let src = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{} unreadable: {e}", manifest.display()));
            let name = manifest_package_name(&src)
                .unwrap_or_else(|| panic!("{} has no [package] name", manifest.display()));
            packages.insert(name, dir);
        }
        for crate_name in COMMERCE_PATH_CRATES {
            assert!(
                packages.contains_key(*crate_name),
                "commerce-path crate {crate_name} is not a workspace member; the closure \
                 would silently certify nothing"
            );
        }
        let mut offenders = Vec::new();
        let mut dev_offenders = Vec::new();
        let mut dev_edges = Vec::new();
        let mut visited: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut queue: std::collections::VecDeque<(String, std::path::PathBuf, Vec<String>)> =
            COMMERCE_PATH_CRATES
                .iter()
                .map(|c| {
                    (
                        (*c).to_string(),
                        packages[*c].clone(),
                        vec![(*c).to_string()],
                    )
                })
                .collect();
        while let Some((crate_name, dir, chain)) = queue.pop_front() {
            if !visited.insert(crate_name.clone()) {
                continue;
            }
            let manifest = dir.join("Cargo.toml");
            let src = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{} unreadable: {e}", manifest.display()));
            for dep in manifest_deps(&src) {
                let resolved = resolve_manifest_dep(root, &dir, &dep, &workspace_deps);
                let name = match &resolved.dir {
                    Some(dep_dir) => {
                        let dep_manifest = dep_dir.join("Cargo.toml");
                        let dep_src = std::fs::read_to_string(&dep_manifest).unwrap_or_else(|e| {
                            panic!("path dependency {} unreadable: {e}", dep_manifest.display())
                        });
                        let path_name = manifest_package_name(&dep_src).unwrap_or_else(|| {
                            panic!("{} has no [package] name", dep_manifest.display())
                        });
                        if dep.renamed && path_name != resolved.name {
                            panic!(
                                "{}: path dependency rename {} does not match the package name \
                                 {path_name} in {}",
                                manifest.display(),
                                resolved.name,
                                dep_manifest.display()
                            );
                        }
                        // Register the path package (member or NOT) so its own
                        // dependency edges are walked.
                        packages
                            .entry(path_name.clone())
                            .or_insert_with(|| dep_dir.clone());
                        path_name
                    }
                    None => {
                        if !packages.contains_key(&resolved.name)
                            && resolved.name.starts_with("faktor-")
                        {
                            panic!(
                                "{}: workspace crate {} cannot be resolved (no member and no \
                                 path); the scan refuses to skip it",
                                manifest.display(),
                                resolved.name
                            );
                        }
                        resolved.name
                    }
                };
                let mut next = chain.clone();
                next.push(name.clone());
                if dep.dev {
                    dev_edges.push((crate_name.clone(), name.clone()));
                    let classified = COMMERCE_DEV_DEP_ALLOWLIST.iter().any(
                        |(declaring, dependency, justification)| {
                            *declaring == crate_name.as_str()
                                && *dependency == name.as_str()
                                && !justification.trim().is_empty()
                        },
                    );
                    if COMMERCE_FORBIDDEN_DEPS.contains(&name.as_str()) && !classified {
                        dev_offenders.push(next.join(" -> "));
                    }
                    continue;
                }
                if COMMERCE_FORBIDDEN_DEPS.contains(&name.as_str()) {
                    offenders.push(next.join(" -> "));
                    continue;
                }
                if let Some(next_dir) = packages.get(&name) {
                    queue.push_back((name, next_dir.clone(), next));
                }
            }
        }
        CommerceClosure {
            offenders,
            dev_offenders,
            dev_edges,
            visited: visited.len(),
            crates: visited,
        }
    }

    /// Dev-dependencies of the commerce path that name a forbidden model
    /// crate. EMPTY by construction (a model crate may never be a commerce
    /// dev-dependency edge); any future entry MUST carry a written
    /// justification and is asserted load-bearing by
    /// [`commerce_dev_dependencies_are_classified_or_refused`].
    const COMMERCE_DEV_DEP_ALLOWLIST: &[(&str, &str, &str)] = &[];

    #[test]
    fn commerce_dependency_closure_never_reaches_model_execution_or_adapters() {
        let closure = commerce_dependency_closure_at(&repo_root());
        assert!(
            closure.visited >= 10,
            "commerce dependency closure suspiciously small: {} crates",
            closure.visited
        );
        assert!(
            closure.crates.contains("faktor-core") && closure.crates.contains("faktor-provider"),
            "commerce dependency closure walked nothing (parser failure): {:?}",
            closure.crates
        );
        assert!(
            closure.offenders.is_empty(),
            "commerce dependency authority violations (a model execution/adapter crate is \
             reachable through normal dependencies):\n  {}",
            closure.offenders.join("\n  ")
        );
    }

    #[test]
    fn commerce_dev_dependencies_are_classified_or_refused() {
        let closure = commerce_dependency_closure_at(&repo_root());
        assert!(
            closure.dev_offenders.is_empty(),
            "a commerce-path dev-dependency reaches a model crate without an explicit \
             classification in COMMERCE_DEV_DEP_ALLOWLIST:\n  {}",
            closure.dev_offenders.join("\n  ")
        );
        // Every classified entry must name a REAL dev-dependency edge with a
        // written justification: a stale exemption is a red scan.
        for (declaring, dependency, justification) in COMMERCE_DEV_DEP_ALLOWLIST {
            assert!(
                !justification.trim().is_empty(),
                "{declaring} -> {dependency}: a classification requires a justification"
            );
            assert!(
                closure
                    .dev_edges
                    .iter()
                    .any(|(from, to)| from == declaring && to == dependency),
                "{declaring} -> {dependency}: stale classification (no such dev-dependency edge)"
            );
        }
    }

    #[test]
    fn manifest_dependency_parser_is_rename_section_table_and_path_aware() {
        let src = r#"
[package]
name = "faktor-commerce"

[dependencies]
faktor-core.workspace = true
serde = { version = "1", features = ["derive"] }
sneaky = { package = "faktor-agent", path = "../agent" }
client = { path = "../provider" }

[dev-dependencies]
faktor-openai.workspace = true
tokio = { workspace = true }

[target.'cfg(unix)'.dependencies]
faktor-router = { path = "../router" }

[target.'cfg(unix)'.dev-dependencies]
faktor-ollama = { path = "../ollama" }

[target.'cfg(windows)'.dependencies.winjob]
path = "../winjob"

[dependencies.agent]
package = "faktor-agent"
path = "../agent"

[dev-dependencies."faktor-google"]
path = "../google"

# faktor-google = { path = "../google" }
"#;
        let deps = manifest_deps(src);
        let normal: Vec<&str> = deps
            .iter()
            .filter(|d| !d.dev)
            .map(|d| d.target.as_str())
            .collect();
        assert_eq!(
            normal,
            vec![
                "faktor-core",
                "serde",
                "faktor-agent",
                "client",
                "faktor-router",
                "winjob",
                "faktor-agent"
            ]
        );
        let dev: Vec<&str> = deps
            .iter()
            .filter(|d| d.dev)
            .map(|d| d.target.as_str())
            .collect();
        assert_eq!(
            dev,
            vec!["faktor-openai", "tokio", "faktor-ollama", "faktor-google"]
        );
        let renamed = deps
            .iter()
            .find(|d| d.target == "faktor-agent")
            .expect("rename parsed");
        assert!(renamed.renamed && renamed.path.as_deref() == Some("../agent"));
        // Table form: the trailing section segment is the dependency key and
        // the following lines are its properties.
        let table = deps
            .iter()
            .find(|d| d.target == "winjob")
            .expect("table-form dep parsed");
        assert!(table.path.as_deref() == Some("../winjob"));
        let table_rename = deps
            .iter()
            .find(|d| d.renamed && d.path.as_deref() == Some("../agent"));
        assert!(table_rename.is_some(), "table-form package rename parsed");
        // Workspace inheritance is flagged for resolution against the root
        // `[workspace.dependencies]` table.
        assert!(deps
            .iter()
            .find(|d| d.target == "faktor-core")
            .is_some_and(|d| d.workspace));
        assert!(deps
            .iter()
            .find(|d| d.target == "tokio")
            .is_some_and(|d| d.workspace));
        assert_eq!(
            manifest_package_name(src).as_deref(),
            Some("faktor-commerce")
        );
        assert_eq!(
            workspace_members("members = [\n  \"crates/a\",\n  \"crates/b\",\n]"),
            vec!["crates/a", "crates/b"]
        );
        assert!(manifest_deps("# faktor-agent = { path = \"../agent\" }\n").is_empty());
        assert!(manifest_deps("names = \"faktor-agent\"\n").is_empty());

        // Workspace dependency table: alias -> (package rename, path).
        let root = r#"
[workspace]
members = ["crates/acquire"]

[workspace.dependencies]
faktor-core = { path = "crates/core" }
agent-alias = { package = "faktor-agent", path = "crates/agent" }
"#;
        let table = workspace_dependencies(root);
        assert_eq!(
            table.get("faktor-core"),
            Some(&(None, Some("crates/core".into())))
        );
        assert_eq!(
            table.get("agent-alias"),
            Some(&(Some("faktor-agent".into()), Some("crates/agent".into())))
        );
    }

    // ---- dependency-closure planted-evasion fixtures ---------------------

    /// Write a synthetic workspace under the temp dir (this crate is
    /// dependency-free on purpose, so no `tempfile`). Callers remove it.
    fn synthetic_workspace(name: &str, files: &[(String, String)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "faktor-static-authority-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, src) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("fixture parent"))
                .expect("fixture parent dir");
            std::fs::write(&path, src).expect("fixture write");
        }
        root
    }

    /// The baseline commerce-path workspace every planted-evasion fixture
    /// extends: the four member crates over `faktor-core`.
    fn closure_fixture(
        name: &str,
        acquire_deps: &str,
        acquire_dev_deps: &str,
        root_workspace_deps: &str,
        extra: &[(&str, &str)],
    ) -> std::path::PathBuf {
        let mut files: Vec<(String, String)> = vec![
            (
                "Cargo.toml".to_string(),
                format!(
                    "[workspace]\nmembers = [\"crates/core\", \"crates/acquire\", \
                     \"crates/commerce\", \"crates/commerce-connectors\", \"crates/browser\"]\n\n\
                     [workspace.dependencies]\nfaktor-core = {{ path = \"crates/core\" }}\n\
                     {root_workspace_deps}\n"
                ),
            ),
            (
                "crates/core/Cargo.toml".to_string(),
                "[package]\nname = \"faktor-core\"\n".to_string(),
            ),
            (
                "crates/acquire/Cargo.toml".to_string(),
                format!(
                    "[package]\nname = \"faktor-acquire\"\n\n[dependencies]\n\
                     faktor-core.workspace = true\n{acquire_deps}\n\n[dev-dependencies]\n\
                     {acquire_dev_deps}\n"
                ),
            ),
            (
                "crates/commerce/Cargo.toml".to_string(),
                "[package]\nname = \"faktor-commerce\"\n\n[dependencies]\n\
                 faktor-core.workspace = true\n"
                    .to_string(),
            ),
            (
                "crates/commerce-connectors/Cargo.toml".to_string(),
                "[package]\nname = \"faktor-commerce-connectors\"\n\n[dependencies]\n\
                 faktor-commerce = { path = \"../commerce\" }\n"
                    .to_string(),
            ),
            (
                "crates/browser/Cargo.toml".to_string(),
                "[package]\nname = \"faktor-browser\"\n\n[dependencies]\n\
                 faktor-core.workspace = true\n"
                    .to_string(),
            ),
        ];
        for (rel, src) in extra {
            files.push(((*rel).to_string(), (*src).to_string()));
        }
        synthetic_workspace(name, &files)
    }

    #[test]
    fn planted_table_form_dependency_evasion_fails_the_closure_scan() {
        let root = closure_fixture(
            "table-form",
            "\n[dependencies.faktor-agent]\npath = \"../agent\"\n",
            "",
            "",
            &[(
                "crates/agent/Cargo.toml",
                "[package]\nname = \"faktor-agent\"\n",
            )],
        );
        let closure = commerce_dependency_closure_at(&root);
        assert!(
            closure
                .offenders
                .iter()
                .any(|chain| chain.contains("faktor-agent")),
            "a table-form dependency on a forbidden crate must be reported: {:?}",
            closure.offenders
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn planted_workspace_package_rename_evasion_fails_the_closure_scan() {
        let root = closure_fixture(
            "ws-rename",
            "agent-alias.workspace = true\n",
            "",
            "agent-alias = { package = \"faktor-agent\", path = \"crates/agent\" }\n",
            &[(
                "crates/agent/Cargo.toml",
                "[package]\nname = \"faktor-agent\"\n",
            )],
        );
        let closure = commerce_dependency_closure_at(&root);
        assert!(
            closure
                .offenders
                .iter()
                .any(|chain| chain.contains("faktor-agent")),
            "a workspace package rename must not hide the real target: {:?}",
            closure.offenders
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn planted_non_member_path_dependency_evasion_fails_the_closure_scan() {
        let root = closure_fixture(
            "path-dep",
            "\nsneaky = { path = \"vendor/sneaky\" }\n",
            "",
            "",
            &[
                (
                    "crates/acquire/vendor/sneaky/Cargo.toml",
                    "[package]\nname = \"sneaky\"\n\n[dependencies]\n\
                     faktor-router = { path = \"../router\" }\n",
                ),
                (
                    "crates/acquire/vendor/router/Cargo.toml",
                    "[package]\nname = \"faktor-router\"\n",
                ),
            ],
        );
        let closure = commerce_dependency_closure_at(&root);
        assert!(
            closure
                .offenders
                .iter()
                .any(|chain| chain.contains("faktor-router")),
            "a non-member path dependency must be walked, not skipped: {:?}",
            closure.offenders
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn planted_model_dev_dependency_fails_the_classification_scan() {
        let root = closure_fixture(
            "dev-dep",
            "",
            "faktor-openai.workspace = true\n",
            "faktor-openai = { path = \"vendor/openai\" }\n",
            &[(
                "vendor/openai/Cargo.toml",
                "[package]\nname = \"faktor-openai\"\n",
            )],
        );
        let closure = commerce_dependency_closure_at(&root);
        assert!(
            closure
                .dev_offenders
                .iter()
                .any(|chain| chain.contains("faktor-openai")),
            "a model-crate dev-dependency must be classified or refused: {:?}",
            closure.dev_offenders
        );
        assert!(
            closure
                .dev_edges
                .iter()
                .any(|(_, to)| to == "faktor-openai"),
            "the dev-dependency edge must be recorded for load-bearing classification"
        );
        assert!(
            closure.offenders.is_empty(),
            "dev-only edge is not a production edge"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_clean_fixture_workspace_certifies_with_no_offenders() {
        let root = closure_fixture("clean", "", "", "", &[]);
        let closure = commerce_dependency_closure_at(&root);
        assert!(closure.offenders.is_empty(), "{:?}", closure.offenders);
        assert!(
            closure.dev_offenders.is_empty(),
            "{:?}",
            closure.dev_offenders
        );
        assert!(
            closure.visited >= 5,
            "fixture walk visited {}",
            closure.visited
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Commerce-path production sources that must never construct an HTTP
    /// client or spawn a child process of their own.
    const COMMERCE_PATH_PROCESS_MARKERS: &[&str] = &[
        "reqwest::Client::new",
        "reqwest::Client::builder",
        "Command::new(",
    ];

    fn is_commerce_path_source(rel: &str) -> bool {
        let rel = normalize_rel(rel);
        rel == COMMERCE_TOOL_GATEWAY
            || rel.starts_with("crates/acquire/src/")
            || rel.starts_with("crates/commerce/src/")
            || rel.starts_with("crates/commerce-connectors/src/")
            || rel.starts_with("crates/browser/src/")
    }

    #[test]
    fn commerce_paths_never_build_clients_or_spawn_children() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            if !is_commerce_path_source(&rel) || is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            scanned += 1;
            for (line, text) in find_markers(&f, COMMERCE_PATH_PROCESS_MARKERS) {
                offenders.push(format!(
                    "{rel}:{line}: {text}  [commerce production source; HTTP goes through \
                     the injected checked transport, children through the ProcessSupervisor]"
                ));
            }
        }
        assert_no_offenders("commerce-path client/child scan", &offenders, scanned, 40);
    }

    /// A chrome-like literal argument of any `Command::new` in production.
    fn chromium_literal_offenders(rel: &str, f: &File<'_>) -> Vec<String> {
        let mut offenders = Vec::new();
        for at in find_marker_offsets(f, "Command::new(") {
            let rest = &f.src[at + "Command::new(".len()..];
            let rest = rest.trim_start();
            if !rest.starts_with('"') {
                continue;
            }
            let after = &rest[1..];
            let Some(end) = after.find('"') else {
                continue;
            };
            let literal = after[..end].to_ascii_lowercase();
            if literal.contains("chrom") || literal.contains("chrome") || literal.contains("webkit")
            {
                offenders.push(format!(
                    "{}:{}: literal browser binary {literal:?} spawned by Command::new; \
                     Chromium launches only through the supervised browser authority",
                    rel,
                    line_of(f.src, at)
                ));
            }
        }
        offenders
    }

    #[test]
    fn no_literal_browser_binary_is_ever_spawned() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        let mut browser_launch_seen = false;
        for rel in walk_crate_sources() {
            if is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            scanned += 1;
            if rel == "crates/browser/src/launch.rs" {
                browser_launch_seen = !find_markers(&f, &["spawn_detached_with_pipes"]).is_empty();
            }
            offenders.extend(chromium_literal_offenders(&rel, &f));
        }
        assert!(
            browser_launch_seen,
            "crates/browser/src/launch.rs no longer launches through the supervisor; the \
             chromium-literal scan needs re-audit"
        );
        assert_no_offenders(
            "literal browser-binary spawn scan",
            &offenders,
            scanned,
            100,
        );
    }

    /// Floating-point markers that must not appear on a price-bearing
    /// commerce source. The serde `visit_f64`/`visit_f32` rejection
    /// signatures are guards that REFUSE floating input, not price paths,
    /// and are the only allowed mention.
    const COMMERCE_FLOAT_MARKERS: &[&str] = &[
        "f64",
        "f32",
        "powf(",
        "is_nan(",
        "is_finite(",
        "to_bits(",
        "from_bits(",
        "EPSILON",
        "NAN",
        "INFINITY",
    ];

    fn commerce_float_offenders(rel: &str, f: &File<'_>) -> Vec<String> {
        find_markers(f, COMMERCE_FLOAT_MARKERS)
            .into_iter()
            .filter(|(_, text)| {
                let trimmed = text.trim_start();
                !(trimmed.starts_with("fn visit_f64") || trimmed.starts_with("fn visit_f32"))
            })
            .map(|(line, text)| {
                format!("{rel}:{line}: {text}  [floating-point type on a commerce path]")
            })
            .collect()
    }

    fn is_commerce_price_source(rel: &str) -> bool {
        let rel = normalize_rel(rel);
        rel == COMMERCE_TOOL_GATEWAY
            || rel.starts_with("crates/acquire/src/")
            || rel.starts_with("crates/commerce/src/")
            || rel.starts_with("crates/commerce-connectors/src/")
    }

    #[test]
    fn commerce_price_paths_never_mention_a_floating_type() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        let mut guards = 0usize;
        for rel in walk_crate_sources() {
            if !is_commerce_price_source(&rel) || is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            scanned += 1;
            for (_, text) in find_markers(&f, COMMERCE_FLOAT_MARKERS) {
                let trimmed = text.trim_start();
                if trimmed.starts_with("fn visit_f64") || trimmed.starts_with("fn visit_f32") {
                    guards += 1;
                }
            }
            offenders.extend(commerce_float_offenders(&rel, &f));
        }
        assert!(
            guards >= 1,
            "the serde floating-rejection guards vanished; the allowlist is stale"
        );
        assert_no_offenders("commerce exact-money scan", &offenders, scanned, 40);
    }

    #[test]
    fn commerce_float_scan_detects_planted_floating_money() {
        let planted = synthetic_file(
            "crates/commerce/src/money.rs",
            "fn total(price: f64) -> f64 { price * 1.0 }\n",
        );
        // Both floating types sit on the same line and the scan reports the
        // offending LINE (deduplicated), so one violation is expected.
        assert_eq!(
            commerce_float_offenders("crates/commerce/src/money.rs", &planted).len(),
            1,
            "the planted floating money line must be reported"
        );
        let guard = synthetic_file(
            "crates/commerce/src/money.rs",
            "fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {\n    Err(E::custom(\"no floats\"))\n}\n",
        );
        assert!(
            commerce_float_offenders("crates/commerce/src/money.rs", &guard).is_empty(),
            "the floating-input rejection guard is the one allowed mention"
        );
        let comment = synthetic_file(
            "crates/commerce/src/money.rs",
            "// exact money only: no f64 anywhere\n",
        );
        assert!(commerce_float_offenders("crates/commerce/src/money.rs", &comment).is_empty());
    }

    // ------------------------------------------------------------------
    // scan 10: plaintext secret-shaped production struct fields
    //
    //  * a production struct field named `password`, `*_token`,
    //    `private_key*` or `client_secret` whose type is `String` /
    //    `Option<String>` is a plaintext credential: refused;
    //  * the ONE exception is a wire DTO that mirrors an external payload
    //    shape, carries the literal
    //    `SECRET-FIELD-GATE-WIRE-DTO: <justification>` annotation in the
    //    contiguous comment block immediately above the struct AND is
    //    listed in `SECRET_WIRE_DTO_ALLOWLIST` with a written
    //    justification; such a DTO must convert immediately (its secret
    //    bytes are wrapped before any domain use) and can never store them.
    // ------------------------------------------------------------------

    /// A field name is secret-shaped when it is `password`, any `*_token`
    /// (this covers `access_token` / `id_token` / `refresh_token`), any
    /// `private_key*` (this covers `private_key_pkcs8_pem`), exactly
    /// `client_secret`, an OIDC/authorization-flow credential name
    /// (`nonce`/`*_nonce`, `code_verifier`/`*_code_verifier`,
    /// `authorization_code`/`*_authorization_code`) or an auth-flow
    /// qualified state name. Everything else (`token_type`, `token_hash`,
    /// `password_hash`, `webhook_secret`, `credentials_path`, bare
    /// `state`/`*_state` state-machine phases, …) is not a credential value
    /// and deliberately passes.
    ///
    /// `nonce`, `code_verifier` and `authorization_code` are exactly the
    /// names the audit bypass used: security value does not follow from the
    /// `*_token` shape, so the gate must recognise the OAuth credential
    /// vocabulary directly. Bare `state`/`task_state`/`delivery_state` are
    /// overwhelmingly state-machine PHASES (session/job/billing state
    /// names), so only state names qualified by an auth-flow token are
    /// credentials: `oauth_state`, `csrf_state`, `login_state`, …
    fn is_secret_field_name(name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        name == "password"
            || name.ends_with("_token")
            || name.starts_with("private_key")
            || name == "client_secret"
            || name == "nonce"
            || name.ends_with("_nonce")
            || name == "code_verifier"
            || name.ends_with("_code_verifier")
            || name == "authorization_code"
            || name.ends_with("_authorization_code")
            || is_auth_flow_state_name(&name)
    }

    /// `state`-shaped credential names: a `state` field is secret-shaped
    /// only when it is QUALIFIED by an authorization-flow token
    /// (`oauth_state`, `oidc_state`, `sso_state`, `csrf_state`,
    /// `login_state`, `logout_state`, `authorization_state`). An unqualified
    /// `state` or a non-auth `*_state` is a phase name, not a credential.
    fn is_auth_flow_state_name(name: &str) -> bool {
        let Some(prefix) = name.strip_suffix("_state") else {
            return false;
        };
        matches!(
            prefix,
            "oauth" | "oauth2" | "oidc" | "sso" | "csrf" | "login" | "logout" | "authorization"
        )
    }

    /// The documented wire-DTO annotation marker. The justification must be
    /// written after the marker (same line) and be at least
    /// [`SECRET_FIELD_WIRE_DTO_MIN_JUSTIFICATION`] characters long.
    const SECRET_FIELD_WIRE_DTO_ANNOTATION: &str = "SECRET-FIELD-GATE-WIRE-DTO:";

    /// Minimum written justification after the annotation marker.
    const SECRET_FIELD_WIRE_DTO_MIN_JUSTIFICATION: usize = 40;

    /// The exact annotated wire DTOs. Each entry is `(file, struct,
    /// justification)`; the justification documents WHY the plaintext shape
    /// is a wire capture that converts immediately. Every entry is asserted
    /// load-bearing (the struct really exists, carries the annotation and
    /// really holds secret-shaped fields) and non-stale by the tests below;
    /// keep this list TINY.
    const SECRET_WIRE_DTO_ALLOWLIST: &[(&str, &str, &str)] = &[
        (
            "crates/cloud/src/oidc_net.rs",
            "RawTokenResponse",
            "OIDC token-endpoint provider response: parsed and converted to OidcTokenSet \
             (SecretValue fields) in the same block, never stored as plaintext.",
        ),
        (
            "crates/commerce-connectors/src/digikey/normalize.rs",
            "TokenResponse",
            "DigiKey OAuth token-endpoint provider response: token_from_response extracts \
             the token into the connector SecretString at the same boundary, never stored.",
        ),
    ];

    fn is_wire_dto_allowlisted(rel: &str, struct_name: &str) -> bool {
        SECRET_WIRE_DTO_ALLOWLIST
            .iter()
            .any(|(file, name, _)| *file == rel && *name == struct_name)
    }

    /// Production text with comments, string/char literals and test-gated
    /// ranges blanked out (newlines kept): byte offsets still match
    /// `f.src` exactly, but only real production code positions carry bytes.
    fn production_code_bytes(f: &File<'_>) -> Vec<u8> {
        let mut out = f.src.as_bytes().to_vec();
        for (i, byte) in out.iter_mut().enumerate() {
            if *byte == b'\n' {
                continue;
            }
            let kept = f.kept.iter().any(|(a, z)| i >= *a && i < *z);
            if !kept || !f.code[i] {
                *byte = b' ';
            }
        }
        out
    }

    fn parse_ident(bytes: &[u8], from: usize) -> Option<(String, usize)> {
        let mut i = from;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i == start {
            return None;
        }
        Some((String::from_utf8_lossy(&bytes[start..i]).to_string(), i))
    }

    /// The secret-shaped plaintext fields of ONE struct body field segment
    /// (`pub access_token: Option<String>` → `Some("access_token")`). The
    /// field name is the last identifier before the top-level `:`; the type
    /// must be EXACTLY `String` or `Option<String>` (whitespace ignored).
    /// Wrappers (`SecretValue`, `Option<SecretValue>`, dedicated newtypes)
    /// never match.
    fn parse_secret_field(segment: &[u8]) -> Option<String> {
        let mut depth = 0i32;
        let mut colon = None;
        for (idx, byte) in segment.iter().enumerate() {
            match byte {
                b'(' | b'[' | b'{' | b'<' => depth += 1,
                b')' | b']' | b'}' | b'>' => depth = depth.saturating_sub(1),
                b':' if depth == 0 => {
                    colon = Some(idx);
                    break;
                }
                _ => {}
            }
        }
        let colon = colon?;
        let before = &segment[..colon];
        let after = &segment[colon + 1..];
        let mut end = before.len();
        while end > 0 && before[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && (before[start - 1].is_ascii_alphanumeric() || before[start - 1] == b'_')
        {
            start -= 1;
        }
        if start == end {
            return None;
        }
        let name = String::from_utf8_lossy(&before[start..end]).to_string();
        if !is_secret_field_name(&name) {
            return None;
        }
        let ty: String = after
            .iter()
            .filter(|b| !b.is_ascii_whitespace())
            .map(|b| char::from(*b))
            .collect();
        matches!(ty.as_str(), "String" | "Option<String>").then_some(name)
    }

    /// One production struct with its secret-shaped plaintext fields.
    struct SecretStruct {
        name: String,
        fields: Vec<String>,
        line: usize,
        at: usize,
    }

    /// The brace-body structs of one production file that declare
    /// secret-shaped plaintext fields. Tuple/unit structs never carry named
    /// fields; generics and where-clauses before the body are skipped.
    fn production_secret_structs(f: &File<'_>) -> Vec<SecretStruct> {
        let bytes = production_code_bytes(f);
        let n = bytes.len();
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + 6 <= n {
            if &bytes[i..i + 6] == b"struct" {
                let left_ok = i == 0 || {
                    let b = bytes[i - 1];
                    !(b.is_ascii_alphanumeric() || b == b'_')
                };
                let right_ok = i + 6 == n || {
                    let b = bytes[i + 6];
                    !(b.is_ascii_alphanumeric() || b == b'_')
                };
                if left_ok && right_ok {
                    if let Some((name, fields)) = parse_struct_secret_fields(&bytes, i) {
                        if !fields.is_empty() {
                            out.push(SecretStruct {
                                name,
                                fields,
                                line: line_of(f.src, i),
                                at: i,
                            });
                        }
                    }
                }
            }
            i += 1;
        }
        out
    }

    fn parse_struct_secret_fields(bytes: &[u8], struct_at: usize) -> Option<(String, Vec<String>)> {
        let n = bytes.len();
        let (name, mut i) = parse_ident(bytes, struct_at + "struct".len())?;
        let mut depth = 0i32;
        let mut body = None;
        while i < n {
            match bytes[i] {
                b'(' | b'[' | b'<' => depth += 1,
                b')' | b']' | b'>' => depth -= 1,
                b'{' if depth <= 0 => {
                    body = Some(i);
                    break;
                }
                b';' if depth <= 0 => return None,
                _ => {}
            }
            i += 1;
        }
        let body = body?;
        let mut d = 0i32;
        let mut end = None;
        let mut j = body;
        while j < n {
            match bytes[j] {
                b'{' => d += 1,
                b'}' => {
                    d -= 1;
                    if d == 0 {
                        end = Some(j);
                        break;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        let end = end?;
        let mut fields = Vec::new();
        let mut segment_start = body + 1;
        let mut depth = 0i32;
        let mut k = body + 1;
        while k <= end {
            if k == end || (bytes[k] == b',' && depth == 0) {
                if let Some(field) = parse_secret_field(&bytes[segment_start..k]) {
                    fields.push(field);
                }
                segment_start = k + 1;
            } else {
                match bytes[k] {
                    b'(' | b'[' | b'{' | b'<' => depth += 1,
                    b')' | b']' | b'}' | b'>' => depth = depth.saturating_sub(1),
                    _ => {}
                }
            }
            k += 1;
        }
        Some((name, fields))
    }

    /// The justification of the documented wire-DTO annotation, when a
    /// comment in the contiguous comment/attribute block immediately above
    /// the struct carries the marker with a long-enough justification. A
    /// blank line, a code line or a marker inside a string/attribute never
    /// counts; only `//`-comment lines and single-line attributes may sit in
    /// the block.
    fn wire_dto_annotation(f: &File<'_>, struct_at: usize) -> Option<String> {
        let line = line_of(f.src, struct_at);
        if line < 2 {
            return None;
        }
        let lines: Vec<&str> = f.src.lines().collect();
        let mut i = line - 2;
        loop {
            let text = lines.get(i)?.trim();
            if text.is_empty() {
                return None;
            }
            if text.starts_with("//") {
                if let Some((_, rest)) = text.split_once(SECRET_FIELD_WIRE_DTO_ANNOTATION) {
                    let justification = rest.trim_start_matches(':').trim();
                    if justification.chars().count() >= SECRET_FIELD_WIRE_DTO_MIN_JUSTIFICATION {
                        return Some(justification.to_string());
                    }
                    return None;
                }
            } else if !(text.starts_with("#[") || text.starts_with("#!")) {
                return None;
            }
            if i == 0 {
                return None;
            }
            i -= 1;
        }
    }

    /// Scan-10 offenders of one production file: secret-shaped plaintext
    /// struct fields outside the allowlisted, annotated wire DTOs. An
    /// annotation that is not allowlisted is itself an offender (a new wire
    /// DTO must be documented in the scan), so the exception can never be
    /// widened silently.
    fn secret_field_offenders(rel: &str, f: &File<'_>) -> Vec<String> {
        let rel = normalize_rel(rel);
        let mut offenders = Vec::new();
        for found in production_secret_structs(f) {
            let fields = found.fields.join(", ");
            match wire_dto_annotation(f, found.at) {
                Some(_) if is_wire_dto_allowlisted(&rel, &found.name) => {}
                Some(_) => offenders.push(format!(
                    "{rel}:{}: struct {} carries the {} annotation but is not in \
                     SECRET_WIRE_DTO_ALLOWLIST; document it (with a justification) or wrap \
                     {fields} in a redacting secret type",
                    found.line, found.name, SECRET_FIELD_WIRE_DTO_ANNOTATION
                )),
                None => offenders.push(format!(
                    "{rel}:{}: struct {} declares secret-shaped plaintext field(s) {fields}; \
                     wrap them in faktor_security::secret::SecretValue (a genuine wire DTO \
                     must carry the {SECRET_FIELD_WIRE_DTO_ANNOTATION} annotation and be \
                     allowlisted)",
                    found.line, found.name
                )),
            }
        }
        offenders
    }

    /// The real-tree invariant: no production struct declares a plaintext
    /// secret-shaped `String`/`Option<String>` field outside the exact,
    /// annotated, justified wire-DTO allowlist.
    #[test]
    fn production_structs_never_hold_plaintext_secret_fields() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            if is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            scanned += 1;
            offenders.extend(secret_field_offenders(&rel, &f));
        }
        assert_no_offenders(
            "secret-field scan: production structs must wrap password/*_token/private_key*/\
             client_secret values in a redacting secret type (faktor_security::secret::\
             SecretValue); only annotated+allowlisted wire DTOs may mirror a plaintext wire \
             shape and must convert immediately",
            &offenders,
            scanned,
            100,
        );
    }

    /// The allowlist entries are exact, justified, load-bearing (the struct
    /// exists, is annotated, and really declares the secret-shaped fields)
    /// and never stale; every annotated struct in the tree is listed, so a
    /// new annotation is a red scan until it is documented.
    #[test]
    fn secret_wire_dto_allowlist_entries_are_documented_and_load_bearing() {
        assert!(
            SECRET_WIRE_DTO_ALLOWLIST.len() <= 4,
            "the wire-DTO allowlist must stay tiny ({} entries)",
            SECRET_WIRE_DTO_ALLOWLIST.len()
        );
        for (file, struct_name, justification) in SECRET_WIRE_DTO_ALLOWLIST {
            assert!(
                justification.len() >= SECRET_FIELD_WIRE_DTO_MIN_JUSTIFICATION,
                "allowlist entry {file} {struct_name} needs a real written justification"
            );
            let f = load(file).unwrap_or_else(|| panic!("allowlisted file missing: {file}"));
            let found = production_secret_structs(&f)
                .into_iter()
                .find(|found| found.name == *struct_name)
                .unwrap_or_else(|| {
                    panic!(
                        "allowlist entry {file} {struct_name} is stale: the struct no longer \
                         declares secret-shaped plaintext fields"
                    )
                });
            assert!(
                wire_dto_annotation(&f, found.at).is_some(),
                "allowlist entry {file} {struct_name} lost its \
                 {SECRET_FIELD_WIRE_DTO_ANNOTATION} annotation"
            );
            assert!(
                !found.fields.is_empty(),
                "allowlist entry {file} {struct_name} is load-bearing: it must declare a \
                 secret-shaped field"
            );
        }
        // Every annotated struct in the tree is documented: exactly the
        // allowlisted set may carry the annotation.
        let mut documented = Vec::new();
        for rel in walk_crate_sources() {
            if is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            for found in production_secret_structs(&f) {
                if wire_dto_annotation(&f, found.at).is_some() {
                    documented.push(format!("{rel}:{}", found.name));
                }
            }
        }
        documented.sort();
        let mut expected: Vec<String> = SECRET_WIRE_DTO_ALLOWLIST
            .iter()
            .map(|(file, name, _)| format!("{file}:{name}"))
            .collect();
        expected.sort();
        assert_eq!(
            documented, expected,
            "the set of wire-DTO-annotated structs must be exactly SECRET_WIRE_DTO_ALLOWLIST"
        );
    }

    /// Planted-fixture proof: every secret-shaped family fires; wrapped
    /// fields, comments, strings and test-gated structs never do; a
    /// non-adjacent or unjustified annotation never exempts; an annotated
    /// struct outside the allowlist is still an offender.
    #[test]
    fn secret_field_scan_fires_on_planted_plaintext_credentials() {
        for (src, needle) in [
            ("pub struct Cfg { pub password: String }\n", "password"),
            ("struct Cfg { access_token: String }\n", "access_token"),
            ("struct Cfg { id_token: Option<String> }\n", "id_token"),
            (
                "struct Cfg { refresh_token: Option<String> }\n",
                "refresh_token",
            ),
            (
                "struct Cfg { pub client_secret: String }\n",
                "client_secret",
            ),
            (
                "pub struct Cfg { pub private_key_pkcs8_pem: String }\n",
                "private_key_pkcs8_pem",
            ),
            // The audit-bypass vocabulary: OAuth flow credentials are
            // secret-shaped even though their names never say "token".
            ("struct Cfg { nonce: Option<String> }\n", "nonce"),
            ("struct Cfg { id_nonce: String }\n", "id_nonce"),
            (
                "struct Cfg { code_verifier: Option<String> }\n",
                "code_verifier",
            ),
            (
                "struct Cfg { pkce_code_verifier: String }\n",
                "pkce_code_verifier",
            ),
            (
                "struct Cfg { authorization_code: Option<String> }\n",
                "authorization_code",
            ),
            ("struct Cfg { oauth_state: String }\n", "oauth_state"),
            ("struct Cfg { csrf_state: Option<String> }\n", "csrf_state"),
            ("struct Cfg { login_state: String }\n", "login_state"),
        ] {
            let f = synthetic_file("crates/cloud/src/evil.rs", src);
            let offenders = secret_field_offenders("crates/cloud/src/evil.rs", &f);
            assert_eq!(offenders.len(), 1, "{src}: {offenders:?}");
            assert!(
                offenders[0].contains(needle),
                "the offender must name {needle}: {offenders:?}"
            );
        }
        // Multiple planted fields in one struct are reported together.
        let f = synthetic_file(
            "crates/cloud/src/evil.rs",
            "struct Cfg { password: String, access_token: Option<String>, token_type: String }\n",
        );
        let offenders = secret_field_offenders("crates/cloud/src/evil.rs", &f);
        assert_eq!(offenders.len(), 1);
        assert!(
            offenders[0].contains("password") && offenders[0].contains("access_token"),
            "{offenders:?}"
        );
        assert!(
            !offenders[0].contains("token_type"),
            "token_type is not a credential: {offenders:?}"
        );
        // Wrapped fields never fire.
        let f = synthetic_file(
            "crates/cloud/src/good.rs",
            "struct Cfg {\n    password: SecretValue,\n    access_token: Option<SecretValue>,\n\
             client_secret: WrappedSecret,\n    private_key_pem: Pem,\n\
             nonce: Option<OidcNonce>,\n    code_verifier: SecretValue,\n\
             authorization_code: SecretValue,\n    oauth_state: OAuthState,\n}\n",
        );
        assert!(
            secret_field_offenders("crates/cloud/src/good.rs", &f).is_empty(),
            "{:?}",
            secret_field_offenders("crates/cloud/src/good.rs", &f)
        );
        // State-machine phase names are not credentials: bare `state` and
        // non-auth `*_state` fields deliberately pass.
        let f = synthetic_file(
            "crates/cloud/src/good.rs",
            "struct Cfg {\n    state: String,\n    task_state: String,\n    delivery_state: Option<String>,\n\
             token_type: String,\n}\n",
        );
        assert!(
            secret_field_offenders("crates/cloud/src/good.rs", &f).is_empty(),
            "phase names must pass: {:?}",
            secret_field_offenders("crates/cloud/src/good.rs", &f)
        );
        // A mention in a comment, a string or a test-gated struct is never
        // production code.
        let f = synthetic_file(
            "crates/cloud/src/good.rs",
            "// password: String is forbidden\nconst X: &str = \"id_token: String\";\n\
             #[cfg(test)]\nmod tests {\n    struct T { access_token: String }\n}\n",
        );
        assert!(secret_field_offenders("crates/cloud/src/good.rs", &f).is_empty());
        // An annotation alone does not exempt: the struct must ALSO be
        // allowlisted, and the justification must be real.
        let annotated = format!(
            "// {SECRET_FIELD_WIRE_DTO_ANNOTATION} a planted fixture struct that is not in the allowlist\n\
             #[derive(Debug)]\nstruct Planted {{ access_token: String }}\n"
        );
        let f = synthetic_file("crates/cloud/src/evil.rs", &annotated);
        let offenders = secret_field_offenders("crates/cloud/src/evil.rs", &f);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert!(
            offenders[0].contains("not in SECRET_WIRE_DTO_ALLOWLIST"),
            "{offenders:?}"
        );
        let short = format!(
            "// {SECRET_FIELD_WIRE_DTO_ANNOTATION} too short\nstruct Planted {{ id_token: String }}\n"
        );
        let f = synthetic_file("crates/cloud/src/evil.rs", &short);
        assert_eq!(
            secret_field_offenders("crates/cloud/src/evil.rs", &f).len(),
            1,
            "a marker without a written justification must not exempt"
        );
        // A blank line between the annotation and the struct breaks adjacency.
        let not_adjacent = format!(
            "// {SECRET_FIELD_WIRE_DTO_ANNOTATION} a sufficiently long planted justification text here\n\n\
             struct Planted {{ id_token: String }}\n"
        );
        let f = synthetic_file("crates/cloud/src/evil.rs", &not_adjacent);
        assert_eq!(
            secret_field_offenders("crates/cloud/src/evil.rs", &f).len(),
            1
        );
        // A field-level annotation can never exempt the struct.
        let field_level = format!(
            "struct Planted {{\n    // {SECRET_FIELD_WIRE_DTO_ANNOTATION} a sufficiently long planted justification text here\n    id_token: String,\n}}\n"
        );
        let f = synthetic_file("crates/cloud/src/evil.rs", &field_level);
        assert_eq!(
            secret_field_offenders("crates/cloud/src/evil.rs", &f).len(),
            1
        );
    }

    // ------------------------------------------------------------------
    // scan 2c: budgeted response-body reads in production egress consumers
    // ------------------------------------------------------------------

    /// Every production crate whose code consumes a response body from the
    /// `HttpTransport` seam / the checked client. A body read here MUST go
    /// through the budget-aware helpers (`CheckedResponse` +
    /// `ResponseBudget` + `BudgetedBody::next_chunk`/`read_all`/
    /// `stream_frames`, the `read_json_bounded` helper, or `execute_raw`
    /// with a budget), never a direct read method and never a raw
    /// `reqwest::Response`.
    const EGRESS_CONSUMER_ROOTS: &[&str] = &[
        "crates/provider/src/",
        "crates/openai/src/",
        "crates/anthropic/src/",
        "crates/google/src/",
        "crates/deepseek/src/",
        "crates/gateway/src/",
        "crates/ollama/src/",
        "crates/scm/src/",
        "crates/cloud/src/",
        "crates/updater/src/",
        "crates/semantic/src/",
        "crates/commerce-connectors/src/",
    ];

    /// The ONE file allowed to name the raw read methods: it implements the
    /// budget-aware helpers themselves.
    const EGRESS_BUDGET_HELPER: &str = "crates/provider/src/egress.rs";

    fn is_egress_consumer(rel: &str) -> bool {
        let rel = normalize_rel(rel);
        EGRESS_CONSUMER_ROOTS
            .iter()
            .any(|root| rel.starts_with(root))
    }

    /// Offenders of one production consumer file: a direct response-body
    /// read or raw body consumption. `.bytes_stream(`, `.copy_to(`,
    /// `.copy_to_bytes(`, `.body_mut(` and `read_to_end(` are unambiguous
    /// (they can only act on a body stream/reader). `.chunk(`, `.json()`
    /// and `.json::<` are the raw reqwest body-read shapes the audit
    /// caught (`resp.json().await` bypassed every budget); the
    /// request-builder spelling `.json(&value)` deliberately does NOT match
    /// (`.json()`/`.json::<` require the body-read call parens). `.bytes()`
    /// and `.text()` are flagged only when AWAITED: a response body read
    /// always awaits, while synchronous accessors (`payload.text()` on a
    /// browser capture, `str::bytes()` iteration) never do. The `.await`
    /// window tolerates rustfmt splitting the call and the await across
    /// lines.
    fn direct_body_read_offenders(f: &File<'_>) -> Vec<String> {
        let mut offenders = Vec::new();
        for marker in [
            ".bytes_stream(",
            ".copy_to(",
            ".copy_to_bytes(",
            ".body_mut(",
            "read_to_end(",
        ] {
            for (line, text) in find_markers(f, &[marker]) {
                offenders.push(format!(
                    "{}:{line}: {text}  [raw body consumption; use \
                     CheckedResponse/BudgetedBody under a ResponseBudget]",
                    f.rel
                ));
            }
        }
        for marker in [".chunk(", ".json()", ".json::<"] {
            for (line, text) in find_markers(f, &[marker]) {
                offenders.push(format!(
                    "{}:{line}: {text}  [direct body read; use \
                     BudgetedBody::read_all / read_json_bounded]",
                    f.rel
                ));
            }
        }
        for marker in [".bytes()", ".text()"] {
            for at in find_marker_offsets(f, marker) {
                if awaited_within(f, at + marker.len(), 64) {
                    let line = line_of(f.src, at);
                    offenders.push(format!(
                        "{}:{line}: {}  [direct body read; use BudgetedBody::read_all]",
                        f.rel,
                        trim_line(f.src, at)
                    ));
                }
            }
        }
        offenders
    }

    /// The budgeted-body-read certification: production egress consumers
    /// never read a response body directly. The budget authority must exist
    /// first, so the scan cannot pass by deleting the helpers everyone is
    /// supposed to use.
    #[test]
    fn no_direct_response_body_reads_in_production_egress_consumers() {
        let authority = std::fs::read_to_string(repo_root().join(EGRESS_BUDGET_HELPER))
            .expect("the checked transport crates/provider/src/egress.rs must exist");
        for marker in [
            "pub struct ResponseBudget",
            "pub struct BudgetedBody",
            "pub enum BudgetComponent",
            "pub struct CheckedResponse",
            "pub struct ResponseHead",
            "pub async fn read_json_bounded",
        ] {
            assert!(
                authority.contains(marker),
                "the checked transport is missing {marker:?}; the response budget \
                 authority must live in egress.rs"
            );
        }
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            if !is_egress_consumer(&rel) || is_test_file(&rel) || rel == EGRESS_BUDGET_HELPER {
                continue;
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            scanned += 1;
            offenders.extend(direct_body_read_offenders(&f));
        }
        assert_no_offenders(
            "egress body-read scan: production consumers must read response bodies \
             through the budget-aware helpers (CheckedResponse/ResponseBudget/\
             BudgetedBody/read_json_bounded); direct .json()/.bytes()/.text()/\
             .chunk(/.bytes_stream()/.body_mut()/.copy_to(/.copy_to_bytes(/\
             read_to_end( body consumption is unbounded",
            &offenders,
            scanned,
            30,
        );
    }

    /// Planted-fixture proof: a direct unbounded read FAILS the scan (the
    /// regression it exists for), while the budget-aware shapes — and a
    /// non-await `.bytes()` iterator over a `&str` — pass. Every marker
    /// family has its own planted violation, including the audit's
    /// `resp.json().await` bypass and the `.json::<T>()` typed spelling.
    #[test]
    fn budgeted_body_read_scan_fires_on_planted_unbounded_reads() {
        for src in [
            "async fn f(r: reqwest::Response) -> Vec<u8> { r.bytes().await.unwrap() }\n",
            "async fn f(r: reqwest::Response) -> String { r.text().await.unwrap() }\n",
            "fn f(r: reqwest::Response) { let _ = r.bytes_stream(); }\n",
            "async fn f(r: reqwest::Response) { let _ = r.chunk().await; }\n",
            "async fn f(r: reqwest::Response) -> serde_json::Value { r.json().await.unwrap() }\n",
            "async fn f(r: reqwest::Response) -> serde_json::Value { r.json::<serde_json::Value>().await.unwrap() }\n",
            "async fn f(mut r: reqwest::Response, w: &mut Vec<u8>) { let _ = r.copy_to(w).await; }\n",
            "fn f(r: reqwest::Response) { let _ = r.copy_to_bytes(16); }\n",
            "fn f(r: &mut reqwest::Response) { let _ = r.body_mut(); }\n",
            "async fn f(mut r: reqwest::Response) { let mut b = Vec::new(); let _ = r.read_to_end(&mut b).await; }\n",
            // rustfmt may split the call and the await across lines: the
            // window must still catch both.
            "async fn f(r: reqwest::Response) -> Vec<u8> {\n    r.bytes()\n        .await\n        .unwrap()\n}\n",
            "async fn f(r: reqwest::Response) -> String {\n    r.text()\n        .await\n        .unwrap()\n}\n",
        ] {
            let f = synthetic_file("crates/scm/src/evil.rs", src);
            assert_eq!(
                direct_body_read_offenders(&f).len(),
                1,
                "the planted unbounded read must fire: {src}"
            );
        }
        for src in [
            "async fn f(r: reqwest::Response, b: ResponseBudget) -> Vec<u8> { BudgetedBody::new(r, b).read_all().await.unwrap() }\n",
            "fn f(r: reqwest::Response, b: ResponseBudget) { let _ = BudgetedBody::new(r, b).into_stream(); }\n",
            "fn f(v: &str) -> bool { v.bytes().all(|b| b.is_ascii_graphic()) }\n",
            // The checked-response APIs are the compliant shapes.
            "async fn f(r: CheckedResponse, b: ResponseBudget) -> Vec<u8> { r.read_bytes(&b).await.unwrap() }\n",
            "async fn f(r: CheckedResponse, b: ResponseBudget) -> serde_json::Value { r.read_json(&b).await.unwrap() }\n",
            "async fn f(r: CheckedResponse, b: ResponseBudget) -> serde_json::Value { read_json_bounded(r, &b).await.unwrap() }\n",
            "fn f(r: CheckedResponse, b: ResponseBudget) { let _ = r.stream_frames(&b); }\n",
            "fn f(r: CheckedResponse, b: ResponseBudget) { let _ = r.into_budgeted(b); }\n",
            // A REQUEST-BUILDER `.json(&value)` is not a body read.
            "fn f(rb: RequestBuilder, v: &serde_json::Value) -> RequestBuilder { rb.json(v) }\n",
        ] {
            let f = synthetic_file("crates/scm/src/good.rs", src);
            assert!(
                direct_body_read_offenders(&f).is_empty(),
                "the compliant fixture must pass: {src} -> {:?}",
                direct_body_read_offenders(&f)
            );
        }
        // The rule is scoped to the production consumer trees only.
        assert!(is_egress_consumer("crates/scm/src/lib.rs"));
        assert!(is_egress_consumer("crates/openai/src/lib.rs"));
        assert!(is_egress_consumer(
            r"crates\commerce-connectors\src\http.rs"
        ));
        assert!(!is_egress_consumer("crates/browser/src/egress.rs"));
        assert!(!is_egress_consumer("tests/integration/src/lib.rs"));
    }

    // ------------------------------------------------------------------
    // scan N: unsafe / OS-boundary policy (Phase D item 22)
    // ------------------------------------------------------------------

    /// Every crate source file allowed to contain `unsafe` code, with the
    /// EXACT number of `allow(unsafe_code)` attributes it must carry.
    ///
    /// - Authority modules (`pty`/`fs`/`terminal`/`sandbox`/`browser-platform`
    ///   plus the `winjob` Job-Object crate) put a FILE-level
    ///   `#![allow(unsafe_code)]` on the platform module; `terminal` and
    ///   `fs::tree_manifest` place FUNCTION-level allows on the few raw
    ///   syscall functions.
    /// - OS-boundary seams outside those crates: `git` process identity,
    ///   `session` thread-CPU clock (function-level allows).
    /// - Test-only locations: the out-of-line integration tests
    ///   (`pty/tests`, `winjob/tests`, `cloud/tests`) and cfg(test) modules
    ///   inside `terminal`, `fs::tree_manifest`, `orchestrator` and `cli`.
    ///
    /// A new unsafe site, a new `allow(unsafe_code)` attribute, a stale
    /// entry or a missing `// SAFETY:` justification is a red test.
    const UNSAFE_ALLOWED_FILES: &[(&str, usize)] = &[
        // --- platform authority modules (file-level allow) ---
        ("crates/pty/src/unix.rs", 1),
        ("crates/pty/src/windows.rs", 1),
        ("crates/pty/src/guardian/mod.rs", 1),
        ("crates/fs/src/platform/unix.rs", 1),
        ("crates/fs/src/platform/windows.rs", 1),
        ("crates/fs/src/rooted.rs", 1),
        ("crates/terminal/src/budget.rs", 1),
        ("crates/terminal/src/sandbox/linux.rs", 1),
        ("crates/winjob/src/lib.rs", 1),
        // --- function-level allows in mixed files ---
        ("crates/terminal/src/lib.rs", 12),
        ("crates/fs/src/tree_manifest.rs", 3),
        ("crates/git/src/guard.rs", 3),
        ("crates/session/src/actor.rs", 1),
        // --- test-only locations ---
        ("crates/pty/tests/guardian.rs", 1),
        ("crates/pty/tests/windows_lifecycle.rs", 1),
        ("crates/winjob/tests/windows_tree.rs", 1),
        ("crates/cloud/tests/durability_memory.rs", 1),
        ("crates/orchestrator/src/shadow_tests.rs", 1),
        ("crates/cli/src/main.rs", 2),
        ("tests/coding-benchmark/src/daemon.rs", 1),
        ("tests/coding-benchmark/src/process.rs", 1),
    ];

    /// Every `.rs` file under `crates/` and `tests/` (including out-of-line
    /// test dirs and examples) — the exact surface the workspace
    /// `unsafe_code` deny lint compiles.
    fn crate_rs_files_all() -> Vec<String> {
        let mut out = Vec::new();
        for root in [repo_root().join("crates"), repo_root().join("tests")] {
            crate_rs_files_under(&root, &mut out);
        }
        out.sort();
        out
    }

    fn crate_rs_files_under(root: &std::path::Path, out: &mut Vec<String>) {
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                let name = entry.file_name().to_string_lossy().into_owned();
                if ft.is_dir() {
                    if name == "target" || name.starts_with('.') {
                        continue;
                    }
                    stack.push(path);
                } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let rel = path
                        .strip_prefix(repo_root())
                        .expect("under repo root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push(rel);
                }
            }
        }
    }

    /// Code-masked `unsafe` occurrences in raw source (comments and string
    /// literals can never satisfy the scan): `(line index, trimmed line)`.
    fn unsafe_sites_in(src: &str) -> Vec<(usize, String)> {
        let bytes = src.as_bytes();
        let code = code_mask(src);
        let mut starts: Vec<usize> = vec![0];
        for (i, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                starts.push(i + 1);
            }
        }
        let mut out = Vec::new();
        let mut from = 0usize;
        while let Some(rel) = src[from..].find("unsafe") {
            let at = from + rel;
            from = at + 6;
            let before_ok =
                at == 0 || !bytes[at - 1].is_ascii_alphanumeric() && bytes[at - 1] != b'_';
            let after_ok = at + 6 >= bytes.len()
                || !bytes[at + 6].is_ascii_alphanumeric() && bytes[at + 6] != b'_';
            if !before_ok || !after_ok {
                continue;
            }
            if !code[at..at + 6].iter().all(|c| *c) {
                continue;
            }
            // Any whitespace may separate `unsafe` from its block: a newline
            // (or several) before `{` is still an unsafe SITE and must not
            // escape the scan (the old space/tab-only trim let `unsafe\n{`
            // through silently).
            let tail = src[at + 6..].trim_start();
            if !(tail.starts_with('{')
                || tail.starts_with("fn")
                || tail.starts_with("impl")
                || tail.starts_with("extern")
                || tail.starts_with("trait"))
            {
                continue;
            }
            let li = starts.partition_point(|s| *s <= at).saturating_sub(1);
            out.push((li, src.lines().nth(li).unwrap_or("").trim().to_string()));
        }
        out
    }

    /// The `// SAFETY:` justification must sit within the eight lines above
    /// the occurrence (the workspace convention for blocks and `unsafe fn`s).
    fn safety_declared(src: &str, line: usize) -> bool {
        let lines: Vec<&str> = src.lines().collect();
        let lo = line.saturating_sub(8);
        lines[lo..=line.min(lines.len().saturating_sub(1))]
            .iter()
            .any(|l| l.contains("SAFETY:"))
    }

    fn allow_attr_occurrences(src: &str) -> usize {
        let bytes = src.as_bytes();
        let code = code_mask(src);
        let mut n = 0usize;
        let mut from = 0usize;
        while let Some(rel) = src[from..].find("allow(unsafe_code)") {
            let at = from + rel;
            from = at + 1;
            if code[at..at + "allow(unsafe_code)".len()].iter().all(|c| *c) {
                n += 1;
            }
        }
        let _ = bytes;
        n
    }

    /// A file's unsafe-policy offenders: sites outside the enumerated
    /// locations, missing SAFETY comments, and count/documented mismatches.
    fn unsafe_policy_offenders(rel: &str, src: &str) -> Vec<String> {
        let normalized = normalize_rel(rel);
        let expected = UNSAFE_ALLOWED_FILES
            .iter()
            .find(|(file, _)| *file == normalized.as_str());
        let sites = unsafe_sites_in(src);
        let mut offenders = Vec::new();
        if sites.is_empty() {
            if let Some((_, want)) = expected {
                offenders.push(format!(
                    "{normalized}: documented unsafe location has zero unsafe sites (stale entry)"
                ));
                let _ = want;
            }
            return offenders;
        }
        match expected {
            None => {
                for (line, text) in &sites {
                    offenders.push(format!(
                        "{normalized}:{}: unsafe outside the enumerated authority locations: {text}",
                        line + 1
                    ));
                }
            }
            Some((_, want)) => {
                let seen = allow_attr_occurrences(src);
                if seen != *want {
                    offenders.push(format!(
                        "{normalized}: {seen} `allow(unsafe_code)` attribute(s), documented {want}; \
                         a new allow location requires updating the static policy"
                    ));
                }
                for (line, text) in &sites {
                    if !safety_declared(src, *line) {
                        offenders.push(format!(
                            "{normalized}:{}: unsafe without a preceding `// SAFETY:` comment: {text}",
                            line + 1
                        ));
                    }
                }
            }
        }
        offenders
    }

    #[test]
    fn unsafe_code_lives_only_in_the_enumerated_authority_locations() {
        let mut offenders = Vec::new();
        let mut scanned = 0usize;
        for rel in crate_rs_files_all() {
            let path = repo_root().join(&rel);
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            scanned += 1;
            offenders.extend(unsafe_policy_offenders(&rel, &src));
        }
        // Every documented location must exist on disk (stale entries are
        // red, never silently skipped).
        for (file, _) in UNSAFE_ALLOWED_FILES {
            if !repo_root().join(file).is_file() {
                offenders.push(format!("{file}: documented unsafe location does not exist"));
            }
        }
        assert_no_offenders(
            "unsafe/OS-boundary policy: ordinary crates must contain no `unsafe`; \
             authority locations are enumerated exactly and every unsafe block/function \
             needs a `// SAFETY:` comment",
            &offenders,
            scanned,
            50,
        );
    }

    #[test]
    fn unsafe_scan_fires_on_new_sites_allows_and_missing_justification() {
        // A new unsafe site in an ordinary crate is an offender.
        let planted = "pub fn f() {\n    unsafe { libc::kill(0, 0) }\n}\n";
        assert!(
            !unsafe_policy_offenders("crates/evil/src/lib.rs", planted).is_empty(),
            "a planted unsafe site outside the authority list must fire"
        );
        // The documented file with an extra UNJUSTIFIED site fires even
        // though its allow count is right.
        let unjustified = "pub fn f() {\n    unsafe { libc::kill(0, 0) }\n}\n";
        let file = "crates/session/src/actor.rs";
        let offenders = unsafe_policy_offenders(file, unjustified);
        assert!(
            offenders.iter().any(|o| o.contains("SAFETY:")),
            "missing SAFETY must fire: {offenders:?}"
        );
        // The same site WITH a SAFETY comment passes the site check (the
        // allow-count check is independent and needs the documented count).
        let justified = "pub fn f() {\n    // SAFETY: zero-signal probe on a live pid.\n    #[allow(unsafe_code)]\n    unsafe { libc::kill(0, 0) }\n}\n";
        let offenders = unsafe_policy_offenders(file, justified);
        assert!(
            !offenders.iter().any(|o| o.contains("SAFETY:")),
            "a justified site must not be a SAFETY offender: {offenders:?}"
        );
        // The audit blind spot: `unsafe` with its block brace on the NEXT
        // line (`unsafe\n{`) is still a site and must fire.
        let multiline = "pub fn f() {\n    unsafe\n    { libc::kill(0, 0) }\n}\n";
        assert_eq!(
            unsafe_sites_in(multiline).len(),
            1,
            "a newline-separated unsafe block is a site: {:?}",
            unsafe_sites_in(multiline)
        );
        assert!(
            !unsafe_policy_offenders("crates/evil/src/lib.rs", multiline).is_empty(),
            "the newline-separated unsafe block must fire the policy scan"
        );
        let multiline_justified = "pub fn f() {\n    // SAFETY: zero-signal probe on a live pid.\n    #[allow(unsafe_code)]\n    unsafe\n    { libc::kill(0, 0) }\n}\n";
        let offenders = unsafe_policy_offenders("crates/session/src/actor.rs", multiline_justified);
        assert!(
            !offenders.iter().any(|o| o.contains("SAFETY:")),
            "the newline form with SAFETY must pass the justification check: {offenders:?}"
        );
        // A comment that merely names unsafe never counts as a site.
        assert!(unsafe_sites_in("// unsafe { inside a comment }\nfn f() {}\n").is_empty());
        assert!(unsafe_sites_in("fn f() -> &'static str { \"unsafe { x }\" }\n").is_empty());
        assert!(
            unsafe_sites_in("fn f() -> &'static str { \"x\" } // unsafe fn trailing\n").is_empty()
        );
    }

    /// The workspace lint configuration itself: every member opts into the
    /// workspace `unsafe_code = "deny"`, and no member re-allows it at the
    /// package level (the only allows are the code-level, enumerated ones).
    #[test]
    fn workspace_lint_config_denies_unsafe_code_for_every_member() {
        let root_manifest =
            std::fs::read_to_string(repo_root().join("Cargo.toml")).expect("workspace Cargo.toml");
        assert!(
            root_manifest.contains("[workspace.lints.rust]"),
            "workspace lints must define the rust lint set"
        );
        assert!(
            root_manifest.contains("unsafe_code = \"deny\""),
            "the workspace must deny unsafe_code"
        );
        let members = {
            let at = root_manifest.find("members = [").expect("members list");
            let body = &root_manifest[at + "members = [".len()..];
            let end = body.find(']').expect("members terminator");
            body[..end]
                .split(',')
                .filter_map(|entry| {
                    let entry = entry.trim();
                    let entry = entry.strip_prefix('"')?.strip_suffix('"')?;
                    Some(entry.to_string())
                })
                .collect::<Vec<_>>()
        };
        assert!(members.len() > 40, "workspace member walk is not empty");
        for member in &members {
            let manifest_path = repo_root().join(member).join("Cargo.toml");
            let manifest = std::fs::read_to_string(&manifest_path)
                .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
            assert!(
                !manifest.contains("unsafe_code = \"allow\""),
                "{member}: a package-level unsafe_code allow would bypass the policy; \
                 use a narrow #[allow(unsafe_code)] in the enumerated location instead"
            );
            let inherits = manifest.contains("[lints]") && manifest.contains("workspace = true");
            let manual_deny = manifest.contains("unsafe_code = \"deny\"");
            assert!(
                inherits || manual_deny,
                "{member}: must opt into the workspace lints ([lints] workspace = true) or \
                 deny unsafe_code manually"
            );
        }
    }
}
