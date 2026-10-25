//! A small, strict JSON parser, used only for LibreHardwareMonitor's sensor tree.
//!
//! It rejects anything outside RFC 8259 rather than guessing, limits nesting depth so hostile
//! input can't overflow the stack, and never panics on any input (see the robustness tests).

/// Nesting deeper than this is rejected.
const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    /// Members in document order; lookups take the first match.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Parses a complete document; `None` if it isn't valid JSON.
    pub fn parse(text: &str) -> Option<Json> {
        let mut parser = Parser { bytes: text.as_bytes(), pos: 0 };
        let value = parser.value(0)?;
        parser.skip_whitespace();
        (parser.pos == parser.bytes.len()).then_some(value)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    /// The elements of an array; empty for anything else.
    pub fn items(&self) -> &[Json] {
        match self {
            Json::Array(items) => items,
            _ => &[],
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, literal: &[u8]) -> Option<()> {
        let end = self.pos.checked_add(literal.len())?;
        (self.bytes.get(self.pos..end)? == literal).then(|| self.pos = end)
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_whitespace();
        match self.peek()? {
            b'n' => self.eat(b"null").map(|()| Json::Null),
            b't' => self.eat(b"true").map(|()| Json::Bool(true)),
            b'f' => self.eat(b"false").map(|()| Json::Bool(false)),
            b'"' => self.string().map(Json::String),
            b'[' => self.array(depth),
            b'{' => self.object(depth),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn array(&mut self, depth: usize) -> Option<Json> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek()? == b']' {
            self.pos += 1;
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            match self.peek()? {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    return Some(Json::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn object(&mut self, depth: usize) -> Option<Json> {
        self.pos += 1; // '{'
        let mut members = Vec::new();
        self.skip_whitespace();
        if self.peek()? == b'}' {
            self.pos += 1;
            return Some(Json::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek()? != b'"' {
                return None;
            }
            let key = self.string()?;
            self.skip_whitespace();
            self.eat(b":")?;
            members.push((key, self.value(depth + 1)?));
            self.skip_whitespace();
            match self.peek()? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Some(Json::Object(members));
                }
                _ => return None,
            }
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.pos;
        let digits = |p: &mut Self| {
            let begin = p.pos;
            while matches!(p.peek(), Some(b'0'..=b'9')) {
                p.pos += 1;
            }
            p.pos > begin
        };
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // No leading zeros: "0" alone, or a nonzero digit followed by more digits.
        if self.peek() == Some(b'0') {
            self.pos += 1;
        } else if !digits(self) {
            return None;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !digits(self) {
                return None;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return None;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        text.parse().ok().filter(|n: &f64| n.is_finite()).map(Json::Number)
    }

    fn hex4(&mut self) -> Option<u32> {
        let end = self.pos.checked_add(4)?;
        let text = std::str::from_utf8(self.bytes.get(self.pos..end)?).ok()?;
        if !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        self.pos = end;
        u32::from_str_radix(text, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            // Copy the run of plain characters up to the next quote, backslash or control byte.
            let start = self.pos;
            while matches!(self.peek(), Some(b) if b != b'"' && b != b'\\' && b >= 0x20) {
                self.pos += 1;
            }
            out.push_str(std::str::from_utf8(&self.bytes[start..self.pos]).ok()?);
            match self.peek()? {
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let escape = self.peek()?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let code = match first {
                                0xD800..=0xDBFF => {
                                    self.eat(b"\\u")?;
                                    let second = self.hex4()?;
                                    if !(0xDC00..=0xDFFF).contains(&second) {
                                        return None;
                                    }
                                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                                }
                                0xDC00..=0xDFFF => return None,
                                code => code,
                            };
                            out.push(char::from_u32(code)?);
                        }
                        _ => return None,
                    }
                }
                // A raw control character inside a string.
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_documents() {
        let doc = Json::parse(r#" {"a": [1, -2.5e2, true, null], "b": {"c": "d\u00e9\n"}} "#).unwrap();
        assert_eq!(
            doc.get("a").unwrap().items(),
            [Json::Number(1.0), Json::Number(-250.0), Json::Bool(true), Json::Null]
        );
        assert_eq!(doc.get("b").and_then(|b| b.get("c")).and_then(Json::as_str), Some("dé\n"));
    }

    #[test]
    fn decodes_surrogate_pairs() {
        assert_eq!(Json::parse(r#""\ud83d\ude00""#), Some(Json::String("😀".into())));
        assert_eq!(Json::parse(r#""\ud83d""#), None, "lone high surrogate");
        assert_eq!(Json::parse(r#""\ude00""#), None, "lone low surrogate");
    }

    #[test]
    fn rejects_invalid_json() {
        for bad in
            ["", "{", "[1,]", "{\"a\":}", "01", "1.", "-", "nul", "\"\u{1}\"", "{} x", "[1 2]", "'a'", "NaN", "1e999"]
        {
            assert_eq!(Json::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn deep_nesting_is_rejected_not_a_stack_overflow() {
        let deep = "[".repeat(100_000) + &"]".repeat(100_000);
        assert_eq!(Json::parse(&deep), None);
        let ok = "[".repeat(100) + &"]".repeat(100);
        assert!(Json::parse(&ok).is_some());
    }

    #[test]
    fn never_panics_on_arbitrary_input() {
        let seed = br#"{"Text":"CPU","Children":[{"Value":"45.5 \u00b0C","n":-1.5e3,"t":true}],"x":null}"#;
        crate::fuzz::mutate(seed, 20_000, |bytes| {
            if let Ok(text) = std::str::from_utf8(bytes) {
                let _ = Json::parse(text);
            }
        });
    }
}
