//! Лексический анализатор SGA.

pub mod token;

use crate::sga_alphabet::{decode_word, is_sga_letter};
use token::{keyword_from_mnemonic, Token, TokenKind};

#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}:{}] ошибка лексера: {}", self.line, self.col, self.message)
    }
}

pub struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Lexer {
            chars: source.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn make(&self, kind: TokenKind, line: usize, col: usize) -> Token {
        Token { kind, line, col }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let (line, col) = (self.line, self.col);
            let c = match self.peek() {
                None => {
                    tokens.push(self.make(TokenKind::Eof, line, col));
                    break;
                }
                Some(c) => c,
            };

            if is_sga_letter(c) {
                tokens.push(self.read_sga_keyword(line, col)?);
                continue;
            }

            if c.is_ascii_digit() {
                tokens.push(self.read_number(line, col)?);
                continue;
            }

            if c == '"' {
                tokens.push(self.read_string(line, col)?);
                continue;
            }

            if c.is_ascii_alphabetic() || c == '_' {
                tokens.push(self.read_ident(line, col));
                continue;
            }

            tokens.push(self.read_operator(line, col)?);
        }
        Ok(tokens)
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.advance();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while self.peek().is_some() && self.peek() != Some('\n') {
                        self.advance();
                    }
                }
                _ => break,
            }
        }
    }

    fn read_sga_keyword(&mut self, line: usize, col: usize) -> Result<Token, LexError> {
        let mut raw = String::new();
        while let Some(c) = self.peek() {
            if is_sga_letter(c) {
                raw.push(c);
                self.advance();
            } else {
                break;
            }
        }
        let mnemonic = decode_word(&raw);
        match keyword_from_mnemonic(&mnemonic) {
            Some(kind) => Ok(self.make(kind, line, col)),
            None => Err(LexError {
                message: format!(
                    "неизвестное SGA-ключевое слово (декодировано как '{}'). Допустимые: LET VAR CONST FN RETURN IF ELSE WHILE FOR IN TRUE FALSE STRUCT PRINT BREAK CONTINUE AND OR NOT NIL MUT IMPORT",
                    mnemonic
                ),
                line,
                col,
            }),
        }
    }

    fn read_number(&mut self, line: usize, col: usize) -> Result<Token, LexError> {
        let mut raw = String::new();
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                raw.push(c);
                self.advance();
            } else if c == '.' && !is_float && self.peek_at(1).map(|c2| c2.is_ascii_digit()).unwrap_or(false) {
                is_float = true;
                raw.push(c);
                self.advance();
            } else {
                break;
            }
        }
        if is_float {
            raw.parse::<f64>()
                .map(|v| self.make(TokenKind::FloatLit(v), line, col))
                .map_err(|_| LexError { message: format!("некорректный float-литерал '{}'", raw), line, col })
        } else {
            raw.parse::<i64>()
                .map(|v| self.make(TokenKind::IntLit(v), line, col))
                .map_err(|_| LexError { message: format!("некорректный int-литерал '{}'", raw), line, col })
        }
    }

    fn read_string(&mut self, line: usize, col: usize) -> Result<Token, LexError> {
        self.advance(); // открывающая "
        let mut s = String::new();
        loop {
            match self.advance() {
                None => {
                    return Err(LexError { message: "незакрытая строка".into(), line, col });
                }
                Some('"') => break,
                Some('\\') => {
                    let esc = self.advance().ok_or_else(|| LexError {
                        message: "незакрытый escape-символ в строке".into(),
                        line,
                        col,
                    })?;
                    s.push(match esc {
                        'n' => '\n',
                        't' => '\t',
                        '"' => '"',
                        '\\' => '\\',
                        other => other,
                    });
                }
                Some(c) => s.push(c),
            }
        }
        Ok(self.make(TokenKind::StringLit(s), line, col))
    }

    fn read_ident(&mut self, line: usize, col: usize) -> Token {
        let mut raw = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                raw.push(c);
                self.advance();
            } else {
                break;
            }
        }
        self.make(TokenKind::Ident(raw), line, col)
    }

    fn read_operator(&mut self, line: usize, col: usize) -> Result<Token, LexError> {
        let c = self.advance().unwrap();
        let kind = match c {
            '+' => TokenKind::Plus,
            '-' => {
                if self.peek() == Some('>') {
                    self.advance();
                    TokenKind::Arrow
                } else {
                    TokenKind::Minus
                }
            }
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '%' => TokenKind::Percent,
            '=' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::Eq
                } else {
                    TokenKind::Assign
                }
            }
            '!' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::NotEq
                } else {
                    return Err(LexError { message: "ожидался '=' после '!'".into(), line, col });
                }
            }
            '<' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::LtEq
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.peek() == Some('=') {
                    self.advance();
                    TokenKind::GtEq
                } else {
                    TokenKind::Gt
                }
            }
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            '[' => TokenKind::LBracket,
            ']' => TokenKind::RBracket,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            ':' => TokenKind::Colon,
            '.' => TokenKind::Dot,
            other => {
                return Err(LexError {
                    message: format!("неожиданный символ '{}' (U+{:04X})", other, other as u32),
                    line,
                    col,
                })
            }
        };
        Ok(self.make(kind, line, col))
    }
}
