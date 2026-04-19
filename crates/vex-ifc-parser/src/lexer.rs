//! Streaming byte-level lexer for STEP Part 21 (ISO 10303-21).
//!
//! We work on a `&mut impl BufRead` so the whole file never needs to fit in
//! memory. Comments `/* ... */` are stripped transparently. Line and column
//! counters are maintained for diagnostics.
//!
//! The lexer is deliberately minimal: it recognises the STEP tokens that
//! appear in IFC data sections. Higher-level concerns (entity structure,
//! reference resolution, schema awareness) live in the parser.

use std::io::BufRead;

use vex_utils::{VexError, VexResult};

use crate::limits::ParseLimits;

/// A single STEP token.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// `#123` — an entity reference or definition head.
    Hash(u64),
    /// An identifier. For IFC this is the entity type name (e.g. `IFCWALL`).
    Ident(String),
    /// Integer literal.
    Int(i64),
    /// Floating-point literal.
    Real(f64),
    /// String literal, already unescaped.
    Str(String),
    /// Enum literal, without the surrounding dots (e.g. `.T.` → `"T"`).
    Enum(String),
    /// Binary literal `"0F…"`. Stored as raw hex without the quotes.
    Binary(String),
    LParen,
    RParen,
    Comma,
    /// `$` — omitted / null.
    Dollar,
    /// `*` — derived value placeholder.
    Star,
    /// `=`
    Equals,
    /// `;`
    Semi,
    /// End of meaningful input (after `END-ISO-10303-21;`).
    Eof,
}

/// Position in the source file for diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

/// Streaming lexer.
pub struct Lexer<R: BufRead> {
    reader: R,
    // Single-byte lookahead buffer.
    peeked: Option<u8>,
    pub pos: Pos,
    bytes_read: u64,
    limits: ParseLimits,
}

impl<R: BufRead> std::fmt::Debug for Lexer<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lexer")
            .field("pos", &self.pos)
            .field("bytes_read", &self.bytes_read)
            .finish_non_exhaustive()
    }
}

impl<R: BufRead> Lexer<R> {
    pub fn new(reader: R, limits: ParseLimits) -> Self {
        Self {
            reader,
            peeked: None,
            pos: Pos { line: 1, col: 1 },
            bytes_read: 0,
            limits,
        }
    }

    fn read_byte(&mut self) -> VexResult<Option<u8>> {
        if let Some(b) = self.peeked.take() {
            return Ok(Some(b));
        }
        let mut buf = [0u8; 1];
        let n = self.reader.read(&mut buf).map_err(|e| VexError::Io {
            path: None,
            source: e,
        })?;
        if n == 0 {
            return Ok(None);
        }
        self.bytes_read += 1;
        if let Some(cap) = self.limits.max_input_bytes {
            if self.bytes_read > cap {
                return Err(VexError::ParseLimit(format!("input exceeded {cap} bytes")));
            }
        }
        let b = buf[0];
        if b == b'\n' {
            self.pos.line = self.pos.line.saturating_add(1);
            self.pos.col = 1;
        } else {
            self.pos.col = self.pos.col.saturating_add(1);
        }
        Ok(Some(b))
    }

    fn peek_byte(&mut self) -> VexResult<Option<u8>> {
        if self.peeked.is_none() {
            self.peeked = self.read_byte()?;
            // Undo the position advance caused by the eager read.
            // We accept slightly imprecise column counts on peek boundaries
            // in exchange for implementation simplicity; diagnostics remain
            // accurate to within one byte.
        }
        Ok(self.peeked)
    }

    fn unget(&mut self, b: u8) {
        debug_assert!(self.peeked.is_none(), "cannot double-unget");
        self.peeked = Some(b);
    }

