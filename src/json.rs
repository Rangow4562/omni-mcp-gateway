//! High-performance RFC-8259 JSON AST, parser, serializer, and `json!` macro.
//! Preserves object key insertion order for deterministic schema and payload rendering.

use std::fmt;

#[derive(Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl fmt::Debug for JsonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_string_compact())
    }
}

impl fmt::Display for JsonValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_string_compact())
    }
}

impl JsonValue {
    pub fn object() -> Self {
        JsonValue::Object(Vec::new())
    }

    pub fn array() -> Self {
        JsonValue::Array(Vec::new())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, JsonValue::Null)
    }

    pub fn is_object(&self) -> bool {
        matches!(self, JsonValue::Object(_))
    }

    pub fn is_array(&self) -> bool {
        matches!(self, JsonValue::Array(_))
    }

    pub fn is_string(&self) -> bool {
        matches!(self, JsonValue::Str(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Int(n) => Some(*n),
            JsonValue::Float(f) => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_i64().map(|v| v.max(0) as u64)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Float(f) => Some(*f),
            JsonValue::Int(n) => Some(*n as f64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<JsonValue>> {
        match self {
            JsonValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<JsonValue>> {
        match self {
            JsonValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Vec<(String, JsonValue)>> {
        match self {
            JsonValue::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut Vec<(String, JsonValue)>> {
        match self {
            JsonValue::Object(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut JsonValue> {
        match self {
            JsonValue::Object(entries) => {
                entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn insert(&mut self, key: impl Into<String>, val: impl Into<JsonValue>) {
        let key = key.into();
        let val = val.into();
        if let JsonValue::Object(entries) = self {
            if let Some((_, existing)) = entries.iter_mut().find(|(k, _)| *k == key) {
                *existing = val;
            } else {
                entries.push((key, val));
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<JsonValue> {
        if let JsonValue::Object(entries) = self {
            if let Some(pos) = entries.iter().position(|(k, _)| k == key) {
                return Some(entries.remove(pos).1);
            }
        }
        None
    }

    pub fn push(&mut self, val: impl Into<JsonValue>) {
        if let JsonValue::Array(arr) = self {
            arr.push(val.into());
        }
    }

    pub fn to_string_compact(&self) -> String {
        let mut buf = String::with_capacity(128);
        self.write_compact(&mut buf);
        buf
    }

    pub fn to_string_pretty(&self) -> String {
        let mut buf = String::with_capacity(256);
        self.write_pretty(&mut buf, 0);
        buf
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            JsonValue::Int(n) => out.push_str(&n.to_string()),
            JsonValue::Float(f) => {
                if f.is_finite() {
                    let s = f.to_string();
                    out.push_str(&s);
                } else {
                    out.push_str("null");
                }
            }
            JsonValue::Str(s) => escape_json_string(s, out),
            JsonValue::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_compact(out);
                }
                out.push(']');
            }
            JsonValue::Object(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    escape_json_string(k, out);
                    out.push(':');
                    v.write_compact(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty(&self, out: &mut String, indent: usize) {
        let pad = "  ".repeat(indent);
        let next_pad = "  ".repeat(indent + 1);
        match self {
            JsonValue::Null
            | JsonValue::Bool(_)
            | JsonValue::Int(_)
            | JsonValue::Float(_)
            | JsonValue::Str(_) => {
                self.write_compact(out);
            }
            JsonValue::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&next_pad);
                    item.write_pretty(out, indent + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad);
                out.push(']');
            }
            JsonValue::Object(entries) => {
                if entries.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (i, (k, v)) in entries.iter().enumerate() {
                    out.push_str(&next_pad);
                    escape_json_string(k, out);
                    out.push_str(": ");
                    v.write_pretty(out, indent + 1);
                    if i + 1 < entries.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad);
                out.push('}');
            }
        }
    }

    pub fn parse(input: &str) -> Result<JsonValue, String> {
        let mut parser = JsonParser::new(input);
        let val = parser.parse_value()?;
        parser.skip_whitespace();
        if parser.pos < parser.bytes.len() {
            return Err(format!(
                "Unexpected trailing characters at byte {}",
                parser.pos
            ));
        }
        Ok(val)
    }
}

fn escape_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0C' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct JsonParser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<JsonValue, String> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'n') => self.parse_null(),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'"') => self.parse_string().map(JsonValue::Str),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(c) => Err(format!(
                "Unexpected character '{}' at pos {}",
                c as char, self.pos
            )),
            None => Err("Unexpected end of JSON input".to_string()),
        }
    }

    fn parse_null(&mut self) -> Result<JsonValue, String> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(format!("Invalid literal at pos {}", self.pos))
        }
    }

    fn parse_bool(&mut self) -> Result<JsonValue, String> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(JsonValue::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(JsonValue::Bool(false))
        } else {
            Err(format!("Invalid boolean at pos {}", self.pos))
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err(format!("Expected '\"' at pos {}", self.pos));
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            } else if b == b'\\' {
                self.pos += 1;
                if self.pos >= self.bytes.len() {
                    return Err("Unterminated escape sequence".to_string());
                }
                let esc = self.bytes[self.pos];
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\x08'),
                    b'f' => out.push('\x0C'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        if self.pos + 4 > self.bytes.len() {
                            return Err("Invalid unicode escape".to_string());
                        }
                        let hex = &self.input[self.pos..self.pos + 4];
                        self.pos += 4;
                        let code = u32::from_str_radix(hex, 16)
                            .map_err(|_| format!("Invalid hex in unicode escape: {}", hex))?;
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                    }
                    other => out.push(other as char),
                }
            } else {
                // Decode UTF-8 char slice
                let rest = &self.input[self.pos..];
                let ch = rest.chars().next().ok_or("Invalid UTF-8")?;
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
        Err("Unterminated JSON string".to_string())
    }

    fn parse_number(&mut self) -> Result<JsonValue, String> {
        let start = self.pos;
        let mut is_float = false;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            while let Some(b'0'..=b'9') = self.peek() {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while let Some(b'0'..=b'9') = self.peek() {
                self.pos += 1;
            }
        }
        let s = &self.input[start..self.pos];
        if is_float {
            let f: f64 = s.parse().map_err(|_| format!("Invalid float: {}", s))?;
            Ok(JsonValue::Float(f))
        } else if let Ok(n) = s.parse::<i64>() {
            Ok(JsonValue::Int(n))
        } else {
            let f: f64 = s.parse().map_err(|_| format!("Invalid number: {}", s))?;
            Ok(JsonValue::Float(f))
        }
    }

    fn parse_array(&mut self) -> Result<JsonValue, String> {
        self.pos += 1; // skip '['
        self.skip_whitespace();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            let val = self.parse_value()?;
            items.push(val);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(items));
                }
                _ => return Err(format!("Expected ',' or ']' at pos {}", self.pos)),
            }
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, String> {
        self.pos += 1; // skip '{'
        self.skip_whitespace();
        let mut entries: Vec<(String, JsonValue)> = Vec::new();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(entries));
        }
        loop {
            self.skip_whitespace();
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(format!("Expected ':' at pos {}", self.pos));
            }
            self.pos += 1;
            let val = self.parse_value()?;
            if let Some((_, existing)) = entries.iter_mut().find(|(k, _)| *k == key) {
                *existing = val;
            } else {
                entries.push((key, val));
            }
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(entries));
                }
                _ => return Err(format!("Expected ',' or '}}' at pos {}", self.pos)),
            }
        }
    }
}

impl From<bool> for JsonValue {
    fn from(v: bool) -> Self {
        JsonValue::Bool(v)
    }
}

impl From<i32> for JsonValue {
    fn from(v: i32) -> Self {
        JsonValue::Int(v as i64)
    }
}

impl From<i64> for JsonValue {
    fn from(v: i64) -> Self {
        JsonValue::Int(v)
    }
}

impl From<usize> for JsonValue {
    fn from(v: usize) -> Self {
        JsonValue::Int(v as i64)
    }
}

impl From<u64> for JsonValue {
    fn from(v: u64) -> Self {
        JsonValue::Int(v as i64)
    }
}

impl From<f64> for JsonValue {
    fn from(v: f64) -> Self {
        JsonValue::Float(v)
    }
}

impl From<&str> for JsonValue {
    fn from(v: &str) -> Self {
        JsonValue::Str(v.to_string())
    }
}

impl From<String> for JsonValue {
    fn from(v: String) -> Self {
        JsonValue::Str(v)
    }
}

impl<T: Into<JsonValue>> From<Vec<T>> for JsonValue {
    fn from(v: Vec<T>) -> Self {
        JsonValue::Array(v.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<JsonValue>> From<Option<T>> for JsonValue {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(inner) => inner.into(),
            None => JsonValue::Null,
        }
    }
}

impl std::ops::Index<&str> for JsonValue {
    type Output = JsonValue;

    fn index(&self, key: &str) -> &Self::Output {
        static NULL: JsonValue = JsonValue::Null;
        self.get(key).unwrap_or(&NULL)
    }
}

impl std::ops::Index<usize> for JsonValue {
    type Output = JsonValue;

    fn index(&self, idx: usize) -> &Self::Output {
        static NULL: JsonValue = JsonValue::Null;
        match self {
            JsonValue::Array(arr) => arr.get(idx).unwrap_or(&NULL),
            _ => &NULL,
        }
    }
}

#[macro_export]
macro_rules! json_internal {
    // Array muncher
    (@array [ $($elems:expr,)* ]) => {
        vec![ $($elems,)* ]
    };
    (@array [ $($elems:expr),* ]) => {
        vec![ $($elems),* ]
    };
    (@array [ $($elems:expr,)* ] null $($rest:tt)*) => {
        $crate::json_internal!(@array [ $($elems,)* $crate::json!(null) ] $($rest)*)
    };
    (@array [ $($elems:expr,)* ] true $($rest:tt)*) => {
        $crate::json_internal!(@array [ $($elems,)* $crate::json!(true) ] $($rest)*)
    };
    (@array [ $($elems:expr,)* ] false $($rest:tt)*) => {
        $crate::json_internal!(@array [ $($elems,)* $crate::json!(false) ] $($rest)*)
    };
    (@array [ $($elems:expr,)* ] [ $($arr:tt)* ] $($rest:tt)*) => {
        $crate::json_internal!(@array [ $($elems,)* $crate::json!([ $($arr)* ]) ] $($rest)*)
    };
    (@array [ $($elems:expr,)* ] { $($map:tt)* } $($rest:tt)*) => {
        $crate::json_internal!(@array [ $($elems,)* $crate::json!({ $($map)* }) ] $($rest)*)
    };
    (@array [ $($elems:expr,)* ] $next:expr, $($rest:tt)*) => {
        $crate::json_internal!(@array [ $($elems,)* $crate::json!($next), ] $($rest)*)
    };
    (@array [ $($elems:expr,)* ] $last:expr) => {
        $crate::json_internal!(@array [ $($elems,)* $crate::json!($last) ])
    };
    (@array [ $($elems:expr),* ] , $($rest:tt)*) => {
        $crate::json_internal!(@array [ $($elems,)* ] $($rest)*)
    };

    // Object muncher
    (@object $obj:ident () () ()) => {};
    (@object $obj:ident [$($key:tt)+] ($val:expr) , $($rest:tt)*) => {
        $obj.insert(($($key)+), $val);
        $crate::json_internal!(@object $obj () ($($rest)*) ($($rest)*));
    };
    (@object $obj:ident [$($key:tt)+] ($val:expr)) => {
        $obj.insert(($($key)+), $val);
    };
    (@object $obj:ident ($($key:tt)+) (: null $($rest:tt)*) $copy:tt) => {
        $crate::json_internal!(@object $obj [$($key)+] ($crate::json!(null)) $($rest)*);
    };
    (@object $obj:ident ($($key:tt)+) (: true $($rest:tt)*) $copy:tt) => {
        $crate::json_internal!(@object $obj [$($key)+] ($crate::json!(true)) $($rest)*);
    };
    (@object $obj:ident ($($key:tt)+) (: false $($rest:tt)*) $copy:tt) => {
        $crate::json_internal!(@object $obj [$($key)+] ($crate::json!(false)) $($rest)*);
    };
    (@object $obj:ident ($($key:tt)+) (: [ $($arr:tt)* ] $($rest:tt)*) $copy:tt) => {
        $crate::json_internal!(@object $obj [$($key)+] ($crate::json!([ $($arr)* ])) $($rest)*);
    };
    (@object $obj:ident ($($key:tt)+) (: { $($map:tt)* } $($rest:tt)*) $copy:tt) => {
        $crate::json_internal!(@object $obj [$($key)+] ($crate::json!({ $($map)* })) $($rest)*);
    };
    (@object $obj:ident ($($key:tt)+) (: $val:expr , $($rest:tt)*) $copy:tt) => {
        $crate::json_internal!(@object $obj [$($key)+] ($crate::json!($val)) , $($rest)*);
    };
    (@object $obj:ident ($($key:tt)+) (: $val:expr) $copy:tt) => {
        $crate::json_internal!(@object $obj [$($key)+] ($crate::json!($val)));
    };
    (@object $obj:ident ($($key:tt)*) ($tt:tt $($rest:tt)*) $copy:tt) => {
        $crate::json_internal!(@object $obj ($($key)* $tt) ($($rest)*) ($($rest)*));
    };
}

#[macro_export]
macro_rules! json {
    (null) => {
        $crate::json::JsonValue::Null
    };
    (true) => {
        $crate::json::JsonValue::Bool(true)
    };
    (false) => {
        $crate::json::JsonValue::Bool(false)
    };
    ([]) => {
        $crate::json::JsonValue::Array(Vec::new())
    };
    ([ $($tt:tt)+ ]) => {
        $crate::json::JsonValue::Array($crate::json_internal!(@array [] $($tt)+))
    };
    ({}) => {
        $crate::json::JsonValue::Object(Vec::new())
    };
    ({ $($tt:tt)+ }) => {{
        let mut object = $crate::json::JsonValue::object();
        $crate::json_internal!(@object object () ($($tt)+) ($($tt)+));
        object
    }};
    ($other:expr) => {
        $crate::json::JsonValue::from($other)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_parser_round_trip_escapes_and_ordering() {
        let val = json!({
            "alpha": "line1\nline2\t\"quoted\"",
            "beta": -42,
            "gamma": 3.125,
            "nested": [true, false, null, {"k": "v"}]
        });
        let compact = val.to_string_compact();
        let reparsed = JsonValue::parse(&compact).expect("Must parse valid compact JSON");
        assert_eq!(val, reparsed);
        assert_eq!(reparsed["alpha"].as_str(), Some("line1\nline2\t\"quoted\""));
        assert_eq!(reparsed["beta"].as_i64(), Some(-42));
        assert_eq!(reparsed["nested"][3]["k"].as_str(), Some("v"));
    }

    #[test]
    fn test_json_parser_rejects_malformed_and_trailing_bytes() {
        assert!(JsonValue::parse(r#"{"a": 1} trailing"#).is_err());
        assert!(JsonValue::parse(r#"{"unclosed": [1, 2}"#).is_err());
        assert!(JsonValue::parse(r#""unterminated"#).is_err());
    }
}
