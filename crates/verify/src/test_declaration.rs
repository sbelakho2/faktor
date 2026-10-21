//! Neutral test-declaration adapters.
//!
//! Proof rebinding must never infer semantic preservation from a file-rename
//! edge alone. Every language adapter answers the same four questions for a
//! declaration it can PROVE:
//!
//! * semantic selector (the module/suite-qualified identity),
//! * executable selector (the runner filter that actually runs it),
//! * body identity (normalized executable structure, comments/trivia stripped,
//!   declaration attributes that change success semantics included),
//! * enclosing target (the runner target the declaration belongs to).
//!
//! An adapter that cannot prove one of these (dynamic test name, unsupported
//! macro/annotation, unparsable body) emits NO declaration. The caller then
//! keeps the old binding unresolved instead of guessing. This is the
//! fail-closed contract: absence of a declaration is a refusal, never a
//! vacuous match.

use blake3::Hasher;

/// One lexical identity token. Whitespace and comments never appear.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdentityToken {
    Ident(String),
    Literal(String),
    Punct(char),
}

/// The normalized executable identity of one declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyIdentity {
    /// BLAKE3 of the normalized token stream.
    pub digest: String,
    pub tokens: Vec<IdentityToken>,
}

/// One declaration whose semantic identity and executable body identity were
/// proven by a [`TestDeclarationAdapter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestDeclaration {
    /// Source path the declaration lives in.
    pub path: String,
    /// Module/suite-qualified semantic identity.
    pub semantic_selector: String,
    /// The runner filter that actually executes the declaration.
    pub executable_selector: String,
    /// The enclosing runner target (integration-file stem, source file, ...).
    pub enclosing_target: Option<String>,
    pub body_identity: BodyIdentity,
}

/// A language-specific declaration authority.
pub trait TestDeclarationAdapter: Send + Sync {
    fn language(&self) -> &'static str;
    fn matches_path(&self, path: &str) -> bool;
    /// Parse one file into declarations this adapter can PROVE. Anything it
    /// cannot prove is omitted (fail closed).
    fn declarations(&self, path: &str, content: &str) -> Vec<TestDeclaration>;
    fn semantic_selector<'a>(&self, declaration: &'a TestDeclaration) -> &'a str {
        &declaration.semantic_selector
    }
    fn executable_selector<'a>(&self, declaration: &'a TestDeclaration) -> &'a str {
        &declaration.executable_selector
    }
    fn body_identity<'a>(&self, declaration: &'a TestDeclaration) -> &'a BodyIdentity {
        &declaration.body_identity
    }
    fn enclosing_target<'a>(&self, declaration: &'a TestDeclaration) -> Option<&'a str> {
        declaration.enclosing_target.as_deref()
    }
}

/// A bound on adapter input: proof evidence is bounded like every other
/// review input.
pub const MAX_DECLARATION_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Hard bound on declarations proven from ONE file: input is bounded, and a
/// hostile file may not turn the rebinding pass into unbounded work/memory.
pub const MAX_DECLARATIONS_PER_FILE: usize = 512;

/// Hard bound on shared fixture/helper context folded into ONE declaration
/// identity (per file). Larger context is refused (no partial proof).
pub const MAX_DECLARATION_CONTEXT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    JavaScript,
    Jvm,
    Python,
    Go,
}

impl Language {
    fn slash_comments(self) -> bool {
        !matches!(self, Self::Python)
    }
    fn hash_comments(self) -> bool {
        matches!(self, Self::Python)
    }
}

/// The adapter for `path`, or `None` when no adapter can prove declarations
/// in that language (an unsupported file is never guessed at).
pub fn adapter_for_path(path: &str) -> Option<&'static dyn TestDeclarationAdapter> {
    static ADAPTERS: [&dyn TestDeclarationAdapter; 5] = [
        &RustAdapter,
        &JavaScriptAdapter,
        &JvmAdapter,
        &PythonAdapter,
        &GoAdapter,
    ];
    ADAPTERS
        .iter()
        .copied()
        .find(|adapter| adapter.matches_path(path))
}

/// TRUE when `path` is a test-bearing source path some adapter understands.
pub fn is_supported_test_path(path: &str) -> bool {
    adapter_for_path(path).is_some()
}

fn canonical_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn has_extension(path: &str, extensions: &[&str]) -> bool {
    let path = canonical_path(path);
    let Some((_, extension)) = path.rsplit_once('.') else {
        return false;
    };
    extensions
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

fn declaration(
    path: &str,
    semantic_selector: String,
    executable_selector: String,
    enclosing_target: Option<String>,
    identity_source: &str,
    language: Language,
) -> Option<TestDeclaration> {
    if semantic_selector.is_empty() || executable_selector.is_empty() {
        return None;
    }
    let tokens = tokenize(identity_source, language);
    if tokens.is_empty() {
        return None;
    }
    Some(TestDeclaration {
        path: canonical_path(path),
        semantic_selector,
        executable_selector,
        enclosing_target,
        body_identity: BodyIdentity {
            digest: digest_tokens(&tokens),
            tokens,
        },
    })
}

/// Sort declarations and drop every selector that appears more than once:
/// a duplicated identity is ambiguous and can never prove a migration.
fn finalize(mut declarations: Vec<TestDeclaration>) -> Vec<TestDeclaration> {
    declarations.sort_by(|a, b| {
        a.semantic_selector
            .cmp(&b.semantic_selector)
            .then_with(|| a.executable_selector.cmp(&b.executable_selector))
    });
    let mut ambiguous: Vec<String> = Vec::new();
    for window in declarations.windows(2) {
        if window[0].semantic_selector == window[1].semantic_selector {
            ambiguous.push(window[0].semantic_selector.clone());
        }
    }
    declarations.retain(|declaration| !ambiguous.contains(&declaration.semantic_selector));
    declarations.truncate(MAX_DECLARATIONS_PER_FILE);
    declarations
}

// ------------------------------------------------------------------ rust

struct RustAdapter;

impl TestDeclarationAdapter for RustAdapter {
    fn language(&self) -> &'static str {
        "rust"
    }

    fn matches_path(&self, path: &str) -> bool {
        has_extension(path, &["rs"])
    }

    fn declarations(&self, path: &str, content: &str) -> Vec<TestDeclaration> {
        if content.len() > MAX_DECLARATION_FILE_BYTES {
            return Vec::new();
        }
        let declarations: Vec<TestDeclaration> =
            faktor_core::contract_compile::rust_test_declarations_for_path(path, content)
                .into_iter()
                .filter(|discovered| discovered.body_complete)
                .filter_map(|discovered| {
                    let mut identity = String::new();
                    for attribute in &discovered.attributes {
                        identity.push_str(attribute);
                        identity.push('\n');
                    }
                    identity.push_str(&discovered.body);
                    declaration(
                        path,
                        discovered.selector,
                        discovered.executable,
                        discovered.target,
                        &identity,
                        Language::Rust,
                    )
                })
                .collect();
        finalize(declarations)
    }
}

