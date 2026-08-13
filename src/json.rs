//! Tiny JSON parser — enough for komorebi state and LHM data.json.
//! No serde: one dependency-free file.

#[derive(Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn parse(text: &str) -> Option<Value> {
        let bytes = text.as_bytes();
        let mut pos = 0;
        let v = parse_value(bytes, &mut pos)?;
        Some(v)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn idx(&self, i: usize) -> Option<&Value> {
        match self {
            Value::Arr(items) => items.get(i),
            _ => None,
        }
    }

    pub fn arr(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

fn skip_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && matches!(b[*pos], b' ' | b'\t' | b'\r' | b'\n') {
        *pos += 1;
    }
}

fn parse_value(b: &[u8], pos: &mut usize) -> Option<Value> {
    skip_ws(b, pos);
    match *b.get(*pos)? {
        b'{' => parse_obj(b, pos),
        b'[' => parse_arr(b, pos),
        b'"' => parse_str(b, pos).map(Value::Str),
        b't' => eat(b, pos, "true").then_some(Value::Bool(true)),
        b'f' => eat(b, pos, "false").then_some(Value::Bool(false)),
        b'n' => eat(b, pos, "null").then_some(Value::Null),
        _ => parse_num(b, pos),
    }
}

fn eat(b: &[u8], pos: &mut usize, word: &str) -> bool {
    if b[*pos..].starts_with(word.as_bytes()) {
        *pos += word.len();
        true
    } else {
        false
    }
}

fn parse_obj(b: &[u8], pos: &mut usize) -> Option<Value> {
    *pos += 1; // {
    let mut pairs = Vec::new();
    loop {
        skip_ws(b, pos);
        match *b.get(*pos)? {
            b'}' => {
                *pos += 1;
                return Some(Value::Obj(pairs));
            }
            b',' => {
                *pos += 1;
            }
            b'"' => {
                let key = parse_str(b, pos)?;
                skip_ws(b, pos);
                if *b.get(*pos)? != b':' {
                    return None;
                }
                *pos += 1;
                let val = parse_value(b, pos)?;
                pairs.push((key, val));
            }
            _ => return None,
        }
    }
}

fn parse_arr(b: &[u8], pos: &mut usize) -> Option<Value> {
    *pos += 1; // [
    let mut items = Vec::new();
    loop {
        skip_ws(b, pos);
        match *b.get(*pos)? {
            b']' => {
                *pos += 1;
                return Some(Value::Arr(items));
            }
            b',' => {
                *pos += 1;
            }
            _ => items.push(parse_value(b, pos)?),
        }
    }
}

fn parse_str(b: &[u8], pos: &mut usize) -> Option<String> {
    *pos += 1; // opening quote
    let mut out = String::new();
    loop {
        match *b.get(*pos)? {
            b'"' => {
                *pos += 1;
                return Some(out);
            }
            b'\\' => {
                *pos += 1;
                match *b.get(*pos)? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        let hex = std::str::from_utf8(b.get(*pos + 1..*pos + 5)?).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        *pos += 4;
                        // Surrogate pairs: peek for a following \uXXXX low half.
                        if (0xD800..0xDC00).contains(&code)
                            && b.get(*pos + 1..*pos + 3).is_some_and(|s| s == b"\\u")
                        {
                            if let Some(hex2) =
                                b.get(*pos + 3..*pos + 7).and_then(|s| std::str::from_utf8(s).ok())
                            {
                                if let Ok(low) = u32::from_str_radix(hex2, 16) {
                                    if (0xDC00..0xE000).contains(&low) {
                                        let c = 0x10000
                                            + ((code - 0xD800) << 10)
                                            + (low - 0xDC00);
                                        if let Some(ch) = char::from_u32(c) {
                                            out.push(ch);
                                        }
                                        *pos += 6;
                                        *pos += 1;
                                        continue;
                                    }
                                }
                            }
                        }
                        if let Some(ch) = char::from_u32(code) {
                            out.push(ch);
                        }
                    }
                    _ => {}
                }
                *pos += 1;
            }
            _ => {
                // Copy the full UTF-8 sequence.
                let start = *pos;
                *pos += 1;
                while *pos < b.len() && b[*pos] & 0xC0 == 0x80 {
                    *pos += 1;
                }
                out.push_str(std::str::from_utf8(&b[start..*pos]).unwrap_or(""));
            }
        }
    }
}

fn parse_num(b: &[u8], pos: &mut usize) -> Option<Value> {
    let start = *pos;
    while *pos < b.len()
        && matches!(b[*pos], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
    {
        *pos += 1;
    }
    std::str::from_utf8(&b[start..*pos])
        .ok()?
        .parse()
        .ok()
        .map(Value::Num)
}

#[cfg(test)]
mod tests {
    use super::Value;

    #[test]
    fn round_trip() {
        let v = Value::parse(
            r#"{"monitors":{"elements":[{"workspaces":{"elements":[{"name":"one","containers":{"elements":[1]}}],"focused":0}}]},"n":-3.5,"ok":true,"nul":null}"#,
        )
        .unwrap();
        let ws = v
            .get("monitors").unwrap()
            .get("elements").unwrap()
            .idx(0).unwrap()
            .get("workspaces").unwrap();
        assert_eq!(
            ws.get("elements").unwrap().idx(0).unwrap().get("name").unwrap().as_str(),
            Some("one")
        );
        assert_eq!(ws.get("focused").unwrap().as_f64(), Some(0.0));
        assert_eq!(v.get("n").unwrap().as_f64(), Some(-3.5));
        assert!(v.get("nul").unwrap().is_null());
    }

    #[test]
    fn escapes() {
        let v = Value::parse(r#"{"s":"a\"b\\cé"}"#).unwrap();
        assert_eq!(v.get("s").unwrap().as_str(), Some("a\"b\\cé"));
    }
}
