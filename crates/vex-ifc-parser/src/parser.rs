//! Streaming parser for the STEP DATA section.
//!
//! Emits a [`RawEntity`] per `#N = TYPE(args);` statement without resolving
//! references. The caller (see `vex-graph::builder`) wires references up in
//! a second pass.

use std::io::BufRead;

use vex_utils::{VexError, VexResult};

use crate::header::{parse_header, IfcHeader};
use crate::lexer::{Lexer, Token};
use crate::limits::ParseLimits;

/// A STEP argument value. Mirrors the token grammar but preserves nesting.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Omitted argument (`$`).
    Null,
    /// Derived value placeholder (`*`).
    Derived,
    Int(i64),
    Real(f64),
    Str(String),
    /// Enum literal without surrounding dots.
    Enum(String),
    /// Raw hex from a binary literal.
    Binary(String),
    /// Reference to another entity (`#N`).
    Ref(u64),
    /// List of values (`(v1, v2, ...)`).
    List(Vec<Value>),
    /// Typed wrapper, e.g. `IFCLENGTHMEASURE(3.0)`. The name is uppercased.
    Typed { name: String, value: Box<Value> },
}

/// A parsed but unresolved STEP entity.
#[derive(Debug, Clone)]
pub struct RawEntity {
    pub id: u64,
    /// Uppercased type name, e.g. `IFCWALL`.
    pub type_name: String,
    pub args: Vec<Value>,
}

/// Top-level parser driving a lexer. `parse_header` is called once up front;
/// then `next_entity` is called repeatedly until it returns `Ok(None)`.
pub struct Parser<R: BufRead> {
    lx: Lexer<R>,
    limits: ParseLimits,
    entity_count: u64,
    /// Tokens we need to "un-consume" because we peeked one ahead.
    lookahead: Option<Token>,
    /// Whether we've passed ENDSEC of DATA.
    finished: bool,
}

impl<R: BufRead> std::fmt::Debug for Parser<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("entity_count", &self.entity_count)
            .field("finished", &self.finished)
            .finish()
    }
}

impl<R: BufRead> Parser<R> {
    pub fn new(reader: R, limits: ParseLimits) -> Self {
        Self {
            lx: Lexer::new(reader, limits),
            limits,
            entity_count: 0,
            lookahead: None,
            finished: false,
        }
    }

    /// Consume the leading `ISO-10303-21;` banner and `HEADER ... ENDSEC;`.
    /// After this call the parser is positioned at the start of `DATA;`.
    pub fn parse_preamble(&mut self) -> VexResult<IfcHeader> {
        let header = parse_header(&mut self.lx)?;
        // Expect `DATA;`
        self.expect_ident("DATA")?;
        self.expect(Token::Semi)?;
        Ok(header)
    }

    /// Return the next raw entity, or `Ok(None)` at end of DATA section.
    pub fn next_entity(&mut self) -> VexResult<Option<RawEntity>> {
        if self.finished {
            return Ok(None);
        }
        // Skip possible trailing whitespace / comments via the lexer.
        let t = self.next()?;
        match t {
            Token::Ident(ref s) if s.eq_ignore_ascii_case("ENDSEC") => {
                self.expect(Token::Semi)?;
                // Optional `END-ISO-10303-21;` trailer.
                self.finished = true;
                return Ok(None);
            }
            Token::Hash(id) => {
                if id == 0 {
                    return Err(self.err("entity id 0 is not allowed"));
                }
                self.entity_count += 1;
                if self.entity_count > self.limits.max_entities {
                    return Err(VexError::ParseLimit(format!(
                        "entity count exceeds {}",
                        self.limits.max_entities
                    )));
                }
                self.expect(Token::Equals)?;
                let head = self.next()?;
                match head {
                    Token::Ident(type_name) => {
                        let args = self.parse_arg_list(0)?;
                        self.expect(Token::Semi)?;
                        Ok(Some(RawEntity {
                            id,
                            type_name: type_name.to_ascii_uppercase(),
                            args,
                        }))
                    }
                    Token::LParen => {
                        // Complex entity: `#N = (SUB1(...) SUB2(...));`
                        // Represent as synthetic type `__COMPLEX` with a single
                        // List argument; canonicalization layer will handle.
                        let mut parts: Vec<Value> = Vec::new();
                        loop {
                            let t = self.next()?;
                            match t {
                                Token::RParen => break,
                                Token::Ident(sub) => {
                                    let sub_args = self.parse_arg_list(0)?;
                                    parts.push(Value::Typed {
                                        name: sub.to_ascii_uppercase(),
                                        value: Box::new(Value::List(sub_args)),
                                    });
                                }
                                other => {
                                    return Err(self.err(format!(
                                        "unexpected token in complex entity: {other:?}"
                                    )));
                                }
                            }
                        }
                        self.expect(Token::Semi)?;
                        Ok(Some(RawEntity {
                            id,
                            type_name: "__COMPLEX".to_string(),
                            args: parts,
                        }))
                    }
                    other => Err(self.err(format!("expected type name, got {other:?}"))),
                }
            }
            Token::Eof => {
                self.finished = true;
                Ok(None)
            }
            other => Err(self.err(format!("unexpected token at entity start: {other:?}"))),
        }
    }