// ------------------------------------------------------------ javascript

struct JavaScriptAdapter;

impl TestDeclarationAdapter for JavaScriptAdapter {
    fn language(&self) -> &'static str {
        "javascript"
    }

    fn matches_path(&self, path: &str) -> bool {
        has_extension(
            path,
            &["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts"],
        )
    }

    fn declarations(&self, path: &str, content: &str) -> Vec<TestDeclaration> {
        if content.len() > MAX_DECLARATION_FILE_BYTES {
            return Vec::new();
        }
        let language = Language::JavaScript;
        let calls = scan_calls(content, language);
        let mut suites: Vec<Suite> = Vec::new();
        let mut opaque: Vec<(usize, usize)> = Vec::new();
        for call in &calls {
            if call.chain.first().map(String::as_str) != Some("describe") {
                continue;
            }
            // Only a PLAIN `describe("literal", () => { ... })` proves suite
            // containment. `describe.skip/.only/.each`, options objects,
            // dynamic names and expression bodies change execution (or make
            // the span unprovable): every test inside is withheld.
            if call.chain.len() == 1 {
                if let Some((name, after_name)) = literal_argument(content, call.open_paren) {
                    if let Some((_, open, close)) = callback_span(content, after_name, language) {
                        suites.push(Suite {
                            name,
                            open,
                            close,
                            context: String::new(),
                        });
                        continue;
                    }
                }
            }
            if let Some(close) = find_matching(content, call.open_paren, b'(', b')', language) {
                opaque.push((call.open_paren, close));
            }
        }
        suites.sort_by_key(|suite| suite.open);
        for suite in &mut suites {
            let Some(context) =
                javascript_suite_context(content, suite.open, suite.close, &calls, language)
            else {
                return Vec::new();
            };
            if context.len() > MAX_DECLARATION_CONTEXT_BYTES {
                return Vec::new();
            }
            suite.context = context;
        }
        let Some(global_context) = javascript_global_context(content, &calls, language) else {
            return Vec::new();
        };
        if global_context.len() > MAX_DECLARATION_CONTEXT_BYTES {
            return Vec::new();
        }
        let mut hooks: Vec<Hook> = Vec::new();
        // A hook whose callback cannot be parsed (named reference, expression)
        // makes every test in its scope unprovable: the setup it runs is not
        // part of the identity we can prove.
        let mut unprovable_hooks: Vec<Vec<usize>> = Vec::new();
        for call in &calls {
            let kind = call.chain.first().map(String::as_str).unwrap_or_default();
            if !is_javascript_hook(kind) {
                continue;
            }
            let suite_opens = enclosing_suite_opens(call.start, &suites);
            if call.chain.len() != 1 {
                unprovable_hooks.push(suite_opens);
                continue;
            }
            if let Some((body, open, close)) = callback_argument(content, call.open_paren, language)
            {
                opaque.push((open, close));
                hooks.push(Hook {
                    kind: kind.to_string(),
                    start: call.start,
                    body,
                    suite_opens,
                });
            } else {
                unprovable_hooks.push(suite_opens);
            }
        }
        hooks.sort_by_key(|hook| hook.start);
        if hooks.iter().map(|hook| hook.body.len()).sum::<usize>() > MAX_DECLARATION_CONTEXT_BYTES {
            return Vec::new();
        }
        let mut out = Vec::new();
        for call in &calls {
            let kind = call.chain.first().map(String::as_str).unwrap_or_default();
            // Modifiers (`test.skip`, `test.only`, `test.each`, ...) and
            // options objects cannot prove executability; only the plain
            // runner entry points are declarations.
            if call.chain.len() != 1 || (kind != "test" && kind != "it") {
                continue;
            }
            if opaque
                .iter()
                .any(|(open, close)| *open < call.start && call.start < *close)
            {
                continue;
            }
            let Some((name, after_name)) = literal_argument(content, call.open_paren) else {
                continue;
            };
            let Some((body, _open, _close)) = callback_span(content, after_name, language) else {
                continue;
            };
            let mut enclosing: Vec<&Suite> = suites
                .iter()
                .filter(|suite| suite.open < call.start && call.start < suite.close)
                .collect();
            enclosing.sort_by_key(|suite| suite.open);
            let suite_opens: Vec<usize> = enclosing.iter().map(|suite| suite.open).collect();
            if unprovable_hooks
                .iter()
                .any(|scope| suite_opens.starts_with(scope))
            {
                continue;
            }
            let shared_bytes = global_context.len()
                + enclosing
                    .iter()
                    .map(|suite| suite.context.len())
                    .sum::<usize>();
            if shared_bytes > MAX_DECLARATION_CONTEXT_BYTES {
                continue;
            }
            let mut semantic: Vec<String> =
                enclosing.iter().map(|suite| suite.name.clone()).collect();
            semantic.push(name.clone());
            let mut identity = call.chain.join(".");
            identity.push('\n');
            identity.push_str(&global_context);
            identity.push('\n');
            // Direct suite-level code (variables, helper calls) is shared
            // setup: changing/removing it must invalidate the migration.
            for suite in &enclosing {
                identity.push_str("suite\n");
                identity.push_str(&suite.context);
                identity.push('\n');
            }
            // Fixture hooks are part of the executable test identity:
            // removing/changing setup must invalidate the migration.
            for hook in &hooks {
                if suite_opens.starts_with(&hook.suite_opens) {
                    identity.push_str(&hook.kind);
                    identity.push('\n');
                    identity.push_str(&hook.body);
                    identity.push('\n');
                }
            }
            identity.push_str(&body);
            if let Some(declared) = declaration(
                path,
                semantic.join("::"),
                name,
                Some(canonical_path(path)),
                &identity,
                language,
            ) {
                out.push(declared);
            }
        }
        finalize(out)
    }
}

