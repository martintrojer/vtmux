//! Recursive descent ERE parser. Bracket backslashes are ordinary characters in ERE.
use super::{Anchor, Bracket, Class, Error, Expr, Item, upper};

pub(super) fn parse(pattern: &str, ignore_case: bool) -> Result<(Expr, usize), Error> {
    let mut p = Parser {
        chars: pattern.chars().collect(),
        at: 0,
        groups: 0,
        closed: Vec::new(),
        quantifiers: 0,
        ignore_case,
    };
    let expr = p.alt(0)?;
    Ok((expr, p.groups))
}
struct Parser {
    chars: Vec<char>,
    at: usize,
    groups: usize,
    closed: Vec<usize>,
    quantifiers: usize,
    ignore_case: bool,
}
impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }
    fn take(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += 1;
        Some(c)
    }
    fn alt(&mut self, depth: usize) -> Result<Expr, Error> {
        if depth > 64 {
            return Err(Error::Limit);
        }
        let incoming = self.closed.clone();
        let mut completed = incoming.clone();
        let mut alternatives = Vec::new();
        loop {
            let mut seq = Vec::new();
            while self
                .peek()
                .is_some_and(|c| c != '|' && (c != ')' || depth == 0))
            {
                let atom = self.atom(depth)?;
                seq.push(self.quantify(atom)?);
            }
            alternatives.push(if seq.is_empty() {
                Expr::Empty
            } else {
                Expr::Seq(seq)
            });
            if self.peek() != Some('|') {
                break;
            }
            completed.extend(self.closed.iter().copied());
            self.at += 1;
            self.closed.clone_from(&incoming);
        }
        self.closed.extend(completed);
        self.closed.sort_unstable();
        self.closed.dedup();
        Ok(if alternatives.len() == 1 {
            alternatives.pop().unwrap()
        } else {
            Expr::Alt(alternatives)
        })
    }
    fn atom(&mut self, depth: usize) -> Result<Expr, Error> {
        Ok(match self.take().ok_or(Error::Syntax)? {
            '(' => {
                self.groups += 1;
                if self.groups > 128 {
                    return Err(Error::Limit);
                }
                let n = self.groups;
                let expr = self.alt(depth + 1)?;
                if self.take() != Some(')') {
                    return Err(Error::Syntax);
                }
                self.closed.push(n);
                Expr::Group(n, Box::new(expr))
            }
            '[' => Expr::Bracket(self.bracket()?),
            '.' => Expr::Any,
            '^' => Expr::Anchor(Anchor::Start),
            '$' => Expr::Anchor(Anchor::End),
            '*' | '+' | '?' | '{' => return Err(Error::Syntax),
            '\\' => match self.take().ok_or(Error::Syntax)? {
                c @ '1'..='9' => {
                    let n = usize::try_from(c.to_digit(10).unwrap()).unwrap();
                    if !self.closed.contains(&n) {
                        return Err(Error::Syntax);
                    }
                    Expr::Backref(n)
                }
                'b' => Expr::Anchor(Anchor::Boundary),
                'B' => Expr::Anchor(Anchor::NotBoundary),
                '<' => Expr::Anchor(Anchor::WordStart),
                '>' => Expr::Anchor(Anchor::WordEnd),
                '`' => Expr::Anchor(Anchor::Start),
                '\'' => Expr::Anchor(Anchor::End),
                c @ ('w' | 'W' | 's' | 'S') => Expr::Bracket(Bracket {
                    negated: c.is_ascii_uppercase(),
                    items: if c.eq_ignore_ascii_case(&'w') {
                        vec![Item::Class(Class::Alnum), Item::Char('_')]
                    } else {
                        vec![Item::Class(Class::Space)]
                    },
                }),
                c => Expr::Literal(c),
            },
            c => Expr::Literal(c),
        })
    }
    fn quantify(&mut self, mut expr: Expr) -> Result<Expr, Error> {
        let mut count = 0;
        while let Some(c @ ('*' | '+' | '?' | '{')) = self.peek() {
            count += 1;
            self.quantifiers += 1;
            if count > 64 || self.quantifiers > 128 {
                return Err(Error::Limit);
            }
            if matches!(expr, Expr::Anchor(_)) {
                return Err(Error::Syntax);
            }
            self.at += 1;
            let (min, max) = match c {
                '*' => (0, None),
                '+' => (1, None),
                '?' => (0, Some(1)),
                _ => {
                    let min = self.number()?.unwrap_or(0);
                    let max = if self.peek() == Some(',') {
                        self.at += 1;
                        self.number()?
                    } else {
                        Some(min)
                    };
                    if self.take() != Some('}') || max.is_some_and(|m| m < min) {
                        return Err(Error::Syntax);
                    }
                    (min, max)
                }
            };
            expr = Expr::Repeat {
                expr: Box::new(expr),
                min,
                max,
            };
        }
        Ok(expr)
    }
    fn number(&mut self) -> Result<Option<usize>, Error> {
        let mut n = None;
        while let Some(c) = self.peek().filter(char::is_ascii_digit) {
            self.at += 1;
            let value = n
                .unwrap_or(0usize)
                .checked_mul(10)
                .and_then(|n| n.checked_add(usize::try_from(c.to_digit(10).unwrap()).unwrap()))
                .ok_or(Error::Syntax)?;
            if value > 32767 {
                return Err(Error::Syntax);
            }
            n = Some(value);
        }
        Ok(n)
    }
    fn bracket(&mut self) -> Result<Bracket, Error> {
        let negated = self.peek() == Some('^');
        if negated {
            self.at += 1;
        }
        let mut items = Vec::new();
        while self.peek().is_some() {
            if self.peek() == Some(']') && !items.is_empty() {
                self.at += 1;
                return Ok(Bracket { negated, items });
            }
            let a = self.bracket_item()?;
            if self.peek() == Some('-') && self.chars.get(self.at + 1) != Some(&']') {
                self.at += 1;
                let b = self.bracket_item()?;
                let (Item::Char(mut a), Item::Char(mut b)) = (a, b) else {
                    return Err(Error::Syntax);
                };
                // glibc's C LC_COLLATE rejects multibyte range endpoints (REG_ECOLLATE).
                if !a.is_ascii() || !b.is_ascii() {
                    return Err(Error::Syntax);
                }
                if self.ignore_case {
                    a = upper(a);
                    b = upper(b);
                }
                if b < a {
                    return Err(Error::Syntax);
                }
                items.push(Item::Range(a, b));
                if self.peek() == Some('-') && self.chars.get(self.at + 1) != Some(&']') {
                    return Err(Error::Syntax);
                }
            } else {
                items.push(a);
            }
        }
        Err(Error::Syntax)
    }
    fn bracket_item(&mut self) -> Result<Item, Error> {
        let c = self.take().ok_or(Error::Syntax)?;
        if c == '[' && self.peek().is_some_and(|c| matches!(c, ':' | '.' | '=')) {
            let delimiter = self.take().unwrap();
            let start = self.at;
            while self.peek().is_some() {
                if self.peek() == Some(delimiter) && self.chars.get(self.at + 1) == Some(&']') {
                    let name: String = self.chars[start..self.at].iter().collect();
                    self.at += 2;
                    return if delimiter == ':' {
                        Class::parse(&name).map(Item::Class).ok_or(Error::Syntax)
                    } else if name.len() == 1 {
                        Ok(Item::Char(name.chars().next().unwrap()))
                    } else {
                        Err(Error::Syntax)
                    };
                }
                self.at += 1;
            }
            return Err(Error::Syntax);
        }
        Ok(Item::Char(c))
    }
}