    fn parse_arg_list(&mut self, depth: u32) -> VexResult<Vec<Value>> {
        if depth > self.limits.max_list_depth {
            return Err(VexError::ParseLimit(format!(
                "list depth exceeds {}",
                self.limits.max_list_depth
            )));
        }
        self.expect(Token::LParen)?;
        let mut out = Vec::new();
        // Allow empty list.
        if matches!(self.peek()?, Token::RParen) {
            self.next()?;
            return Ok(out);
        }
        loop {
            let v = self.parse_value(depth + 1)?;
            out.push(v);
            match self.next()? {
                Token::Comma => continue,
                Token::RParen => return Ok(out),
                other => {
                    return Err(self.err(format!("expected ',' or ')', got {other:?}")));
                }
            }
        }
    }

    fn parse_value(&mut self, depth: u32) -> VexResult<Value> {
        if depth > self.limits.max_list_depth {
            return Err(VexError::ParseLimit(format!(
                "value nesting exceeds {}",
                self.limits.max_list_depth
            )));
        }
        let t = self.next()?;
        match t {
            Token::Dollar => Ok(Value::Null),
            Token::Star => Ok(Value::Derived),
            Token::Int(v) => Ok(Value::Int(v)),
            Token::Real(v) => Ok(Value::Real(v)),
            Token::Str(s) => Ok(Value::Str(s)),
            Token::Enum(s) => Ok(Value::Enum(s)),
            Token::Binary(s) => Ok(Value::Binary(s)),
            Token::Hash(id) => Ok(Value::Ref(id)),
            Token::LParen => {
                // Already consumed LParen; emulate parse_arg_list body.
                self.put_back(Token::LParen);
                Ok(Value::List(self.parse_arg_list(depth)?))
            }
            Token::Ident(name) => {
                // Typed value: IDENT ( value )
                let inner = self.parse_arg_list(depth + 1)?;
                let v = if inner.len() == 1 {
                    inner.into_iter().next().unwrap_or(Value::Null)
                } else {
                    Value::List(inner)
                };
                Ok(Value::Typed {
                    name: name.to_ascii_uppercase(),
                    value: Box::new(v),
                })
            }
            other => Err(self.err(format!("unexpected token in value: {other:?}"))),
        }
    }

    fn next(&mut self) -> VexResult<Token> {
        if let Some(t) = self.lookahead.take() {
            return Ok(t);
        }
        self.lx.next_token()
    }

    fn peek(&mut self) -> VexResult<&Token> {
        if self.lookahead.is_none() {
            self.lookahead = Some(self.lx.next_token()?);
        }
        Ok(self.lookahead.as_ref().expect("just set"))
    }

    fn put_back(&mut self, t: Token) {
        debug_assert!(self.lookahead.is_none(), "double put_back");
        self.lookahead = Some(t);
    }