#[derive(Debug, Clone)]
struct Suite {
    name: String,
    open: usize,
    close: usize,
    /// Direct suite-body code with nested test/suite/hook calls removed.
    context: String,
}

#[derive(Debug, Clone)]
struct Hook {
    kind: String,
    start: usize,
    body: String,
    suite_opens: Vec<usize>,
}

fn javascript_module_line_complete(line: &str) -> bool {
    line.ends_with(';')
        || line.contains("from \"")
        || line.contains("from '")
        || line.contains("require(")
        || line.starts_with("import \"")
        || line.starts_with("import '")
}

/// The DIRECT code of a suite callback (variables, statements, helper
/// calls) with every nested test/suite/hook call span removed. Nested suites
/// contribute their own context to their own tests when applicable.
fn javascript_suite_context(
    source: &str,
    open: usize,
    close: usize,
    calls: &[Call],
    language: Language,
) -> Option<String> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for call in calls {
        if call.start <= open || call.start >= close {
            continue;
        }
        let kind = call.chain.first().map(String::as_str).unwrap_or_default();
        if !matches!(
            kind,
            "test" | "it" | "describe" | "before" | "beforeEach" | "after" | "afterEach"
        ) {
            continue;
        }
        let end = find_matching(source, call.open_paren, b'(', b')', language)?;
        let start = call.start.max(open + 1);
        let end = (end + 1).min(close);
        if start < end {
            ranges.push((start, end));
        }
    }
    ranges.sort_unstable();
    let mut out = String::new();
    let mut cursor = open + 1;
    for (start, end) in ranges {
        if start > cursor {
            out.push_str(&source[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    if cursor < close {
        out.push_str(&source[cursor..close]);
    }
    Some(out)
}

/// Top-level module code (fixtures/helpers/constants) with every test,
/// suite and hook call span removed and import/export boilerplate dropped.
/// Import paths often change on a legitimate file move, but a test's shared
/// top-level fixture setup does not.
fn javascript_global_context(source: &str, calls: &[Call], language: Language) -> Option<String> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for call in calls {
        let kind = call.chain.first().map(String::as_str).unwrap_or_default();
        if !matches!(
            kind,
            "test" | "it" | "describe" | "before" | "beforeEach" | "after" | "afterEach"
        ) {
            continue;
        }
        let end = find_matching(source, call.open_paren, b'(', b')', language)?;
        ranges.push((call.start, (end + 1).min(source.len())));
    }
    ranges.sort_unstable();
    let mut stripped = String::new();
    let mut cursor = 0usize;
    for (start, end) in ranges {
        if start > cursor {
            stripped.push_str(&source[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    if cursor < source.len() {
        stripped.push_str(&source[cursor..]);
    }
    let mut out = String::new();
    let mut skipping_import = false;
    for line in stripped.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if skipping_import {
            if javascript_module_line_complete(trimmed) {
                skipping_import = false;
            }
            continue;
        }
        if trimmed.starts_with("import") {
            skipping_import = !javascript_module_line_complete(trimmed);
            continue;
        }
        if trimmed.starts_with("export") && trimmed.contains("from") {
            skipping_import = !javascript_module_line_complete(trimmed);
            continue;
        }
        out.push_str(line);
    }
    Some(out)
}

fn is_javascript_hook(kind: &str) -> bool {
    matches!(kind, "before" | "beforeEach" | "after" | "afterEach")
}

fn enclosing_suite_opens(start: usize, suites: &[Suite]) -> Vec<usize> {
    let mut enclosing: Vec<&Suite> = suites
        .iter()
        .filter(|suite| suite.open < start && start < suite.close)
        .collect();
    enclosing.sort_by_key(|suite| suite.open);
    enclosing.iter().map(|suite| suite.open).collect()
}

#[derive(Debug, Clone)]
struct Call {
    chain: Vec<String>,
    start: usize,
    open_paren: usize,
}

fn scan_calls(source: &str, language: Language) -> Vec<Call> {
    let bytes = source.as_bytes();
    let mut calls = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        index = skip_trivia(source, index, language);
        if index >= bytes.len() {
            break;
        }
        let byte = bytes[index];
        if is_ident_start(byte) {
            let start = index;
            let Some((first, mut next)) = read_ident(source, index) else {
                index += 1;
                continue;
            };
            let mut chain = vec![first];
            loop {
                let dot = skip_trivia(source, next, language);
                if dot >= bytes.len() || bytes[dot] != b'.' {
                    break;
                }
                let after_dot = skip_trivia(source, dot + 1, language);
                let Some((part, end)) = read_ident(source, after_dot) else {
                    break;
                };
                chain.push(part);
                next = end;
            }
            let paren = skip_trivia(source, next, language);
            if paren < bytes.len() && bytes[paren] == b'(' {
                calls.push(Call {
                    chain,
                    start,
                    open_paren: paren,
                });
                index = paren + 1;
            } else {
                index = next;
            }
            continue;
        }
        if is_string_start(byte, language) {
            index = skip_string(source, index, language);
            continue;
        }
        if language == Language::JavaScript && byte == b'/' {
            if let Some(end) = javascript_regex_end(source, index) {
                index = end;
                continue;
            }
        }
        index += 1;
    }
    calls
}

fn literal_argument(source: &str, open_paren: usize) -> Option<(String, usize)> {
    let index = skip_trivia(source, open_paren + 1, Language::JavaScript);
    string_content_at(source, index, Language::JavaScript)
}

/// The callback passed as the SECOND argument (after a literal test name).
fn callback_span(source: &str, from: usize, language: Language) -> Option<(String, usize, usize)> {
    let bytes = source.as_bytes();
    let index = skip_trivia(source, from, language);
    if index >= bytes.len() || bytes[index] != b',' {
        return None;
    }
    callback_value(source, skip_trivia(source, index + 1, language), language)
}

/// The callback passed as the FIRST argument (a hook).
fn callback_argument(
    source: &str,
    open_paren: usize,
    language: Language,
) -> Option<(String, usize, usize)> {
    callback_value(
        source,
        skip_trivia(source, open_paren + 1, language),
        language,
    )
}

fn callback_value(
    source: &str,
    mut index: usize,
    language: Language,
) -> Option<(String, usize, usize)> {
    let bytes = source.as_bytes();
    if let Some((word, end)) = read_ident(source, index) {
        if word == "async" {
            index = skip_trivia(source, end, language);
        }
    }
    let block_start = if let Some((word, end)) = read_ident(source, index) {
        if word == "function" {
            index = skip_trivia(source, end, language);
            if let Some((_, name_end)) = read_ident(source, index) {
                index = skip_trivia(source, name_end, language);
            }
            if index >= bytes.len() || bytes[index] != b'(' {
                return None;
            }
            let close = find_matching(source, index, b'(', b')', language)?;
            index = skip_trivia(source, close + 1, language);
            while index < bytes.len() && bytes[index] != b'{' {
                if bytes[index] == b';' {
                    return None;
                }
                index += 1;
            }
            index
        } else {
            arrow_body_start(source, index, language)?
        }
    } else {
        arrow_body_start(source, index, language)?
    };
    if block_start >= bytes.len() || bytes[block_start] != b'{' {
        return None;
    }
    let end = find_matching(source, block_start, b'{', b'}', language)?;
    Some((source[block_start + 1..end].to_string(), block_start, end))
}

fn arrow_body_start(source: &str, mut index: usize, language: Language) -> Option<usize> {
    let bytes = source.as_bytes();
    if index < bytes.len() && bytes[index] == b'(' {
        index = find_matching(source, index, b'(', b')', language)?;
        index = skip_trivia(source, index + 1, language);
    } else if let Some((_, end)) = read_ident(source, index) {
        index = skip_trivia(source, end, language);
    } else {
        return None;
    }
    // TypeScript arrow return annotation (`(): void => { ... }`).
    if index < bytes.len() && bytes[index] == b':' {
        index += 1;
        while index + 1 < bytes.len() && !(bytes[index] == b'=' && bytes[index + 1] == b'>') {
            if bytes[index] == b';' || bytes[index] == b'{' {
                return None;
            }
            index += 1;
        }
    }
    if index + 1 >= bytes.len() || bytes[index] != b'=' || bytes[index + 1] != b'>' {
        return None;
    }
    index = skip_trivia(source, index + 2, language);
    (index < bytes.len() && bytes[index] == b'{').then_some(index)
}

// ------------------------------------------------------------------- jvm

struct JvmAdapter;

impl TestDeclarationAdapter for JvmAdapter {
    fn language(&self) -> &'static str {
        "jvm"
    }

    fn matches_path(&self, path: &str) -> bool {
        has_extension(path, &["java", "kt", "kts"])
    }

    fn declarations(&self, path: &str, content: &str) -> Vec<TestDeclaration> {
        if content.len() > MAX_DECLARATION_FILE_BYTES {
            return Vec::new();
        }
        let language = Language::Jvm;
        let mut package = String::new();
        let mut classes: Vec<(String, i32)> = Vec::new();
        let mut attributes: Vec<String> = Vec::new();
        let mut depth = 0i32;
        let mut offset = 0usize;
        let mut tests: Vec<JvmTest> = Vec::new();
        let mut test_spans: Vec<(usize, usize)> = Vec::new();
        for raw in content.split_inclusive('\n') {
            let line = raw.trim_end_matches(['\r', '\n']);
            let trimmed = line.trim_start();
            if trimmed.starts_with("package ") {
                package = trimmed
                    .trim_start_matches("package ")
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
            }
            if !trimmed.is_empty() && !trimmed.starts_with("//") && !trimmed.starts_with('*') {
                let mut signature_line = trimmed;
                let mut signature_offset = 0usize;
                if let Some((_, sig)) = method_signature(trimmed) {
                    if trimmed.starts_with('@') && sig > 0 {
                        let annotation = trimmed[..sig].trim();
                        if !annotation.is_empty() {
                            attributes.push(annotation.to_string());
                        }
                        signature_line = trimmed[sig..].trim_start();
                        signature_offset = sig + (trimmed[sig..].len() - signature_line.len());
                    }
                }
                if let Some((name, _)) = method_signature(signature_line) {
                    let test_annotation = attributes
                        .iter()
                        .any(|attribute| is_jvm_test_annotation(attribute));
                    let class = classes.last().map(|(name, _)| name.clone());
                    if test_annotation {
                        if let Some(class) = class {
                            let line_offset = line.len() - trimmed.len();
                            let absolute = offset + line_offset + signature_offset;
                            if let Some((body, _open, close)) =
                                method_body_span(content, absolute, language)
                            {
                                if tests.len() < MAX_DECLARATIONS_PER_FILE {
                                    tests.push(JvmTest {
                                        name,
                                        class,
                                        attributes: attributes.clone(),
                                        signature: signature_line.to_string(),
                                        body,
                                    });
                                    test_spans.push((absolute, close + 1));
                                }
                            }
                        }
                    }
                    attributes.clear();
                } else if trimmed.starts_with('@') {
                    attributes.push(trimmed.to_string());
                } else {
                    attributes.clear();
                }
            }
            if let Some(name) = class_declaration(trimmed) {
                let open = (depth + brace_delta(line, language)).max(1);
                classes.push((name, open));
            }
            depth += brace_delta(line, language);
            while let Some((_, open_depth)) = classes.last() {
                if *open_depth > depth {
                    classes.pop();
                } else {
                    break;
                }
            }
            offset += raw.len();
        }
        // Shared class context (fields, helpers, @BeforeEach/@BeforeAll ...)
        // is part of every test's executable identity. The test method spans
        // themselves and the class header (whose name may legitimately
        // change) are excluded so only shared setup/fixture code is
        // compared. Removing or changing setup therefore refuses.
        let mut context = String::new();
        let mut line_start = 0usize;
        for raw in content.split_inclusive('\n') {
            let line_end = line_start + raw.len();
            let trimmed = raw.trim_start();
            let class_header = class_declaration(trimmed).is_some();
            let test_span = test_spans
                .iter()
                .any(|(start, end)| line_start < *end && line_end > *start);
            if !class_header && !test_span {
                context.push_str(raw);
            }
            line_start = line_end;
        }
        if context.len() > MAX_DECLARATION_CONTEXT_BYTES {
            return Vec::new();
        }
        let mut out = Vec::new();
        for test in tests {
            let mut identity = test.attributes.join("\n");
            identity.push('\n');
            identity.push_str(&test.signature);
            identity.push('\n');
            identity.push_str(&test.body);
            identity.push('\n');
            identity.push_str(&context);
            let package_prefix = if package.is_empty() {
                String::new()
            } else {
                format!("{package}.")
            };
            if let Some(declared) = declaration(
                path,
                format!("{package_prefix}{}#{}", test.class, test.name),
                format!("{}#{}", test.class, test.name),
                Some(canonical_path(path)),
                &identity,
                language,
            ) {
                out.push(declared);
            }
        }
        finalize(out)
    }
}

struct JvmTest {
    name: String,
    class: String,
    attributes: Vec<String>,
    signature: String,
    body: String,
}

fn is_jvm_test_annotation(attribute: &str) -> bool {
    [
        "@Test",
        "@ParameterizedTest",
        "@RepeatedTest",
        "@TestFactory",
        "@TestTemplate",
    ]
    .iter()
    .any(|name| attribute.trim_start().starts_with(name))
}

fn method_signature(line: &str) -> Option<(String, usize)> {
    let (keyword, sig) = if let Some(index) = find_word(line, "void") {
        ("void", index)
    } else {
        let index = find_word(line, "fun")?;
        ("fun", index)
    };
    let after = sig + keyword.len();
    if !line[after..].contains('(') {
        return None;
    }
    let (name, _) = read_ident(line, skip_ascii_space(line, after))?;
    Some((name, sig))
}

fn find_word(line: &str, word: &str) -> Option<usize> {
    let mut start = 0usize;
    while let Some(found) = line[start..].find(word) {
        let index = start + found;
        let before_ok = index == 0 || !is_ident_continue(line.as_bytes()[index - 1]);
        let end = index + word.len();
        let after_ok = end >= line.len() || !is_ident_continue(line.as_bytes()[end]);
        if before_ok && after_ok {
            return Some(index);
        }
        start = end;
    }
    None
}

fn skip_ascii_space(line: &str, from: usize) -> usize {
    let bytes = line.as_bytes();
    let mut index = from;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn class_declaration(line: &str) -> Option<String> {
    if line.starts_with('@') || line.starts_with("//") {
        return None;
    }
    for keyword in ["class", "object", "interface", "record"] {
        if let Some(index) = find_word(line, keyword) {
            let after = index + keyword.len();
            let (name, _) = read_ident(line, skip_ascii_space(line, after))?;
            return Some(name);
        }
    }
    None
}

fn method_body(source: &str, from: usize, language: Language) -> Option<String> {
    method_body_span(source, from, language).map(|(body, _open, _close)| body)
}

fn method_body_span(
    source: &str,
    from: usize,
    language: Language,
) -> Option<(String, usize, usize)> {
    let bytes = source.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        index = skip_trivia(source, index, language);
        if index >= bytes.len() {
            return None;
        }
        match bytes[index] {
            b'{' => {
                let end = find_matching(source, index, b'{', b'}', language)?;
                return Some((source[index + 1..end].to_string(), index, end));
            }
            b';' | b'=' => return None,
            _ => index += 1,
        }
    }
    None
}

// ---------------------------------------------------------------- python

struct PythonAdapter;

impl TestDeclarationAdapter for PythonAdapter {
    fn language(&self) -> &'static str {
        "python"
    }

    fn matches_path(&self, path: &str) -> bool {
        has_extension(path, &["py", "pyi"])
    }

    fn declarations(&self, path: &str, content: &str) -> Vec<TestDeclaration> {
        if content.len() > MAX_DECLARATION_FILE_BYTES {
            return Vec::new();
        }
        let language = Language::Python;
        let lines: Vec<&str> = content.lines().collect();
        let module = std::path::Path::new(&canonical_path(path))
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut scopes: Vec<(usize, String)> = Vec::new();
        let mut decorators: Vec<(usize, String)> = Vec::new();
        let mut tests: Vec<PythonTest> = Vec::new();
        let mut context: Vec<(Vec<String>, String)> = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line.len() - trimmed.len();
            if trimmed.starts_with('@') {
                decorators.push((indent, trimmed.to_string()));
                continue;
            }
            while scopes
                .last()
                .map(|(scope_indent, _)| *scope_indent >= indent)
                .unwrap_or(false)
            {
                scopes.pop();
            }
            if let Some(name) = python_class_name(trimmed) {
                scopes.push((indent, name));
                decorators.clear();
                continue;
            }
            let Some((name, is_async)) = python_def_name(trimmed) else {
                decorators.clear();
                continue;
            };
            let active_decorators: Vec<String> = decorators
                .iter()
                .filter(|(decorator_indent, _)| *decorator_indent == indent)
                .map(|(_, decorator)| decorator.clone())
                .collect();
            let body = python_body(&lines, index, indent);
            if python_is_fixture_context(&name, &active_decorators) {
                let mut shared = active_decorators.join("\n");
                shared.push('\n');
                shared.push_str(if is_async { "async def" } else { "def" });
                shared.push(' ');
                shared.push_str(&name);
                shared.push('\n');
                shared.push_str(&body);
                context.push((
                    scopes.iter().map(|(_, name)| name.clone()).collect(),
                    shared,
                ));
                decorators.clear();
                continue;
            }
            if name.starts_with("test")
                && !body.trim().is_empty()
                && tests.len() < MAX_DECLARATIONS_PER_FILE
            {
                let mut semantic_parts = vec![module.clone()];
                semantic_parts.extend(scopes.iter().map(|(_, name)| name.clone()));
                semantic_parts.push(name.clone());
                tests.push(PythonTest {
                    name,
                    is_async,
                    decorators: active_decorators,
                    body,
                    scopes: scopes.iter().map(|(_, name)| name.clone()).collect(),
                    semantic_parts,
                });
            }
            decorators.clear();
        }
        if context.iter().map(|(_, body)| body.len()).sum::<usize>() > MAX_DECLARATION_CONTEXT_BYTES
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        for test in tests {
            let mut identity = test.decorators.join("\n");
            identity.push('\n');
            identity.push_str(if test.is_async { "async def" } else { "def" });
            identity.push(' ');
            identity.push_str(&test.name);
            identity.push('\n');
            identity.push_str(&test.body);
            // pytest fixtures and unittest/class setup bodies are part of
            // the executable identity: removing or changing one in the
            // test's own class scope must refuse. Module-level fixtures
            // (empty scope) apply to every test.
            for (scope, shared) in &context {
                if test.scopes.starts_with(scope) {
                    identity.push('\n');
                    identity.push_str(shared);
                }
            }
            if let Some(declared) = declaration(
                path,
                test.semantic_parts.join("::"),
                test.name,
                Some(canonical_path(path)),
                &identity,
                language,
            ) {
                out.push(declared);
            }
        }
        finalize(out)
    }
}

struct PythonTest {
    name: String,
    is_async: bool,
    decorators: Vec<String>,
    body: String,
    scopes: Vec<String>,
    semantic_parts: Vec<String>,
}

fn python_is_fixture_context(name: &str, decorators: &[String]) -> bool {
    decorators.iter().any(|decorator| {
        let decorator = decorator.trim_start();
        decorator.starts_with("@pytest.fixture")
            || decorator.starts_with("@pytest_asyncio.fixture")
            || decorator.starts_with("@fixture")
    }) || matches!(
        name,
        "setUp"
            | "setUpClass"
            | "tearDown"
            | "tearDownClass"
            | "setup_method"
            | "setup_class"
            | "teardown_method"
            | "teardown_class"
            | "setup_module"
            | "teardown_module"
            | "setup_function"
            | "teardown_function"
    )
}

fn python_class_name(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("class ")?;
    let (name, _) = read_ident(rest, 0)?;
    Some(name)
}

fn python_def_name(trimmed: &str) -> Option<(String, bool)> {
    let (rest, is_async) = match trimmed.strip_prefix("async def ") {
        Some(rest) => (rest, true),
        None => (trimmed.strip_prefix("def ")?, false),
    };
    let (name, _) = read_ident(rest, 0)?;
    Some((name, is_async))
}

fn python_body(lines: &[&str], def_index: usize, def_indent: usize) -> String {
    let mut body = String::new();
    for line in lines.iter().skip(def_index + 1) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            body.push_str(line);
            body.push('\n');
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent <= def_indent {
            break;
        }
        body.push_str(line);
        body.push('\n');
    }
    body
}

