//! Read the data in a vendored TypeScript source: object and array
//! literals, strings, numbers and booleans, as JSON. Tests use it to
//! derive Branchyard's catalogs from pinned upstream files (emdash's agent
//! plugins and MCP catalog, Orca's agent table) and to fail when upstream
//! changes. It evaluates nothing: an identifier becomes `{"$ident": name}`,
//! a call `{"$call": name, "$args": [...]}`, a spread `"$spread<N>": value`,
//! and a template literal its raw text with `${...}` left in place.

use serde_json::{json, Map, Value};

/// A parse failure: the byte offset and what was expected there.
#[derive(Debug)]
pub struct Error {
    pub offset: usize,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "at byte {}: {}", self.offset, self.message)
    }
}

/// The value starting at the first `{`, `[`, quote or identifier at or
/// after `offset` in `source`, and where it ends.
pub fn value_at(source: &str, offset: usize) -> Result<(Value, usize), Error> {
    let mut parser = Parser {
        text: source.as_bytes(),
        src: source,
        at: offset,
    };
    let value = parser.value()?;
    Ok((value, parser.at))
}

/// The arguments of the first call `name(` in `source`.
pub fn call_args(source: &str, name: &str) -> Result<Vec<Value>, Error> {
    let needle = format!("{name}(");
    let start = source.find(&needle).ok_or_else(|| Error {
        offset: 0,
        message: format!("no call to {name}"),
    })?;
    let mut parser = Parser {
        text: source.as_bytes(),
        src: source,
        at: start + needle.len(),
    };
    parser.args()
}

