//! A bounded, dependency-free structural DOM scanner (spec §8: structured
//! markup / semantic DOM extraction with hard bounds).
//!
//! The connectors do not ship an HTML parser: hostile markup must not be able
//! to allocate unbounded memory or recurse. This scanner is a single forward
//! pass with hard bounds on node count, tag length, attribute count and text
//! length, and it skips `<script>`/`<style>` bodies (those are handled by the
//! embedded-state strategy, which reads the raw block on purpose).
//!
//! The output is deliberately lossy: tags, attributes and collapsed text —
//! enough to anchor semantic fields, never a rendering engine.

/// Hard bound on scanned nodes.
pub const MAX_DOM_NODES: usize = 4096;
/// Hard bound on one tag name.
pub const MAX_TAG_BYTES: usize = 32;
/// Hard bound on attributes per node.
pub const MAX_ATTRIBUTES: usize = 32;
/// Hard bound on one attribute name / value.
pub const MAX_ATTRIBUTE_NAME_BYTES: usize = 64;
/// Hard bound on one attribute value.
pub const MAX_ATTRIBUTE_VALUE_BYTES: usize = 512;
/// Hard bound on one node's text.
pub const MAX_TEXT_BYTES: usize = 2048;

/// One scanned element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomNode {
    /// The lowercased tag name.
    pub tag: String,
    /// The lowercased attributes.
    pub attributes: Vec<(String, String)>,
    /// The collapsed text content (empty for void/script/style elements).
    pub text: String,
}

impl DomNode {
    /// The attribute value, case-insensitively.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The `id` attribute.
    pub fn id(&self) -> Option<&str> {
        self.attr("id")
    }

    /// A `data-*` attribute value.
    pub fn data(&self, name: &str) -> Option<&str> {
        self.attr(&format!("data-{name}"))
    }

    /// True when a class token is present.
    pub fn has_class(&self, class: &str) -> bool {
        self.attr("class")
            .is_some_and(|classes| classes.split_whitespace().any(|token| token == class))
    }

    /// A stable landmark signature for fingerprinting.
    pub fn landmark(&self) -> String {
        let mut out = self.tag.clone();
        if let Some(id) = self.id() {
            out.push('#');
            out.push_str(id);
        }
        if let Some(classes) = self.attr("class") {
            let mut tokens: Vec<&str> = classes.split_whitespace().take(8).collect();
            tokens.sort();
            for token in tokens {
                out.push('.');
                out.push_str(token);
            }
        }
        out
    }
}

/// Scan one HTML document into bounded elements.
pub fn scan(html: &str) -> Vec<DomNode> {
    let bytes = html.as_bytes();
    let mut nodes: Vec<DomNode> = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() && nodes.len() < MAX_DOM_NODES {
        let Some(offset) = html[index..].find('<') else {
            break;
        };
        index += offset;
        if html[index..].starts_with("<!--") {
            match html[index..].find("-->") {
                Some(end) => index += end + 3,
                None => break,
            }
            continue;
        }
        if html[index..].starts_with("<!") || html[index..].starts_with("<?") {
            match html[index..].find('>') {
                Some(end) => index += end + 1,
                None => break,
            }
            continue;
        }
        if html[index..].starts_with("</") {
            match html[index..].find('>') {
                Some(end) => index += end + 1,
                None => break,
            }
            continue;
        }
        index += 1;
        let name_start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'-' | b'_' | b':'))
            && index - name_start < MAX_TAG_BYTES
        {
            index += 1;
        }
        if index == name_start {
            continue;
        }
        let tag = html[name_start..index].to_ascii_lowercase();
        let mut attributes: Vec<(String, String)> = Vec::new();
        let mut self_closing = false;
        while index < bytes.len() {
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index >= bytes.len() {
                break;
            }
            if bytes[index] == b'>' {
                index += 1;
                break;
            }
            if bytes[index] == b'/' && html[index..].starts_with("/>") {
                self_closing = true;
                index += 2;
                break;
            }
            let attribute_start = index;
            while index < bytes.len()
                && !bytes[index].is_ascii_whitespace()
                && !matches!(bytes[index], b'=' | b'>' | b'/')
                && index - attribute_start < MAX_ATTRIBUTE_NAME_BYTES
            {
                index += 1;
            }
            if index == attribute_start {
                index += 1;
                continue;
            }
            let name = html[attribute_start..index].to_ascii_lowercase();
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            let mut value = String::new();
            if index < bytes.len() && bytes[index] == b'=' {
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                if index < bytes.len() && matches!(bytes[index], b'"' | b'\'') {
                    let quote = bytes[index];
                    index += 1;
                    let value_start = index;
                    while index < bytes.len() && bytes[index] != quote {
                        index += 1;
                    }
                    value = html[value_start..index.min(bytes.len())].to_string();
                    if index < bytes.len() {
                        index += 1;
                    }
                } else {
                    let value_start = index;
                    while index < bytes.len()
                        && !bytes[index].is_ascii_whitespace()
                        && !matches!(bytes[index], b'>')
                    {
                        index += 1;
                    }
                    value = html[value_start..index].to_string();
                }
            }
            if value.len() > MAX_ATTRIBUTE_VALUE_BYTES {
                value.truncate(MAX_ATTRIBUTE_VALUE_BYTES);
            }
            if attributes.len() < MAX_ATTRIBUTES {
                attributes.push((name, decode_entities(&value)));
            }
        }
        let mut text = String::new();
        if !self_closing && !is_void(&tag) {
            if matches!(tag.as_str(), "script" | "style") {
                let close = format!("</{tag}");
                match html[index..].find(&close) {
                    Some(end) => {
                        index += end;
                        continue;
                    }
                    None => break,
                }
            }
            let text_start = index;
            match html[index..].find('<') {
                Some(end) => index += end,
                None => index = bytes.len(),
            }
            text = collapse_text(&html[text_start..index]);
        }
        nodes.push(DomNode {
            tag,
            attributes,
            text,
        });
    }
    nodes
}