// -------------------------------------------------------------------- go

struct GoAdapter;

impl TestDeclarationAdapter for GoAdapter {
    fn language(&self) -> &'static str {
        "go"
    }

    fn matches_path(&self, path: &str) -> bool {
        canonical_path(path).ends_with("_test.go")
    }

    fn declarations(&self, path: &str, content: &str) -> Vec<TestDeclaration> {
        if content.len() > MAX_DECLARATION_FILE_BYTES {
            return Vec::new();
        }
        let language = Language::Go;
        let mut package = String::new();
        let mut offset = 0usize;
        let mut tests: Vec<(String, String, String)> = Vec::new();
        let mut helpers: Vec<String> = Vec::new();
        for raw in content.split_inclusive('\n') {
            let line = raw.trim_end_matches(['\r', '\n']);
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("package ") {
                package = rest.trim().to_string();
            }
            let line_offset = line.len() - trimmed.len();
            if let Some((name, signature, params_end)) = go_test_signature(trimmed) {
                let absolute = offset + line_offset + params_end + 1;
                if let Some(body) = method_body(content, absolute, language) {
                    if tests.len() < MAX_DECLARATIONS_PER_FILE {
                        tests.push((name, signature, body));
                    }
                }
            } else if trimmed.starts_with("func ") {
                // Package helpers (including TestMain) are shared fixture
                // code: changing one must invalidate every test identity.
                let absolute = offset + line_offset;
                if let Some((body, _open, _close)) =
                    go_function_body(content, absolute, trimmed, language)
                {
                    helpers.push(format!("{trimmed}\n{body}"));
                }
            }
            offset += raw.len();
        }
        helpers.sort();
        helpers.dedup();
        if helpers.iter().map(String::len).sum::<usize>() > MAX_DECLARATION_CONTEXT_BYTES {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (name, signature, body) in tests {
            let mut identity = signature;
            identity.push('\n');
            identity.push_str(&body);
            for helper in &helpers {
                identity.push('\n');
                identity.push_str(helper);
            }
            if let Some(declared) = declaration(
                path,
                if package.is_empty() {
                    name.clone()
                } else {
                    format!("{package}.{name}")
                },
                name,
                Some(canonical_path(path)),
                &identity,
                language,
            ) {
                out.push(declared);
            }
        }
        finalize(out)
    }
}

