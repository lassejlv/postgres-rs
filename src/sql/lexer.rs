//! Hand-written SQL tokenizer.
//!
//! Produces a flat token stream the parser consumes. Keywords are not
//! distinguished from identifiers here; the parser matches keywords
//! case-insensitively against [`Token::Word`].

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A bare word: keyword or unquoted identifier (stored as written).
    Word(String),
    /// A double-quoted identifier, e.g. `"My Col"` (case preserved, unquoted).
    QuotedIdent(String),
    /// A single-quoted string literal with escapes already resolved.
    StringLit(String),
    /// A numeric literal, kept as text so the parser can choose int vs float.
    Number(String),
    /// A positional parameter placeholder, e.g. `$1` (1-based).
    Param(u32),
    /// Punctuation / operators.
    Comma,
    Semicolon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Star,
    Plus,
    Minus,
    /// JSON extraction `->`.
    Arrow,
    /// JSON text extraction `->>`.
    ArrowText,
    /// Array contains `@>`.
    ArrayContains,
    /// Full text match `@@`.
    TextSearchMatch,
    /// Array is contained by `<@`.
    ArrayContainedBy,
    /// Array overlap `&&`.
    ArrayOverlap,
    /// Network is strictly contained by `<<`.
    NetworkContainedBy,
    /// Network is contained by or equals `<<=`.
    NetworkContainedByEq,
    /// Network strictly contains `>>`.
    NetworkContains,
    /// Network contains or equals `>>=`.
    NetworkContainsEq,
    Slash,
    Percent,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Dot,
    /// String concatenation `||`.
    Concat,
    /// Type cast `::`.
    DoubleColon,
    /// POSIX regex match `~`.
    Match,
    /// Case-insensitive regex match `~*`.
    MatchCi,
    /// Negated regex match `!~`.
    NotMatch,
    /// Negated case-insensitive regex match `!~*`.
    NotMatchCi,
}

pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Lexer {
            input: input.as_bytes(),
            pos: 0,
        }
    }

    /// Tokenize the whole input, returning an error on malformed literals.
    pub fn tokenize(mut self) -> Result<Vec<Token>, String> {
        let mut out = Vec::new();
        while let Some(tok) = self.next_token()? {
            out.push(tok);
        }
        Ok(out)
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.input.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn next_token(&mut self) -> Result<Option<Token>, String> {
        self.skip_whitespace_and_comments();
        let Some(c) = self.peek() else {
            return Ok(None);
        };

        // Identifiers / keywords.
        if c == b'_' || c.is_ascii_alphabetic() {
            return Ok(Some(self.read_word()));
        }
        // Numbers (optionally starting with a leading dot, e.g. `.5`).
        if c.is_ascii_digit() || (c == b'.' && self.peek_at(1).is_some_and(|d| d.is_ascii_digit()))
        {
            return Ok(Some(self.read_number()));
        }
        // Quoted identifier.
        if c == b'"' {
            return Ok(Some(self.read_quoted_ident()?));
        }
        // String literal.
        if c == b'\'' {
            return Ok(Some(self.read_string()?));
        }
        // Dollar-quoted string literal: $$...$$ or $tag$...$tag$.
        if c == b'$' && self.starts_dollar_quote() {
            return Ok(Some(self.read_dollar_string()?));
        }
        // Positional parameter `$N`.
        if c == b'$' && self.peek_at(1).is_some_and(|d| d.is_ascii_digit()) {
            return Ok(Some(self.read_param()));
        }

        // Operators and punctuation.
        self.bump();
        let tok = match c {
            b',' => Token::Comma,
            b';' => Token::Semicolon,
            b'(' => Token::LParen,
            b')' => Token::RParen,
            b'[' => Token::LBracket,
            b']' => Token::RBracket,
            b'*' => Token::Star,
            b'+' => Token::Plus,
            b'-' => match self.peek() {
                Some(b'>') => {
                    self.bump();
                    if self.peek() == Some(b'>') {
                        self.bump();
                        Token::ArrowText
                    } else {
                        Token::Arrow
                    }
                }
                _ => Token::Minus,
            },
            b'/' => Token::Slash,
            b'%' => Token::Percent,
            b'.' => Token::Dot,
            b':' => match self.peek() {
                Some(b':') => {
                    self.bump();
                    Token::DoubleColon
                }
                _ => return Err("unexpected character ':'".to_string()),
            },
            b'=' => Token::Eq,
            b'<' => match self.peek() {
                Some(b'<') => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::NetworkContainedByEq
                    } else {
                        Token::NetworkContainedBy
                    }
                }
                Some(b'>') => {
                    self.bump();
                    Token::NotEq
                }
                Some(b'@') => {
                    self.bump();
                    Token::ArrayContainedBy
                }
                Some(b'=') => {
                    self.bump();
                    Token::LtEq
                }
                _ => Token::Lt,
            },
            b'>' => match self.peek() {
                Some(b'>') => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::NetworkContainsEq
                    } else {
                        Token::NetworkContains
                    }
                }
                Some(b'=') => {
                    self.bump();
                    Token::GtEq
                }
                _ => Token::Gt,
            },
            b'!' => match self.peek() {
                Some(b'=') => {
                    self.bump();
                    Token::NotEq
                }
                Some(b'~') => {
                    self.bump();
                    if self.peek() == Some(b'*') {
                        self.bump();
                        Token::NotMatchCi
                    } else {
                        Token::NotMatch
                    }
                }
                _ => return Err("unexpected character '!'".to_string()),
            },
            b'~' => {
                if self.peek() == Some(b'*') {
                    self.bump();
                    Token::MatchCi
                } else {
                    Token::Match
                }
            }
            b'@' => match self.peek() {
                Some(b'@') => {
                    self.bump();
                    Token::TextSearchMatch
                }
                Some(b'>') => {
                    self.bump();
                    Token::ArrayContains
                }
                _ => return Err("unexpected character '@'".to_string()),
            },
            b'&' => match self.peek() {
                Some(b'&') => {
                    self.bump();
                    Token::ArrayOverlap
                }
                _ => return Err("unexpected character '&'".to_string()),
            },
            b'|' => match self.peek() {
                Some(b'|') => {
                    self.bump();
                    Token::Concat
                }
                _ => return Err("unexpected character '|'".to_string()),
            },
            other => return Err(format!("unexpected character '{}'", other as char)),
        };
        Ok(Some(tok))
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_ascii_whitespace() => {
                    self.bump();
                }
                // Line comment `-- ...`.
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                // Block comment `/* ... */` (PostgreSQL allows nesting).
                Some(b'/') if self.peek_at(1) == Some(b'*') => {
                    self.bump();
                    self.bump();
                    let mut depth = 1;
                    while depth > 0 {
                        match self.peek() {
                            Some(b'/') if self.peek_at(1) == Some(b'*') => {
                                self.bump();
                                self.bump();
                                depth += 1;
                            }
                            Some(b'*') if self.peek_at(1) == Some(b'/') => {
                                self.bump();
                                self.bump();
                                depth -= 1;
                            }
                            Some(_) => {
                                self.bump();
                            }
                            None => break,
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn read_word(&mut self) -> Token {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == b'_' || c == b'$' || c.is_ascii_alphanumeric() {
                self.bump();
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.input[start..self.pos])
            .unwrap_or("")
            .to_string();
        Token::Word(s)
    }

    fn read_number(&mut self) -> Token {
        let start = self.pos;
        let mut seen_dot = false;
        let mut seen_exp = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.bump();
            } else if c == b'.' && !seen_dot && !seen_exp {
                seen_dot = true;
                self.bump();
            } else if (c == b'e' || c == b'E') && !seen_exp {
                seen_exp = true;
                self.bump();
                if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                    self.bump();
                }
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.input[start..self.pos])
            .unwrap_or("")
            .to_string();
        Token::Number(s)
    }

    fn read_param(&mut self) -> Token {
        self.bump(); // `$`
        let start = self.pos;
        while self.peek().is_some_and(|d| d.is_ascii_digit()) {
            self.bump();
        }
        let digits = std::str::from_utf8(&self.input[start..self.pos]).unwrap_or("0");
        Token::Param(digits.parse::<u32>().unwrap_or(0))
    }

    fn starts_dollar_quote(&self) -> bool {
        if self.peek() != Some(b'$') {
            return false;
        }
        let mut i = 1;
        while let Some(c) = self.peek_at(i) {
            if c == b'$' {
                return true;
            }
            if i == 1 {
                if !(c == b'_' || c.is_ascii_alphabetic()) {
                    return false;
                }
            } else if !(c == b'_' || c.is_ascii_alphanumeric()) {
                return false;
            }
            i += 1;
        }
        false
    }

    fn read_dollar_string(&mut self) -> Result<Token, String> {
        let tag_start = self.pos;
        self.bump(); // opening $
        while self.peek() != Some(b'$') {
            self.bump();
        }
        self.bump(); // closing $ of opening delimiter
        let delimiter = self.input[tag_start..self.pos].to_vec();
        let content_start = self.pos;
        while self.pos + delimiter.len() <= self.input.len() {
            if self.input[self.pos..].starts_with(&delimiter) {
                let bytes = self.input[content_start..self.pos].to_vec();
                self.pos += delimiter.len();
                let s = String::from_utf8(bytes)
                    .map_err(|_| "invalid UTF-8 in dollar-quoted string literal".to_string())?;
                return Ok(Token::StringLit(s));
            }
            self.bump();
        }
        Err("unterminated dollar-quoted string literal".to_string())
    }

    fn read_quoted_ident(&mut self) -> Result<Token, String> {
        self.bump(); // opening quote
        let mut s = String::new();
        loop {
            match self.bump() {
                Some(b'"') => {
                    // Doubled quote is an escaped quote.
                    if self.peek() == Some(b'"') {
                        self.bump();
                        s.push('"');
                    } else {
                        return Ok(Token::QuotedIdent(s));
                    }
                }
                Some(c) => s.push(c as char),
                None => return Err("unterminated quoted identifier".to_string()),
            }
        }
    }

    fn read_string(&mut self) -> Result<Token, String> {
        self.bump(); // opening quote
        let mut bytes = Vec::new();
        loop {
            match self.bump() {
                Some(b'\'') => {
                    // Doubled quote `''` is an escaped single quote.
                    if self.peek() == Some(b'\'') {
                        self.bump();
                        bytes.push(b'\'');
                    } else {
                        let s = String::from_utf8(bytes)
                            .map_err(|_| "invalid UTF-8 in string literal".to_string())?;
                        return Ok(Token::StringLit(s));
                    }
                }
                Some(c) => bytes.push(c),
                None => return Err("unterminated string literal".to_string()),
            }
        }
    }
}