/// Void elements never carry text.
fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Collapse whitespace, decode entities, bound the length.
fn collapse_text(raw: &str) -> String {
    let decoded = decode_entities(raw);
    let mut out = String::with_capacity(decoded.len().min(MAX_TEXT_BYTES));
    let mut pending_space = false;
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
        if out.len() >= MAX_TEXT_BYTES {
            break;
        }
    }
    out
}

/// Decode the small entity set marketplaces use in structural markup.
fn decode_entities(raw: &str) -> String {
    raw.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&yen;", "¥")
        .replace("&#165;", "¥")
}

/// Find the first node whose class list contains `class`.
pub fn by_class<'a>(nodes: &'a [DomNode], class: &str) -> Option<&'a DomNode> {
    nodes.iter().find(|node| node.has_class(class))
}

/// Find the first node whose `data-*` attribute exists.
pub fn by_data<'a>(nodes: &'a [DomNode], name: &str) -> Option<&'a DomNode> {
    nodes.iter().find(|node| node.data(name).is_some())
}

/// Find the first node with this tag.
pub fn by_tag<'a>(nodes: &'a [DomNode], tag: &str) -> Option<&'a DomNode> {
    nodes.iter().find(|node| node.tag == tag)
}

/// Find the first non-empty text of a node with this class.
pub fn text_by_class<'a>(nodes: &'a [DomNode], class: &str) -> Option<&'a str> {
    nodes
        .iter()
        .find(|node| node.has_class(class) && !node.text.is_empty())
        .map(|node| node.text.as_str())
}

/// Extract the value that follows one of `labels` inside free text
/// (rendered-text fallback). Labels are matched literally; the value is the
/// rest of the line, trimmed of punctuation.
pub fn text_after_label<'a>(text: &'a str, labels: &[&str]) -> Option<&'a str> {
    for line in text.lines() {
        let trimmed = line.trim();
        for label in labels {
            if let Some(rest) = trimmed.strip_prefix(label) {
                let rest = rest.trim_start_matches([':', '：', ' ', '\t']);
                if !rest.is_empty() {
                    return Some(rest.trim());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanning_is_bounded_and_structural() {
        let html = r#"<!doctype html><html><head><style>.a{color:red}</style></head>
            <body><div id="main" class="offer card" data-offer-id="678"><h1>USB 数据线</h1>
            <span class="price">&yen;<b>36.00</b></span></div><!-- ignore -->
            <script>window.__DATA__ = {"price":"36.00"};</script></body></html>"#;
        let nodes = scan(html);
        let main = by_class(&nodes, "offer").expect("main");
        assert_eq!(main.id(), Some("main"));
        assert_eq!(main.data("offer-id"), Some("678"));
        assert_eq!(by_tag(&nodes, "h1").expect("h1").text, "USB 数据线");
        assert_eq!(text_by_class(&nodes, "price"), Some("¥"));
        // Script bodies are skipped: embedded state is read on purpose.
        assert!(!nodes
            .iter()
            .any(|node| node.tag == "script" && node.text.contains("__DATA__")));
    }

    #[test]
    fn hostile_markup_cannot_exceed_bounds() {
        let mut html = String::new();
        for index in 0..(MAX_DOM_NODES * 2) {
            html.push_str(&format!("<div class=\"n{index}\">x</div>"));
        }
        let nodes = scan(&html);
        assert!(nodes.len() <= MAX_DOM_NODES);
        let deep = format!("<div class=\"price\">{}</div>", "x".repeat(10_000));
        let nodes = scan(&deep);
        assert!(nodes[0].text.len() <= MAX_TEXT_BYTES);
        let unterminated = "<div class=\"x\">no close <span";
        assert!(!scan(unterminated).is_empty());
    }

    #[test]
    fn labels_are_read_from_free_text() {
        let text = "价格：¥36.00\n起订量: 10 件\n库存 500";
        assert_eq!(text_after_label(text, &["价格", "价格："]), Some("¥36.00"));
        assert_eq!(text_after_label(text, &["起订量"]), Some("10 件"));
        assert_eq!(text_after_label(text, &["nope"]), None);
    }
}