    fn skip_ws_and_comments(&mut self) -> VexResult<()> {
        loop {
            match self.read_byte()? {
                None => return Ok(()),
                Some(b) if b.is_ascii_whitespace() => {}
                Some(b'/') => match self.read_byte()? {
                    Some(b'*') => self.skip_block_comment()?,
                    Some(other) => {
                        self.unget(other);
                        self.unget(b'/');
                        return Ok(());
                    }
                    None => {
                        return Err(self.err("unexpected EOF after '/'"));
                    }
                },
                Some(other) => {
                    self.unget(other);
                    return Ok(());
                }
            }
        }
    }

    fn skip_block_comment(&mut self) -> VexResult<()> {
        loop {
            match self.read_byte()? {
                None => return Err(self.err("unterminated block comment")),
                Some(b'*') => {
                    if matches!(self.read_byte()?, Some(b'/')) {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
    }

    fn read_hash(&mut self) -> VexResult<Token> {
        let mut n: u64 = 0;
        let mut any = false;
        while let Some(b) = self.read_byte()? {
            if b.is_ascii_digit() {
                any = true;
                n = n
                    .checked_mul(10)
                    .and_then(|v| v.checked_add(u64::from(b - b'0')))
                    .ok_or_else(|| self.err("entity id overflow"))?;
                if n > self.limits.max_entity_id {
                    return Err(VexError::ParseLimit(format!(
                        "entity id {n} exceeds max {}",
                        self.limits.max_entity_id
                    )));
                }
            } else {
                self.unget(b);
                break;
            }
        }
        if !any {
            return Err(self.err("expected digits after '#'"));
        }
        Ok(Token::Hash(n))
    }

    fn read_ident(&mut self, first: u8) -> VexResult<Token> {
        // STEP identifiers: [A-Za-z_][A-Za-z0-9_]*. Uppercased by convention
        // for IFC entity and type names but we preserve case of the raw input.
        let mut s = String::with_capacity(16);
        s.push(first as char);
        while let Some(b) = self.read_byte()? {
            if b.is_ascii_alphanumeric() || b == b'_' {
                s.push(b as char);
            } else {
                self.unget(b);
                break;
            }
        }
        Ok(Token::Ident(s))
    }

    fn read_number(&mut self, first: u8) -> VexResult<Token> {
        // Numbers: optional sign handled by caller; we see [0-9.eE+-].
        let mut s = String::with_capacity(16);
        s.push(first as char);
        let mut is_real = matches!(first, b'.');
        while let Some(b) = self.read_byte()? {
            match b {
                b'0'..=b'9' => s.push(b as char),
                b'.' | b'e' | b'E' => {
                    is_real = true;
                    s.push(b as char);
                }
                b'+' | b'-' => {
                    // Only valid inside an exponent.
                    let prev = s.as_bytes().last().copied();
                    if matches!(prev, Some(b'e' | b'E')) {
                        s.push(b as char);
                    } else {
                        self.unget(b);
                        break;
                    }
                }
                _ => {
                    self.unget(b);
                    break;
                }
            }
        }
        if is_real {
            let v: f64 = s
                .parse()
                .map_err(|_| self.err(format!("invalid real: {s}")))?;
            Ok(Token::Real(v))
        } else {
            let v: i64 = s
                .parse()
                .map_err(|_| self.err(format!("invalid integer: {s}")))?;
            Ok(Token::Int(v))
        }
    }

    fn read_string(&mut self) -> VexResult<Token> {
        // STEP strings are single-quoted; `''` is a literal quote; and
        // `\X\hh`, `\X2\hhhh…\X0\`, `\X4\hhhhhhhh…\X0\` are unicode escapes.
        let mut out = String::new();
        loop {
            let Some(b) = self.read_byte()? else {
                return Err(self.err("unterminated string literal"));
            };
            if out.len() > self.limits.max_string_bytes {
                return Err(VexError::ParseLimit(format!(
                    "string literal exceeds {} bytes",
                    self.limits.max_string_bytes
                )));
            }
            match b {
                b'\'' => {
                    // Doubled quote → literal.
                    if matches!(self.peek_byte()?, Some(b'\'')) {
                        self.read_byte()?;
                        out.push('\'');
                    } else {
                        return Ok(Token::Str(out));
                    }
                }
                b'\\' => self.read_string_escape(&mut out)?,
                other => out.push(other as char),
            }
        }
    }

    fn read_string_escape(&mut self, out: &mut String) -> VexResult<()> {
        // We already consumed the leading backslash.
        let Some(tag) = self.read_byte()? else {
            return Err(self.err("unterminated string escape"));
        };
        match tag {
            b'X' | b'x' => {
                // \X\hh  |  \X2\hhhh…\X0\  |  \X4\hhhhhhhh…\X0\
                let Some(sep) = self.read_byte()? else {
                    return Err(self.err("truncated \\X escape"));
                };
                match sep {
                    b'\\' => {
                        // \X\hh — single ISO-8859-1 code point.
                        let hi = self.read_hex_digit()?;
                        let lo = self.read_hex_digit()?;
                        out.push(char::from((hi << 4) | lo));
                    }
                    b'2' | b'4' => {
                        let width = if sep == b'2' { 4 } else { 8 };
                        if !matches!(self.read_byte()?, Some(b'\\')) {
                            return Err(self.err("expected '\\' after \\X2 or \\X4"));
                        }
                        // Read repeated units of `width` hex digits until \X0\.
                        loop {
                            // Peek for \X0\ terminator.
                            if matches!(self.peek_byte()?, Some(b'\\')) {
                                // Look ahead non-destructively. We consume and
                                // push back if it isn't the terminator.
                                self.read_byte()?;
                                let next = self.read_byte()?;
                                let term = self.read_byte()?;
                                match (next, term) {
                                    (Some(b'X'), Some(b'0')) => {
                                        if !matches!(self.read_byte()?, Some(b'\\')) {
                                            return Err(self.err("malformed \\X0\\ terminator"));
                                        }
                                        return Ok(());
                                    }
                                    _ => {
                                        return Err(
                                            self.err("unexpected content inside \\X2/\\X4 body")
                                        );
                                    }
                                }
                            }
                            let mut code: u32 = 0;
                            for _ in 0..width {
                                code = (code << 4) | u32::from(self.read_hex_digit()?);
                            }
                            if let Some(ch) = char::from_u32(code) {
                                out.push(ch);
                            } else {
                                return Err(
                                    self.err(format!("invalid unicode codepoint U+{code:04X}"))
                                );
                            }
                        }
                    }
                    other => {
                        return Err(
                            self.err(format!("unknown \\X escape variant: {:?}", other as char))
                        )
                    }
                }
            }
            b'S' | b's' => {
                // \S\c — ISO-8859 with high bit set on next char. Rare.
                if !matches!(self.read_byte()?, Some(b'\\')) {
                    return Err(self.err("expected '\\' in \\S escape"));
                }
                let Some(c) = self.read_byte()? else {
                    return Err(self.err("truncated \\S escape"));
                };
                out.push(char::from(c | 0x80));
            }
            other => {
                // Unrecognised escape — preserve verbatim so we don't lose data.
                out.push('\\');
                out.push(other as char);
            }
        }
        Ok(())
    }

    fn read_hex_digit(&mut self) -> VexResult<u8> {
        let Some(b) = self.read_byte()? else {
            return Err(self.err("unexpected EOF in hex escape"));
        };
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(self.err(format!("invalid hex digit {:?}", b as char))),
        }
    }

    fn read_enum(&mut self) -> VexResult<Token> {
        // `.NAME.` — identifier between two periods.
        let mut s = String::with_capacity(8);
        loop {
            let Some(b) = self.read_byte()? else {
                return Err(self.err("unterminated enum"));
            };
            if b == b'.' {
                return Ok(Token::Enum(s));
            }
            if b.is_ascii_alphanumeric() || b == b'_' {
                s.push(b as char);
            } else {
                return Err(self.err(format!("invalid char in enum: {:?}", b as char)));
            }
        }
    }

    fn read_binary(&mut self) -> VexResult<Token> {
        // `"HEX…"` — hex-encoded binary literal.
        let mut s = String::new();
        loop {
            let Some(b) = self.read_byte()? else {
                return Err(self.err("unterminated binary literal"));
            };
            if b == b'"' {
                return Ok(Token::Binary(s));
            }
            if b.is_ascii_hexdigit() {
                s.push(b as char);
            } else {
                return Err(self.err(format!("invalid char in binary: {:?}", b as char)));
            }
        }
    }

    fn err(&self, msg: impl Into<String>) -> VexError {
        VexError::parse(self.pos.line, self.pos.col, msg)
    }

    /// Return the next token, or `Token::Eof` at end of input.
    pub fn next_token(&mut self) -> VexResult<Token> {
        self.skip_ws_and_comments()?;
        let Some(b) = self.read_byte()? else {
            return Ok(Token::Eof);
        };
        match b {
            b'#' => self.read_hash(),
            b'(' => Ok(Token::LParen),
            b')' => Ok(Token::RParen),
            b',' => Ok(Token::Comma),
            b'$' => Ok(Token::Dollar),
            b'*' => Ok(Token::Star),
            b'=' => Ok(Token::Equals),
            b';' => Ok(Token::Semi),
            b'\'' => self.read_string(),
            b'.' => {
                // Could be the start of an enum `.X.` OR a number starting with `.`.
                match self.peek_byte()? {
                    Some(b2) if b2.is_ascii_alphabetic() || b2 == b'_' => self.read_enum(),
                    _ => self.read_number(b'.'),
                }
            }
            b'"' => self.read_binary(),
            b'-' | b'+' => {
                // Sign: must precede a digit or '.'. `read_number` will
                // include the sign byte in its accumulator, so f64::parse /
                // i64::parse handles it directly — do NOT double-negate.
                self.read_number(b)
            }
            b'0'..=b'9' => self.read_number(b),
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => self.read_ident(b),
            other => Err(self.err(format!("unexpected byte {:?}", other as char))),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::approx_constant
)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn toks(src: &str) -> Vec<Token> {
        let mut lx = Lexer::new(Cursor::new(src), ParseLimits::default());
        let mut out = Vec::new();
        loop {
            let t = lx.next_token().expect("lex");
            if matches!(t, Token::Eof) {
                break;
            }
            out.push(t);
        }
        out
    }

    #[test]
    fn basic_entity() {
        let src = "#42 = IFCWALL('abc', 3.14, $, .T., (#1, #2));";
        let got = toks(src);
        use Token::*;
        assert!(matches!(got[0], Hash(42)));
        assert!(matches!(got[1], Equals));
        assert!(matches!(got[2], Ident(ref s) if s == "IFCWALL"));
        assert!(matches!(got[3], LParen));
        assert!(matches!(got[4], Str(ref s) if s == "abc"));
        assert!(matches!(got[6], Real(x) if (x - 3.14).abs() < 1e-9));
        assert!(matches!(got[8], Dollar));
        assert!(matches!(got[10], Enum(ref s) if s == "T"));
    }

    #[test]
    fn doubled_quote_in_string() {
        let got = toks("X('it''s');");
        assert!(matches!(&got[2], Token::Str(s) if s == "it's"));
    }

    #[test]
    fn unicode_x2_escape() {
        let got = toks("X('\\X2\\00E9\\X0\\');");
        assert!(matches!(&got[2], Token::Str(s) if s == "é"));
    }

    #[test]
    fn block_comment_skipped() {
        let got = toks("/* hi */ #1 = X();");
        assert!(matches!(got[0], Token::Hash(1)));
    }

    #[test]
    fn negative_numbers() {
        let got = toks("X(-1, -2.5);");
        assert!(matches!(got[2], Token::Int(-1)));
        assert!(matches!(got[4], Token::Real(x) if (x + 2.5).abs() < 1e-9));
    }

    #[test]
    fn rejects_invalid_byte() {
        let mut lx = Lexer::new(Cursor::new("@"), ParseLimits::default());
        assert!(lx.next_token().is_err());
    }
}
