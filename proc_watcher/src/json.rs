//! A minimal JSON parser, enough to read one log line. Keeps object keys in
//! order and numbers exactly as written.

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(fields) => Some(fields),
            _ => None,
        }
    }

    /// Strings and numbers as written, anything else as compact JSON.
    pub fn text(&self) -> String {
        match self {
            Value::String(s) | Value::Number(s) => s.clone(),
            other => {
                let mut out = String::new();
                other.write_compact(&mut out);
                out
            }
        }
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(n),
            Value::String(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        c if c.is_control() => {}
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_compact(out);
                }
                out.push(']');
            }
            Value::Object(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    Value::String(key.clone()).write_compact(out);
                    out.push(':');
                    value.write_compact(out);
                }
                out.push('}');
            }
        }
    }
}

/// Deeper nesting is rejected rather than risking the stack.
const MAX_DEPTH: usize = 32;

/// Parses a complete JSON document; `None` if it is not valid JSON.
pub fn parse(text: &str) -> Option<Value> {
    let mut parser = Parser { bytes: text.as_bytes(), pos: 0 };
    let value = parser.value(0)?;
    parser.skip_whitespace();
    (parser.pos == parser.bytes.len()).then_some(value)
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

    fn eat(&mut self, byte: u8) -> Option<()> {
        (self.peek() == Some(byte)).then(|| self.pos += 1)
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_whitespace();
        match self.peek()? {
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b'"' => self.string().map(Value::String),
            b't' => self.literal("true", Value::Bool(true)),
            b'f' => self.literal("false", Value::Bool(false)),
            b'n' => self.literal("null", Value::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Option<Value> {
        self.bytes[self.pos..].starts_with(word.as_bytes()).then(|| {
            self.pos += word.len();
            value
        })
    }

    fn object(&mut self, depth: usize) -> Option<Value> {
        self.eat(b'{')?;
        let mut fields = Vec::new();
        self.skip_whitespace();
        if self.eat(b'}').is_some() {
            return Some(Value::Object(fields));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            self.eat(b':')?;
            fields.push((key, self.value(depth + 1)?));
            self.skip_whitespace();
            if self.eat(b',').is_none() {
                self.eat(b'}')?;
                return Some(Value::Object(fields));
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Value> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.eat(b']').is_some() {
            return Some(Value::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            if self.eat(b',').is_none() {
                self.eat(b']')?;
                return Some(Value::Array(items));
            }
        }
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
            self.pos += 1;
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        raw.parse::<f64>().ok()?;
        Some(Value::Number(raw.to_owned()))
    }

    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while self.peek().is_some_and(|b| b != b'"' && b != b'\\' && b >= 0x20) {
                self.pos += 1;
            }
            // Runs stop at ASCII bytes, so the slice stays on char boundaries.
            out.push_str(std::str::from_utf8(&self.bytes[start..self.pos]).ok()?);
            match self.peek()? {
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let escaped = self.peek()?;
                    self.pos += 1;
                    out.push(match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return None,
                    });
                }
                _ => return None,
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?).ok()?;
        self.pos += 4;
        u32::from_str_radix(digits, 16).ok()
    }

    /// The `XXXX` of `\uXXXX`, joining surrogate pairs; lone surrogates become U+FFFD.
    fn unicode_escape(&mut self) -> Option<char> {
        let first = self.hex4()?;
        if (0xD800..0xDC00).contains(&first) && self.bytes[self.pos..].starts_with(b"\\u") {
            self.pos += 2;
            let second = self.hex4()?;
            if (0xDC00..0xE000).contains(&second) {
                let code = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                return Some(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
        }
        Some(char::from_u32(first).unwrap_or('\u{fffd}'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_log_line_in_order() {
        let value = parse(r#"{"level":"info","ms":16.1,"user":null,"ok":true,"tags":["a",1]}"#).unwrap();
        let keys: Vec<&str> = value.as_object().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["level", "ms", "user", "ok", "tags"]);
        assert_eq!(value.get("level"), Some(&Value::String("info".into())));
        assert_eq!(value.get("ms").unwrap().text(), "16.1");
        assert_eq!(value.get("tags").unwrap().text(), r#"["a",1]"#);
    }

    #[test]
    fn decodes_escapes() {
        let value = parse(r#""a\"b\\c\ndé😀""#).unwrap();
        assert_eq!(value, Value::String("a\"b\\c\nd\u{e9}\u{1f600}".into()));
    }

    #[test]
    fn rejects_invalid_input() {
        for text in ["", "{", r#"{"a":}"#, r#"{"a":1,}"#, "[1 2]", r#"{"a":1} x"#, "nul"] {
            assert_eq!(parse(text), None, "{text}");
        }
    }
}
