//! Rust lexical helpers for declaration discovery and proof identity.
//!
//! Braces inside string/char literals, raw strings and comments are never
//! delimiters. A body truncated at a `"}"` literal would hide every assertion
//! after it and let an inequivalent rewrite look identical.

/// Lexical state that must survive line boundaries (block comments, raw
/// strings and ordinary strings may span lines in Rust source).
#[derive(Debug, Default)]
pub(crate) struct ScanState {
    block_comment_depth: usize,
    raw_string_hashes: Option<usize>,
    in_string: bool,
}

impl ScanState {
    /// TRUE when a new line starts in ordinary code (not inside a comment or
    /// literal): declaration-looking text in trivia is never a declaration.
    pub(crate) fn at_code_boundary(&self) -> bool {
        self.block_comment_depth == 0 && !self.in_string && self.raw_string_hashes.is_none()
    }
}

/// The bounded body of the function starting at `start`: everything after
/// the first CODE opening brace through the matching closing brace.
///
/// Braces inside string/char literals, raw strings and comments are never
/// delimiters. That matters for proof identity: a body truncated at a `"}"`
/// literal would hide every assertion after it and let an inequivalent
/// rewrite look identical. Malformed / unterminated declarations yield an
/// empty body (the declaration adapters fail closed).
pub(crate) fn test_body(lines: &[&str], start: usize) -> (String, bool) {
    let source = lines.get(start..).unwrap_or_default().join("\n");
    match rust_body_span(&source) {
        Some((open, close)) => (source[open + 1..close].to_string(), true),
        None => (String::new(), false),
    }
}

/// The byte span of the first code-level `{ ... }` pair in `source`.
#[allow(clippy::collapsible_match)]
fn rust_body_span(source: &str) -> Option<(usize, usize)> {
    let mut state = ScanState::default();
    let mut open: Option<usize> = None;
    let mut depth = 0usize;
    let mut found: Option<(usize, usize)> = None;
    rust_scan_code(source, &mut state, |index, byte| {
        match byte {
            b'{' => {
                if open.is_none() {
                    open = Some(index);
                }
                depth += 1;
            }
            b'}' => {
                if open.is_some() && depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        found = open.map(|open| (open, index));
                        return false;
                    }
                }
            }
            _ => {}
        }
        true
    });
    found
}

/// Walk only the CODE bytes of `source` (comments and literals are skipped),
/// invoking `visit` for each code byte. `visit` returns `false` to stop.
fn rust_scan_code(source: &str, state: &mut ScanState, mut visit: impl FnMut(usize, u8) -> bool) {
    let bytes = source.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if let Some(hashes) = state.raw_string_hashes {
            if bytes[index] == b'"' && raw_closes_at(bytes, index + 1, hashes) {
                index += 1 + hashes;
                state.raw_string_hashes = None;
            } else {
                index += 1;
            }
            continue;
        }
        if state.block_comment_depth > 0 {
            if bytes[index] == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
                state.block_comment_depth += 1;
                index += 2;
            } else if bytes[index] == b'*' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
                state.block_comment_depth -= 1;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if state.in_string {
            match bytes[index] {
                b'\\' => index = (index + 2).min(bytes.len()),
                b'"' => {
                    state.in_string = false;
                    index += 1;
                }
                _ => index += 1,
            }
            continue;
        }
        match bytes[index] {
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'/' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'*' => {
                state.block_comment_depth = 1;
                index += 2;
            }
            b'"' => {
                state.in_string = true;
                index += 1;
            }
            b'\'' => {
                if let Some(end) = rust_char_literal_end(bytes, index) {
                    index = end;
                } else {
                    index += 1;
                }
            }
            b'r' | b'b' | b'c' => {
                if let Some((end, hashes)) = rust_raw_string_start(bytes, index) {
                    state.raw_string_hashes = Some(hashes);
                    index = end;
                } else {
                    if !visit(index, bytes[index]) {
                        return;
                    }
                    index += 1;
                }
            }
            byte => {
                if !visit(index, byte) {
                    return;
                }
                index += 1;
            }
        }
    }
}

fn raw_closes_at(bytes: &[u8], start: usize, hashes: usize) -> bool {
    start + hashes <= bytes.len()
        && bytes[start..start + hashes]
            .iter()
            .all(|byte| *byte == b'#')
}

/// `r"..."`, `r#"..."#`, `br#"..."#`, `cr"..."`: the opening quote's end
/// index and the number of hashes that close it.
fn rust_raw_string_start(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut index = start;
    if matches!(bytes.get(index), Some(b'b' | b'c')) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'r') {
        return None;
    }
    index += 1;
    let mut hashes = 0usize;
    while bytes.get(index) == Some(&b'#') {
        hashes += 1;
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    Some((index + 1, hashes))
}

/// The end of a Rust char literal at `start`, or `None` for a lifetime.
fn rust_char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    if index >= bytes.len() {
        return None;
    }
    if bytes[index] == b'\\' {
        index += 1;
        match bytes.get(index) {
            Some(b'u') if bytes.get(index + 1) == Some(&b'{') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'}' {
                    index += 1;
                }
                index += 1;
            }
            Some(b'x') => index += 3,
            Some(_) => index += 1,
            None => return None,
        }
        return (bytes.get(index) == Some(&b'\'')).then_some(index + 1);
    }
    let character = std::str::from_utf8(&bytes[index..]).ok()?.chars().next()?;
    let end = index + character.len_utf8();
    (bytes.get(end) == Some(&b'\'')).then_some(end + 1)
}

/// Code-level brace delta of one line (literals/comments ignored), carrying
/// the lexical state so a multi-line string/comment cannot shift the module
/// nesting and mis-attribute every later test declaration.
pub(crate) fn brace_delta(state: &mut ScanState, line: &str) -> i32 {
    let mut delta = 0i32;
    rust_scan_code(line, state, |_, byte| {
        match byte {
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
        true
    });
    delta
}