fn go_test_signature(line: &str) -> Option<(String, String, usize)> {
    let rest = line.strip_prefix("func ")?;
    let (name, after_name) = read_ident(rest, 0)?;
    if !name.starts_with("Test") {
        return None;
    }
    let params_start = rest[after_name..].find('(')? + after_name;
    let params_end = find_matching(rest, params_start, b'(', b')', Language::Go)?;
    let params = &rest[params_start + 1..params_end];
    if !(params.contains("*testing.T") || params.contains("testing.TB")) {
        return None;
    }
    Some((name, line.to_string(), params_end))
}

/// The body of ANY Go function (test or helper), parsed from the function
/// keyword so a struct/interface return type's braces are never mistaken for
/// the function body.
fn go_function_body(
    source: &str,
    line_start: usize,
    line: &str,
    language: Language,
) -> Option<(String, usize, usize)> {
    let bytes = line.as_bytes();
    let mut index = 5; // past `func `
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if bytes.get(index) == Some(&b'(') {
        // Method receiver.
        index = find_matching(line, index, b'(', b')', language)? + 1;
        index = skip_ascii_space(line, index);
    }
    let (_, after_name) = read_ident(line, index)?;
    index = skip_ascii_space(line, after_name);
    if bytes.get(index) == Some(&b'[') {
        index = find_matching(line, index, b'[', b']', language)? + 1;
        index = skip_ascii_space(line, index);
    }
    if bytes.get(index) != Some(&b'(') {
        return None;
    }
    let params_end = find_matching(line, index, b'(', b')', language)?;
    go_body_after_signature(source, line_start + params_end + 1, language)
}

