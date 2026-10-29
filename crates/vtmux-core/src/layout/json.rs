//! The JSON subset layout strings use, a port of json.c (3.8) with its error
//! texts. The subset: the top level and array members are objects; numbers are
//! base-10 `i64`, no fraction or exponent; no `null`; strings are never empty;
//! escapes are validated but not decoded; a key may not repeat (compared
//! undecoded); objects nest at most 200 deep.
//!
//! Errors are the `cause` strings tmux builds: `reason`, or `reason: <up to 8
//! bytes of input at the fault>` plus `...` when more input follows
//! (json.c:372-392). A cut through a multi-byte character prints U+FFFD
//! where tmux prints the raw byte.

use std::collections::BTreeMap;

const ERROR_CTX_LEN: usize = 8;
const PARSE_DEPTH_MAX: u32 = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Str(String),
    Num(i64),
    Bool(bool),
    Object(Object),
    Array(Vec<Object>),
}

/// An object: fields by (undecoded) key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Object(pub BTreeMap<String, Node>);

impl Object {
    #[must_use]
    pub fn find(&self, key: &str) -> Option<&Node> {
        self.0.get(key)
    }

    /// `json_find_string`.
    pub fn string(&self, key: &str) -> Result<&str, String> {
        match self.need(key)? {
            Node::Str(s) => Ok(s),
            _ => Err(expected(key, "a string")),
        }
    }

    /// `json_find_number`.
    pub fn number(&self, key: &str) -> Result<i64, String> {
        match self.need(key)? {
            Node::Num(n) => Ok(*n),
            _ => Err(expected(key, "a number")),
        }
    }

    /// `json_find_boolean`.
    pub fn boolean(&self, key: &str) -> Result<bool, String> {
        match self.need(key)? {
            Node::Bool(b) => Ok(*b),
            _ => Err(expected(key, "a boolean")),
        }
    }

    /// `json_find_object`.
    pub fn object(&self, key: &str) -> Result<&Object, String> {
        match self.need(key)? {
            Node::Object(o) => Ok(o),
            _ => Err(expected(key, "an object")),
        }
    }

    /// `json_find_array`.
    pub fn array(&self, key: &str) -> Result<&[Object], String> {
        match self.need(key)? {
            Node::Array(a) => Ok(a),
            _ => Err(expected(key, "an array")),
        }
    }

    fn need(&self, key: &str) -> Result<&Node, String> {
        self.find(key)
            .ok_or_else(|| format!("key \"{key}\" not found"))
    }
}