struct Parser<'a> {
    text: &'a [u8],
    src: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn fail<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        Err(Error {
            offset: self.at,
            message: message.into(),
        })
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    /// Skip whitespace and comments.
    fn skip(&mut self) {
        loop {
            while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                self.at += 1;
            }
            if self.src[self.at..].starts_with("//") {
                while self.peek().is_some_and(|c| c != b'\n') {
                    self.at += 1;
                }
            } else if self.src[self.at..].starts_with("/*") {
                match self.src[self.at + 2..].find("*/") {
                    Some(end) => self.at += end + 4,
                    None => self.at = self.text.len(),
                }
            } else {
                return;
            }
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        self.skip();
        if self.peek() == Some(byte) {
            self.at += 1;
            return true;
        }
        false
    }

    fn expect(&mut self, byte: u8) -> Result<(), Error> {
        match self.eat(byte) {
            true => Ok(()),
            false => self.fail(format!("expected {:?}", byte as char)),
        }
    }

    fn value(&mut self) -> Result<Value, Error> {
        self.skip();
        let value = match self.peek() {
            Some(b'{') => self.object()?,
            Some(b'[') => self.array()?,
            Some(b'!') => {
                self.at += 1;
                json!({ "$not": self.value()? })
            }
            Some(b'\'' | b'"' | b'`') => Value::String(self.string()?),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number()?,
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => self.word()?,
            _ => return self.fail("expected a value"),
        };
        // `'a' + 'b'`: string concatenation, joined.
        self.skip();
        if self.peek() == Some(b'+') {
            self.at += 1;
            let rest = self.value()?;
            if let (Value::String(a), Value::String(b)) = (&value, &rest) {
                return Ok(Value::String(format!("{a}{b}")));
            }
            return Ok(json!({"$concat": [value, rest]}));
        }
        // `a && b` and the like: kept as an expression, not evaluated (and
        // grouped to the right, which is wrong for evaluation but harmless
        // for data nobody evaluates).
        for op in ["===", "!==", "==", "!=", "&&", "||", "??"] {
            if self.src[self.at..].starts_with(op) {
                self.at += op.len();
                let rest = self.value()?;
                return Ok(json!({ "$op": [op, value, rest] }));
            }
        }
        // `c ? a : b`: kept as an expression.
        if self.peek() == Some(b'?') && self.text.get(self.at + 1) != Some(&b'.') {
            self.at += 1;
            let then = self.value()?;
            self.expect(b':')?;
            let otherwise = self.value()?;
            return Ok(json!({"$ternary": [value, then, otherwise]}));
        }
        // `x as const`, `x satisfies T`: the value.
        for keyword in ["as ", "satisfies "] {
            if self.src[self.at..].starts_with(keyword) {
                self.at += keyword.len();
                self.word()?;
            }
        }
        Ok(value)
    }

    fn object(&mut self) -> Result<Value, Error> {
        self.expect(b'{')?;
        let mut map = Map::new();
        let mut spreads = 0;
        loop {
            if self.eat(b'}') {
                return Ok(Value::Object(map));
            }
            self.skip();
            if self.src[self.at..].starts_with("...") {
                self.at += 3;
                let value = self.value()?;
                map.insert(format!("$spread{spreads}"), value);
                spreads += 1;
            } else {
                let key = match self.peek() {
                    Some(b'\'' | b'"' | b'`') => self.string()?,
                    Some(b'[') => return self.fail("computed keys are not data"),
                    _ => self.name()?,
                };
                if self.eat(b':') {
                    let value = self.value()?;
                    map.insert(key, value);
                } else if self.peek() == Some(b'(') {
                    return self.fail(format!("method {key} is not data"));
                } else {
                    // Shorthand `{ icon }`.
                    map.insert(key.clone(), json!({ "$ident": key }));
                }
            }
            if !self.eat(b',') {
                self.expect(b'}')?;
                return Ok(Value::Object(map));
            }
        }
    }

    fn array(&mut self) -> Result<Value, Error> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        loop {
            if self.eat(b']') {
                return Ok(Value::Array(items));
            }
            self.skip();
            if self.src[self.at..].starts_with("...") {
                self.at += 3;
                let value = self.value()?;
                items.push(json!({ "$spread": value }));
            } else {
                items.push(self.value()?);
            }
            if !self.eat(b',') {
                self.expect(b']')?;
                return Ok(Value::Array(items));
            }
        }
    }

    fn args(&mut self) -> Result<Vec<Value>, Error> {
        let mut args = Vec::new();
        loop {
            if self.eat(b')') {
                return Ok(args);
            }
            args.push(self.value()?);
            if !self.eat(b',') {
                self.expect(b')')?;
                return Ok(args);
            }
        }
    }

    fn string(&mut self) -> Result<String, Error> {
        let quote = self.peek().unwrap_or_default();
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.src[self.at..].chars().next() else {
                return self.fail("unterminated string");
            };
            self.at += c.len_utf8();
            match c {
                c if c as u32 == u32::from(quote) => return Ok(out),
                '\\' => {
                    let Some(e) = self.src[self.at..].chars().next() else {
                        return self.fail("unterminated escape");
                    };
                    self.at += e.len_utf8();
                    out.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                }
                c => out.push(c),
            }
        }
    }

    fn number(&mut self) -> Result<Value, Error> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || c == b'_' || c == b'.')
        {
            self.at += 1;
        }
        let text: String = self.src[start..self.at]
            .chars()
            .filter(|c| *c != '_')
            .collect();
        match text.parse::<i64>() {
            Ok(n) => Ok(json!(n)),
            Err(_) => match text.parse::<f64>() {
                Ok(n) => Ok(json!(n)),
                Err(_) => self.fail(format!("bad number {text}")),
            },
        }
    }

    fn name(&mut self) -> Result<String, Error> {
        self.skip();
        let start = self.at;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
        {
            self.at += 1;
        }
        if start == self.at {
            return self.fail("expected a name");
        }
        Ok(self.src[start..self.at].to_owned())
    }

    /// A keyword, an identifier path or a call.
    fn word(&mut self) -> Result<Value, Error> {
        let mut name = self.name()?;
        while self.peek() == Some(b'.') && self.text.get(self.at + 1) != Some(&b'.') {
            self.at += 1;
            name.push('.');
            name.push_str(&self.name()?);
        }
        match name.as_str() {
            "true" => return Ok(Value::Bool(true)),
            "false" => return Ok(Value::Bool(false)),
            "null" | "undefined" => return Ok(Value::Null),
            _ => {}
        }
        self.skip();
        if self.peek() == Some(b'(') {
            self.at += 1;
            let args = self.args()?;
            return Ok(json!({"$call": name, "$args": args}));
        }
        Ok(json!({ "$ident": name }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_calls_and_comments_read_as_json() {
        let source = r#"
            export const x = definePlugin(
              { id: 'a', 'b-c': "d\"e", n: 20_000, list: [1, 2,], t: `x ${y}` },
              // a comment
              { dep: npmDependency({ id: 'z', package: '@z/z' }), icon, ...rest, s: 'a' + 'b', c: m ? { v: m } : undefined } /* c */
            );
        "#;
        let args = call_args(source, "definePlugin").unwrap();
        assert_eq!(
            args[0],
            json!({"id": "a", "b-c": "d\"e", "n": 20000, "list": [1, 2], "t": "x ${y}"})
        );
        assert_eq!(
            args[1],
            json!({
                "dep": {"$call": "npmDependency", "$args": [{"id": "z", "package": "@z/z"}]},
                "icon": {"$ident": "icon"},
                "$spread0": {"$ident": "rest"},
                "s": "ab",
                "c": {"$ternary": [{"$ident": "m"}, {"v": {"$ident": "m"}}, null]}
            })
        );
        assert!(call_args("{ f() {} }", "nothing").is_err());
    }
}