fn go_body_after_signature(
    source: &str,
    mut index: usize,
    language: Language,
) -> Option<(String, usize, usize)> {
    let bytes = source.as_bytes();
    loop {
        index = skip_trivia(source, index, language);
        if index >= bytes.len() {
            return None;
        }
        let byte = bytes[index];
        if is_string_start(byte, language) {
            index = skip_string(source, index, language);
            continue;
        }
        match byte {
            b'{' => {
                let end = find_matching(source, index, b'{', b'}', language)?;
                return Some((source[index + 1..end].to_string(), index, end));
            }
            b'(' | b'[' => {
                index = find_matching(
                    source,
                    index,
                    byte,
                    if byte == b'(' { b')' } else { b']' },
                    language,
                )? + 1;
            }
            b's' if go_word_at(source, index, "struct") => {
                index += "struct".len();
                index = skip_trivia(source, index, language);
                if bytes.get(index) == Some(&b'{') {
                    index = find_matching(source, index, b'{', b'}', language)? + 1;
                }
            }
            b'i' if go_word_at(source, index, "interface") => {
                index += "interface".len();
                index = skip_trivia(source, index, language);
                if bytes.get(index) == Some(&b'{') {
                    index = find_matching(source, index, b'{', b'}', language)? + 1;
                }
            }
            b';' | b'=' => return None,
            _ => index += 1,
        }
    }
}

