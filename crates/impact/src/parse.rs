//! Bounded lexical parsers for Rust and the JS/TS family.
//!
//! These parsers are the always-available structural layer. They never
//! claim compiler exactness: every edge they emit is tagged
//! [`Provenance::Parser`], and the LSP adapter upgrades the same edge to
//! [`Provenance::Lsp`] when rust-analyzer/TypeScript confirms it. Parsing is
//! total (malformed sources produce a prefix, never a panic), bounded by
//! file size and token count, and deterministic.

use crate::model::{Language, MAX_SYMBOL_BYTES};

const MAX_TOKENS_PER_FILE: usize = 2_000_000;
const MAX_IMPORTS_PER_FILE: usize = 4096;
const MAX_REFS_PER_FILE: usize = 100_000;
const MAX_IMPL_PER_FILE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Function,
    Struct,
    Enum,
    Union,
    Trait,
    TypeAlias,
    Const,
    Static,
    Module,
    Macro,
}

impl DefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Union => "union",
            Self::Trait => "trait",
            Self::TypeAlias => "type",
            Self::Const => "const",
            Self::Static => "static",
            Self::Module => "module",
            Self::Macro => "macro",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDef {
    pub id: String,
    pub name: String,
    pub line: u32,
    pub kind: DefKind,
    pub is_public: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestDef {
    pub id: String,
    pub name: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImportName {
    pub local: String,
    pub original: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImportRef {
    pub segments: Vec<String>,
    pub names: Vec<ImportName>,
    pub glob: bool,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    File,
    Symbol,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub kind: OwnerKind,
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameRef {
    pub name: String,
    pub line: u32,
    pub is_call: bool,
    pub owner: Owner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplRef {
    pub trait_name: Option<String>,
    pub type_name: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    pub path: String,
    pub language: Language,
    pub is_test_file: bool,
    pub symbols: Vec<SymbolDef>,
    pub tests: Vec<TestDef>,
    pub imports: Vec<ImportRef>,
    pub mods: Vec<String>,
    pub refs: Vec<NameRef>,
    pub impls: Vec<ImplRef>,
    pub truncated: bool,
}

impl ParsedFile {
    fn empty(path: &str, language: Language, is_test_file: bool) -> Self {
        Self {
            path: path.to_string(),
            language,
            is_test_file,
            symbols: Vec::new(),
            tests: Vec::new(),
            imports: Vec::new(),
            mods: Vec::new(),
            refs: Vec::new(),
            impls: Vec::new(),
            truncated: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Tok<'a> {
    text: &'a str,
    line: u32,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Replace comments and string/char literal bodies with spaces while
/// preserving byte offsets and newlines. Raw strings (`r#"..."#`, `br#"..."#`)
/// are handled so their quotes cannot leak into tokenization.
pub fn scrub(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0usize;
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in out.iter_mut().take(to).skip(from) {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    while i < bytes.len() {
        // Line comment.
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            blank(&mut out, start, i);
            continue;
        }
        // Nested block comment.
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            let mut depth = 0usize;
            while i + 1 < bytes.len() {
                if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth = depth.saturating_sub(1);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            blank(&mut out, start, i);
            continue;
        }
        // Raw string: optional b/c prefix, r, N hashes, quote ... quote, N hashes.
        let prefix = if bytes[i] == b'r' {
            Some((i, 0usize))
        } else if (bytes[i] == b'b' || bytes[i] == b'c')
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'r'
        {
            Some((i, 1usize))
        } else {
            None
        };
        if let Some((start, r_offset)) = prefix {
            let mut j = start + r_offset + 1;
            let mut hashes = 0usize;
            while j < bytes.len() && bytes[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                j += 1;
                let body_start = j;
                while j < bytes.len() {
                    if bytes[j] == b'"' {
                        let mut k = j + 1;
                        let mut seen = 0usize;
                        while seen < hashes && k < bytes.len() && bytes[k] == b'#' {
                            seen += 1;
                            k += 1;
                        }
                        if seen == hashes {
                            j = k;
                            break;
                        }
                    }
                    j += 1;
                }
                blank(&mut out, body_start, j);
                i = j;
                continue;
            }
        }
        // Normal / byte string.
        if bytes[i] == b'"'
            || ((bytes[i] == b'b' || bytes[i] == b'c')
                && i + 1 < bytes.len()
                && bytes[i + 1] == b'"')
        {
            let quote = if bytes[i] == b'"' { i } else { i + 1 };
            let start = quote + 1;
            let mut j = start;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if bytes[j] == b'"' {
                    j += 1;
                    break;
                }
                j += 1;
            }
            blank(&mut out, start, j.saturating_sub(1).max(start));
            i = j;
            continue;
        }
        // Char literal vs lifetime.
        if bytes[i] == b'\'' {
            let lifetime = i + 1 < bytes.len() && is_ident_start(bytes[i + 1]) && {
                let mut j = i + 1;
                while j < bytes.len() && is_ident_continue(bytes[j]) {
                    j += 1;
                }
                j >= bytes.len() || bytes[j] != b'\''
            };
            if !lifetime {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() {
                    if bytes[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if bytes[j] == b'\'' {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                blank(&mut out, start, j.saturating_sub(1).max(start));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    // Scrubbing only replaces ASCII with ASCII, so UTF-8 boundaries hold.
    String::from_utf8(out).unwrap_or_else(|_| source.to_string())
}

fn tokenize<'a>(cleaned: &'a str, truncated: &mut bool) -> Vec<Tok<'a>> {
    let bytes = cleaned.as_bytes();
    let mut out = Vec::new();
    let mut line = 1u32;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if out.len() >= MAX_TOKENS_PER_FILE {
            *truncated = true;
            break;
        }
        if is_ident_start(b) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            out.push(Tok {
                text: &cleaned[start..i],
                line,
            });
            continue;
        }
        if b == b':' && i + 1 < bytes.len() && bytes[i + 1] == b':' {
            out.push(Tok { text: "::", line });
            i += 2;
            continue;
        }
        // Single punctuation byte. Multi-byte UTF-8 is skipped one byte at a
        // time only for non-ASCII; those bytes are never identifiers.
        if b.is_ascii() {
            out.push(Tok {
                text: &cleaned[i..i + 1],
                line,
            });
        }
        i += 1;
    }
    out
}

fn next_ident<'a>(tokens: &[Tok<'a>], index: usize) -> Option<(usize, &'a str)> {
    tokens
        .get(index)
        .filter(|t| is_ident_start(t.text.as_bytes()[0]))
        .map(|t| (index, t.text))
}

fn matching(tokens: &[Tok<'_>], start: usize, open: &str, close: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, token) in tokens.iter().enumerate().skip(start) {
        if token.text == open {
            depth += 1;
        } else if token.text == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(offset);
            }
        }
    }
    None
}

fn is_test_attribute(tokens: &[Tok<'_>]) -> bool {
    tokens.iter().any(|t| t.text == "test")
}

fn parse_use(tokens: &[Tok<'_>], start: usize, end: usize) -> Vec<ImportRef> {
    // `start` is the `use` keyword; `end` is the `;`.
    let mut out = Vec::new();
    let mut index = start + 1;
    let line = tokens[start].line;
    parse_use_tree(tokens, &mut index, end, &[], line, &mut out);
    out
}

fn parse_use_tree(
    tokens: &[Tok<'_>],
    index: &mut usize,
    end: usize,
    prefix: &[String],
    line: u32,
    out: &mut Vec<ImportRef>,
) {
    let mut segments: Vec<String> = prefix.to_vec();
    let mut names = Vec::new();
    let mut glob = false;
    while *index < end {
        let token = &tokens[*index];
        match token.text {
            "{" => {
                let Some(close) = matching(tokens, *index, "{", "}") else {
                    return;
                };
                *index += 1;
                while *index < close {
                    if tokens[*index].text == "," {
                        *index += 1;
                        continue;
                    }
                    let before = *index;
                    parse_use_tree(tokens, index, close, &segments, line, out);
                    if *index == before {
                        *index += 1;
                    }
                }
                *index = close.saturating_add(1);
                return;
            }
            ";" => break,
            "*" => {
                glob = true;
                *index += 1;
            }
            "as" => {
                let alias = tokens
                    .get(*index + 1)
                    .map(|t| t.text.to_string())
                    .unwrap_or_default();
                if let Some(last) = segments.pop() {
                    if !alias.is_empty() {
                        names.push(ImportName {
                            local: alias,
                            original: last,
                        });
                    }
                }
                *index += 1;
            }
            "," => break,
            "::" => *index += 1,
            other => {
                if is_ident_start(other.as_bytes().first().copied().unwrap_or(b' ')) {
                    segments.push(other.to_string());
                }
                *index += 1;
            }
        }
    }
    if *index < end && tokens[*index].text == "as" {
        let alias = tokens
            .get(*index + 1)
            .map(|t| t.text.to_string())
            .unwrap_or_default();
        if let Some(last) = segments.pop() {
            if !alias.is_empty() {
                names.push(ImportName {
                    local: alias,
                    original: last,
                });
            }
        }
        *index += 1;
    }
    if segments.is_empty() {
        return;
    }
    if names.is_empty() && !glob {
        if let Some(last) = segments.last() {
            if last != "self" && last != "super" && last != "crate" {
                names.push(ImportName {
                    local: last.clone(),
                    original: last.clone(),
                });
            }
        }
    }
    out.push(ImportRef {
        segments,
        names,
        glob,
        line,
    });
}

fn collect_refs(
    tokens: &[Tok<'_>],
    start: usize,
    end: usize,
    owner: &Owner,
    out: &mut Vec<NameRef>,
) {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "union", "unsafe", "use", "where", "while", "yield",
    ];
    let mut index = start;
    while index < end {
        let token = &tokens[index];
        if !is_ident_start(token.text.as_bytes()[0]) || KEYWORDS.contains(&token.text) {
            index += 1;
            continue;
        }
        // Skip the declared name of a nested item.
        let prev = index.checked_sub(1).map(|i| tokens[i].text);
        if matches!(
            prev,
            Some(
                "fn" | "struct" | "enum" | "trait" | "type" | "const" | "static" | "mod" | "union"
            )
        ) {
            index += 1;
            continue;
        }
        let is_call = tokens.get(index + 1).is_some_and(|t| t.text == "(");
        if out.len() < MAX_REFS_PER_FILE {
            out.push(NameRef {
                name: token.text.to_string(),
                line: token.line,
                is_call,
                owner: owner.clone(),
            });
        }
        index += 1;
    }
}

pub fn parse_rust(path: &str, source: &str, is_test_file: bool) -> ParsedFile {
    let cleaned = scrub(source);
    let mut truncated = false;
    let tokens = tokenize(&cleaned, &mut truncated);
    let mut out = ParsedFile::empty(path, Language::Rust, is_test_file);
    out.truncated = truncated;
    let mut index = 0usize;
    let mut pending_test = false;
    let mut pending_cfg_test = false;
    while index < tokens.len() {
        let token = &tokens[index];
        if token.text == "#" {
            let next = index + 1;
            if tokens.get(next).is_some_and(|t| t.text == "[") {
                if let Some(end) = matching(&tokens, next, "[", "]") {
                    let inner = &tokens[next + 1..end];
                    let is_cfg = inner.iter().any(|t| t.text == "cfg");
                    if is_test_attribute(inner) && !is_cfg {
                        pending_test = true;
                    }
                    if is_cfg && inner.iter().any(|t| t.text == "test") {
                        pending_cfg_test = true;
                    }
                    index = end + 1;
                    continue;
                }
            }
            index += 1;
            continue;
        }
        let mut is_public = false;
        let mut look = index;
        if token.text == "pub" {
            is_public = true;
            look += 1;
            if tokens.get(look).is_some_and(|t| t.text == "(") {
                if let Some(end) = matching(&tokens, look, "(", ")") {
                    look = end + 1;
                }
            }
        }
        let Some(keyword) = tokens.get(look).map(|t| t.text) else {
            break;
        };
        match keyword {
            "fn" => {
                if let Some((name_index, name)) = next_ident(&tokens, look + 1) {
                    if name.len() <= MAX_SYMBOL_BYTES {
                        let line = tokens[name_index].line;
                        let id = crate::model::symbol_id(path, name, line);
                        out.symbols.push(SymbolDef {
                            id: id.clone(),
                            name: name.to_string(),
                            line,
                            kind: DefKind::Function,
                            is_public,
                        });
                        let is_test = pending_test || (pending_cfg_test && is_test_file);
                        if is_test {
                            let test = crate::model::test_id(path, name, line);
                            out.tests.push(TestDef {
                                id: test.clone(),
                                name: name.to_string(),
                                line,
                            });
                        }
                        let body = find_body(&tokens, look + 1);
                        let owner = if is_test {
                            Owner {
                                kind: OwnerKind::Test,
                                id: crate::model::test_id(path, name, line),
                                name: name.to_string(),
                            }
                        } else {
                            Owner {
                                kind: OwnerKind::Symbol,
                                id: id.clone(),
                                name: name.to_string(),
                            }
                        };
                        if let Some((body_start, body_end)) = body {
                            collect_refs(&tokens, body_start, body_end, &owner, &mut out.refs);
                            index = body_end;
                        } else {
                            index = name_index + 1;
                        }
                        pending_test = false;
                        continue;
                    }
                }
                pending_test = false;
            }
            "struct" | "enum" | "union" | "trait" | "type" | "const" | "static" | "mod" => {
                if let Some((name_index, name)) = next_ident(&tokens, look + 1) {
                    if name.len() <= MAX_SYMBOL_BYTES {
                        let kind = match keyword {
                            "struct" => DefKind::Struct,
                            "enum" => DefKind::Enum,
                            "union" => DefKind::Union,
                            "trait" => DefKind::Trait,
                            "type" => DefKind::TypeAlias,
                            "const" => DefKind::Const,
                            "static" => DefKind::Static,
                            _ => DefKind::Module,
                        };
                        out.symbols.push(SymbolDef {
                            id: crate::model::symbol_id(path, name, tokens[name_index].line),
                            name: name.to_string(),
                            line: tokens[name_index].line,
                            kind,
                            is_public,
                        });
                        if keyword == "mod"
                            && tokens.get(name_index + 1).is_some_and(|t| t.text == ";")
                        {
                            out.mods.push(name.to_string());
                        }
                    }
                }
                pending_test = false;
            }
            "macro_rules" => {
                if tokens.get(look + 1).is_some_and(|t| t.text == "!") {
                    if let Some((name_index, name)) = next_ident(&tokens, look + 2) {
                        out.symbols.push(SymbolDef {
                            id: crate::model::symbol_id(path, name, tokens[name_index].line),
                            name: name.to_string(),
                            line: tokens[name_index].line,
                            kind: DefKind::Macro,
                            is_public,
                        });
                    }
                }
                pending_test = false;
            }
            "use" => {
                let mut end = look;
                while end < tokens.len() && tokens[end].text != ";" {
                    end += 1;
                }
                if end < tokens.len() {
                    let line = tokens[look].line;
                    let mut imports = parse_use(&tokens, look, end);
                    for import in &mut imports {
                        if import.line == 0 {
                            import.line = line;
                        }
                    }
                    for import in imports {
                        if out.imports.len() < MAX_IMPORTS_PER_FILE {
                            out.imports.push(import);
                        }
                    }
                    index = end + 1;
                } else {
                    index = look + 1;
                }
                pending_test = false;
                pending_cfg_test = false;
                continue;
            }
            "impl" => {
                if out.impls.len() < MAX_IMPL_PER_FILE {
                    if let Some(parsed) = parse_impl(&tokens, look) {
                        out.impls.push(parsed.0.impl_ref);
                        index = parsed.1;
                        pending_test = false;
                        continue;
                    }
                }
                pending_test = false;
            }
            _ => {
                pending_test = false;
            }
        }
        index += 1;
    }
    // Deterministic order regardless of source order.
    out.symbols
        .sort_by(|a, b| (&a.name, a.line, &a.id).cmp(&(&b.name, b.line, &b.id)));
    out.tests
        .sort_by(|a, b| (&a.name, a.line, &a.id).cmp(&(&b.name, b.line, &b.id)));
    out.imports
        .sort_by(|a, b| (a.line, &a.segments, &a.names).cmp(&(b.line, &b.segments, &b.names)));
    out.mods.sort();
    out.mods.dedup();
    out.refs.sort_by(|a, b| {
        (a.line, &a.name, a.is_call, &a.owner.id).cmp(&(b.line, &b.name, b.is_call, &b.owner.id))
    });
    out.impls.sort_by(|a, b| {
        (a.line, &a.type_name, &a.trait_name).cmp(&(b.line, &b.type_name, &b.trait_name))
    });
    out
}

struct ImplParse {
    impl_ref: ImplRef,
}

fn parse_impl(tokens: &[Tok<'_>], start: usize) -> Option<(ImplParse, usize)> {
    let mut header = Vec::new();
    let mut index = start + 1;
    while index < tokens.len() {
        match tokens[index].text {
            "{" => break,
            ";" => return None,
            other => header.push((index, other)),
        }
        index += 1;
    }
    let body_start = index;
    let body_end = matching(tokens, body_start, "{", "}").unwrap_or(tokens.len().saturating_sub(1));
    let for_index = header.iter().position(|(_, text)| *text == "for");
    let ident = |text: &str| is_ident_start(text.as_bytes().first().copied().unwrap_or(b' '));
    let (trait_name, type_name) = match for_index {
        Some(position) => {
            let trait_name = header[..position]
                .iter()
                .rev()
                .find(|(_, text)| ident(text))
                .map(|(_, text)| text.to_string());
            let type_name = header[position + 1..]
                .iter()
                .find(|(_, text)| ident(text))
                .map(|(_, text)| text.to_string())?;
            (trait_name, type_name)
        }
        None => {
            let type_name = header
                .iter()
                .skip(1)
                .find(|(_, text)| ident(text))
                .map(|(_, text)| text.to_string())?;
            (None, type_name)
        }
    };
    let line = tokens.get(start).map(|t| t.line).unwrap_or(0);
    Some((
        ImplParse {
            impl_ref: ImplRef {
                trait_name,
                type_name,
                line,
            },
        },
        body_end,
    ))
}

/// Find the body brace of an item starting at `search`: the first `{` before
/// a top-level `;`. Returns token indices exclusive of the braces.
fn find_body(tokens: &[Tok<'_>], search: usize) -> Option<(usize, usize)> {
    let mut index = search;
    while index < tokens.len() {
        match tokens[index].text {
            ";" => return None,
            "{" => {
                let end = matching(tokens, index, "{", "}")?;
                return Some((index + 1, end));
            }
            _ => index += 1,
        }
    }
    None
}

pub fn parse_javascript(path: &str, source: &str, is_test_file: bool) -> ParsedFile {
    let cleaned = scrub(source);
    let mut truncated = false;
    let tokens = tokenize(&cleaned, &mut truncated);
    let mut out = ParsedFile::empty(path, Language::JavaScript, is_test_file);
    out.truncated = truncated;
    let mut index = 0usize;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.text {
            "import" => {
                if let Some((import, next)) = parse_js_import(source, &tokens, index) {
                    if out.imports.len() < MAX_IMPORTS_PER_FILE {
                        out.imports.push(import);
                    }
                    index = next;
                    continue;
                }
            }
            "require" => {
                if tokens.get(index + 1).is_some_and(|t| t.text == "(") {
                    if let Some(import) =
                        parse_js_string_import(source, &tokens, index + 2, "require")
                    {
                        if out.imports.len() < MAX_IMPORTS_PER_FILE {
                            out.imports.push(import);
                        }
                    }
                }
            }
            "function" => {
                if let Some((name_index, name)) = next_ident(&tokens, index + 1) {
                    out.symbols.push(SymbolDef {
                        id: crate::model::symbol_id(path, name, tokens[name_index].line),
                        name: name.to_string(),
                        line: tokens[name_index].line,
                        kind: DefKind::Function,
                        is_public: true,
                    });
                    if let Some((body_start, body_end)) = find_body(&tokens, name_index + 1) {
                        let owner = Owner {
                            kind: OwnerKind::Symbol,
                            id: crate::model::symbol_id(path, name, tokens[name_index].line),
                            name: name.to_string(),
                        };
                        collect_refs(&tokens, body_start, body_end, &owner, &mut out.refs);
                        index = body_end;
                        continue;
                    }
                }
            }
            "class" => {
                if let Some((name_index, name)) = next_ident(&tokens, index + 1) {
                    out.symbols.push(SymbolDef {
                        id: crate::model::symbol_id(path, name, tokens[name_index].line),
                        name: name.to_string(),
                        line: tokens[name_index].line,
                        kind: DefKind::Struct,
                        is_public: true,
                    });
                }
            }
            "const" | "let" | "var" => {
                if let Some((name_index, name)) = next_ident(&tokens, index + 1) {
                    if tokens.get(name_index + 1).is_some_and(|t| t.text == "=") {
                        out.symbols.push(SymbolDef {
                            id: crate::model::symbol_id(path, name, tokens[name_index].line),
                            name: name.to_string(),
                            line: tokens[name_index].line,
                            kind: DefKind::Const,
                            is_public: true,
                        });
                    }
                }
            }
            "test" | "it" => {
                if tokens.get(index + 1).is_some_and(|t| t.text == "(") {
                    let line = token.line;
                    let name = js_call_name(source, &tokens, index + 2)
                        .unwrap_or_else(|| format!("test@{line}"));
                    let test_id = crate::model::test_id(path, &name, line);
                    out.tests.push(TestDef {
                        id: test_id.clone(),
                        name: name.clone(),
                        line,
                    });
                    if let Some(close) = matching(&tokens, index + 1, "(", ")") {
                        // The reference owner MUST be the same id as the test
                        // node: the old fallback id (`test@line`) produced a
                        // dangling edge whenever the test had a real name.
                        let owner = Owner {
                            kind: OwnerKind::Test,
                            id: test_id,
                            name,
                        };
                        collect_refs(&tokens, index + 2, close, &owner, &mut out.refs);
                    }
                }
            }
            "describe" => {
                if let Some(close) = matching(&tokens, index + 1, "(", ")") {
                    index = close;
                    continue;
                }
            }
            _ => {}
        }
        index += 1;
    }
    out.symbols
        .sort_by(|a, b| (&a.name, a.line, &a.id).cmp(&(&b.name, b.line, &b.id)));
    out.tests
        .sort_by(|a, b| (&a.name, a.line, &a.id).cmp(&(&b.name, b.line, &b.id)));
    out.imports
        .sort_by(|a, b| (a.line, &a.segments, &a.names).cmp(&(b.line, &b.segments, &b.names)));
    out.refs.sort_by(|a, b| {
        (a.line, &a.name, a.is_call, &a.owner.id).cmp(&(b.line, &b.name, b.is_call, &b.owner.id))
    });
    out
}

fn js_call_name(source: &str, tokens: &[Tok<'_>], index: usize) -> Option<String> {
    let token = tokens.get(index)?;
    if token.text.starts_with('\'') || token.text.starts_with('"') {
        let name = token.text.trim_matches(['\'', '"']);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    // Strings are scrubbed in the token stream; recover the first quoted
    // literal on the same line from the original source.
    let line_start = source
        .lines()
        .take(token.line.saturating_sub(1) as usize)
        .map(|line| line.len() + 1)
        .sum::<usize>();
    let line = source[line_start..].lines().next().unwrap_or("");
    for quote in ['\'', '"', '`'] {
        if let Some(start) = line.find(quote) {
            if let Some(end) = line[start + 1..].find(quote) {
                let name = line[start + 1..start + 1 + end].trim();
                if !name.is_empty() {
                    return Some(name.chars().take(MAX_SYMBOL_BYTES).collect());
                }
            }
        }
    }
    None
}

fn parse_js_import(source: &str, tokens: &[Tok<'_>], index: usize) -> Option<(ImportRef, usize)> {
    let mut names = Vec::new();
    let mut cursor = index + 1;
    let mut saw_from = false;
    while cursor < tokens.len() {
        match tokens[cursor].text {
            "from" => {
                saw_from = true;
                cursor += 1;
                break;
            }
            "{" => {
                let close = matching(tokens, cursor, "{", "}")?;
                let mut item = cursor + 1;
                while item < close {
                    if is_ident_start(tokens[item].text.as_bytes()[0]) {
                        let original = tokens[item].text.to_string();
                        let mut local = original.clone();
                        if tokens.get(item + 1).is_some_and(|t| t.text == "as") {
                            if let Some(alias) = tokens.get(item + 2) {
                                local = alias.text.to_string();
                                item += 2;
                            }
                        }
                        names.push(ImportName { local, original });
                    }
                    item += 1;
                }
                cursor = close + 1;
            }
            ";" => return None,
            "*" => cursor += 1,
            other if is_ident_start(other.as_bytes()[0]) => {
                if tokens.get(cursor + 1).is_some_and(|t| t.text == "as") {
                    cursor += 3;
                } else {
                    names.push(ImportName {
                        local: other.to_string(),
                        original: other.to_string(),
                    });
                    cursor += 1;
                }
            }
            _ => cursor += 1,
        }
    }
    if !saw_from {
        return None;
    }
    let line = tokens[index].line;
    // The module string was scrubbed; recover it from the source line.
    let module = source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .and_then(|text| {
            for quote in ['\'', '"'] {
                if let Some(start) = text.find(quote) {
                    if let Some(end) = text[start + 1..].find(quote) {
                        return Some(text[start + 1..start + 1 + end].to_string());
                    }
                }
            }
            None
        })?;
    let import = ImportRef {
        segments: module.split('/').map(str::to_string).collect(),
        names,
        glob: false,
        line,
    };
    Some((import, cursor))
}

fn parse_js_string_import(
    source: &str,
    tokens: &[Tok<'_>],
    index: usize,
    _kind: &str,
) -> Option<ImportRef> {
    let line = tokens.get(index.saturating_sub(1))?.line;
    let module = source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .and_then(|text| {
            for quote in ['\'', '"', '`'] {
                if let Some(start) = text.find(quote) {
                    if let Some(end) = text[start + 1..].find(quote) {
                        return Some(text[start + 1..start + 1 + end].to_string());
                    }
                }
            }
            None
        })?;
    Some(ImportRef {
        segments: module.split('/').map(str::to_string).collect(),
        names: Vec::new(),
        glob: false,
        line,
    })
}

pub fn parse_file(path: &str, language: Language, source: &str, is_test_file: bool) -> ParsedFile {
    match language {
        Language::Rust => parse_rust(path, source, is_test_file),
        Language::JavaScript | Language::TypeScript => parse_javascript(path, source, is_test_file),
        Language::Python => parse_python(path, source, is_test_file),
        Language::Java | Language::Kotlin => parse_jvm(path, source, language, is_test_file),
    }
}

/// Bounded Python structural parser: functions/classes, imports and test
/// definitions (`def test_*` plus unittest/pytest class methods).
pub fn parse_python(path: &str, source: &str, is_test_file: bool) -> ParsedFile {
    let mut out = ParsedFile::empty(path, Language::Python, is_test_file);
    for (index, line) in source.lines().enumerate() {
        if index >= 200_000 {
            out.truncated = true;
            break;
        }
        let line_no = (index + 1) as u32;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("import ") {
            for name in rest.split(',').take(MAX_IMPORTS_PER_FILE) {
                let name = name.trim().split(" as ").next().unwrap_or("").trim();
                if !name.is_empty() {
                    out.imports.push(ImportRef {
                        segments: name.split('.').map(str::to_string).collect(),
                        names: Vec::new(),
                        glob: false,
                        line: line_no,
                    });
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("from ") {
            let mut parts = rest.splitn(2, " import ");
            let module = parts.next().unwrap_or("").trim();
            let names = parts.next().unwrap_or("");
            if !module.is_empty() && !module.starts_with('.') {
                let mut segments: Vec<String> = module.split('.').map(str::to_string).collect();
                let mut imported = Vec::new();
                for name in names.trim_matches(|c| c == '(' || c == ')').split(',') {
                    let name = name.trim();
                    if name.is_empty() || name == "*" {
                        continue;
                    }
                    let name = name.split(" as ").next().unwrap_or(name).trim();
                    if !name.is_empty() {
                        imported.push(ImportName {
                            local: name.to_string(),
                            original: name.to_string(),
                        });
                    }
                }
                if imported.is_empty() {
                    segments = segments.into_iter().take(1).collect();
                }
                out.imports.push(ImportRef {
                    segments,
                    names: imported,
                    glob: names.contains('*'),
                    line: line_no,
                });
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("def ") {
            let name = rest
                .split('(')
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches("async ");
            if is_ident_name(name) {
                let is_test = name.starts_with("test") || is_test_file;
                if is_test {
                    out.tests.push(TestDef {
                        id: crate::model::test_id(path, name, line_no),
                        name: name.to_string(),
                        line: line_no,
                    });
                } else {
                    out.symbols.push(SymbolDef {
                        id: crate::model::symbol_id(path, name, line_no),
                        name: name.to_string(),
                        line: line_no,
                        kind: DefKind::Function,
                        is_public: !name.starts_with('_'),
                    });
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("class ") {
            let name = rest.split(['(', ':']).next().unwrap_or("").trim();
            if is_ident_name(name) {
                out.symbols.push(SymbolDef {
                    id: crate::model::symbol_id(path, name, line_no),
                    name: name.to_string(),
                    line: line_no,
                    kind: DefKind::Struct,
                    is_public: !name.starts_with('_'),
                });
            }
        }
    }
    out
}

fn is_ident_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SYMBOL_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        && !name.as_bytes()[0].is_ascii_digit()
}

/// Bounded JVM structural parser: imports plus JUnit-style `@Test`
/// annotated methods and their owning classes.
pub fn parse_jvm(path: &str, source: &str, language: Language, is_test_file: bool) -> ParsedFile {
    let mut out = ParsedFile::empty(path, language, is_test_file);
    let lines: Vec<&str> = source.lines().take(200_000).collect();
    if source.lines().count() > 200_000 {
        out.truncated = true;
    }
    let mut pending_test = false;
    for (index, line) in lines.iter().enumerate() {
        let line_no = (index + 1) as u32;
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("import ") {
            let module = rest.trim_end_matches(';').trim();
            if !module.is_empty() && !module.starts_with("java.") {
                let segments: Vec<String> = module.split('.').map(str::to_string).collect();
                out.imports.push(ImportRef {
                    segments,
                    names: Vec::new(),
                    glob: false,
                    line: line_no,
                });
            }
            continue;
        }
        if trimmed.starts_with("@Test")
            || trimmed.contains(" @Test")
            || trimmed.starts_with("@org.junit.Test")
        {
            pending_test = true;
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("class ")
            .or_else(|| trimmed.strip_prefix("public class "))
            .or_else(|| trimmed.strip_prefix("internal class "))
        {
            let name = rest.split(['(', ' ', '{', ':']).next().unwrap_or("").trim();
            if is_ident_name(name) {
                out.symbols.push(SymbolDef {
                    id: crate::model::symbol_id(path, name, line_no),
                    name: name.to_string(),
                    line: line_no,
                    kind: DefKind::Struct,
                    is_public: true,
                });
            }
            continue;
        }
        if pending_test || is_test_file {
            let candidate = trimmed
                .split('(')
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches('{')
                .trim();
            let name = candidate.split_whitespace().last().unwrap_or(candidate);
            if is_ident_name(name) && name != "void" && name != "fun" {
                out.tests.push(TestDef {
                    id: crate::model::test_id(path, name, line_no),
                    name: name.to_string(),
                    line: line_no,
                });
                pending_test = false;
            }
        }
    }
    out
}