fn expected(key: &str, what: &str) -> String {
    format!("key \"{key}\" expected {what}")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tok {
    OpenObject,
    CloseObject,
    OpenArray,
    CloseArray,
    Comma,
    Colon,
    Quote,
    Value,
    Eof,
}

#[derive(Clone, Copy)]
struct Token {
    kind: Tok,
    offset: usize,
    len: usize,
}

/// `json_error`: `reason`, with the input from `loc` on as context.
fn error(input: &[u8], reason: &str, loc: usize) -> String {
    let rest = input.get(loc..).unwrap_or_default();
    if rest.is_empty() {
        return reason.to_owned();
    }
    let ellipsis = if rest.len() > ERROR_CTX_LEN {
        "..."
    } else {
        ""
    };
    let ctx = &rest[..rest.len().min(ERROR_CTX_LEN)];
    format!("{reason}: {}{ellipsis}", String::from_utf8_lossy(ctx))
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `json_tokenize_input`.
fn tokenize(input: &[u8]) -> Result<Vec<Token>, String> {
    let at = |i: usize| input.get(i).copied().unwrap_or(0);
    let mut toks: Vec<Token> = Vec::new();
    let mut in_string = false;
    let mut i = 0;
    let mut loc = 0;
    while at(i) != 0 {
        loc = i;
        let mut scan = 1;
        let kind = if in_string && at(i) != b'"' {
            Tok::Value
        } else {
            match at(i) {
                b' ' | b'\t' | b'\n' | b'\r' => {
                    i += 1;
                    continue;
                }
                b'{' => Tok::OpenObject,
                b'}' => Tok::CloseObject,
                b'[' => Tok::OpenArray,
                b']' => Tok::CloseArray,
                b'"' => Tok::Quote,
                b':' => Tok::Colon,
                b',' => Tok::Comma,
                _ => Tok::Value,
            }
        };
        if kind == Tok::Value {
            match tokenize_value(input, toks.last().map(|t| t.kind), loc) {
                Some(n) => {
                    scan = n;
                    i += n - 1;
                }
                None => return Err(error(input, "tokenization error", loc)),
            }
        }
        toks.push(Token {
            kind,
            offset: loc,
            len: scan,
        });
        if kind == Tok::Quote {
            in_string = !in_string;
        }
        i += 1;
    }
    toks.push(Token {
        kind: Tok::Eof,
        offset: loc,
        len: 0,
    });
    Ok(toks)
}

/// `json_tokenize_value`: a string body after a quote, or a number/boolean
/// after a colon. `None` is tmux's -1.
fn tokenize_value(input: &[u8], prev: Option<Tok>, loc: usize) -> Option<usize> {
    let at = |i: usize| input.get(i).copied().unwrap_or(0);
    let mut scan = 0;
    match prev? {
        Tok::Quote => {
            while at(loc + scan) != b'"' {
                let c = at(loc + scan);
                if c == 0 || c < 0x20 {
                    return None;
                }
                if c != b'\\' {
                    scan += 1;
                    continue;
                }
                scan += 1;
                match at(loc + scan) {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => scan += 1,
                    b'u' => {
                        if !(1..=4).all(|k| at(loc + scan + k).is_ascii_hexdigit()) {
                            return None;
                        }
                        scan += 5;
                    }
                    _ => return None,
                }
            }
        }
        Tok::Colon => loop {
            if at(loc + scan) == 0 {
                return None;
            }
            scan += 1;
            let c = at(loc + scan);
            if matches!(c, b']' | b'}' | b',') || is_space(c) {
                break;
            }
        },
        _ => return None,
    }
    Some(scan)
}

struct Parser<'a> {
    input: &'a [u8],
    toks: Vec<Token>,
    pos: usize,
    depth: u32,
}

impl Parser<'_> {
    fn tok(&self, ahead: usize) -> Token {
        *self
            .toks
            .get(self.pos + ahead)
            .unwrap_or_else(|| self.toks.last().expect("EOF token"))
    }

    fn err(&self, reason: &str, loc: usize) -> String {
        error(self.input, reason, loc)
    }

    fn text(&self, t: Token) -> &str {
        std::str::from_utf8(&self.input[t.offset..t.offset + t.len]).unwrap_or("")
    }

    /// `json_parse_key`.
    fn key(&mut self) -> Result<String, String> {
        let start = self.tok(0).offset;
        let fail = |p: &Self| Err(p.err("invalid key", start));
        if self.tok(0).kind != Tok::Quote {
            return fail(self);
        }
        self.pos += 1;
        let t = self.tok(0);
        if t.kind != Tok::Value {
            return fail(self);
        }
        self.pos += 1;
        if self.tok(0).kind != Tok::Quote {
            return fail(self);
        }
        self.pos += 1;
        Ok(self.text(t).to_owned())
    }

    /// `json_parse_object`; the current token is the `{`.
    fn object(&mut self) -> Result<Object, String> {
        self.depth += 1;
        if self.depth > PARSE_DEPTH_MAX {
            return Err(self.err("parse depth exceeded", self.tok(0).offset));
        }
        self.pos += 1;
        let mut object = Object::default();
        while self.tok(0).kind != Tok::CloseObject {
            let key = self.key()?;
            if object.0.contains_key(&key) {
                return Err(self.err("duplicate key", self.tok(0).offset));
            }
            if self.tok(0).kind != Tok::Colon {
                return Err(self.err("missing colon", self.tok(0).offset));
            }
            self.pos += 1;
            let t = self.tok(0);
            let node = match t.kind {
                Tok::Quote => Node::Str(self.string()?),
                Tok::Value => {
                    let b = self.input[t.offset];
                    let next = self.input.get(t.offset + 1).copied().unwrap_or(0);
                    if b.is_ascii_digit() || (b == b'-' && next.is_ascii_digit()) {
                        Node::Num(self.number()?)
                    } else {
                        Node::Bool(self.boolean()?)
                    }
                }
                Tok::OpenObject => Node::Object(self.object()?),
                Tok::OpenArray => Node::Array(self.array()?),
                _ => {
                    return Err(self.err("unexpected value when parsing object", t.offset));
                }
            };
            object.0.insert(key, node);
            if self.tok(0).kind == Tok::Comma {
                if self.tok(1).kind == Tok::CloseObject {
                    return Err(self.err("invalid object", self.tok(0).offset));
                }
                self.pos += 1;
            } else if self.tok(0).kind != Tok::CloseObject {
                return Err(self.err("invalid object", self.tok(0).offset));
            }
        }
        self.pos += 1;
        self.depth -= 1;
        Ok(object)
    }

    /// `json_parse_array`: members are objects only.
    fn array(&mut self) -> Result<Vec<Object>, String> {
        self.pos += 1;
        let mut members = Vec::new();
        while self.tok(0).kind != Tok::CloseArray {
            if self.tok(0).kind != Tok::OpenObject {
                return Err(self.err("invalid array member", self.tok(0).offset));
            }
            members.push(self.object()?);
            if self.tok(0).kind == Tok::Comma {
                if self.tok(1).kind == Tok::CloseArray {
                    return Err(self.err("invalid array", self.tok(0).offset));
                }
                self.pos += 1;
            } else if self.tok(0).kind != Tok::CloseArray {
                return Err(self.err("invalid array", self.tok(0).offset));
            }
        }
        self.pos += 1;
        Ok(members)
    }

    /// `json_parse_string`: quote, value, quote.
    fn string(&mut self) -> Result<String, String> {
        let start = self.tok(0).offset;
        self.pos += 1;
        let t = self.tok(0);
        if t.kind == Tok::Value {
            self.pos += 1;
            if self.tok(0).kind == Tok::Quote {
                self.pos += 1;
                return Ok(self.text(t).to_owned());
            }
        }
        Err(self.err("invalid string", start))
    }

    /// `json_parse_number`: `-?digits`, no leading zeros, fits `i64`.
    fn number(&mut self) -> Result<i64, String> {
        let t = self.tok(0);
        let s = self.text(t);
        let ok = !((s.starts_with('0') && s.len() != 1) || (s.starts_with("-0") && s.len() != 2));
        let digits = s.strip_prefix('-').unwrap_or(s);
        let n = if ok && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            s.parse::<i64>().ok()
        } else {
            None
        };
        match n {
            Some(n) => {
                self.pos += 1;
                Ok(n)
            }
            None => Err(self.err("invalid number", t.offset)),
        }
    }

    /// `json_parse_boolean`.
    fn boolean(&mut self) -> Result<bool, String> {
        let t = self.tok(0);
        let b = match self.text(t) {
            "true" => true,
            "false" => false,
            _ => return Err(self.err("invalid boolean", t.offset)),
        };
        self.pos += 1;
        Ok(b)
    }
}

/// `json_parse`: the top-level object of `input`.
pub fn parse(input: &str) -> Result<Object, String> {
    // C strings end at the first NUL.
    let input = input.as_bytes();
    let input = &input[..input.iter().position(|&b| b == 0).unwrap_or(input.len())];
    if input.is_empty() {
        return Err("empty input".to_owned());
    }
    let toks = tokenize(input)?;
    let mut p = Parser {
        input,
        toks,
        pos: 0,
        depth: 0,
    };
    if p.tok(0).kind != Tok::OpenObject {
        return Err(p.err("expected object", p.tok(0).offset));
    }
    let object = p.object()?;
    if p.tok(0).kind != Tok::Eof {
        return Err(p.err("unexpected trailing data", p.tok(0).offset));
    }
    Ok(object)
}
