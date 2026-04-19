//! IFC HEADER section parsing.
//!
//! The header carries file metadata (author, originating system, timestamp,
//! schema version). For diffing we **ignore most of it** (see
//! `vex-graph::canonical`), but the schema name is critical — it tells us
//! which IFC version the DATA section conforms to.

use vex_utils::{VexError, VexResult};

use crate::lexer::{Lexer, Token};
use std::io::BufRead;

#[derive(Debug, Clone, Default)]
pub struct IfcHeader {
    pub description: Vec<String>,
    pub implementation_level: Option<String>,
    pub name: Option<String>,
    pub time_stamp: Option<String>,
    pub author: Vec<String>,
    pub organization: Vec<String>,
    pub preprocessor_version: Option<String>,
    pub originating_system: Option<String>,
    pub authorization: Option<String>,
    pub schemas: Vec<String>,
}

impl IfcHeader {
    /// Best-effort IFC schema detection. Examples of raw `schemas` entries:
    /// - `"IFC4"`, `"IFC4X3_ADD2"`, `"IFC2X3"`
    #[must_use]
    pub fn schema_family(&self) -> Option<&str> {
        self.schemas.first().map(String::as_str)
    }
}

/// Read through `ISO-10303-21;` and `HEADER;` tokens and return the parsed header.
/// The lexer is left positioned just past `ENDSEC;` of the HEADER section.
pub(crate) fn parse_header<R: BufRead>(lx: &mut Lexer<R>) -> VexResult<IfcHeader> {
    expect_ident(lx, "ISO")?; // ISO-10303-21 tokens often arrive split; we accept loosely.
                              // The "ISO-10303-21" banner is not strictly a single STEP ident — real files
                              // write it as `ISO-10303-21;`. Our lexer will see `ISO`, `-`, `10303`, `-`, `21`, `;`.
                              // We just skip tokens until the first `;` for forgiveness.
    skip_until_semi(lx)?;
    expect_ident(lx, "HEADER")?;
    expect(lx, &Token::Semi)?;

    let mut header = IfcHeader::default();
    loop {
        let t = lx.next_token()?;
        match t {
            Token::Ident(ref s) if s.eq_ignore_ascii_case("ENDSEC") => {
                expect(lx, &Token::Semi)?;
                return Ok(header);
            }
            Token::Ident(s) => {
                let args = read_args(lx)?;
                expect(lx, &Token::Semi)?;
                store_header_entry(&mut header, &s, &args);
            }
            other => {
                return Err(VexError::parse(
                    lx.pos.line,
                    lx.pos.col,
                    format!("unexpected token in HEADER: {other:?}"),
                ))
            }
        }
    }
}

fn store_header_entry(header: &mut IfcHeader, name: &str, args: &[Token]) {
    // Trivially shape-recognise well-known header entities.
    match name.to_ascii_uppercase().as_str() {
        "FILE_DESCRIPTION" => {
            if let Some(list) = pick_string_list(args, 0) {
                header.description = list;
            }
            if let Some(s) = pick_string(args, 1) {
                header.implementation_level = Some(s);
            }
        }
        "FILE_NAME" => {
            header.name = pick_string(args, 0);
            header.time_stamp = pick_string(args, 1);
            header.author = pick_string_list(args, 2).unwrap_or_default();
            header.organization = pick_string_list(args, 3).unwrap_or_default();
            header.preprocessor_version = pick_string(args, 4);
            header.originating_system = pick_string(args, 5);
            header.authorization = pick_string(args, 6);
        }
        "FILE_SCHEMA" => {
            if let Some(list) = pick_string_list(args, 0) {
                header.schemas = list;
            }
        }
        _ => {
            // Unknown header entries (e.g. FILE_POPULATION) are tolerated.
        }
    }
}

fn pick_string(tokens: &[Token], index: usize) -> Option<String> {
    // The header argument stream is flattened; we pull the N-th top-level
    // argument by counting comma depth. For robustness we accept the simple
    // case where each argument is exactly one token.
    let mut arg_index = 0usize;
    for t in tokens {
        if matches!(t, Token::Comma) {
            arg_index += 1;
            continue;
        }
        if arg_index == index {
            if let Token::Str(s) = t {
                return Some(s.clone());
            }
        }
    }
    None
}

fn pick_string_list(tokens: &[Token], index: usize) -> Option<Vec<String>> {
    // Pick the N-th top-level arg, expecting it to be a parenthesised list of strings.
    let mut arg_index = 0usize;
    let mut i = 0;
    while i < tokens.len() {
        if matches!(tokens[i], Token::Comma) {
            arg_index += 1;
            i += 1;
            continue;
        }
        if arg_index == index {
            if matches!(tokens[i], Token::LParen) {
                let mut out = Vec::new();
                let mut depth = 1;
                i += 1;
                while i < tokens.len() && depth > 0 {
                    match &tokens[i] {
                        Token::LParen => depth += 1,
                        Token::RParen => depth -= 1,
                        Token::Str(s) if depth == 1 => out.push(s.clone()),
                        _ => {}
                    }
                    i += 1;
                }
                return Some(out);
            }
            return None;
        }
        i += 1;
    }
    None
}

fn read_args<R: BufRead>(lx: &mut Lexer<R>) -> VexResult<Vec<Token>> {
    expect(lx, &Token::LParen)?;
    let mut out = Vec::new();
    let mut depth = 1i32;
    loop {
        let t = lx.next_token()?;
        match &t {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Ok(out);
                }
            }
            Token::Eof => {
                return Err(VexError::parse(
                    lx.pos.line,
                    lx.pos.col,
                    "unexpected EOF in argument list",
                ));
            }
            _ => {}
        }
        out.push(t);
    }
}

fn expect<R: BufRead>(lx: &mut Lexer<R>, want: &Token) -> VexResult<()> {
    let got = lx.next_token()?;
    if std::mem::discriminant(&got) == std::mem::discriminant(want) {
        Ok(())
    } else {
        Err(VexError::parse(
            lx.pos.line,
            lx.pos.col,
            format!("expected {want:?}, got {got:?}"),
        ))
    }
}

fn expect_ident<R: BufRead>(lx: &mut Lexer<R>, name: &str) -> VexResult<()> {
    let got = lx.next_token()?;
    match got {
        Token::Ident(ref s) if s.eq_ignore_ascii_case(name) => Ok(()),
        other => Err(VexError::parse(
            lx.pos.line,
            lx.pos.col,
            format!("expected ident {name}, got {other:?}"),
        )),
    }
}

fn skip_until_semi<R: BufRead>(lx: &mut Lexer<R>) -> VexResult<()> {
    loop {
        match lx.next_token()? {
            Token::Semi => return Ok(()),
            Token::Eof => {
                return Err(VexError::parse(
                    lx.pos.line,
                    lx.pos.col,
                    "EOF looking for ';'",
                ));
            }
            _ => {}
        }
    }
}
