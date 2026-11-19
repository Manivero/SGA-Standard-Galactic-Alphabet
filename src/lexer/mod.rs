//! Лексический анализатор SGA.

pub mod token;

use crate::sga_alphabet::{decode_word, is_sga_letter};
use token::{keyword_from_mnemonic, Token, TokenKind, TokenSpan};

/// Ошибка лексера.
///
/// `line`/`col` — позиция начала проблемного фрагмента (1-based, колонка в
/// символах), `end_line`/`end_col` — позиция лексера В МОМЕНТ ОШИБКИ, то есть
/// сразу после всего, что он успел потребить: после целого слова из
/// SGA-букв, целого числового литерала, одиночного символа; у незакрытой
/// строки — конец текста (он может лежать на более поздней строке, чем
/// начало). Концы используются языковым сервером (`lsp`, T016, M007).
#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}:{}] ошибка лексера: {}",
            self.line, self.col, self.message
        )
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

    /// Строит ошибку, начавшуюся в `(line, col)`; конец — текущая позиция
    /// лексера (см. `LexError`).
    fn error(&self, message: impl Into<String>, line: usize, col: usize) -> LexError {
        LexError {
            message: message.into(),
            line,
            col,
            end_line: self.line,
            end_col: self.col,
        }
    }

    pub fn tokenize(self) -> Result<Vec<Token>, LexError> {
        self.tokenize_with(|_, _, _| {})
    }

    /// То же, что `tokenize`, но дополнительно возвращает границы каждого
    /// токена (по одному `TokenSpan` на токен, включая `Eof`, в том же
    /// порядке). Нужен языковому серверу: диапазон диагностики парсера — это
    /// токен, на котором разбор остановился (T016, M007).
    ///
    /// Границы вынесены в ОТДЕЛЬНУЮ таблицу, а не в поля `Token`: структура
    /// `Token` лежит в кадре каждой функции рекурсивного спуска парсера, и её
    /// рост съедает запас стека отладочной сборки (см. TD-018).
    pub fn tokenize_with_spans(self) -> Result<(Vec<Token>, Vec<TokenSpan>), LexError> {
        let mut spans = Vec::new();
        let tokens = self.tokenize_with(|token, end_line, end_col| {
            spans.push(TokenSpan {
                line: token.line,
                col: token.col,
                end_line,
                end_col,
            });
        })?;
        Ok((tokens, spans))
    }

    /// Общий цикл лексера. `on_token` вызывается для каждого токена сразу
    /// после его потребления с позицией лексера в этот момент — это и есть
    /// (исключающий) конец токена. Для `tokenize` замыкание пустое и
    /// выбрасывается компилятором.
    fn tokenize_with(
        mut self,
        mut on_token: impl FnMut(&Token, usize, usize),
    ) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let (line, col) = (self.line, self.col);
            let c = match self.peek() {
                None => {
                    let token = self.make(TokenKind::Eof, line, col);
                    on_token(&token, self.line, self.col);
                    tokens.push(token);
                    break;
                }
                Some(c) => c,
            };

            let token = if is_sga_letter(c) {
                self.read_sga_keyword(line, col)?
            } else if c.is_ascii_digit() {
                self.read_number(line, col)?
            } else if c == '"' {
                self.read_string(line, col)?
            } else if c.is_ascii_alphabetic() || c == '_' {
                self.read_ident(line, col)
            } else {
                self.read_operator(line, col)?
            };
            on_token(&token, self.line, self.col);
            tokens.push(token);
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
            None => Err(self.error(
                format!(
                    "неизвестное SGA-ключевое слово (декодировано как '{}'). Допустимые: LET VAR CONST FN RETURN IF ELSE WHILE FOR IN TRUE FALSE STRUCT PRINT BREAK CONTINUE AND OR NOT NIL MUT IMPORT",
                    mnemonic
                ),
                line,
                col,
            )),
        }
    }

    fn read_number(&mut self, line: usize, col: usize) -> Result<Token, LexError> {
        let mut raw = String::new();
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                raw.push(c);
                self.advance();
            } else if c == '.'
                && !is_float
                && self
                    .peek_at(1)
                    .map(|c2| c2.is_ascii_digit())
                    .unwrap_or(false)
            {
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
                .map_err(|_| self.error(format!("некорректный float-литерал '{}'", raw), line, col))
        } else {
            raw.parse::<i64>()
                .map(|v| self.make(TokenKind::IntLit(v), line, col))
                .map_err(|_| self.error(format!("некорректный int-литерал '{}'", raw), line, col))
        }
    }

    fn read_string(&mut self, line: usize, col: usize) -> Result<Token, LexError> {
        self.advance(); // открывающая "
        let mut s = String::new();
        loop {
            match self.advance() {
                None => {
                    return Err(self.error("незакрытая строка", line, col));
                }
                Some('"') => break,
                Some('\\') => {
                    let esc = self.advance().ok_or_else(|| {
                        self.error("незакрытый escape-символ в строке", line, col)
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
                    return Err(self.error("ожидался '=' после '!'", line, col));
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
                return Err(self.error(
                    format!("неожиданный символ '{}' (U+{:04X})", other, other as u32),
                    line,
                    col,
                ))
            }
        };
        Ok(self.make(kind, line, col))
    }
}

/// Прямые unit-тесты лексера — в отличие от `tests/integration_test.rs`
/// (сквозной API `run_source`/`run_source_file`), здесь `Lexer` вызывается
/// напрямую, без прохода через parser/semantic/vm. Добавлено в T004 (см.
/// IMPLEMENTATION_LOG.md, TECH_DEBT.md TD-004): до этой задачи лексер был
/// покрыт только косвенно.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sga_alphabet::encode_word as kw;

    /// Токенизирует и разворачивает `Result`, отбрасывая финальный `Eof` —
    /// удобно для тестов, которым нужны только "содержательные" токены.
    fn lex_ok(src: &str) -> Vec<TokenKind> {
        let tokens = Lexer::new(src)
            .tokenize()
            .expect("ожидался успешный лексинг");
        let mut kinds: Vec<TokenKind> = tokens.into_iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds.pop(),
            Some(TokenKind::Eof),
            "последний токен обязан быть Eof"
        );
        kinds
    }

    fn lex_err(src: &str) -> LexError {
        Lexer::new(src).tokenize().expect_err("ожидалась LexError")
    }

    // ── ключевые слова ──────────────────────────────────────────────

    #[test]
    fn test_all_keywords_tokenize_correctly() {
        let pairs: &[(&str, TokenKind)] = &[
            ("LET", TokenKind::Let),
            ("VAR", TokenKind::Var),
            ("CONST", TokenKind::Const),
            ("FN", TokenKind::Fn),
            ("RETURN", TokenKind::Return),
            ("IF", TokenKind::If),
            ("ELSE", TokenKind::Else),
            ("WHILE", TokenKind::While),
            ("FOR", TokenKind::For),
            ("IN", TokenKind::In),
            ("TRUE", TokenKind::True),
            ("FALSE", TokenKind::False),
            ("STRUCT", TokenKind::Struct),
            ("PRINT", TokenKind::Print),
            ("BREAK", TokenKind::Break),
            ("CONTINUE", TokenKind::Continue),
            ("AND", TokenKind::And),
            ("OR", TokenKind::Or),
            ("NOT", TokenKind::Not),
            ("NIL", TokenKind::Nil),
            ("MUT", TokenKind::Mut),
            ("IMPORT", TokenKind::Import),
            ("MATCH", TokenKind::Match),
        ];
        for (mnemonic, expected) in pairs {
            let src = kw(mnemonic);
            let kinds = lex_ok(&src);
            assert_eq!(
                kinds,
                vec![expected.clone()],
                "ключевое слово {mnemonic} должно давать ровно один токен {expected:?}"
            );
        }
    }

    #[test]
    fn test_keywords_are_case_insensitive_in_sga_encoding() {
        // letter_to_sga() приводит к uppercase ДО кодирования — то есть
        // сам факт "регистра" существует только в ASCII-мнемонике на
        // входе encode_word, а не в результирующих SGA-кодпоинтах
        // (у SGA-алфавита нет отдельных заглавных/строчных кодпоинтов).
        assert_eq!(
            kw("let"),
            kw("LET"),
            "encode_word должен игнорировать регистр входа"
        );
        assert_eq!(lex_ok(&kw("let")), vec![TokenKind::Let]);
    }

    // ── идентификаторы ──────────────────────────────────────────────

    #[test]
    fn test_identifiers() {
        for name in [
            "x",
            "foo",
            "_bar",
            "x1",
            "snake_case_name",
            "CamelCase",
            "__",
        ] {
            assert_eq!(
                lex_ok(name),
                vec![TokenKind::Ident(name.to_string())],
                "идентификатор '{name}'"
            );
        }
    }

    #[test]
    fn test_identifier_stops_at_non_alphanumeric() {
        assert_eq!(
            lex_ok("foo+bar"),
            vec![
                TokenKind::Ident("foo".into()),
                TokenKind::Plus,
                TokenKind::Ident("bar".into()),
            ]
        );
    }

    // ── числовые литералы ───────────────────────────────────────────

    #[test]
    fn test_int_literals() {
        for (src, expected) in [("0", 0i64), ("42", 42), ("1234567890", 1234567890)] {
            assert_eq!(
                lex_ok(src),
                vec![TokenKind::IntLit(expected)],
                "int '{src}'"
            );
        }
    }

    #[test]
    fn test_float_literals() {
        for (src, expected) in [("3.25", 3.25f64), ("0.5", 0.5), ("100.001", 100.001)] {
            assert_eq!(
                lex_ok(src),
                vec![TokenKind::FloatLit(expected)],
                "float '{src}'"
            );
        }
    }

    #[test]
    fn test_trailing_dot_without_following_digit_is_not_part_of_float() {
        // read_number трактует '.' как часть числа, только если СЛЕДУЮЩИЙ
        // символ тоже цифра — иначе точка отдаётся отдельным токеном Dot
        // (это соответствует грамматике field-access: `foo.bar` не должно
        // ломаться на "foo" + число).
        assert_eq!(
            lex_ok("1."),
            vec![TokenKind::IntLit(1), TokenKind::Dot],
            "'1.' без цифры после точки — это IntLit(1) + Dot, не float"
        );
    }

    #[test]
    fn test_invalid_int_literal_overflow_is_lex_error() {
        // i64::MAX = 9223372036854775807; на порядок больше -> переполнение
        let err = lex_err("99999999999999999999");
        assert!(
            err.message.contains("int-литерал"),
            "сообщение должно объяснять причину, получено: {}",
            err.message
        );
    }

    // ── строковые литералы ──────────────────────────────────────────

    #[test]
    fn test_string_literal_simple() {
        assert_eq!(
            lex_ok(r#""hello world""#),
            vec![TokenKind::StringLit("hello world".into())]
        );
    }

    #[test]
    fn test_string_literal_empty() {
        assert_eq!(lex_ok(r#""""#), vec![TokenKind::StringLit(String::new())]);
    }

    #[test]
    fn test_string_literal_escapes() {
        // \n \t \" \\ — ровно те 4 escape-последовательности, что
        // реализованы в read_string (остальные символы после '\'
        // передаются как есть, см. match esc { ... other => other }).
        assert_eq!(
            lex_ok(r#""a\nb\tc\"d\\e""#),
            vec![TokenKind::StringLit("a\nb\tc\"d\\e".into())]
        );
    }

    #[test]
    fn test_string_literal_unclosed_is_lex_error() {
        let err = lex_err(r#""начало без конца"#);
        assert!(
            err.message.contains("незакрыт"),
            "ожидалось сообщение о незакрытой строке, получено: {}",
            err.message
        );
        assert_eq!(
            (err.line, err.col),
            (1, 1),
            "позиция должна указывать на открывающую кавычку"
        );
    }

    #[test]
    fn test_string_literal_unclosed_escape_is_lex_error() {
        let err = lex_err(r#""abc\"#);
        assert!(
            err.message.contains("escape"),
            "ожидалось сообщение о незакрытом escape, получено: {}",
            err.message
        );
    }

    // ── операторы и пунктуация ──────────────────────────────────────

    #[test]
    fn test_all_single_and_double_char_operators() {
        let pairs: &[(&str, TokenKind)] = &[
            ("+", TokenKind::Plus),
            ("-", TokenKind::Minus),
            ("*", TokenKind::Star),
            ("/", TokenKind::Slash),
            ("%", TokenKind::Percent),
            ("=", TokenKind::Assign),
            ("==", TokenKind::Eq),
            ("!=", TokenKind::NotEq),
            ("<", TokenKind::Lt),
            ("<=", TokenKind::LtEq),
            (">", TokenKind::Gt),
            (">=", TokenKind::GtEq),
            ("(", TokenKind::LParen),
            (")", TokenKind::RParen),
            ("{", TokenKind::LBrace),
            ("}", TokenKind::RBrace),
            ("[", TokenKind::LBracket),
            ("]", TokenKind::RBracket),
            (",", TokenKind::Comma),
            (";", TokenKind::Semicolon),
            (":", TokenKind::Colon),
            ("->", TokenKind::Arrow),
            (".", TokenKind::Dot),
        ];
        for (src, expected) in pairs {
            assert_eq!(lex_ok(src), vec![expected.clone()], "оператор '{src}'");
        }
    }

    #[test]
    fn test_minus_vs_arrow_disambiguation() {
        assert_eq!(lex_ok("-"), vec![TokenKind::Minus]);
        assert_eq!(lex_ok("->"), vec![TokenKind::Arrow]);
        assert_eq!(
            lex_ok("- >"), // пробел между ними — уже НЕ Arrow
            vec![TokenKind::Minus, TokenKind::Gt]
        );
    }

    #[test]
    fn test_bang_without_eq_is_lex_error() {
        // В SGA нет токена "просто !" — логическое отрицание это
        // ключевое слово NOT; '!' валиден только как первый символ '!='.
        let err = lex_err("!");
        assert!(err.message.contains("!"), "получено: {}", err.message);
    }

    #[test]
    fn test_unexpected_ascii_symbol_is_lex_error_with_position() {
        for bad in ["@", "#", "$", "^", "&", "|", "~", "?"] {
            let err = lex_err(bad);
            assert_eq!((err.line, err.col), (1, 1), "символ '{bad}'");
            assert!(
                err.message.contains(bad),
                "сообщение об ошибке должно называть сам символ '{bad}', получено: {}",
                err.message
            );
        }
    }

    // ── пробелы и комментарии ────────────────────────────────────────

    #[test]
    fn test_whitespace_variants_are_skipped() {
        assert_eq!(
            lex_ok("1  \t\n  +\r\n  2"),
            vec![TokenKind::IntLit(1), TokenKind::Plus, TokenKind::IntLit(2)]
        );
    }

    #[test]
    fn test_line_comment_is_skipped_to_end_of_line() {
        assert_eq!(
            lex_ok("1 // это комментарий с любыми символами @#$\n+ 2"),
            vec![TokenKind::IntLit(1), TokenKind::Plus, TokenKind::IntLit(2)]
        );
    }

    #[test]
    fn test_line_comment_at_very_end_of_source_without_trailing_newline() {
        assert_eq!(
            lex_ok("1 // хвостовой комментарий без \\n в конце"),
            vec![TokenKind::IntLit(1)]
        );
    }

    #[test]
    fn test_single_slash_is_not_confused_with_comment() {
        assert_eq!(
            lex_ok("6 / 2"),
            vec![TokenKind::IntLit(6), TokenKind::Slash, TokenKind::IntLit(2)]
        );
    }

    // ── позиции (line/col) ───────────────────────────────────────────

    #[test]
    fn test_line_and_column_tracking_across_multiple_lines() {
        let tokens = Lexer::new("12\n345\n  6").tokenize().unwrap();
        // "12" начинается на 1:1; "345" на 2:1 (после \n); "6" на 3:3
        // (два пробела перед ним на новой строке).
        assert_eq!((tokens[0].line, tokens[0].col), (1, 1), "12");
        assert_eq!((tokens[1].line, tokens[1].col), (2, 1), "345");
        assert_eq!(
            (tokens[2].line, tokens[2].col),
            (3, 3),
            "6 (после двух пробелов)"
        );
    }

    #[test]
    fn test_unknown_sga_word_is_lex_error_with_correct_position() {
        // Валидная последовательность SGA-букв, декодирующаяся в
        // мнемонику, которой нет в keyword_from_mnemonic (ни один
        // реальный keyword не называется "XYZ").
        let src = format!("  {}", kw("XYZ")); // 2 пробела для ненулевой col
        let err = lex_err(&src);
        assert!(
            err.message.contains("XYZ"),
            "сообщение должно содержать декодированную мнемонику, получено: {}",
            err.message
        );
        assert_eq!(
            (err.line, err.col),
            (1, 3),
            "позиция должна указывать на начало SGA-слова, а не на конец строки"
        );
    }

    // ── EOF и пустой ввод ────────────────────────────────────────────

    #[test]
    fn test_empty_source_produces_only_eof() {
        let tokens = Lexer::new("").tokenize().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Eof);
    }

    #[test]
    fn test_whitespace_only_source_produces_only_eof() {
        let tokens = Lexer::new("   \n\t\n  // только комментарий\n")
            .tokenize()
            .unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Eof);
    }

    #[test]
    fn test_eof_token_always_present_at_end() {
        let tokens = Lexer::new("1 + 1").tokenize().unwrap();
        assert_eq!(tokens.last().unwrap().kind, TokenKind::Eof);
    }

    // ── границы токенов и концы ошибок (T016, M007) ──────────────────

    type Range = ((usize, usize), (usize, usize));

    /// Границы всех токенов `src` (включая `Eof`): `((строка, колонка),
    /// (конечная строка, конечная колонка))`.
    fn spans_of(src: &str) -> Vec<Range> {
        let (_, spans) = Lexer::new(src)
            .tokenize_with_spans()
            .expect("ожидался успешный лексинг");
        spans
            .iter()
            .map(|s| ((s.line, s.col), (s.end_line, s.end_col)))
            .collect()
    }

    /// Диапазон единственного содержательного токена `src` (без `Eof`);
    /// заодно проверяет, что `Eof` лежит ровно в конце токена и пуст.
    fn only_span(src: &str) -> Range {
        let spans = spans_of(src);
        assert_eq!(spans.len(), 2, "ожидался один токен и Eof для {src:?}");
        let (_, token_end) = spans[0];
        assert_eq!(
            spans[1],
            (token_end, token_end),
            "Eof обязан быть пустым и стоять сразу после токена {src:?}"
        );
        spans[0]
    }

    /// Диапазон ошибки лексера: `((строка, колонка), (конечная строка,
    /// конечная колонка))`.
    fn error_range(src: &str) -> Range {
        let e = lex_err(src);
        ((e.line, e.col), (e.end_line, e.end_col))
    }

    #[test]
    fn test_span_of_every_single_token_kind() {
        let cases: Vec<(String, usize)> = vec![
            // слово из SGA-букв (ключевое слово), 3 и 8 символов
            (kw("LET"), 3),
            (kw("CONTINUE"), 8),
            // идентификаторы
            ("abc".to_string(), 3),
            ("_x1".to_string(), 3),
            ("a".to_string(), 1),
            // целое и дробное
            ("7".to_string(), 1),
            ("12345".to_string(), 5),
            ("3.14".to_string(), 4),
            ("0.5".to_string(), 3),
            // строки: пустая, обычная, с escape-последовательностями,
            // с символом вне BMP (один символ в колонках лексера)
            ("\"\"".to_string(), 2),
            ("\"ab\"".to_string(), 4),
            ("\"a\\\"b\"".to_string(), 6),
            ("\"\\n\\t\"".to_string(), 6),
            ("\"😀\"".to_string(), 3),
            // односимвольные операторы и пунктуация
            ("+".to_string(), 1),
            ("-".to_string(), 1),
            ("*".to_string(), 1),
            ("/".to_string(), 1),
            ("%".to_string(), 1),
            ("=".to_string(), 1),
            ("<".to_string(), 1),
            (">".to_string(), 1),
            ("(".to_string(), 1),
            (")".to_string(), 1),
            ("{".to_string(), 1),
            ("}".to_string(), 1),
            ("[".to_string(), 1),
            ("]".to_string(), 1),
            (",".to_string(), 1),
            (";".to_string(), 1),
            (":".to_string(), 1),
            (".".to_string(), 1),
            // двухсимвольные операторы
            ("==".to_string(), 2),
            ("!=".to_string(), 2),
            ("<=".to_string(), 2),
            (">=".to_string(), 2),
            ("->".to_string(), 2),
        ];
        for (src, width) in &cases {
            assert_eq!(
                only_span(src),
                ((1, 1), (1, 1 + width)),
                "источник {src:?}, ширина {width}"
            );
        }
    }

    #[test]
    fn test_span_of_a_multiline_string_ends_on_its_last_line() {
        // Строка занимает три строки: конец — сразу после закрывающей кавычки
        // на третьей строке (`  c"` — четыре символа, колонки 1..4, конец 5).
        assert_eq!(only_span("\"ab\ncd\n  c\""), ((1, 1), (3, 5)));
        // Перевод строки сразу после открывающей кавычки
        assert_eq!(only_span("\"\n\""), ((1, 1), (2, 2)));
    }

    #[test]
    fn test_spans_of_several_tokens_follow_whitespace_comments_and_lines() {
        let spans = spans_of("x = 12; // конец\n  y");
        assert_eq!(
            spans,
            vec![
                ((1, 1), (1, 2)), // x
                ((1, 3), (1, 4)), // =
                ((1, 5), (1, 7)), // 12
                ((1, 7), (1, 8)), // ;
                ((2, 3), (2, 4)), // y
                ((2, 4), (2, 4)), // Eof (пуст)
            ]
        );
    }

    #[test]
    fn test_spans_of_empty_and_blank_sources_are_just_an_empty_eof() {
        assert_eq!(spans_of(""), vec![((1, 1), (1, 1))]);
        // Eof после завершающего перевода строки — на следующей строке
        assert_eq!(spans_of("1\n"), vec![((1, 1), (1, 2)), ((2, 1), (2, 1))]);
        assert_eq!(
            spans_of("  // только комментарий"),
            vec![((1, 24), (1, 24))]
        );
    }

    #[test]
    fn test_adjacent_tokens_have_touching_spans() {
        // Без пробелов конец одного токена — начало следующего.
        assert_eq!(
            spans_of("a<=b"),
            vec![
                ((1, 1), (1, 2)),
                ((1, 2), (1, 4)),
                ((1, 4), (1, 5)),
                ((1, 5), (1, 5)),
            ]
        );
        // Диапазон `..`: число перед точками не «съедает» их
        assert_eq!(
            spans_of("1..3"),
            vec![
                ((1, 1), (1, 2)),
                ((1, 2), (1, 3)),
                ((1, 3), (1, 4)),
                ((1, 4), (1, 5)),
                ((1, 5), (1, 5)),
            ]
        );
    }

    #[test]
    fn test_tokenize_with_spans_returns_the_same_tokens_as_tokenize() {
        let sources = [
            String::new(),
            "x = 1 + 2 * (3 - 4);".to_string(),
            format!("{} f(a, b) {{ {} a >= b; }}", kw("FN"), kw("RETURN")),
            "\"многострочная\nстрока\" != 1.5".to_string(),
        ];
        for src in &sources {
            let plain = Lexer::new(src).tokenize().unwrap();
            let (tokens, spans) = Lexer::new(src).tokenize_with_spans().unwrap();
            assert_eq!(tokens, plain, "токены различаются для {src:?}");
            assert_eq!(spans.len(), tokens.len(), "по одной границе на токен");
        }
    }

    #[test]
    fn test_spans_of_every_example_are_consistent_with_the_tokens() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).expect("каталог examples/") {
            let path = entry.expect("запись каталога").path();
            if path.extension().and_then(|e| e.to_str()) != Some("sga") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("чтение примера");
            let (tokens, spans) = Lexer::new(&src).tokenize_with_spans().unwrap();
            assert_eq!(spans.len(), tokens.len(), "{}", path.display());
            let mut previous_end = (1, 1);
            for (token, span) in tokens.iter().zip(&spans) {
                assert_eq!(
                    (span.line, span.col),
                    (token.line, token.col),
                    "начало границы совпадает с началом токена: {} {token:?}",
                    path.display()
                );
                let start = (span.line, span.col);
                let end = (span.end_line, span.end_col);
                assert!(
                    start >= previous_end,
                    "токены не перекрываются: {} {token:?}",
                    path.display()
                );
                if token.kind == TokenKind::Eof {
                    assert_eq!(start, end, "Eof пуст: {}", path.display());
                } else {
                    assert!(end > start, "токен непуст: {} {token:?}", path.display());
                }
                previous_end = end;
            }
            checked += 1;
        }
        assert!(checked >= 17, "ожидалось не меньше 17 примеров: {checked}");
    }

    #[test]
    fn test_unknown_sga_word_error_ends_after_the_whole_word() {
        assert_eq!(error_range(&kw("XYZ")), ((1, 1), (1, 4)));
        assert_eq!(error_range(&format!("  {} 1", kw("XYZ"))), ((1, 3), (1, 6)));
        // слово из одной буквы
        assert_eq!(error_range(&kw("Q")), ((1, 1), (1, 2)));
    }

    #[test]
    fn test_int_literal_overflow_error_ends_after_the_whole_literal() {
        let twenty_nines = "9".repeat(20);
        assert_eq!(error_range(&twenty_nines), ((1, 1), (1, 21)));
        assert_eq!(
            error_range(&format!("x = {twenty_nines};")),
            ((1, 5), (1, 25))
        );
    }

    #[test]
    fn test_unclosed_string_error_ends_where_the_text_ends() {
        assert_eq!(error_range("\"abc"), ((1, 1), (1, 5)));
        assert_eq!(error_range("x = \"abc"), ((1, 5), (1, 9)));
        // строка тянется на следующие строки: конец — конец текста
        assert_eq!(error_range("\"ab\ncd"), ((1, 1), (2, 3)));
        // пустой остаток после открывающей кавычки
        assert_eq!(error_range("\""), ((1, 1), (1, 2)));
    }

    #[test]
    fn test_unclosed_escape_error_ends_where_the_text_ends() {
        assert_eq!(error_range("\"abc\\"), ((1, 1), (1, 6)));
        assert_eq!(error_range("\"\\"), ((1, 1), (1, 3)));
    }

    #[test]
    fn test_lone_bang_error_covers_exactly_the_bang() {
        assert_eq!(error_range("!"), ((1, 1), (1, 2)));
        assert_eq!(error_range("a !b"), ((1, 3), (1, 4)));
    }

    #[test]
    fn test_unexpected_character_error_covers_exactly_that_character() {
        assert_eq!(error_range("@"), ((1, 1), (1, 2)));
        assert_eq!(error_range("1 + #"), ((1, 5), (1, 6)));
        // символ вне BMP — один символ в колонках лексера
        assert_eq!(error_range("x 😀"), ((1, 3), (1, 4)));
        // NUL и неразрывный пробел не считаются пробелом лексера как NUL
        assert_eq!(error_range("\u{0}"), ((1, 1), (1, 2)));
    }

    #[test]
    fn test_error_end_is_on_the_line_of_a_later_error_token() {
        // Ошибка на второй строке: начало и конец — на ней
        assert_eq!(error_range("ok\n  @"), ((2, 3), (2, 4)));
        assert_eq!(error_range("ok\n\n!"), ((3, 1), (3, 2)));
    }
}