    fn expect(&mut self, want: Token) -> VexResult<()> {
        let got = self.next()?;
        if std::mem::discriminant(&got) == std::mem::discriminant(&want) {
            Ok(())
        } else {
            Err(self.err(format!("expected {want:?}, got {got:?}")))
        }
    }

    fn expect_ident(&mut self, name: &str) -> VexResult<()> {
        let got = self.next()?;
        match got {
            Token::Ident(ref s) if s.eq_ignore_ascii_case(name) => Ok(()),
            other => Err(self.err(format!("expected ident '{name}', got {other:?}"))),
        }
    }

    fn err(&self, msg: impl Into<String>) -> VexError {
        VexError::parse(self.lx.pos.line, self.lx.pos.col, msg)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const SAMPLE: &str = "\
ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('ViewDefinition [CoordinationView_V2.0]'),'2;1');
FILE_NAME('test.ifc','2024-01-01T00:00:00',('Author'),('Org'),'pre','sys','auth');
FILE_SCHEMA(('IFC4'));
ENDSEC;
DATA;
#1 = IFCPROJECT('0YvctVUKr0kugbFTf53O9L',$,'Project',$,$,$,$,(#2),#3);
#2 = IFCGEOMETRICREPRESENTATIONCONTEXT($,'Model',3,1.0E-5,#4,$);
#3 = IFCUNITASSIGNMENT((#5,#6));
#4 = IFCAXIS2PLACEMENT3D(#7,$,$);
#5 = IFCSIUNIT(*,.LENGTHUNIT.,$,.METRE.);
#6 = IFCSIUNIT(*,.PLANEANGLEUNIT.,$,.RADIAN.);
#7 = IFCCARTESIANPOINT((0.,0.,0.));
#8 = IFCWALL('2O2Fr$t4X7Zf8NOew3FNr2',$,'Wall',$,$,#4,$,$,.STANDARD.);
ENDSEC;
END-ISO-10303-21;
";

    #[test]
    fn parses_preamble_and_streams_entities() {
        let mut p = Parser::new(Cursor::new(SAMPLE), ParseLimits::default());
        let header = p.parse_preamble().expect("header");
        assert_eq!(header.schemas, vec!["IFC4".to_string()]);
        assert_eq!(header.author, vec!["Author".to_string()]);

        let mut ids = Vec::new();
        while let Some(e) = p.next_entity().expect("entity") {
            ids.push((e.id, e.type_name));
        }
        assert_eq!(ids.len(), 8);
        assert_eq!(ids[0], (1, "IFCPROJECT".to_string()));
        assert_eq!(ids[7], (8, "IFCWALL".to_string()));
    }

    #[test]
    fn typed_values_and_refs() {
        let src = "\
ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = X(IFCLENGTHMEASURE(3.5), #2, (#3, #4));
ENDSEC;
END-ISO-10303-21;
";
        let mut p = Parser::new(Cursor::new(src), ParseLimits::default());
        let _ = p.parse_preamble().expect("header");
        let e = p.next_entity().expect("entity").expect("some");
        assert_eq!(e.args.len(), 3);
        matches!(&e.args[0], Value::Typed { name, .. } if name == "IFCLENGTHMEASURE");
        matches!(&e.args[1], Value::Ref(2));
        match &e.args[2] {
            Value::List(xs) => {
                assert_eq!(xs.len(), 2);
                matches!(&xs[0], Value::Ref(3));
            }
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn entity_limit_enforced() {
        let src = "\
ISO-10303-21;
HEADER; FILE_DESCRIPTION((''),'2;1'); FILE_NAME('','',(''),(''),'','',''); FILE_SCHEMA(('IFC4')); ENDSEC;
DATA;
#1 = X();
#2 = X();
#3 = X();
ENDSEC;
END-ISO-10303-21;
";
        let limits = ParseLimits {
            max_entities: 2,
            ..ParseLimits::default()
        };
        let mut p = Parser::new(Cursor::new(src), limits);
        p.parse_preamble().expect("header");
        p.next_entity().expect("ok 1");
        p.next_entity().expect("ok 2");
        let err = p.next_entity().expect_err("should exceed");
        assert!(matches!(err, VexError::ParseLimit(_)));
    }
}
