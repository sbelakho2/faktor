//! Static source-authority certification (audit 31/107-109).
//!
//! Fifteen structural invariants are locked by scanning the repository's
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
//! 6. **Constructor-site authority** (audits 30/57/83) — every daemon
//!    authority has exactly ONE canonical production construction in the
//!    canonical daemon builder (`crates/cli/src/graph.rs` or the
//!    `build_daemon*` functions in `crates/cli/src/main.rs`): the router
//!    service, acquisition planner/service, index service, process
//!    supervisor, semantic registry, budget ledger, SCM completion provider
//!    and TaskExecutor. Every other production site is an explicitly
//!    documented embedded-host/standalone entry; a new (or stale)
//!    construction site anywhere is a red test, never a review nit. Tests
//!    may build isolated units (test bodies and test-gated out-of-line
//!    modules are invisible), and planted-violation fixtures prove the
//!    exact-count rule fires.
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
//! 13. **Repository hygiene** (audit items 20-21) — the COMMITTED file set
//!     (`git ls-files`) carries no Python bytecode (`__pycache__/`,
//!     `*.py[cod]`), no compiled build artifacts (`*.o`, `*.rlib`,
//!     `*.dylib`, `*.vsix`, `*.jar`, …) and no opaque extensionless binary
//!     at or above 64 KiB. Deliberate binary fixtures are exact,
//!     justified, load-bearing allowlist entries (asserted non-stale), and
//!     the license authority is wired: canonical Apache-2.0 `LICENSE`
//!     pinned by SHA-256, `NOTICE`, every workspace member inheriting
//!     `license.workspace = true`, and the `deny.toml` policy step
//!     (`scripts/check-licenses.sh`) present in the trusted static lane.
//! 14. **No post-build authority replacement in the production-wiring
//!     certification** — `tests/production-wiring/src|tests/**` must reach
//!     the daemon through its own production builder; after the graph is
//!     built the tests may substitute EXTERNAL seams only (fake servers,
//!     clocks, credentials, transports, config). Installing/replacing a
//!     built authority (`set_*provider`, `replace_*`) or constructing a
//!     top-level subsystem service (executor, supervisor, ledger, runtime,
//!     session manager, provider registry, durable SCM store) turns the
//!     certification into a look-alike rig and is a red test. Exactly ONE
//!     explicitly named module is exempt: the manual adapter-contract
//!     certification, whose setter use is asserted load-bearing and
//!     non-stale.
//! 15. **Prepared writer jobs execute SQL only** (audit items 8, 4) — a
//!     production writer job closure (`.writer.execute` / `execute_raw` /
//!     `Store::writer_debug_job`) must not contain filesystem I/O,
//!     serialization (`serde_json`, `to_vec`/`from_str`/`.json`), hashing,
//!     digest/string materialization (`.to_hex(`/`.to_string()`), sleeps or
//!     wall-clock acquisition (`now_ms(`/`SystemTime`/`Utc::now`): those run
//!     on the single writer owner and stall every domain's mutations (and the
//!     bounded shutdown). Preparation — including the timestamp — happens on
//!     the caller's thread BEFORE enqueueing.
//!     `Instant::now()` stays legal inside a job: it is the writer service's
//!     own monotonic queue/transaction-latency telemetry, not a row clock.
//!     Grandfathered call sites are exact-line allowlisted, asserted
//!     load-bearing and non-stale, and a planted violation in every family
//!     fails the scan.
//!
//! Scanning methodology: per file, comments and string literals are masked
//! out and every `#[cfg(...)]`-gated item that can never compile in a
//! non-test build (`#[cfg(test)]`, `#[cfg(all(test, unix))]`, …) is
//! removed with brace-matched ranges, so markers in tests, docs or
//! examples can never certify production code. Out-of-line modules declared
//! ONLY as test-gated `mod`/`#[path]` modules (any name: `tests.rs`,
//! `*_tests.rs`, `acquire_certification.rs`, `test_http.rs`,
//! `modelcheck.rs`) are skipped by the production scans. The machinery is
//! itself adversarially tested against synthetic sources.

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
        // Test-only out-of-line modules carry no production ranges: the
        // module-decomposition move keeps test code out-of-line, so the
        // `#[cfg(test)] mod` masking inside a file is not enough.
        let kept = if is_test_file(rel) {
            Vec::new()
        } else {
            kept_ranges(&src, &code)
        };
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
    /// Normalize a repo-relative path: Windows separators become `/` and
    /// `.`/`..` segments are collapsed lexically, so a `#[path = "../x.rs"]`
    /// out-of-line test module resolves to the same key the file walk
    /// produces (audit-17 module splits keep tests next to or beside the
    /// module they cover).
    fn normalize_rel(rel: &str) -> String {
        let rel = rel.replace('\\', "/");
        let mut out: Vec<&str> = Vec::new();
        for seg in rel.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    if out.pop().is_none() {
                        out.push("..");
                    }
                }
                other => out.push(other),
            }
        }
        out.join("/")
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
    /// 2. Any OTHER file is test code only when every out-of-line `mod`
    ///    declaration that includes it carries a test-gated `#[cfg(...)]`
    ///    attribute (`mod <stem>;` with only test-gated declarations, or
    ///    `#[path = "<file>"]` with test-gated declarations). The include is
    ///    verified on disk, so a production-compiled module stays scanned
    ///    even when its name ends in `_tests.rs`; the same rule now covers
    ///    test-gated out-of-line modules with ordinary names
    ///    (`acquire_certification.rs`, `test_http.rs`, `modelcheck.rs`),
    ///    which the historical name check mis-scanned as production.
    ///
    /// Windows separators never matter: `rel` is normalized before any
    /// comparison and `repo_root().join` accepts `/` on every platform.
    fn is_test_file(rel: &str) -> bool {
        let rel = normalize_rel(rel);
        if rel.contains("/tests/") {
            return true;
        }
        test_only_out_of_line_modules().contains(&rel)
    }

    /// The repo-relative files that are declared ONLY as test-gated
    /// out-of-line modules, computed once from the declaring sources (every
    /// declaring file is itself inside the walk). A file with at least one
    /// non-test declaration stays production code.
    fn test_only_out_of_line_modules() -> &'static std::collections::HashSet<String> {
        static SET: std::sync::OnceLock<std::collections::HashSet<String>> =
            std::sync::OnceLock::new();
        SET.get_or_init(|| {
            let mut states: std::collections::HashMap<String, (bool, bool)> =
                std::collections::HashMap::new();
            for rel in walk_crate_sources() {
                let Ok(src) = std::fs::read_to_string(repo_root().join(&rel)) else {
                    continue;
                };
                for (at, stem) in out_of_line_mods(&src) {
                    let attrs = declaration_attrs(&src, at);
                    let declared = declared_file_path(&rel, &stem, &attrs);
                    let entry = states.entry(declared).or_insert((false, false));
                    if has_test_gated_cfg(&attrs) {
                        entry.0 = true;
                    } else {
                        entry.1 = true;
                    }
                }
            }
            states
                .into_iter()
                .filter(|(_, (test_gated, production))| *test_gated && !*production)
                .map(|(file, _)| file)
                .collect()
        })
    }

    /// `(attribute offset, module stem)` for every out-of-line `mod <stem>;`
    /// declaration (comments/strings masked; inline `mod <stem> { … }` bodies
    /// are not declarations of separate files and are skipped).
    fn out_of_line_mods(src: &str) -> Vec<(usize, String)> {
        let bytes = src.as_bytes();
        let mut out = Vec::new();
        for at in code_needle_offsets(src, "mod ") {
            if at > 0 {
                let prev = bytes[at - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' {
                    continue;
                }
            }
            let mut end = at + "mod ".len();
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end == at + "mod ".len() {
                continue;
            }
            let stem = std::str::from_utf8(&bytes[at + "mod ".len()..end])
                .unwrap_or("")
                .to_string();
            let mut k = end;
            while k < bytes.len() && (bytes[k] as char).is_ascii_whitespace() {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b';' {
                out.push((at, stem));
            }
        }
        out
    }

    /// The `path = "<file>"` value of an attribute run, when present.
    fn attr_path_value(attrs: &str) -> Option<String> {
        let at = attrs.find("path = \"")?;
        let rest = &attrs[at + "path = \"".len()..];
        let end = rest.find('"')?;
        Some(normalize_rel(&rest[..end]))
    }

    /// Resolve the file a declaration refers to (Rust module rules):
    /// `#[path = "…"]` is relative to the declaring file's directory; a
    /// plain `mod <stem>;` in a crate root (`main.rs`/`lib.rs`/`mod.rs`)
    /// resolves next to it, and in any other module file (`foo.rs`)
    /// resolves into its module directory (`foo/<stem>.rs`).
    fn declared_file_path(declaring_rel: &str, stem: &str, attrs: &str) -> String {
        let Some((dir, name)) = declaring_rel.rsplit_once('/') else {
            return normalize_rel(&format!("{stem}.rs"));
        };
        if let Some(path) = attr_path_value(attrs) {
            return normalize_rel(&format!("{dir}/{path}"));
        }
        if name == "main.rs" || name == "lib.rs" || name == "mod.rs" {
            normalize_rel(&format!("{dir}/{stem}.rs"))
        } else {
            let module_dir = name.strip_suffix(".rs").unwrap_or(name);
            normalize_rel(&format!("{dir}/{module_dir}/{stem}.rs"))
        }
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

    /// The attribute run of one declaration whose `at` points at the `mod`
    /// keyword: a visibility qualifier (`pub`, `pub(crate)`, `pub(in path)`)
    /// between the attributes and `mod` is skipped, so
    /// `#[cfg(test)] #[path = "x.rs"] pub(crate) mod tests;` is recognized
    /// as test-gated (the historical helper looked at the text before `mod`
    /// only and missed the visibility form).
    fn declaration_attrs(src: &str, at: usize) -> String {
        let bytes = src.as_bytes();
        let mut end = src[..at].trim_end().len();
        if end > 0 && bytes[end - 1] == b')' {
            let mut depth = 0i32;
            let mut i = end;
            let mut open = None;
            while i > 0 {
                i -= 1;
                match bytes[i] {
                    b')' => depth += 1,
                    b'(' => {
                        depth -= 1;
                        if depth == 0 {
                            open = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(open) = open {
                let mut j = open;
                while j > 0 && (bytes[j - 1] as char).is_ascii_whitespace() {
                    j -= 1;
                }
                let word_end = j;
                while j > 0 && (bytes[j - 1].is_ascii_alphanumeric() || bytes[j - 1] == b'_') {
                    j -= 1;
                }
                if &src[j..word_end] == "pub" {
                    end = j;
                }
            }
        } else {
            let mut j = end;
            while j > 0 && (bytes[j - 1] as char).is_ascii_whitespace() {
                j -= 1;
            }
            let word_end = j;
            while j > 0 && (bytes[j - 1].is_ascii_alphanumeric() || bytes[j - 1] == b'_') {
                j -= 1;
            }
            if &src[j..word_end] == "pub" {
                end = j;
            }
        }
        let tail = src[..end].trim_end();
        preceding_attributes(src, tail.len())
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
    /// production sites, with exact per-file occurrence counts.
    ///
    /// Audit 30 adds the eight authorities whose ONE canonical construction
    /// must live in the canonical daemon builder (`crates/cli/src/graph.rs`
    /// / the `build_daemon*` functions in `crates/cli/src/main.rs`): router
    /// service, acquisition planner/service, index service, supervisor,
    /// semantic registry, budget ledger, SCM completion provider and
    /// TaskExecutor. Tests may build isolated units (test bodies and
    /// test-gated out-of-line modules are invisible to this scan); a
    /// production module outside the builder may only construct one through
    /// an explicitly documented embedded-host/standalone entry, and a new
    /// (or stale) site anywhere is a red test, never a review nit.
    /// `one_production_constructor_per_daemon_authority` below asserts the
    /// canonical-builder half and proves the scan fires on planted
    /// violations.
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
    /// - `ProcessSupervisor::new`: the two daemon entries (the sync
    ///   `build_daemon_with_acquisition_planner` path that `build_daemon`
    ///   delegates to, and `build_daemon_with_mcp_inner`), each handing the
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
            &[("crates/agent/src/runtime/turn/state.rs", 1)],
        ),
        (
            "DurableBudgetLedger::new",
            &[
                ("crates/cli/src/daemon/builder.rs", 1),
                ("crates/orchestrator/src/task_executor/mod.rs", 2),
                ("crates/server/src/api/deps.rs", 1),
                ("crates/session/src/manager.rs", 1),
                ("crates/session/src/task/mod.rs", 2),
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
                ("crates/cli/src/daemon/builder.rs", 2),
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
                ("crates/index/src/service/mod.rs", 1),
                ("crates/verify/src/exec.rs", 1),
                ("crates/verify/src/inventory.rs", 1),
                ("crates/cli/src/main.rs", 1),
            ],
        ),
        // --------------------------------------------- the eight daemon authorities
        // (audit 30). For every authority the ONE canonical production
        // construction happens in the daemon builder (crates/cli/src/main.rs
        // or crates/cli/src/graph.rs); every other production site is an
        // explicitly documented embedded-host/standalone entry. A zero-site
        // marker pins a compatibility constructor that production must never
        // use (tests may).
        //
        // Router service: the canonical builder's two mode arms
        // (`build_router_service_with_outcomes`); no production site may
        // construct the legacy variants (`new`, `with_pricing`,
        // `with_outcomes`, `with_pricing_and_outcomes` are test-only there).
        (
            "RouterService::with_route_candidates",
            &[("crates/cli/src/graph.rs", 1)],
        ),
        (
            "RouterService::with_pinned_route_candidates",
            &[("crates/cli/src/graph.rs", 1)],
        ),
        ("RouterService::new", &[]),
        ("RouterService::with_pricing(", &[]),
        ("RouterService::with_outcomes(", &[]),
        ("RouterService::with_pricing_and_outcomes(", &[]),
        // Acquisition planner/service: the planner's default is constructed
        // ONLY by the commerce service's own disabled/open constructors
        // (`disabled_with_planner` and the defaulted `open_with_planner`
        // argument); the daemon wires the service through the canonical
        // `open_commerce_service_with_planner`, and the marketplace login
        // command uses the explicit `open` (no planner seam) path. The
        // planner's `new` is test-only in production terms.
        ("AcquisitionPlanner::new", &[]),
        (
            "AcquisitionPlanner::default",
            &[("crates/commerce/src/service.rs", 2)],
        ),
        (
            "CommerceSourceService::open_with_planner",
            &[("crates/cli/src/daemon/builder.rs", 1)],
        ),
        (
            "CommerceSourceService::open(",
            &[("crates/cli/src/tools_market.rs", 1)],
        ),
        ("CommerceSourceService::disabled_with_planner", &[]),
        // Index service: the canonical daemon builder constructs it in
        // crates/cli/src/main.rs; the ONLY other production site is the
        // agent runtime's embedded-host path (`index_service()`), which
        // falls back to a standalone `open` when no supervisor is injected.
        (
            "IndexService::open_with_supervisor",
            &[
                ("crates/agent/src/runtime/retrieval.rs", 1),
                ("crates/cli/src/daemon/builder.rs", 1),
            ],
        ),
        (
            "IndexService::open(",
            &[("crates/agent/src/runtime/retrieval.rs", 1)],
        ),
        ("IndexService::new", &[]),
        // SCM completion provider: exactly ONE production site — the
        // canonical builder's `wire_completion_scm` (the adapter's own
        // contract tests construct it; production must not).
        (
            "GitHubCompletionScm::new",
            &[("crates/cli/src/daemon/wiring.rs", 1)],
        ),
        // TaskExecutor: the canonical daemon builder constructs ONE executor
        // and hands it to the graph; the server crate's `ServerDeps::new`
        // embedded-host seam is the only other production construction. The
        // direct test-harness constructor must never appear in production.
        (
            "TaskExecutor::new(",
            &[
                ("crates/cli/src/daemon/builder.rs", 1),
                ("crates/server/src/api/deps.rs", 1),
            ],
        ),
        ("TaskExecutor::new_owner_direct_for_test_harness(", &[]),
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

    /// Audit 30: exactly ONE canonical production construction per daemon
    /// authority, in the canonical daemon builder (`crates/cli/src/graph.rs`
    /// or a `build_daemon*` function in `crates/cli/src/main.rs`), with the
    /// exact-count scan (scan 4) as the enforcement. Tests may build
    /// isolated units: a test body or a test-gated out-of-line module is
    /// invisible, while a production module that constructs an authority
    /// outside its documented sites is a red test. The planted fixtures
    /// below prove the offender rule fires (and does not fire on the
    /// sanctioned shapes), so the invariant can never pass vacuously.
    #[test]
    fn one_production_constructor_per_daemon_authority() {
        const AUTHORITIES: &[(&str, &[&str])] = &[
            (
                "router_service",
                &[
                    "RouterService::with_route_candidates",
                    "RouterService::with_pinned_route_candidates",
                    "RouterService::new",
                    "RouterService::with_pricing(",
                    "RouterService::with_outcomes(",
                    "RouterService::with_pricing_and_outcomes(",
                ],
            ),
            (
                "acquisition_planner_service",
                &[
                    "AcquisitionPlanner::new",
                    "AcquisitionPlanner::default",
                    "CommerceSourceService::open_with_planner",
                    "CommerceSourceService::open(",
                    "CommerceSourceService::disabled_with_planner",
                ],
            ),
            (
                "index_service",
                &[
                    "IndexService::open_with_supervisor",
                    "IndexService::open(",
                    "IndexService::new",
                ],
            ),
            ("process_supervisor", &["ProcessSupervisor::new"]),
            ("semantic_registry", &["SemanticProviderRegistry::new"]),
            ("budget_ledger", &["DurableBudgetLedger::new"]),
            ("scm_completion_provider", &["GitHubCompletionScm::new"]),
            (
                "task_executor",
                &[
                    "TaskExecutor::new(",
                    "TaskExecutor::new_owner_direct_for_test_harness(",
                ],
            ),
        ];
        const CANONICAL_BUILDER: &[&str] = &[
            "crates/cli/src/main.rs",
            "crates/cli/src/graph.rs",
            "crates/cli/src/daemon/builder.rs",
            "crates/cli/src/daemon/wiring.rs",
        ];

        for (authority, markers) in AUTHORITIES {
            for marker in markers.iter().copied() {
                assert!(
                    CONSTRUCTOR_SITES
                        .iter()
                        .any(|(documented, _)| *documented == marker),
                    "{authority}: marker '{marker}' has no documented site in \
                     CONSTRUCTOR_SITES (the exact-count scan would never fire for it)"
                );
            }
            let canonical = markers.iter().copied().any(|marker| {
                let sites = CONSTRUCTOR_SITES
                    .iter()
                    .find(|(documented, _)| *documented == marker)
                    .map(|(_, sites)| *sites)
                    .unwrap_or(&[]);
                sites
                    .iter()
                    .any(|(file, _)| CANONICAL_BUILDER.contains(file))
            });
            assert!(
                canonical,
                "{authority}: no documented construction site in the canonical daemon \
                 builder ({CANONICAL_BUILDER:?}); every daemon authority must be assembled \
                 by the one canonical builder"
            );
        }

        // Planted violation: a production module outside the documented
        // sites constructing the authority is an offender (the exact-count
        // rule of scan 4, applied to a synthetic source).
        let planted = "pub fn shadow() -> TaskExecutor {\n    \
                       let executor = TaskExecutor::new();\n    executor\n}\n";
        let offenders = constructor_site_offenders(
            "crates/session/src/planted_authority.rs",
            planted,
            "TaskExecutor::new(",
            &[
                ("crates/cli/src/daemon/builder.rs", 1),
                ("crates/server/src/api/deps.rs", 1),
            ],
        );
        assert!(
            !offenders.is_empty(),
            "a production construction outside the documented sites must fire"
        );
        // The sanctioned shape (the canonical builder file, exact count) is
        // clean.
        let sanctioned = constructor_site_offenders(
            "crates/cli/src/daemon/builder.rs",
            "fn build() { let _ = TaskExecutor::new(); }\n",
            "TaskExecutor::new(",
            &[
                ("crates/cli/src/daemon/builder.rs", 1),
                ("crates/server/src/api/deps.rs", 1),
            ],
        );
        assert!(
            sanctioned.is_empty(),
            "the canonical builder's own construction is not an offender: {sanctioned:?}"
        );
        // Tests may build isolated units: a test body and a test-gated
        // out-of-line module are invisible to the production scan, while the
        // same marker in production text is visible.
        let test_gated = "pub fn build() {}\n#[cfg(test)]\nmod tests {\n    \
                          fn t() { let _ = TaskExecutor::new_owner_direct_for_test_harness(); }\n}\n";
        let f = synthetic_file("crates/session/src/planted_authority.rs", test_gated);
        assert!(
            find_markers(&f, &["TaskExecutor::new_owner_direct_for_test_harness("]).is_empty(),
            "#[cfg(test)] bodies build isolated units and must stay invisible"
        );
        let commented = "// TaskExecutor::new( in a comment\n\
                         pub fn note() -> &'static str { \"TaskExecutor::new( in a string\" }\n";
        let f = synthetic_file("crates/session/src/planted_authority.rs", commented);
        assert!(
            find_markers(&f, &["TaskExecutor::new("]).is_empty(),
            "comments and string literals can never instantiate an authority"
        );

        assert!(
            is_test_file("crates/cli/src/acquire_certification.rs"),
            "test-gated #[path] modules with ordinary names are test-only"
        );
        assert!(
            is_test_file("crates/cli/src/test_http.rs"),
            "test-gated out-of-line modules are test-only"
        );
        assert!(
            is_test_file("crates/scheduler/src/modelcheck.rs"),
            "test-gated out-of-line modules are test-only"
        );
        assert!(!is_test_file("crates/cli/src/main.rs"));
        assert!(!is_test_file("crates/cli/src/tools_market.rs"));
        assert!(!is_test_file("crates/orchestrator/src/shadow.rs"));
        assert_eq!(
            out_of_line_mods("pub fn f() {}\n#[cfg(test)]\nmod helpers;\n").len(),
            1,
            "the test-gated out-of-line declaration must be recognized"
        );
        assert_eq!(
            out_of_line_mods("pub mod x;\nmod y;\n").len(),
            2,
            "production out-of-line declarations must be recognized"
        );
    }

    /// The exact-count offender rule of scan 4, reusable on synthetic
    /// sources (the planted-violation proof): the documented count applies
    /// to the source's own path; a source with no documented entry is an
    /// offender on every hit.
    fn constructor_site_offenders(
        rel: &str,
        src: &str,
        marker: &str,
        documented: &[(&str, usize)],
    ) -> Vec<String> {
        let f = synthetic_file(rel, src);
        let want = documented
            .iter()
            .find(|(file, _)| *file == rel)
            .map(|(_, want)| *want)
            .unwrap_or(0);
        find_markers(&f, &[marker])
            .iter()
            .skip(want)
            .map(|(line, text)| format!("{rel}:{line}: {text} [undocumented {marker}]"))
            .collect()
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
        "crates/orchestrator/src/task_executor/mod.rs",
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
        "crates/session/src/task/mod.rs",
        "crates/session/src/ledger/mod.rs",
        "crates/orchestrator/src/merge.rs",
        "crates/orchestrator/src/task_executor/mod.rs",
        "crates/orchestrator/src/task_executor/settlement.rs",
        "crates/orchestrator/src/task_executor/integration.rs",
        "crates/orchestrator/src/task_executor/verification.rs",
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
            "crates/session/src/task/mod.rs",
            "const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;",
        ),
        (
            "crates/session/src/task/mod.rs",
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
            "crates/session/src/task/mod.rs",
            "        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;\n",
        );
        assert!(
            authority_digest_offenders("crates/session/src/task/mod.rs", &allowlisted).is_empty(),
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
    // scan 15: writer job closures execute SQL only (audit item 8)
    // ------------------------------------------------------------------

    /// Call shapes that hand a command closure to the durable writer service
    /// (or its `#[doc(hidden)]` test seam). Anything inside that call's
    /// parentheses runs on the SINGLE writer owner thread, where it stalls
    /// every domain's mutations and the bounded shutdown.
    const WRITER_JOB_CALL_MARKERS: &[&str] = &[
        ".writer.execute(",
        ".writer.execute_raw(",
        ".writer_debug_job(",
        // Receiver spelled without a leading dot (`store.writer.execute(`),
        // and chains rustfmt may unfold.
        "writer.execute(",
        "writer.execute_raw(",
        "writer_debug_job(",
    ];

    /// Work a writer job closure must never perform — it belongs on the
    /// caller's thread BEFORE enqueueing. Five families:
    ///
    /// * filesystem I/O (`std::fs`, `OpenOptions`, `File::open/create`,
    ///   temp files, read/write/remove helpers),
    /// * serialization (`serde_json`, `to_vec`, `from_str`, `.json(`),
    /// * hashing (`Sha256`/`sha2`/`blake3`/`Digest`/`Hasher`) and sleeps,
    /// * digest/string materialization (`.to_hex(`, `.to_string()`): the
    ///   prepared representation is built on the caller's thread and the job
    ///   binds it as-is (`&str`/`&String` parameters stay borrows),
    /// * wall-clock acquisition (`now_ms(`, `SystemTime`, `Utc::now`):
    ///   the timestamp is part of the caller's preparation. `Instant::now()`
    ///   is deliberately NOT forbidden — it is a monotonic elapsed-time
    ///   probe, and the writer service itself measures queue/transaction
    ///   latency with it on the owner thread.
    ///
    /// Markers are matched on the code mask (strings/comments invisible) and
    /// must begin at a non-identifier boundary, so `faktor_fs::` and
    /// `cas_hash` identifiers do not fire.
    const WRITER_JOB_FORBIDDEN_MARKERS: &[&str] = &[
        // filesystem I/O
        "std::fs::",
        "fs::",
        "OpenOptions",
        "File::open",
        "File::create",
        "File::options",
        "read_to_string",
        "write_all",
        "create_dir",
        "remove_file",
        "remove_dir",
        "canonicalize",
        "NamedTempFile",
        "sync_all",
        "sync_data",
        // serialization
        "serde_json",
        ".to_vec(",
        "::to_vec(",
        ".from_str(",
        "::from_str(",
        ".json(",
        "json!(",
        // indirect decode helpers: a closure that merely NAMES one of these
        // still parses/serializes on the writer owner (the call site is a
        // marker-free identifier, so the lexical scanner needs them spelled
        // out here)
        "task_row_map(",
        "parse_json(",
        // hashing
        "Sha256",
        "Sha512",
        "sha2::",
        "blake3::",
        "Digest",
        "Hasher",
        ".digest(",
        // digest text materialization (`FileHash::to_hex` allocates a String;
        // prepare it on the caller's thread)
        ".to_hex(",
        // string formatting on the writer owner (prepared values bind as-is;
        // an owned copy of an already-prepared String is `.clone()`)
        ".to_string()",
        // sleeps
        "thread::sleep",
        "sleep(",
        // wall clocks (Instant::now is owner-side latency telemetry, allowed)
        "now_ms(",
        "SystemTime",
        "Utc::now",
    ];

    /// Writer-job closures that still prepare nothing. The ratchet is
    /// COMPLETE: every audited call site now prepares its serialization,
    /// construction, filesystem work and hashing on the caller's thread, so
    /// this list is EMPTY and must stay empty. `writer_job_allowlist_is_empty`
    /// fails loudly the moment an entry is re-added — re-adding one is a
    /// regression; fix the call site instead. The scan's forbidden-marker
    /// coverage is unchanged: `crates/store/src/connection.rs` and
    /// `writer.rs` (the owned writer core) stay prepared.
    const WRITER_JOB_ALLOWLIST: &[(&str, &str)] = &[];

    /// True for bytes that continue a Rust identifier (used to keep marker
    /// matches from firing inside longer paths/identifiers).
    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    /// The byte span of every writer-job call in one file: from the call's
    /// `(` through its matching `)` (parens balanced over the code mask, so
    /// strings/comments cannot unbalance it).
    ///
    /// Rustfmt may UNFOLD a long chain as `self.writer\n    .execute(`, so the
    /// receiver search skips whitespace between `writer` and
    /// `.execute(`/`.execute_raw(` — otherwise a reformatted call silently
    /// drops out of the scan (the proof gap audit finding 4 closed).
    fn writer_job_spans(f: &File<'_>) -> Vec<(usize, usize)> {
        let bytes = f.src.as_bytes();
        let mut opens: Vec<usize> = Vec::new();
        for &marker in WRITER_JOB_CALL_MARKERS {
            for at in find_marker_offsets(f, marker) {
                let open = at + marker.len() - 1;
                debug_assert_eq!(bytes[open], b'(');
                opens.push(open);
            }
        }
        for at in find_marker_offsets(f, "writer") {
            let mut i = at + "writer".len();
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            for suffix in [".execute(", ".execute_raw(", "_debug_job("] {
                if f.src[i..].starts_with(suffix) {
                    opens.push(i + suffix.len() - 1);
                    break;
                }
            }
        }
        opens.sort_unstable();
        opens.dedup();
        let mut spans = Vec::new();
        for open in opens {
            let mut depth = 0usize;
            let mut i = open;
            while i < bytes.len() {
                if f.code[i] {
                    match bytes[i] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
            spans.push((open, (i + 1).min(bytes.len())));
        }
        spans
    }

    /// Every scan-15 offender of one production file: a forbidden marker
    /// inside a writer-job call span, minus the exact-line allowlist.
    fn writer_job_offenders(f: &File<'_>) -> Vec<String> {
        let spans = writer_job_spans(f);
        if spans.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(usize, String, &'static str)> = Vec::new();
        for &marker in WRITER_JOB_FORBIDDEN_MARKERS {
            let mb = marker.as_bytes();
            let mut pos = 0usize;
            while let Some(rel) = f.src[pos..].find(marker) {
                let at = pos + rel;
                let in_span = spans.iter().any(|(a, z)| at >= *a && at + mb.len() <= *z);
                let in_code = f.code[at..at + mb.len()].iter().all(|c| *c);
                // Identifier-boundary rule applies only to markers that start
                // with an identifier byte (`fs::`, `serde_json`, `sleep(`);
                // method markers (`.to_vec(`, `::from_str(`) legitimately
                // follow an identifier.
                let starts_ident = is_ident_byte(mb[0]);
                let boundary = at == 0 || !starts_ident || !is_ident_byte(f.src.as_bytes()[at - 1]);
                if in_span && in_code && boundary {
                    let line = line_of(f.src, at);
                    let text = trim_line(f.src, at);
                    let allowlisted = WRITER_JOB_ALLOWLIST
                        .iter()
                        .any(|(p, t)| *p == f.rel && *t == text);
                    if !allowlisted {
                        hits.push((line, text, marker));
                    }
                }
                pos = at + mb.len();
            }
        }
        hits.sort();
        hits.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        hits.into_iter()
            .map(|(line, text, marker)| {
                format!(
                    "{}:{line}: {text}  [forbidden `{marker}` runs on the single writer \
                     owner; prepare it BEFORE enqueueing]",
                    f.rel
                )
            })
            .collect()
    }

    /// Audit items 8/4: production writer job closures execute SQLite work
    /// only. Serialization, filesystem I/O, hashing, sleeps and wall-clock
    /// acquisition all belong on the caller's thread BEFORE the command is
    /// enqueued; inside the closure they stall every other domain (and the
    /// bounded shutdown) behind the single owner thread.
    #[test]
    fn prepared_writer_jobs_never_do_fs_serialization_hashing_sleeps_or_clocks() {
        let mut offenders: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        for rel in walk_crate_sources() {
            if is_test_file(&rel) {
                continue; // out-of-line test bodies are invisible
            }
            let Some(f) = load(&rel) else {
                continue;
            };
            scanned += 1;
            offenders.extend(writer_job_offenders(&f));
        }
        assert_no_offenders(
            "writer-job scan: a production writer job closure (.writer.execute / \
             .writer.execute_raw / .writer_debug_job) contains filesystem I/O, \
             serialization, hashing, digest/string materialization, a sleep or \
             wall-clock acquisition; prepare inputs and the timestamp on the \
             caller's thread and keep the closure SQL-only",
            &offenders,
            scanned,
            50,
        );
    }

    /// Planted-fixture proof: every forbidden family fires inside a writer
    /// job (including wall-clock acquisition and digest/string
    /// materialization), the call-shape variants are all covered, and the
    /// prepared shapes (work AND the timestamp before the call, SQL inside),
    /// `Instant::now()` latency telemetry plus `#[cfg(test)]` code pass. The
    /// machinery is not vacuously green.
    #[test]
    fn writer_job_scan_fires_on_planted_unprepared_work() {
        for (family, src) in [
            (
                "fs write",
                "fn f() { self.writer.execute(\"x\", move |conn| { std::fs::write(\"/tmp/z\", b\"y\")?; Ok(()) }) }\n",
            ),
            (
                "fs open options",
                "fn f() { self.writer.execute(\"x\", move |conn| { let _ = OpenOptions::new().write(true).open(\"/tmp/z\"); Ok(()) }) }\n",
            ),
            (
                "serde_json",
                "fn f(v: Value) { self.writer.execute(\"x\", move |conn| { let s = serde_json::to_string(&v)?; Ok(s) }) }\n",
            ),
            (
                "to_vec",
                "fn f() { self.writer.execute(\"x\", move |conn| { let b = v.to_vec(); Ok(b) }) }\n",
            ),
            (
                "from_str",
                "fn f(s: String) { self.writer.execute(\"x\", move |conn| { let t: T = T::from_str(&s)?; Ok(t) }) }\n",
            ),
            (
                "json macro",
                "fn f() { self.writer.execute(\"x\", move |conn| { let v = serde_json::json!({\"a\": 1}); Ok(v) }) }\n",
            ),
            (
                "hash",
                "fn f(b: Vec<u8>) { self.writer.execute(\"x\", move |conn| { let d = Sha256::digest(&b); Ok(d) }) }\n",
            ),
            (
                "to_hex",
                "fn f(h: FileHash) { self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![h.to_hex()])?; Ok(()) }) }\n",
            ),
            (
                "string materialization",
                "fn f(s: String) { self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![s.to_string()])?; Ok(()) }) }\n",
            ),
            (
                "sleep",
                "fn f() { self.writer.execute(\"x\", move |conn| { std::thread::sleep(Duration::from_millis(5)); Ok(()) }) }\n",
            ),
            (
                "store clock",
                "fn f() { self.writer.execute(\"x\", move |conn| { let now = now_ms(); conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![now])?; Ok(()) }) }\n",
            ),
            (
                "system clock",
                "fn f() { self.writer.execute(\"x\", move |conn| { let now = SystemTime::now(); Ok(()) }) }\n",
            ),
            (
                "system clock epoch",
                "fn f() { self.writer.execute(\"x\", move |conn| { let _ = SystemTime::UNIX_EPOCH; Ok(()) }) }\n",
            ),
            (
                "utc clock",
                "fn f() { self.writer.execute(\"x\", move |conn| { let now = Utc::now(); Ok(()) }) }\n",
            ),
            (
                "debug seam",
                "fn f() { store.writer_debug_job(\"x\", move |conn| { std::fs::write(\"/tmp/z\", b\"y\"); }) }\n",
            ),
            (
                "raw seam",
                "fn f(s: String) { self.writer.execute_raw(\"x\", move |conn| { serde_json::from_str::<T>(&s).unwrap() }) }\n",
            ),
            (
                "indirect task row decode",
                "fn f(row: &Row) { self.writer.execute(\"x\", move |conn| { let t = task_row_map(row, sid)?; Ok(t) }) }\n",
            ),
            (
                "indirect json parse",
                "fn f(raw: String) { self.writer.execute(\"x\", move |conn| { let s: T = parse_json(&label, &raw)?; Ok(s) }) }\n",
            ),
        ] {
            let f = synthetic_file("crates/store/src/evil.rs", src);
            assert!(
                !writer_job_offenders(&f).is_empty(),
                "the planted {family} violation must fire: {src}"
            );
        }
        for src in [
            // Preparation BEFORE the call: serialized JSON captured outside.
            "fn f(v: Value) { let json = v.to_string(); self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![json])?; Ok(()) }) }\n",
            // Hashing before the call, digest bytes only inside.
            "fn f(b: Vec<u8>) { let d = Sha256::digest(&b); self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![d])?; Ok(()) }) }\n",
            // Digest text materialized before the call: only the prepared
            // String crosses into the closure.
            "fn f(h: FileHash) { let hex = h.to_hex(); self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![hex])?; Ok(()) }) }\n",
            // String materialization before the call: the prepared String
            // crosses, and a prepared borrowed `&str` binds as-is.
            "fn f(n: u64) { let text = n.to_string(); self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![text])?; Ok(()) }) }\n",
            "fn f(s: String) { self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![s.as_str()])?; Ok(()) }) }\n",
            // Clock-derived values captured before the call (no clock type
            // crosses into the closure).
            "fn f(now: i64) { self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![now])?; Ok(()) }) }\n",
            // Filesystem work before the call, SQL-only inside.
            "fn f() -> std::io::Result<()> { std::fs::create_dir_all(\"/tmp\")?; self.writer.execute(\"x\", move |conn| { conn.execute(\"DELETE FROM t\", [])?; Ok(()) }) }\n",
            // cfg(test) code can never certify production: invisible.
            "#[cfg(test)]\nmod tests { fn t() { self.writer.execute(\"x\", move |conn| { std::fs::read_to_string(\"/tmp/z\") }); } }\n",
            // A writer call with no forbidden work anywhere.
            "fn f() { self.writer.execute(\"x\", move |conn| { conn.execute(\"DELETE FROM t\", [])?; Ok(()) }) }\n",
            // Caller-side clock preparation: the timestamp is captured BEFORE
            // the call and only the value crosses into the closure.
            "fn f() { let now = now_ms(); self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![now])?; Ok(()) }) }\n",
            // Owner-side monotonic latency telemetry (Instant) is deliberate.
            "fn f() { self.writer.execute(\"x\", move |conn| { let started = Instant::now(); conn.execute(\"DELETE FROM t\", [])?; let _ = started.elapsed(); Ok(()) }) }\n",
            // Caller-side decode helpers: the whole-row decode and the JSON
            // parse happen BEFORE the enqueue and only prepared values cross
            // into the closure — the same helper names are not flagged.
            "fn f(row: &Row) { let t = task_row_map(row, sid)?; self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![t.state])?; Ok(()) }) }\n",
            "fn f(raw: String) { let s: T = parse_json(&label, &raw)?; self.writer.execute(\"x\", move |conn| { conn.execute(\"INSERT INTO t(x) VALUES (?1)\", params![s])?; Ok(()) }) }\n",
        ] {
            let f = synthetic_file("crates/store/src/good.rs", src);
            assert!(
                writer_job_offenders(&f).is_empty(),
                "the prepared fixture must pass: {src} -> {:?}",
                writer_job_offenders(&f)
            );
        }
        // The ratchet is empty: even a newly planted unprepared job in a
        // former allowlisted file has no escape hatch left and must fire.
        let planted = synthetic_file(
            "crates/store/src/ledger.rs",
            "fn f() { self.writer.execute(\"x\", move |conn| { let _ = serde_json::json!({}); Ok(()) }) }\n",
        );
        assert!(
            !writer_job_offenders(&planted).is_empty(),
            "a newly planted unprepared job in a former allowlisted file must be caught"
        );
    }

    /// Regression pin for the atomic seed path (audit findings 1/5): the
    /// writer closure of `Store::seed_task_attachments_txn` must stay
    /// SQL-only. The closure compares raw `revision`/`state`/`attachments`
    /// text against a caller-prepared expectation and never runs a whole-row
    /// decode (`task_row_map(`), a JSON parse (`parse_json(`/`serde_json`),
    /// or timestamp/string materialization (`format!(`, `.to_string()`,
    /// `.to_hex(`) on the single writer owner.
    ///
    /// HONEST LIMITATION: this is a lexical source assertion over the
    /// closure's byte span, not whole-program analysis. An arbitrary helper
    /// reached through an indirection the marker list does not spell out can
    /// still escape; the defense is the marker list (which names the known
    /// decode entry points for EVERY production writer job) plus this pin for
    /// the seed path. A new indirect decode helper must be added to
    /// [`WRITER_JOB_FORBIDDEN_MARKERS`] — never grandfathered.
    #[test]
    fn seed_task_attachments_writer_closure_is_sql_only() {
        let f = load("crates/store/src/attachments.rs").expect("attachments.rs readable");
        let fn_at = f
            .src
            .find("pub fn seed_task_attachments_txn")
            .expect("seed_task_attachments_txn present");
        let next_fn = f.src[fn_at + 1..]
            .find("\n    pub fn ")
            .map(|offset| fn_at + 1 + offset)
            .unwrap_or(f.src.len());
        let (open, close) = writer_job_spans(&f)
            .into_iter()
            .find(|(a, z)| *a >= fn_at && *z <= next_fn)
            .expect("the seed writer job span");
        for marker in [
            "task_row_map(",
            "parse_json(",
            "serde_json",
            "format!(",
            ".to_string()",
            ".to_hex(",
        ] {
            let mb = marker.as_bytes();
            let mut pos = open;
            while let Some(rel) = f.src[pos..close].find(marker) {
                let at = pos + rel;
                if f.code[at..at + mb.len()].iter().all(|c| *c) {
                    panic!(
                        "seed_task_attachments_txn closure line {} contains `{marker}`: {} \
                         — a writer job must execute SQL only; prepare the value on the \
                         caller's thread before enqueueing",
                        line_of(f.src, at),
                        trim_line(f.src, at)
                    );
                }
                pos = at + mb.len();
            }
        }
    }

    /// The writer-job ratchet is COMPLETE: the allowlist is empty, so a new
    /// unprepared job in ANY production file is a red scan. Non-emptiness is
    /// itself a failure — re-adding an entry is a regression, not a fix.
    #[test]
    fn writer_job_allowlist_is_empty() {
        assert!(
            WRITER_JOB_ALLOWLIST.is_empty(),
            "the writer-job allowlist ratchet is complete and must stay empty; \
             fix the call site instead of re-adding an entry: {WRITER_JOB_ALLOWLIST:?}"
        );
        // The owned writer core can never be grandfathered, empty or not.
        assert!(
            !WRITER_JOB_ALLOWLIST
                .iter()
                .any(|(rel, _)| *rel == "crates/store/src/connection.rs"
                    || *rel == "crates/store/src/writer.rs"),
            "the owned writer core must stay prepared"
        );
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
        ("crates/terminal/src/lib.rs", 10),
        ("crates/fs/src/tree_manifest.rs", 3),
        ("crates/git/src/guard.rs", 3),
        ("crates/session/src/actor.rs", 1),
        // --- test-only locations ---
        ("crates/pty/tests/guardian.rs", 1),
        ("crates/pty/tests/windows_lifecycle.rs", 1),
        ("crates/winjob/tests/windows_tree.rs", 1),
        ("crates/cloud/tests/durability_memory.rs", 1),
        ("crates/server/tests/attachment_decode_alloc.rs", 1),
        ("crates/orchestrator/src/shadow_tests.rs", 1),
        // The terminal unix test module (moved out of lib.rs by the audit-17
        // split): the three libc probes keep their per-site SAFETY comments.
        ("crates/terminal/src/tests.rs", 3),
        // The CLI's test-gated out-of-line modules (the module-decomposition
        // move out of main.rs): the two libc::kill zero-signal probes.
        ("crates/cli/src/main_acp_tests.rs", 1),
        ("crates/cli/src/main_serve_tests.rs", 1),
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

    // ------------------------------------------------------------------
    // scan 13: repository hygiene + license authority (audit 20-21)
    // ------------------------------------------------------------------

    /// Bytecode / compiled-artifact extensions that must never be committed.
    /// A deliberate fixture with one of these extensions may exist only via a
    /// documented [`HYGIENE_ALLOWLIST`] entry.
    const HYGIENE_ARTIFACT_EXTENSIONS: &[&str] = &[
        "pyc", "pyo", "pyd", "o", "obj", "a", "rlib", "rmeta", "so", "dylib", "dll", "exe",
        "class", "pdb", "jar", "vsix",
    ];

    /// Opaque extensionless binaries at or above this size are refused: a
    /// committed executable has no source story. The historical violation
    /// this rule codifies is root `mu_test` (465576 B Mach-O arm64, added by
    /// 9d4824a, zero references, deleted by audit item 21).
    const HYGIENE_OPAQUE_MIN_BYTES: u64 = 64 * 1024;

    /// Committed-artifact allowlist: exact repository-relative paths with a
    /// written justification. The scan asserts every entry is non-stale
    /// (the committed file exists) AND load-bearing (the rules would flag it
    /// without the exemption), so the list cannot rot into a silent bypass.
    const HYGIENE_ALLOWLIST: &[(&str, &str)] = &[
        (
            "apps/jetbrains/gradle/wrapper/gradle-wrapper.jar",
            "pinned Gradle wrapper jar (build-tool bootstrap, not a product build output); its \
             sha256 is verified by scripts/check-gradle-integrity.sh before any build, and the \
             wrapper cannot bootstrap without a shipped jar",
        ),
        (
            "scripts/certification/fixtures/release-artifacts/full/extension/faktor.vsix",
            "certification fixture simulating a packaged release artifact set; consumed by the \
             scripts/certification evidence tests, never shipped or executed",
        ),
        (
            "scripts/certification/fixtures/release-artifacts/subset/extension/faktor.vsix",
            "certification fixture simulating a packaged release artifact set; consumed by the \
             scripts/certification evidence tests, never shipped or executed",
        ),
    ];

    /// Committed paths that must stay deleted (the historical bytecode and
    /// stray executable the hygiene rules were introduced for). Presence is a
    /// red test even before the file is staged.
    const HYGIENE_RETIRED_PATHS: &[&str] = &[
        "crates/cli/tests/fixtures/__pycache__/mcp_mock.cpython-314.pyc",
        "mu_test",
    ];

    /// Artifact reason for a committed path, independent of file contents.
    fn hygiene_artifact_reason(rel: &str) -> Option<String> {
        if rel.split('/').any(|segment| segment == "__pycache__") {
            return Some("committed Python bytecode cache (__pycache__/)".to_string());
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let extension = name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase());
        if let Some(extension) = extension {
            if HYGIENE_ARTIFACT_EXTENSIONS.contains(&extension.as_str()) {
                return Some(format!("committed build artifact (*.{extension})"));
            }
        }
        None
    }

    /// Opaque-extensionless-binary reason: no extension (dotfiles excluded —
    /// they are configuration, not blobs), size at or above the threshold,
    /// and a NUL byte in the first 8 KiB (the binary sniff).
    fn hygiene_opaque_binary_reason(path: &Path, rel: &str) -> Option<String> {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let has_extension = name
            .rsplit_once('.')
            .is_some_and(|(stem, _)| !stem.is_empty());
        if has_extension || name.starts_with('.') {
            return None;
        }
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() < HYGIENE_OPAQUE_MIN_BYTES {
            return None;
        }
        let mut head = [0u8; 8192];
        let mut file = std::fs::File::open(path).ok()?;
        use std::io::Read;
        let read = file.read(&mut head).ok()?;
        if head[..read].contains(&0) {
            return Some(format!(
                "opaque extensionless binary ({} bytes, no extension, no provenance)",
                metadata.len()
            ));
        }
        None
    }

    /// Hygiene violations for `rels` under `root`; exact allowlist entries
    /// are exempted before any rule runs.
    fn hygiene_violations(root: &Path, rels: &[String], allowlist: &[(&str, &str)]) -> Vec<String> {
        let mut violations = Vec::new();
        for rel in rels {
            let rel = normalize_rel(rel);
            if allowlist.iter().any(|(allowed, _)| *allowed == rel) {
                continue;
            }
            if let Some(reason) = hygiene_artifact_reason(&rel) {
                violations.push(format!("{rel}: {reason}"));
                continue;
            }
            if let Some(reason) = hygiene_opaque_binary_reason(&root.join(&rel), &rel) {
                violations.push(format!("{rel}: {reason}"));
            }
        }
        violations
    }

    /// The COMMITTED file set (`git ls-files --cached`), `/`-normalized and
    /// filtered to files that still exist on disk (an unstaged deletion is a
    /// pending commit, not a committed artifact). The scan judges committed
    /// material, never a developer's untracked scratch files; git is part of
    /// every checkout this suite runs in.
    fn hygiene_tracked_files() -> Vec<String> {
        let root = repo_root();
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["ls-files", "-z", "--cached"])
            .output()
            .unwrap_or_else(|error| panic!("git ls-files must run for the hygiene scan: {error}"));
        assert!(
            output.status.success(),
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| normalize_rel(&String::from_utf8_lossy(entry)))
            .filter(|rel| root.join(rel).is_file())
            .collect()
    }

    /// Compact SHA-256 (zero dependencies are a design constraint of this
    /// crate). Correctness is pinned by the `abc` vector in the license test.
    fn sha256_hex(bytes: &[u8]) -> String {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut state: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut message = bytes.to_vec();
        let bit_len = (bytes.len() as u64).wrapping_mul(8);
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_len.to_be_bytes());
        for chunk in message.chunks(64) {
            let mut w = [0u32; 64];
            for (index, word) in w.iter_mut().take(16).enumerate() {
                let at = index * 4;
                *word =
                    u32::from_be_bytes([chunk[at], chunk[at + 1], chunk[at + 2], chunk[at + 3]]);
            }
            for index in 16..64 {
                let s0 = w[index - 15].rotate_right(7)
                    ^ w[index - 15].rotate_right(18)
                    ^ (w[index - 15] >> 3);
                let s1 = w[index - 2].rotate_right(17)
                    ^ w[index - 2].rotate_right(19)
                    ^ (w[index - 2] >> 10);
                w[index] = w[index - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[index - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
            for index in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let temp1 = h
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[index])
                    .wrapping_add(w[index]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let temp2 = s0.wrapping_add(maj);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(temp1);
                d = c;
                c = b;
                b = a;
                a = temp1.wrapping_add(temp2);
            }
            state[0] = state[0].wrapping_add(a);
            state[1] = state[1].wrapping_add(b);
            state[2] = state[2].wrapping_add(c);
            state[3] = state[3].wrapping_add(d);
            state[4] = state[4].wrapping_add(e);
            state[5] = state[5].wrapping_add(f);
            state[6] = state[6].wrapping_add(g);
            state[7] = state[7].wrapping_add(h);
        }
        state.iter().map(|word| format!("{word:08x}")).collect()
    }

    /// The committed tree carries no bytecode, no compiled build artifacts
    /// and no opaque extensionless binaries; every exception is a
    /// load-bearing documented allowlist entry.
    #[test]
    fn committed_tree_carries_no_bytecode_build_artifacts_or_opaque_binaries() {
        let root = repo_root();
        let tracked = hygiene_tracked_files();
        assert!(
            tracked.len() >= 500,
            "hygiene scan walked nothing: {} committed files",
            tracked.len()
        );
        for (allowed, justification) in HYGIENE_ALLOWLIST {
            assert!(
                !justification.trim().is_empty(),
                "{allowed}: hygiene allowlist entry needs a written justification"
            );
            assert!(
                tracked.iter().any(|rel| rel == allowed),
                "{allowed}: stale hygiene allowlist entry (no such committed file)"
            );
            let unexempted = hygiene_violations(&root, &[allowed.to_string()], &[]);
            assert!(
                !unexempted.is_empty(),
                "{allowed}: hygiene allowlist entry is not load-bearing (the rules pass it)"
            );
        }
        let offenders = hygiene_violations(&root, &tracked, HYGIENE_ALLOWLIST);
        assert_no_offenders(
            "repository-hygiene scan: committed bytecode / compiled artifacts / opaque \
             extensionless binaries are refused; deliberate binary fixtures belong under a \
             named fixture directory with documented provenance + digest and a consuming test",
            &offenders,
            tracked.len(),
            500,
        );
        for retired in HYGIENE_RETIRED_PATHS {
            assert!(
                !root.join(retired).exists(),
                "{retired}: the retired hygiene violation must stay deleted (see docs/repo-hygiene.md)"
            );
        }
    }

    /// Adversarial proof that the hygiene rules fire: a synthetic tree with
    /// planted violations is flagged; legitimate material (extensionless
    /// source scripts, binaries with non-artifact extensions, allowlisted
    /// fixtures) passes. The SHA-256 helper backing the license pin is
    /// checked against the published `abc` vector.
    #[test]
    fn hygiene_scanner_fires_on_planted_violations() {
        let dir = std::env::temp_dir().join(format!("faktor-hygiene-proof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |rel: &str, bytes: &[u8]| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).expect("fixture dir");
            std::fs::write(&path, bytes).expect("fixture write");
        };
        write(
            "pkg/__pycache__/mock.cpython-314.pyc",
            b"\x03\xf3\r\nplanted bytecode",
        );
        write("objects/unit.o", b"\x7fELF\x02\x01\x01\x00\x00\x00\x00");
        let mut opaque = vec![0u8; 70_000];
        opaque[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
        write("bin/tool", &opaque);
        let script = vec![b'#'; 70_000];
        write("scripts/tool", &script);
        let mut icon = vec![0u8; 70_000];
        icon[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        write("assets/icon.png", &icon);
        write("artifacts/pinned.jar", b"PK\x03\x04planted jar");
        let rels = [
            "pkg/__pycache__/mock.cpython-314.pyc",
            "objects/unit.o",
            "bin/tool",
            "scripts/tool",
            "assets/icon.png",
            "artifacts/pinned.jar",
        ]
        .map(str::to_string)
        .to_vec();

        let offenders = hygiene_violations(&dir, &rels, &[]);
        assert_eq!(
            offenders.len(),
            4,
            "planted violations must all fire: {offenders:?}"
        );
        for expected in [
            "__pycache__/mock.cpython-314.pyc",
            "objects/unit.o",
            "bin/tool: opaque extensionless binary",
            "artifacts/pinned.jar",
        ] {
            assert!(
                offenders.iter().any(|offender| offender.contains(expected)),
                "planted violation {expected:?} was not flagged: {offenders:?}"
            );
        }

        let allowlist: &[(&str, &str)] = &[("artifacts/pinned.jar", "planted allowlisted fixture")];
        let exempted = hygiene_violations(&dir, &rels, allowlist);
        assert_eq!(
            exempted.len(),
            3,
            "the allowlisted fixture must be exempted: {exempted:?}"
        );
        assert!(
            exempted
                .iter()
                .all(|offender| !offender.contains("pinned.jar")),
            "an allowlisted path must never be reported: {exempted:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "the local SHA-256 implementation must be correct"
        );
    }

    /// License authority: the canonical Apache-2.0 text is pinned by digest,
    /// NOTICE exists, every workspace member inherits the workspace license
    /// (no overrides), the deny.toml policy is committed, the bytecode class
    /// is ignored, and the policy step is wired into the trusted static lane.
    #[test]
    fn license_and_policy_authority_is_wired() {
        let root = repo_root();

        const LICENSE_SHA256: &str =
            "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30";
        let license = std::fs::read(root.join("LICENSE")).expect("committed root LICENSE");
        assert_eq!(
            sha256_hex(&license),
            LICENSE_SHA256,
            "LICENSE must be the canonical unmodified Apache-2.0 text (apache.org licenses/LICENSE-2.0)"
        );
        assert!(
            String::from_utf8_lossy(&license)
                .trim_start()
                .starts_with("Apache License"),
            "LICENSE must carry the canonical Apache-2.0 header"
        );
        assert!(
            String::from_utf8_lossy(&license).contains("Version 2.0, January 2004"),
            "LICENSE must declare Apache License Version 2.0"
        );

        let notice = std::fs::read_to_string(root.join("NOTICE")).expect("committed root NOTICE");
        assert!(!notice.trim().is_empty(), "NOTICE must not be empty");
        assert!(
            notice.contains("Apache License"),
            "NOTICE must name the Apache License"
        );
        assert!(
            notice.contains("LICENSE"),
            "NOTICE must point at the LICENSE file"
        );

        let workspace =
            std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace Cargo.toml");
        assert!(
            workspace.contains("license = \"Apache-2.0\""),
            "the workspace package must declare the Apache-2.0 SPDX license"
        );
        let members = {
            let at = workspace.find("members = [").expect("members list");
            let body = &workspace[at + "members = [".len()..];
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
            let manifest_path = root.join(member).join("Cargo.toml");
            let manifest = std::fs::read_to_string(&manifest_path)
                .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display()));
            assert!(
                manifest.contains("license.workspace = true"),
                "{member}: must inherit the workspace license (license.workspace = true)"
            );
            assert!(
                !manifest.contains("license-file"),
                "{member}: license-file bypasses the SPDX license policy"
            );
            assert!(
                !manifest.contains("\nlicense = \""),
                "{member}: a hard-coded license override breaks the single-license workspace policy"
            );
        }

        let deny = std::fs::read_to_string(root.join("deny.toml")).expect("committed deny.toml");
        for required in [
            "[licenses]",
            "[bans]",
            "[sources]",
            "unknown-registry = \"deny\"",
            "unknown-git = \"deny\"",
            "allow-registry = [\"https://github.com/rust-lang/crates.io-index\"]",
            "\"Apache-2.0 WITH LLVM-exception\"",
        ] {
            assert!(deny.contains(required), "deny.toml must carry {required}");
        }

        let gate = std::fs::read_to_string(root.join("scripts/check-licenses.sh"))
            .expect("committed scripts/check-licenses.sh");
        assert!(
            gate.contains("cargo metadata --format-version 1 --locked"),
            "the license gate must evaluate the locked cargo metadata graph"
        );
        assert!(
            gate.contains("cargo deny") && gate.contains("check licenses bans sources"),
            "the license gate must run cargo-deny when the binary is available"
        );
        let trusted = std::fs::read_to_string(root.join(".woodpecker/trusted/trusted.yaml"))
            .expect("trusted workflow");
        assert!(
            trusted.matches("scripts/check-licenses.sh").count() >= 2,
            "the license gate must be a static-lane command AND appear in its lane marker command list"
        );

        let ignore = std::fs::read_to_string(root.join(".gitignore")).expect(".gitignore");
        for pattern in ["__pycache__/", "*.py[cod]", "/mu_test"] {
            assert!(
                ignore.contains(pattern),
                ".gitignore must ignore the hygiene class {pattern}"
            );
        }
        for retired in HYGIENE_RETIRED_PATHS {
            assert!(
                !root.join(retired).exists(),
                "{retired}: retired hygiene violation reintroduced"
            );
        }
    }

    // ------------------------------------------------------------------
    // scan 14: production-wiring post-build authority replacement
    // ------------------------------------------------------------------

    /// The production-wiring certification tree: the tests must reach the
    /// daemon through its own builders (`build_production_graph*` /
    /// `build_daemon*`, which run the executable's `build_daemon_core`);
    /// after the graph is built only EXTERNAL seams may be substituted (fake
    /// servers, clocks, credentials, transports, config). Installing or
    /// replacing a built authority (`set_*provider`, `replace_*`) or
    /// constructing a top-level subsystem service in the test turns the
    /// certification into a look-alike rig and is a red test.
    ///
    /// `build.rs` is harness plumbing and `src/lib.rs` is the `include!` of
    /// the REAL `faktor-cli` sources at the crate root, so only the Rust
    /// files under `src/` and `tests/` are scanned (the included production
    /// text is certified by the production scans instead).
    const PRODUCTION_WIRING_ROOT: &str = "tests/production-wiring";

    /// The explicitly named MANUAL adapter-contract module(s): the embedded-
    /// host injection seam is the ONE documented place a production-wiring
    /// test may install a hand-built adapter onto a built executor, so the
    /// authority-replacement markers are exempt there. Exact paths only, no
    /// globs — a new exempt file must be named here deliberately, and every
    /// entry is asserted to exist AND to actually use the replacement API
    /// (stale exemptions are red).
    const AUTHORITY_REPLACEMENT_EXEMPT_FILES: &[&str] =
        &["tests/production-wiring/tests/completion_scm_adapter_contract.rs"];

    /// Top-level daemon subsystem services a production-wiring test must
    /// never construct: the production graph owns each one exactly once, so
    /// a test-local construction is a parallel authority. Audit 30 extends
    /// this with the remaining authority constructors (router service,
    /// acquisition planner/service, index service, SCM completion provider)
    /// so tests cannot instantiate a competing authority either; the planner
    /// default stays allowed because the documented spy seam wraps it and
    /// injects it through the production builder.
    const PRODUCTION_WIRING_FORBIDDEN_CONSTRUCTORS: &[&str] = &[
        "ProcessSupervisor::new",
        "TaskExecutor::new(",
        "TaskExecutor::new_owner_direct_for_test_harness(",
        "OrchestratorRuntime::new",
        "ShadowRoots::new(",
        "DurableBudgetLedger::new",
        "SemanticProviderRegistry::new",
        "AgentRuntime::new",
        "ProviderRegistry::new",
        "ServerDeps::new(",
        "SessionManager::open(",
        "SessionManager::open_quick(",
        "IndexService::open_with_supervisor(",
        "IndexService::open(",
        "IndexService::new",
        "SqliteScmStore::open(",
        "MemoryScmStore::new(",
        "RouterService::new",
        "RouterService::with_route_candidates",
        "RouterService::with_pinned_route_candidates",
        "AcquisitionPlanner::new",
        "CommerceSourceService::open_with_planner(",
        "CommerceSourceService::open(",
        "GitHubCompletionScm::new",
    ];

    /// Every `.rs` file under `tests/production-wiring/{src,tests}`.
    fn walk_production_wiring_sources() -> Vec<String> {
        let root = repo_root().join(PRODUCTION_WIRING_ROOT);
        let mut out = Vec::new();
        let mut stack = vec![root.join("src"), root.join("tests")];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    stack.push(path);
                } else if file_type.is_file()
                    && path.extension().and_then(|e| e.to_str()) == Some("rs")
                {
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

    /// The source with comments and string/char literals replaced by spaces
    /// (newlines preserved), so comments/docs/fixture strings can never fire
    /// the replacement markers.
    fn masked_wiring_code(src: &str) -> String {
        let code = code_mask(src);
        src.bytes()
            .zip(code.iter())
            .map(|(byte, is_code)| {
                if *is_code || byte == b'\n' || byte == b'\r' {
                    byte as char
                } else {
                    ' '
                }
            })
            .collect()
    }

    /// `set_<ident>provider(` calls (the authority-installation shape) on
    /// masked code, with their line and trimmed text.
    fn set_provider_calls(masked: &str) -> Vec<(usize, String)> {
        let bytes = masked.as_bytes();
        let mut hits = Vec::new();
        let mut from = 0usize;
        while let Some(rel) = masked[from..].find("set_") {
            let at = from + rel;
            let boundary =
                at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
            let mut end = at + 4;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let ident = &masked[at + 4..end];
            if boundary
                && !ident.is_empty()
                && ident.ends_with("provider")
                && masked[end..].starts_with('(')
            {
                hits.push((line_of(masked, at), trim_line(masked, at)));
            }
            from = end.max(at + 4);
        }
        hits
    }

    /// `replace_<ident>(` calls (wholesale authority replacement) on masked
    /// code. `String::replace_range`/`replace_all` inside build plumbing is
    /// never scanned (only `src/`+`tests/` sources are walked), and a plain
    /// `str::replace(` has no `replace_` prefix, so the marker stays exact.
    fn replace_calls(masked: &str) -> Vec<(usize, String)> {
        let bytes = masked.as_bytes();
        let mut hits = Vec::new();
        let mut from = 0usize;
        while let Some(rel) = masked[from..].find("replace_") {
            let at = from + rel;
            let boundary =
                at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
            let mut end = at + "replace_".len();
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if boundary && end > at + "replace_".len() && masked[end..].starts_with('(') {
                hits.push((line_of(masked, at), trim_line(masked, at)));
            }
            from = end.max(at + "replace_".len());
        }
        hits
    }

    /// Offenders of one production-wiring source: post-build authority
    /// replacement markers, unless the file is the explicitly named manual
    /// adapter-contract module.
    fn authority_replacement_offenders(rel: &str, src: &str) -> Vec<String> {
        let rel = normalize_rel(rel);
        if AUTHORITY_REPLACEMENT_EXEMPT_FILES.contains(&rel.as_str()) {
            return Vec::new();
        }
        let mut offenders = Vec::new();
        for marker in PRODUCTION_WIRING_FORBIDDEN_CONSTRUCTORS {
            for at in code_needle_offsets(src, marker) {
                offenders.push(format!(
                    "{rel}:{}: {marker}  [top-level subsystem service constructed in a \
                     production-wiring test; reach it through the production builder]",
                    line_of(src, at)
                ));
            }
        }
        let masked = masked_wiring_code(src);
        for (line, text) in set_provider_calls(&masked) {
            offenders.push(format!(
                "{rel}:{line}: {text}  [post-build authority replacement; use an external \
                 seam or the production builder]"
            ));
        }
        for (line, text) in replace_calls(&masked) {
            offenders.push(format!(
                "{rel}:{line}: {text}  [post-build authority replacement; use an external \
                 seam or the production builder]"
            ));
        }
        offenders
    }

    /// The production-wiring tests reach the production daemon through its
    /// own builder and never replace a built authority afterwards. The ONE
    /// exemption (the manual adapter-contract module) is asserted to exist
    /// and to really use `set_completion_scm_provider`, so it can never go
    /// stale silently; the same module passes the scan unchanged.
    #[test]
    fn production_wiring_tests_never_replace_built_daemon_authorities() {
        let files = walk_production_wiring_sources();
        assert!(
            files.len() >= 5,
            "production-wiring walk too small: {files:?}"
        );
        let mut offenders = Vec::new();
        let mut builders = 0usize;
        for rel in &files {
            let src = std::fs::read_to_string(repo_root().join(rel))
                .unwrap_or_else(|e| panic!("read {rel}: {e}"));
            if src.contains("build_production_graph") {
                builders += 1;
            }
            offenders.extend(authority_replacement_offenders(rel, &src));
        }
        assert!(
            builders >= 4,
            "the production-wiring tree must actually call the production builder \
             (build_production_graph*): only {builders} of {} files do",
            files.len()
        );
        for exempt in AUTHORITY_REPLACEMENT_EXEMPT_FILES {
            let src = std::fs::read_to_string(repo_root().join(exempt))
                .unwrap_or_else(|e| panic!("exempt module {exempt}: {e}"));
            assert!(
                src.contains("set_completion_scm_provider"),
                "{exempt}: stale adapter-contract exemption — the module no longer uses the \
                 manual replacement API and must be removed from the allowlist"
            );
            assert!(
                authority_replacement_offenders(exempt, &src).is_empty(),
                "{exempt}: the explicitly named adapter-contract module must stay exempt"
            );
        }
        assert_no_offenders(
            "production-wiring authority-replacement scan: tests/production-wiring must reach the \
             daemon through build_production_graph/build_daemon and substitute external seams only \
             (set_*provider / replace_* / top-level subsystem construction are reserved for the \
             explicitly named adapter-contract module)",
            &offenders,
            files.len(),
            5,
        );
    }

    /// Planted-violation proof: each forbidden shape fires on a synthetic
    /// source, comment/string mentions never fire, external seams stay
    /// allowed, and the explicitly named manual adapter-contract module is
    /// the one exemption.
    #[test]
    fn production_wiring_authority_replacement_scan_fires_on_planted_violations() {
        let non_exempt = "tests/production-wiring/tests/planted.rs";
        let planted_setter = "fn t() { graph.tasks.set_completion_scm_provider(Some(adapter)); }\n";
        let offenders = authority_replacement_offenders(non_exempt, planted_setter);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert!(
            offenders[0].contains("set_completion_scm_provider"),
            "{offenders:?}"
        );

        let planted_replace = "fn t() { replace_scm_store(new_store); }\n";
        assert!(
            !authority_replacement_offenders(non_exempt, planted_replace).is_empty(),
            "a replace_* authority swap must fire"
        );

        let planted_service = "fn t() { let _ = TaskExecutor::new(&orch, s, a, sh); }\n";
        let offenders = authority_replacement_offenders(non_exempt, planted_service);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert!(offenders[0].contains("TaskExecutor::new("), "{offenders:?}");

        // Audit 30 planted authorities: a test-local router/acquisition/index
        // construction fires exactly like the executor one.
        for planted in [
            "fn t() { let _ = RouterService::with_route_candidates(candidates); }\n",
            "fn t() { let _ = AcquisitionPlanner::new(SubstitutionPolicy::default()); }\n",
            "fn t() { let _ = IndexService::open_with_supervisor(sup, ws, cfg); }\n",
            "fn t() { let _ = GitHubCompletionScm::new(app, store, tenant); }\n",
        ] {
            assert!(
                !authority_replacement_offenders(non_exempt, planted).is_empty(),
                "a test-local authority construction must fire: {planted}"
            );
        }

        let comment_and_string = "// graph.tasks.set_completion_scm_provider(Some(x))\n\
             const DOC: &str = \"replace_scm_store(TaskExecutor::new())\";\nfn t() {}\n";
        assert!(
            authority_replacement_offenders(non_exempt, comment_and_string).is_empty(),
            "comments and string literals can never fire the scan"
        );

        let external_seams = "fn t() {\n    let _ = StaticTokenSource::minimal(1);\n    \
             let _ = ManualClock::new(1);\n    let c = Config::default();\n    \
             let _ = PolicyCheckedHttpTransport::permissive();\n    let _ = c.model;\n}\n";
        assert!(
            authority_replacement_offenders(non_exempt, external_seams).is_empty(),
            "external seams, clocks, credentials, transports and config stay allowed"
        );

        assert!(
            authority_replacement_offenders(
                "tests/production-wiring/tests/completion_scm_adapter_contract.rs",
                planted_setter
            )
            .is_empty(),
            "the explicitly named manual adapter-contract module is the one exemption"
        );
    }

    // ---------------------------------------------------------------- source-size ceiling
    //
    // Audit item 9/23/24: the former giants (`agent/runtime.rs`,
    // `store/lib.rs`, `cli/main.rs`, `server/api.rs`, `cli/config.rs`) were
    // decomposed into cohesive modules. This scan keeps the workspace from
    // regrowing: every Rust source under `crates/` and `tests/` (except the
    // enforcing crate itself and fixture trees) must fit the ceiling. The
    // recorded grandfather list is NON-GROWABLE — a file may never exceed
    // its recorded count, an entry must be deleted the moment the file fits
    // under the ceiling, and no new file may join the list.

    /// The per-file source ceiling, in lines.
    const SOURCE_SIZE_CEILING_LINES: usize = 4000;

    /// Files that were already over the ceiling when it was introduced
    /// (recorded counts). This list can only shrink.
    const GRANDFATHERED_OVER_CEILING: &[(&str, usize)] = &[
        ("crates/acp/src/lib.rs", 4465),
        ("crates/cli/src/tools_market.rs", 5555),
        ("crates/git/src/lib.rs", 4269),
        ("crates/ollama/src/lib.rs", 4107),
        ("crates/openai/src/lib.rs", 4234),
        ("crates/orchestrator/src/completion_steps.rs", 4631),
        ("crates/orchestrator/src/runtime.rs", 4262),
        ("crates/orchestrator/src/runtime_tests.rs", 5259),
        ("crates/provider/src/egress.rs", 5699),
        ("crates/scheduler/src/lib.rs", 4176),
        ("crates/server/src/native/terminal_authority.rs", 5189),
        ("crates/session/src/budget.rs", 4010),
    ];

    /// Every Rust source in scope for the ceiling: `crates/**` plus
    /// `tests/**`, excluding the enforcing crate and fixture trees.
    fn walk_ceiling_sources() -> Vec<String> {
        let mut out = walk_crate_sources();
        let tests = repo_root().join("tests");
        let mut stack = vec![tests];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                let name = entry.file_name().to_string_lossy().to_string();
                if ft.is_dir() {
                    if name == "target" || name == "fixtures" || name.starts_with('.') {
                        continue;
                    }
                    if path.ends_with("tests/static-authority") {
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
        out
    }

    fn source_size_violations(counts: &[(String, usize)]) -> Vec<String> {
        let mut out = Vec::new();
        for (rel, lines) in counts {
            match GRANDFATHERED_OVER_CEILING.iter().find(|(f, _)| f == rel) {
                Some((_, recorded)) => {
                    if lines > recorded {
                        out.push(format!(
                            "{rel}: {lines} lines exceeds its recorded grandfather count \
                             {recorded} (the list can only shrink)"
                        ));
                    }
                    if *lines <= SOURCE_SIZE_CEILING_LINES {
                        out.push(format!(
                            "{rel}: {lines} lines now fit under the {SOURCE_SIZE_CEILING_LINES}-line \
                             ceiling; remove the grandfather entry"
                        ));
                    }
                }
                None => {
                    if lines > &SOURCE_SIZE_CEILING_LINES {
                        out.push(format!(
                            "{rel}: {lines} lines exceeds the {SOURCE_SIZE_CEILING_LINES}-line \
                             source ceiling: split the file or record a documented grandfather count"
                        ));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn source_files_stay_under_the_size_ceiling_or_their_recorded_count() {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for rel in walk_ceiling_sources() {
            let Ok(src) = std::fs::read_to_string(repo_root().join(&rel)) else {
                continue;
            };
            counts.push((rel, src.lines().count()));
        }
        assert!(
            counts.len() >= 200,
            "ceiling walk found only {} files",
            counts.len()
        );
        let violations = source_size_violations(&counts);
        assert!(
            violations.is_empty(),
            "source-size ceiling violations:\n  {}\n",
            violations.join("\n  ")
        );
    }

    #[test]
    fn size_ceiling_fires_on_planted_violations() {
        let at_cap = vec![(
            "crates/planted/src/lib.rs".to_string(),
            SOURCE_SIZE_CEILING_LINES,
        )];
        assert!(
            source_size_violations(&at_cap).is_empty(),
            "a file exactly at the ceiling is allowed"
        );
        let planted = vec![(
            "crates/planted/src/lib.rs".to_string(),
            SOURCE_SIZE_CEILING_LINES + 1,
        )];
        let fired = source_size_violations(&planted);
        assert_eq!(fired.len(), 1, "one planted violation: {fired:?}");
        assert!(fired[0].contains("exceeds"), "{fired:?}");

        let (grandfathered, recorded) = GRANDFATHERED_OVER_CEILING[0];
        let grown = vec![(grandfathered.to_string(), recorded + 1)];
        let fired = source_size_violations(&grown);
        assert_eq!(
            fired.len(),
            1,
            "growth of a grandfathered file fires: {fired:?}"
        );
        assert!(fired[0].contains("can only shrink"), "{fired:?}");

        let shrunk = vec![(grandfathered.to_string(), SOURCE_SIZE_CEILING_LINES)];
        let fired = source_size_violations(&shrunk);
        assert_eq!(
            fired.len(),
            1,
            "a shrunk file must leave the list: {fired:?}"
        );
        assert!(
            fired[0].contains("remove the grandfather entry"),
            "{fired:?}"
        );
    }

    // ------------------------------------------- audit 25/27/28 wiring --

    /// Audit 25: the canonical protocol schema artifact, the generated
    /// clients and the drift gate are all present and wired into CI.
    #[test]
    fn protocol_codegen_surface_is_canonical_and_wired() {
        let root = repo_root();
        let artifact_rel = "crates/protocol/schema/faktor-protocol.schema.json";
        let artifact =
            std::fs::read_to_string(root.join(artifact_rel)).expect("canonical schema artifact");
        assert!(
            artifact.contains("\"schema\": \"faktor-protocol-schema/v1\""),
            "artifact schema id drifted"
        );
        for required in [
            "Message",
            "Part",
            "ToolResultBody",
            "PageMeta",
            "MessagesPage",
            "SessionState",
            "AgentStateView",
        ] {
            assert!(
                artifact.contains(&format!("\"name\": \"{required}\"")),
                "artifact misses {required}"
            );
        }
        let code_rows = artifact.matches("\"code\":").count();
        assert!(code_rows >= 14, "error code table shrank: {code_rows} rows");

        for (rel, language) in [
            ("apps/vscode/src/generated/protocolDto.ts", "TypeScript"),
            (
                "apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/GeneratedProtocolDto.kt",
                "Kotlin",
            ),
        ] {
            let src = std::fs::read_to_string(root.join(rel))
                .unwrap_or_else(|e| panic!("{language} generated file missing: {e}"));
            assert!(
                src.starts_with("// GENERATED FILE - DO NOT EDIT BY HAND."),
                "{rel}: generated header missing"
            );
            assert!(
                src.contains("protocol-codegen.mjs --write"),
                "{rel}: regeneration command missing"
            );
            assert!(
                src.contains("CODEGEN.md"),
                "{rel}: generated-vs-handwritten pointer missing"
            );
        }
        assert!(
            root.join("scripts/protocol-codegen.mjs").is_file(),
            "codegen script missing"
        );
        assert!(
            root.join("crates/protocol/schema/CODEGEN.md").is_file(),
            "generated-vs-handwritten list missing"
        );
        let schema_rs = std::fs::read_to_string(root.join("crates/protocol/src/schema.rs"))
            .expect("schema emitter");
        assert!(
            schema_rs.contains("pub fn canonical_json"),
            "schema emitter lost its canonical JSON entry point"
        );
        let bin =
            std::fs::read_to_string(root.join("crates/protocol/src/bin/faktor-protocol-schema.rs"))
                .expect("schema emitter binary");
        assert!(
            bin.contains("--check"),
            "schema emitter lost its --check drift mode"
        );
        let vscode_lane = std::fs::read_to_string(root.join(".woodpecker/trusted/trusted.yaml"))
            .expect("trusted workflow");
        assert!(
            vscode_lane.contains("node scripts/protocol-codegen.mjs --check-clients"),
            "the trusted vscode lane must run the client drift check"
        );
    }

    /// Audit 27 (+audit 5): the reproducible contention benchmark exists, is
    /// #[ignore]d (normal PR runs stay fast) and is assigned to a wired
    /// nightly lane (the trusted perf lane selects the package too). The
    /// audit-5 wiring is pinned too: session-keyed event timing, per-job
    /// writer receipts, the separately instrumented reader-pool permit wait,
    /// and REAL on-disk IndexService generations driven through
    /// request_build/reconcile.
    #[test]
    fn contention_benchmark_is_wired_into_nightly_and_trusted() {
        let root = repo_root();
        let bench = std::fs::read_to_string(root.join("tests/performance/tests/contention.rs"))
            .expect("contention benchmark");
        assert!(
            bench.contains("#[ignore = \"[perf] multi-agent contention"),
            "the benchmark must stay #[ignore]d so PR runs stay fast"
        );
        for axis in [
            "sse_lag",
            "event_lag",
            "writer_queue_wait",
            "writer_exec",
            "messages_page",
            "reader_pool_wait",
            "index_rebuild",
            "wal_peak_bytes",
            "rss_end_kb",
        ] {
            assert!(bench.contains(axis), "benchmark misses the {axis} axis");
        }
        // Audit-5 wiring: the bug classes must not silently regress back to
        // the shapes this item removed (seq-only lag keys, global writer
        // deltas, in-memory index toys, mislabeled page duration).
        for needle in [
            "(SessionId, u64)",
            "take_writer_receipts",
            "take_reader_receipts",
            "IndexService::open",
            "request_build",
            "reconcile_now",
            "write_bench_files",
        ] {
            assert!(
                bench.contains(needle),
                "benchmark misses the audit-5 wiring marker `{needle}`"
            );
        }
        let registry =
            std::fs::read_to_string(root.join("scripts/certification/ignored-tests.json"))
                .expect("ignored-test registry");
        assert!(
            registry.contains("\"contention\""),
            "the contention lane must be registered"
        );
        assert!(
            registry.contains("tests/performance/tests/contention.rs"),
            "the benchmark must be assigned to a lane"
        );
        let nightly = std::fs::read_to_string(root.join(".woodpecker/trusted/nightly.yaml"))
            .expect("nightly workflow");
        assert!(
            nightly.contains("  - name: contention"),
            "the nightly contention lane step is missing"
        );
        assert!(
            nightly.contains(
                "cargo test -p faktor-tests-performance --release --test contention -- --ignored"
            ),
            "the nightly contention lane command drifted"
        );
        let trusted = std::fs::read_to_string(root.join(".woodpecker/trusted/trusted.yaml"))
            .expect("trusted workflow");
        assert!(
            trusted.contains("cargo test -p faktor-tests-performance --release -- --ignored"),
            "the trusted perf lane must keep selecting the performance package"
        );
    }

    /// Audit 28: the visual baseline carries DISTINCT per-platform records,
    /// the checker never inherits a canonical digest, and its policy
    /// self-test runs in the JetBrains smoke.
    #[test]
    fn visual_certification_is_environment_specific() {
        let root = repo_root();
        let baseline_rel =
            "apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json";
        let baseline = std::fs::read_to_string(root.join(baseline_rel)).expect("baseline");
        assert!(
            baseline.contains("\"schema\": \"faktor-parity-visual-baselines/v3\""),
            "baseline schema drifted"
        );
        for platform in ["linux", "macos", "windows"] {
            assert!(
                baseline.contains(&format!("\"{platform}\"")),
                "release platform {platform} missing from the baseline"
            );
        }
        assert!(
            !baseline.contains("panelDigests") && !baseline.contains("environmentDigests"),
            "the v2 canonical digest pool must not survive in the v3 baseline"
        );
        assert!(
            baseline.contains("\"linux\": {") && baseline.contains("\"macos\": {"),
            "per-platform records missing"
        );
        // Each present platform record carries an environment fingerprint and
        // exactly the pinned 64-hex panel digest values.
        let environments = baseline.matches("\"environment\":").count();
        assert!(
            environments >= 2,
            "only {environments} platform records carry an environment fingerprint"
        );
        let digests: Vec<&str> = baseline
            .lines()
            .filter_map(|line| line.split_once(": ").map(|(_, value)| value))
            .map(|value| value.trim().trim_end_matches(',').trim_matches('"'))
            .filter(|value| value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()))
            .collect();
        assert!(
            digests.len() >= 16,
            "only {} pinned 64-hex panel digests in the baseline",
            digests.len()
        );
        let matrix_rel =
            "apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParityMatrix.kt";
        let matrix = std::fs::read_to_string(root.join(matrix_rel)).expect("parity matrix");
        assert!(
            matrix.contains("not certified on platform"),
            "the checker must surface missing platforms as not certified"
        );
        for forbidden in [
            "field(\"panelDigests\")",
            "optionalField(\"environmentDigests\")",
            "field(\"environmentDigests\")",
        ] {
            assert!(
                !matrix.contains(forbidden),
                "the checker must never parse the canonical {forbidden} digest pool"
            );
        }
        assert!(
            matrix.contains("visualCoverageOf") && matrix.contains("visualResultsFor"),
            "the per-platform checker core is missing"
        );
        assert!(
            matrix.contains("visualCertificationPolicySelfTest"),
            "the adversarial policy self-test is missing"
        );
        let smoke_rel =
            "apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParitySmoke.kt";
        let smoke = std::fs::read_to_string(root.join(smoke_rel)).expect("parity smoke");
        assert!(
            smoke.contains("visualCertificationPolicySelfTest"),
            "the smoke must run the visual certification policy self-test"
        );
        assert!(
            root.join("scripts/check-visual-platforms.mjs").is_file(),
            "platform-record checker missing"
        );
        let certify = std::fs::read_to_string(root.join("scripts/certify.sh")).expect("certify.sh");
        assert!(
            certify.contains("check-visual-platforms.mjs"),
            "certify.sh must consume the platform-record checker"
        );
        assert!(
            certify.contains("[ \"$VISUAL_PLATFORMS_STATUS\" = \"pass\" ]"),
            "the visual platform records must gate the release preconditions"
        );
    }

    // ------------------------------------------------- audit 16: allow(unused*)
    //
    // Audit item 16: the module decomposition left file-wide
    // `#![allow(unused_imports)]` masks across the runtime/store/server/cli
    // split modules. They are removed and every submodule imports only what
    // it uses. This scan keeps the masks from coming back: production Rust
    // (test-gated modules and out-of-line test files are never scanned, as
    // are comments and string literals) must not carry ANY `allow(unused*)`
    // attribute. The exception list is empty by design — target-specific
    // mutability is expressed with `#[cfg]` bindings, not a lint mask. A
    // non-empty entry must name the exact file and trimmed line and is
    // asserted live, so a stale entry can never excuse a new mask.

    /// Exact production sites exempt from the ban (rel, trimmed line).
    const ALLOW_UNUSED_EXCEPTIONS: &[(&str, &str)] = &[];

    /// True when `lint` (one comma-separated element of an `allow(...)`
    /// body) names a plain rustc lint starting with `unused` (`unused`,
    /// `unused_imports`, ...). Namespaced lints (`clippy::unused_*`) belong
    /// to another lint namespace and never fire.
    fn is_unused_lint(lint: &str) -> bool {
        let lint = lint.trim();
        !lint.contains("::") && lint.starts_with("unused")
    }

    /// `allow(unused*)` attribute sites in the production text of `f`
    /// (1-based line, trimmed line text).
    fn allow_unused_sites(f: &File<'_>) -> Vec<(usize, String)> {
        let mut hits = Vec::new();
        let mut pos = 0usize;
        while let Some(rel) = f.src[pos..].find("allow(") {
            let at = pos + rel;
            pos = at + "allow(".len();
            let in_kept = f
                .kept
                .iter()
                .any(|(a, z)| at >= *a && at + "allow(".len() <= *z);
            let in_code = f.code[at..at + "allow(".len()].iter().all(|c| *c);
            if !in_kept || !in_code {
                continue;
            }
            let before = f.src[..at].trim_end();
            if !(before.ends_with("#[") || before.ends_with("#![")) {
                continue;
            }
            // Attribute body up to the matching (possibly nested) `)`.
            let bytes = f.src.as_bytes();
            let mut depth = 1usize;
            let mut j = at + "allow(".len();
            while j < bytes.len() && depth > 0 {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            if depth != 0 {
                continue;
            }
            let body = &f.src[at + "allow(".len()..j - 1];
            if body.split(',').any(is_unused_lint) {
                hits.push((line_of(f.src, at), trim_line(f.src, at)));
            }
        }
        hits.sort_unstable();
        hits.dedup();
        hits
    }

    fn allow_unused_offenders(f: &File<'_>) -> Vec<String> {
        allow_unused_sites(f)
            .into_iter()
            .filter(|(_, text)| {
                !ALLOW_UNUSED_EXCEPTIONS
                    .iter()
                    .any(|(rel, t)| *rel == f.rel.as_str() && *t == text)
            })
            .map(|(line, text)| format!("{}:{line}: {text}", f.rel))
            .collect()
    }

    #[test]
    fn no_production_allow_unused_masks() {
        let mut scanned = 0usize;
        let mut offenders = Vec::new();
        for rel in walk_crate_sources() {
            if is_test_file(&rel) {
                continue;
            }
            let Some(f) = load(&rel) else { continue };
            scanned += 1;
            offenders.extend(allow_unused_offenders(&f));
        }
        assert_no_offenders("allow(unused*) ban", &offenders, scanned, 150);
        for (rel, text) in ALLOW_UNUSED_EXCEPTIONS {
            let f = load(rel).unwrap_or_else(|| panic!("exception file {rel} missing"));
            assert!(
                allow_unused_sites(&f).iter().any(|(_, t)| t == text),
                "stale allow(unused*) exception entry {rel}: {text:?} — remove it"
            );
        }
    }

    #[test]
    fn allow_unused_ban_fires_on_planted_violations() {
        let synthetic = |src: &str| -> File<'static> {
            let src = leak(src);
            let code = code_mask(src);
            let kept = kept_ranges(src, &code);
            File {
                rel: "tests/planted_fixture.rs".to_string(),
                src,
                code,
                kept,
            }
        };
        for planted in [
            "#![allow(unused_imports)]\nfn t() {}\n",
            "#[allow(unused_mut)]\nfn t() { let mut x = 0; let _ = x; }\n",
            "#[allow(dead_code, unused_variables)]\nfn t() {}\n",
            "#[allow(unused)]\nfn t() {}\n",
        ] {
            let f = synthetic(planted);
            assert_eq!(
                allow_unused_offenders(&f).len(),
                1,
                "planted mask must fire: {planted}"
            );
        }
        let test_only =
            "#[cfg(test)]\nmod tests {\n    #[allow(unused_imports)]\n    use super::*;\n}\n";
        assert!(
            allow_unused_offenders(&synthetic(test_only)).is_empty(),
            "test-gated masks are not production code"
        );
        let mentions = "// #[allow(unused_imports)]\nconst DOC: &str = \"#[allow(unused_mut)]\";\n\
                        fn allow(unused_imports) {}\n";
        assert!(
            allow_unused_offenders(&synthetic(mentions)).is_empty(),
            "comments, strings and non-attribute mentions never fire"
        );
        let clippy = "#[allow(clippy::unused_io_amount)]\nfn t() { let _ = 1; }\n";
        assert!(
            allow_unused_offenders(&synthetic(clippy)).is_empty(),
            "a namespaced clippy lint is not a rustc unused* allow"
        );
    }

    // ------------------------------------------- audit 22/23 forge wiring --

    /// Audit 22/23: the repository description is the Faktor-owned UI
    /// positioning and certification verifies it (plus the exact-SHA commit
    /// status) through the GitHub API; the trusted workflow publishes the
    /// result on success AND failure with a fail-closed token.
    #[test]
    fn forge_metadata_and_status_publication_are_wired() {
        let root = repo_root();
        let certify = std::fs::read_to_string(root.join("scripts/certify.sh")).expect("certify.sh");
        for needle in [
            "check_forge_metadata",
            "FAKTOR_REPO_DESCRIPTION=\"Faktor — native Rust engineering runtime with Faktor-owned IDE UIs\"",
            "forge-status-absent",
            "forge-status-pending",
            "forge-status-sha-mismatch",
            "external, non-source",
            "certification/publish-status.mjs publish",
        ] {
            assert!(
                certify.contains(needle),
                "certify.sh misses the audit-22/23 marker `{needle}`"
            );
        }
        let trusted = std::fs::read_to_string(root.join(".woodpecker/trusted/trusted.yaml"))
            .expect("trusted workflow");
        for needle in [
            "  - name: status-publish",
            "status: [success, failure]",
            "publish-status.mjs selftest",
            "publish-status.mjs publish",
            "faktor_github_status_token",
        ] {
            assert!(
                trusted.contains(needle),
                "trusted workflow misses the audit-23 marker `{needle}`"
            );
        }
        let publish =
            std::fs::read_to_string(root.join("scripts/certification/publish-status.mjs"))
                .expect("publish-status.mjs");
        assert!(
            publish.contains("github-status-token-missing"),
            "publish-status.mjs must refuse a missing token"
        );
        assert!(
            publish.contains("/^[0-9a-f]{40}$/"),
            "publish-status.mjs must bind the exact 40-hex commit"
        );
        let branding =
            std::fs::read_to_string(root.join("scripts/branding-scan.sh")).expect("branding scan");
        assert!(
            branding.contains("GitHub API by `scripts/certify.sh` gate 11"),
            "the branding scan note must point at the gate-11 API check"
        );
    }

    // ------------------------------------------------------------------
    // scan 8: audit-15 migrated native DTOs stay generated
    // ------------------------------------------------------------------

    /// The migrated attachment/task-run DTOs must stay in the canonical
    /// schema, both IDE clients must delegate to the GENERATED decoders, and
    /// the codegen inventory must keep its shrink-only frozen set and the
    /// migrated routes. A reappearing hand-rolled parse of these fields is a
    /// regression, not a label change.
    fn migrated_native_dto_delegation_offenders(
        vscode_ts: &str,
        jetbrains_kt: &str,
        schema_json: &str,
        codegen_mjs: &str,
        codegen_md: &str,
    ) -> Vec<String> {
        let mut offenders = Vec::new();
        for needle in [
            "type NativeTaskRun = ProtocolTaskRun",
            "validateProtocolTaskRun(",
            "validateProtocolAttachmentId(",
        ] {
            if !vscode_ts.contains(needle) {
                offenders.push(format!(
                    "apps/vscode/src/nativeClient.ts lost the generated delegation `{needle}`"
                ));
            }
        }
        for needle in [
            "typealias NativeTaskRun = ProtocolTaskRun",
            "typealias NativeAttachmentId = ProtocolAttachmentId",
            "parseProtocolTaskRun(it)",
            "parseProtocolAttachmentId(",
        ] {
            if !jetbrains_kt.contains(needle) {
                offenders.push(format!(
                    "NativeProtocol.kt lost the generated delegation `{needle}`"
                ));
            }
        }
        if vscode_ts.contains("item_ids: fStringArray(object") {
            offenders.push(
                "nativeClient.ts hand-parses TaskRun.item_ids again (delegate to \
                 validateProtocolTaskRun)"
                    .to_string(),
            );
        }
        if jetbrains_kt.contains("NativeTaskRun(\n")
            || jetbrains_kt.contains("NativeTaskRunStarted(\n")
            || jetbrains_kt.contains("NativeTaskRunCancelled(\n")
        {
            offenders.push(
                "NativeProtocol.kt hand-constructs a migrated task-run DTO again (delegate to \
                 parseProtocolTaskRun*)"
                    .to_string(),
            );
        }
        for name in [
            "AttachmentId",
            "AttachmentUpload",
            "TaskRun",
            "TaskRunStarted",
            "TaskRunCancelled",
            "TaskRunWorkItem",
            "TaskRunStartRequest",
        ] {
            if !schema_json.contains(&format!("\"name\": \"{name}\"")) {
                offenders.push(format!(
                    "the canonical schema lost `{name}` (crates/protocol/src/schema.rs)"
                ));
            }
        }
        for route in [
            "/native/session/{id}/attachments",
            "/native/session/{id}/task-runs",
        ] {
            if !codegen_mjs.contains(route) {
                offenders.push(format!(
                    "protocol-codegen.mjs lost the migrated route `{route}`"
                ));
            }
        }
        if !codegen_mjs.contains("HANDWRITTEN_FROZEN") {
            offenders.push(
                "protocol-codegen.mjs lost the shrink-only HANDWRITTEN_FROZEN set".to_string(),
            );
        }
        if !codegen_md.contains("SHRINK-ONLY") {
            offenders.push(
                "crates/protocol/schema/CODEGEN.md lost the shrink-only inventory contract"
                    .to_string(),
            );
        }
        offenders
    }

    #[test]
    fn migrated_native_dtos_stay_generated_with_delegating_ide_parsers() {
        let root = repo_root();
        let read = |rel: &str| std::fs::read_to_string(root.join(rel)).unwrap_or_default();
        let offenders = migrated_native_dto_delegation_offenders(
            &read("apps/vscode/src/nativeClient.ts"),
            &read("apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/NativeProtocol.kt"),
            &read("crates/protocol/schema/faktor-protocol.schema.json"),
            &read("scripts/protocol-codegen.mjs"),
            &read("crates/protocol/schema/CODEGEN.md"),
        );
        assert_no_offenders(
            "audit-15 scan: the migrated attachment/task-run DTOs must stay generated and both \
             IDE clients must delegate to the generated decoders",
            &offenders,
            5,
            5,
        );
    }

    #[test]
    fn migrated_native_dto_tripwire_fires_on_a_planted_regression() {
        let root = repo_root();
        let read = |rel: &str| std::fs::read_to_string(root.join(rel)).unwrap_or_default();
        let vscode = read("apps/vscode/src/nativeClient.ts")
            .replace(
                "type NativeTaskRun = ProtocolTaskRun",
                "interface NativeTaskRun {}",
            )
            .replace("validateProtocolTaskRun(", "handRollTaskRun(");
        let offenders = migrated_native_dto_delegation_offenders(
            &vscode,
            &read("apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/NativeProtocol.kt"),
            &read("crates/protocol/schema/faktor-protocol.schema.json"),
            &read("scripts/protocol-codegen.mjs"),
            &read("crates/protocol/schema/CODEGEN.md"),
        );
        assert!(
            offenders
                .iter()
                .any(|offender| offender.contains("nativeClient.ts")),
            "the audit-15 tripwire must fire on a planted VS Code regression: {offenders:?}"
        );
    }
}