fn go_word_at(source: &str, index: usize, word: &str) -> bool {
    source[index..].starts_with(word)
        && source
            .as_bytes()
            .get(index + word.len())
            .is_none_or(|byte| !is_ident_continue(*byte))
}

// -------------------------------------------------------------- tokenizer

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn read_ident(source: &str, start: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    if start >= bytes.len() || !is_ident_start(bytes[start]) {
        return None;
    }
    let mut end = start + 1;
    while end < bytes.len() && is_ident_continue(bytes[end]) {
        end += 1;
    }
    Some((source[start..end].to_string(), end))
}

fn is_string_start(byte: u8, language: Language) -> bool {
    match byte {
        b'"' => true,
        b'\'' => !matches!(language, Language::Rust),
        b'`' => matches!(language, Language::JavaScript | Language::Go),
        _ => false,
    }
}

fn skip_trivia(source: &str, mut index: usize, language: Language) -> usize {
    let bytes = source.as_bytes();
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            return index;
        }
        if language.hash_comments() && bytes[index] == b'#' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if language.slash_comments()
            && bytes[index] == b'/'
            && index + 1 < bytes.len()
            && bytes[index + 1] == b'/'
        {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if language.slash_comments()
            && bytes[index] == b'/'
            && index + 1 < bytes.len()
            && bytes[index + 1] == b'*'
        {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        return index;
    }
}

fn skip_string(source: &str, start: usize, language: Language) -> usize {
    let bytes = source.as_bytes();
    if start >= bytes.len() {
        return bytes.len();
    }
    if matches!(language, Language::Python | Language::Jvm) {
        let triple = if bytes[start..].starts_with(b"\"\"\"") {
            Some(b"\"\"\"".as_slice())
        } else if bytes[start..].starts_with(b"'''") {
            Some(b"'''".as_slice())
        } else {
            None
        };
        if let Some(delimiter) = triple {
            let mut index = start + 3;
            while index + 2 < bytes.len() {
                if &bytes[index..index + 3] == delimiter {
                    return index + 3;
                }
                index += 1;
            }
            return bytes.len();
        }
    }
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if bytes[index] == quote {
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

fn string_content_at(source: &str, start: usize, language: Language) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    if start >= bytes.len() || !is_string_start(bytes[start], language) {
        return None;
    }
    let quote = bytes[start];
    if quote == b'`' {
        let mut index = start + 1;
        while index < bytes.len() {
            if bytes[index] == b'\\' {
                index = (index + 2).min(bytes.len());
                continue;
            }
            if bytes[index] == b'`' {
                let content = &source[start + 1..index];
                if content.contains("${") {
                    return None;
                }
                return Some((content.to_string(), index + 1));
            }
            index += 1;
        }
        return None;
    }
    let end = skip_string(source, start, language);
    if end <= start + 1 || end > bytes.len() {
        return None;
    }
    Some((source[start + 1..end - 1].to_string(), end))
}

fn find_matching(
    source: &str,
    open_index: usize,
    open: u8,
    close: u8,
    language: Language,
) -> Option<usize> {
    let bytes = source.as_bytes();
    if open_index >= bytes.len() || bytes[open_index] != open {
        return None;
    }
    let mut depth = 0i32;
    let mut index = open_index;
    while index < bytes.len() {
        index = skip_trivia(source, index, language);
        if index >= bytes.len() {
            return None;
        }
        let byte = bytes[index];
        if is_string_start(byte, language) {
            index = skip_string(source, index, language);
            continue;
        }
        if language == Language::JavaScript && byte == b'/' {
            if let Some(end) = javascript_regex_end(source, index) {
                index = end;
                continue;
            }
        }
        if byte == open {
            depth += 1;
            index += 1;
            continue;
        }
        if byte == close {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
            index += 1;
            continue;
        }
        index += 1;
    }
    None
}

/// The end of a JavaScript/TypeScript REGEX literal at `start`, or `None`
/// when the slash is division. Regex bodies may contain braces; counting a
/// `}` inside `/}/` as a body delimiter would truncate the identity and hide
/// assertions after it.
fn javascript_regex_end(source: &str, start: usize) -> Option<usize> {
    if !javascript_regex_can_start(source, start) {
        return None;
    }
    let bytes = source.as_bytes();
    let mut index = start + 1;
    let mut in_class = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'[' => {
                in_class = true;
                index += 1;
            }
            b']' => {
                in_class = false;
                index += 1;
            }
            b'/' if !in_class => return Some(index + 1),
            b'\n' | b'\r' => return None,
            _ => index += 1,
        }
    }
    None
}

