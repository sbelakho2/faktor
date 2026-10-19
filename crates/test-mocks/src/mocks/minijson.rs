use std::collections::BTreeMap;

#[derive(Clone, Debug)]
enum J {
    Null,
    Bool(bool),
    Number(String),
    Str(String),
    Array(Vec<J>),
    Object(BTreeMap<String, J>),
}

impl J {
    fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Object(map) => map.get(key),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(value) => Some(value),
            _ => None,
        }
    }
    fn raw(&self) -> String {
        match self {
            J::Null => "null".into(),
            J::Bool(value) => value.to_string(),
            J::Number(value) => value.clone(),
            J::Str(value) => quote(value),
            J::Array(items) => format!(
                "[{}]",
                items.iter().map(J::raw).collect::<Vec<_>>().join(",")
            ),
            J::Object(map) => format!(
                "{{{}}}",
                map.iter()
                    .map(|(key, value)| format!("{}:{}", quote(key), value.raw()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }
}

fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct P<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.at < self.bytes.len() && self.bytes[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }
    fn value(&mut self) -> Option<J> {
        self.ws();
        match self.peek()? {
            b'n' => {
                self.literal(b"null")?;
                Some(J::Null)
            }
            b't' => {
                self.literal(b"true")?;
                Some(J::Bool(true))
            }
            b'f' => {
                self.literal(b"false")?;
                Some(J::Bool(false))
            }
            b'"' => self.string().map(J::Str),
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.peek()? == b']' {
                        self.at += 1;
                        return Some(J::Array(items));
                    }
                    items.push(self.value()?);
                    self.ws();
                    if self.peek()? == b',' {
                        self.at += 1;
                    }
                }
            }
            b'{' => {
                self.at += 1;
                let mut map = BTreeMap::new();
                loop {
                    self.ws();
                    if self.peek()? == b'}' {
                        self.at += 1;
                        return Some(J::Object(map));
                    }
                    let key = self.string()?;
                    self.ws();
                    if self.peek()? != b':' {
                        return None;
                    }
                    self.at += 1;
                    let value = self.value()?;
                    map.insert(key, value);
                    self.ws();
                    if self.peek()? == b',' {
                        self.at += 1;
                    }
                }
            }
            _ => {
                let start = self.at;
                while let Some(byte) = self.peek() {
                    if byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E') {
                        self.at += 1;
                    } else {
                        break;
                    }
                }
                if start == self.at {
                    return None;
                }
                Some(J::Number(
                    String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned(),
                ))
            }
        }
    }
    fn literal(&mut self, text: &[u8]) -> Option<()> {
        if self.bytes.get(self.at..self.at + text.len()) == Some(text) {
            self.at += text.len();
            Some(())
        } else {
            None
        }
    }
    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.at += 1;
        let mut out = String::new();
        while let Some(byte) = self.peek() {
            self.at += 1;
            match byte {
                b'"' => return Some(out),
                b'\\' => {
                    let escaped = self.peek()?;
                    self.at += 1;
                    match escaped {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hex = self.bytes.get(self.at..self.at + 4)?;
                            self.at += 4;
                            let code = u32::from_str_radix(
                                std::str::from_utf8(hex).ok()?,
                                16,
                            )
                            .ok()?;
                            out.push(char::from_u32(code)?);
                        }
                        _ => return None,
                    }
                }
                other => out.push(other as char),
            }
        }
        None
    }
}

fn parse(bytes: &[u8]) -> Option<J> {
    let mut parser = P { bytes, at: 0 };
    let value = parser.value()?;
    parser.ws();
    if parser.at == bytes.len() {
        Some(value)
    } else {
        None
    }
}