fn javascript_regex_can_start(source: &str, start: usize) -> bool {
    let bytes = source.as_bytes();
    let mut index = start;
    while index > 0 {
        index -= 1;
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            continue;
        }
        if is_ident_continue(byte) {
            let mut begin = index + 1;
            while begin > 0 && is_ident_continue(bytes[begin - 1]) {
                begin -= 1;
            }
            return matches!(
                &source[begin..=index],
                "return"
                    | "throw"
                    | "case"
                    | "typeof"
                    | "delete"
                    | "void"
                    | "in"
                    | "of"
                    | "yield"
                    | "await"
            );
        }
        return matches!(
            byte,
            b'=' | b'('
                | b'['
                | b'{'
                | b','
                | b';'
                | b':'
                | b'!'
                | b'&'
                | b'|'
                | b'?'
                | b'<'
                | b'>'
                | b'~'
        );
    }
    true
}

fn brace_delta(line: &str, language: Language) -> i32 {
    let bytes = line.as_bytes();
    let mut delta = 0i32;
    let mut index = 0usize;
    while index < bytes.len() {
        index = skip_trivia(line, index, language);
        if index >= bytes.len() {
            break;
        }
        let byte = bytes[index];
        if is_string_start(byte, language) {
            index = skip_string(line, index, language);
            continue;
        }
        match byte {
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
        index += 1;
    }
    delta
}

fn tokenize(source: &str, language: Language) -> Vec<IdentityToken> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let trivia = skip_trivia(source, index, language);
        if trivia != index {
            index = trivia;
            continue;
        }
        let byte = bytes[index];
        if language == Language::Rust {
            if let Some(end) = rust_raw_string_end(source, index) {
                out.push(IdentityToken::Literal(source[index..end].to_string()));
                index = end;
                continue;
            }
        }
        if is_ident_start(byte) {
            let (ident, end) = read_ident(source, index).expect("checked identifier start");
            out.push(IdentityToken::Ident(ident));
            index = end;
            continue;
        }
        if byte.is_ascii_digit() {
            let mut end = index + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'.')
            {
                end += 1;
            }
            out.push(IdentityToken::Literal(source[index..end].to_string()));
            index = end;
            continue;
        }
        if is_string_start(byte, language) {
            let end = skip_string(source, index, language);
            out.push(IdentityToken::Literal(source[index..end].to_string()));
            index = end;
            continue;
        }
        if language == Language::JavaScript && byte == b'/' {
            if let Some(end) = javascript_regex_end(source, index) {
                out.push(IdentityToken::Literal(source[index..end].to_string()));
                index = end;
                continue;
            }
        }
        out.push(IdentityToken::Punct(byte as char));
        index += 1;
    }
    out
}

fn rust_raw_string_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if start >= bytes.len() {
        return None;
    }
    let mut index = start;
    if bytes[index] == b'b' {
        index += 1;
    }
    if index >= bytes.len() || bytes[index] != b'r' {
        return None;
    }
    index += 1;
    let mut hashes = 0usize;
    while index < bytes.len() && bytes[index] == b'#' {
        hashes += 1;
        index += 1;
    }
    if index >= bytes.len() || bytes[index] != b'"' {
        return None;
    }
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let mut tail = index + 1;
            let mut seen = 0usize;
            while tail < bytes.len() && bytes[tail] == b'#' && seen < hashes {
                tail += 1;
                seen += 1;
            }
            if seen == hashes {
                return Some(tail);
            }
        }
        index += 1;
    }
    Some(bytes.len())
}

fn digest_tokens(tokens: &[IdentityToken]) -> String {
    let mut hasher = Hasher::new();
    for token in tokens {
        match token {
            IdentityToken::Ident(value) => {
                hasher.update(b"i");
                hasher.update(value.as_bytes());
            }
            IdentityToken::Literal(value) => {
                hasher.update(b"l");
                hasher.update(value.as_bytes());
            }
            IdentityToken::Punct(value) => {
                hasher.update(b"p");
                hasher.update(&[*value as u8]);
            }
        }
        hasher.update(b"\x1f");
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_are_dispatched_by_real_path_and_unknown_paths_fail_closed() {
        assert_eq!(
            adapter_for_path("tests/cart.rs").unwrap().language(),
            "rust"
        );
        assert_eq!(
            adapter_for_path("test/x.test.ts").unwrap().language(),
            "javascript"
        );
        assert_eq!(
            adapter_for_path("src/test/java/CartTest.java")
                .unwrap()
                .language(),
            "jvm"
        );
        assert_eq!(
            adapter_for_path("tests/test_cart.py").unwrap().language(),
            "python"
        );
        assert_eq!(adapter_for_path("cart_test.go").unwrap().language(), "go");
        assert!(adapter_for_path("README.md").is_none());
    }

    #[test]
    fn normalized_identity_ignores_comments_and_trivia_but_not_literals() {
        let a = tokenize(
            "#[test]\nfn t() {\n  // note\n  assert_eq!(1 + 1, 2);\n}\n",
            Language::Rust,
        );
        let b = tokenize("#[test]\nfn t(){assert_eq!(1+1,2);}\n", Language::Rust);
        assert_eq!(a, b);
        let c = tokenize("#[test]\nfn t(){assert_eq!(1+1,3);}\n", Language::Rust);
        assert_ne!(a, c);
    }
}
