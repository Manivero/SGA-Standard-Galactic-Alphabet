//! Токены SGA.
//!
//! FUSION-ПРИМЕЧАНИЕ: `Mut` и `Import` — это ДВА разных ключевых слова,
//! сосуществующих одновременно (в одной из родительских веток они были
//! взаимно заменены друг другом — ошибочное решение, см.
//! MIGRATION_REPORT.md). Это возможно без каких-либо конфликтов в
//! кодовом пространстве, потому что `src/sga_alphabet.rs` определяет
//! полный 26-буквенный алфавит (каждая ASCII-буква A-Z -> один SGA
//! Unicode-кодпоинт), а не таблицу с фиксированным числом слотов под
//! конкретные ключевые слова. Ключевое слово — это просто слово,
//! составленное из этих 26 букв и распознанное в `keyword_from_mnemonic`
//! ниже; добавление нового слова не "вытесняет" уже существующее.

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Литералы
    IntLit(i64),
    FloatLit(f64),
    StringLit(String),
    Ident(String),

    // Ключевые слова (распознаются из SGA-кодпоинтов лексером)
    Let,
    Var,
    Const,
    Fn,
    Return,
    If,
    Else,
    While,
    For,
    In,
    True,
    False,
    Struct,
    Print,
    Break,
    Continue,
    And,
    Or,
    Not,
    Nil,
    /// `MUT` — модификатор параметра функции (Ownership/Borrowing,
    /// roadmap-пункт 2). См. `ast::Param::mutable`.
    Mut,
    /// `IMPORT` — статический импорт модуля. См. `ast::Stmt::Import` и
    /// `src/module_resolver.rs`.
    Import,
    /// `MATCH` — pattern-matching выражение (T006, M002). См.
    /// `ast::Expr::Match`, `ast::Pattern`.
    Match,

    // Операторы и пунктуация (обычный ASCII)
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Assign, // =
    Eq,     // ==
    NotEq,  // !=
    Lt,     // <
    Gt,     // >
    LtEq,   // <=
    GtEq,   // >=
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Colon,
    Arrow, // ->
    Dot,

    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub line: usize,
    pub col: usize,
}

/// Границы токена в исходном тексте. Начало (`line`, `col`) совпадает с
/// `Token::line`/`Token::col`; конец (`end_line`, `end_col`) — позиция сразу
/// ПОСЛЕ последнего символа токена (исключающая). Строки и колонки — с
/// единицы, колонка считается в символах Unicode. У токена `Eof` диапазон
/// пуст (`начало == конец`); у строковой константы с переводом строки
/// внутри `end_line` больше `line`. Таблицу границ строит
/// `Lexer::tokenize_with_spans`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenSpan {
    pub line: usize,
    pub col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

/// Таблица ключевых слов: ЕДИНСТВЕННЫЙ источник и для `keyword_from_mnemonic`
/// (лексер), и для `KEYWORD_MNEMONICS` (перечень слов для языкового сервера:
/// подсказки перечисляют слова языка, а не угадывают их, и не могут разойтись
/// с лексером — T018, M007). Новое ключевое слово добавляется ТОЛЬКО сюда.
macro_rules! keyword_table {
    ($($mnemonic:literal => $kind:ident),+ $(,)?) => {
        /// ASCII-мнемоники всех ключевых слов в порядке таблицы. Слово языка —
        /// эта мнемоника, записанная буквами SGA (`sga_alphabet::encode_word`).
        pub const KEYWORD_MNEMONICS: &[&str] = &[$($mnemonic),+];

        /// Таблица соответствия декодированной ASCII-мнемоники SGA-слова -> ключевое слово.
        pub fn keyword_from_mnemonic(word: &str) -> Option<TokenKind> {
            Some(match word {
                $($mnemonic => TokenKind::$kind,)+
                _ => return None,
            })
        }
    };
}

keyword_table! {
    "LET" => Let,
    "VAR" => Var,
    "CONST" => Const,
    "FN" => Fn,
    "RETURN" => Return,
    "IF" => If,
    "ELSE" => Else,
    "WHILE" => While,
    "FOR" => For,
    "IN" => In,
    "TRUE" => True,
    "FALSE" => False,
    "STRUCT" => Struct,
    "PRINT" => Print,
    "BREAK" => Break,
    "CONTINUE" => Continue,
    "AND" => And,
    "OR" => Or,
    "NOT" => Not,
    "NIL" => Nil,
    "MUT" => Mut,
    "IMPORT" => Import,
    "MATCH" => Match,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Эталон: 23 слова в порядке таблицы, записан вручную независимо от неё.
    const GOLDEN: &[&str] = &[
        "LET", "VAR", "CONST", "FN", "RETURN", "IF", "ELSE", "WHILE", "FOR", "IN", "TRUE", "FALSE",
        "STRUCT", "PRINT", "BREAK", "CONTINUE", "AND", "OR", "NOT", "NIL", "MUT", "IMPORT",
        "MATCH",
    ];

    #[test]
    fn keyword_list_matches_the_golden_list_in_order() {
        assert_eq!(KEYWORD_MNEMONICS, GOLDEN);
        assert_eq!(KEYWORD_MNEMONICS.len(), 23);
    }

    #[test]
    fn every_listed_mnemonic_is_a_keyword_and_distinct() {
        for (i, word) in KEYWORD_MNEMONICS.iter().enumerate() {
            assert!(
                keyword_from_mnemonic(word).is_some(),
                "{word} есть в перечне, но лексер его не знает"
            );
            assert!(
                word.chars().all(|c| c.is_ascii_uppercase()),
                "мнемоника — прописные ASCII-буквы: {word}"
            );
            assert!(
                KEYWORD_MNEMONICS.iter().skip(i + 1).all(|o| o != word),
                "повтор {word}"
            );
        }
    }

    #[test]
    fn mnemonics_outside_the_list_are_not_keywords() {
        for word in [
            "", "let", "Let", "XYZ", "LETS", "LE", "FUNC", "FUNCTION", "TRU", "ELIF", "DEF",
            "PRINTLN", "MATCHES", "NULL", "WHILE ", " IF",
        ] {
            assert_eq!(keyword_from_mnemonic(word), None, "{word:?}");
        }
    }

    #[test]
    fn keywords_map_to_their_own_token_kinds() {
        assert_eq!(keyword_from_mnemonic("LET"), Some(TokenKind::Let));
        assert_eq!(keyword_from_mnemonic("MATCH"), Some(TokenKind::Match));
        assert_eq!(keyword_from_mnemonic("IMPORT"), Some(TokenKind::Import));
        assert_eq!(keyword_from_mnemonic("MUT"), Some(TokenKind::Mut));
        assert_eq!(keyword_from_mnemonic("NIL"), Some(TokenKind::Nil));
        // все токены различны: два слова не могут вести в один вид токена
        let kinds: Vec<TokenKind> = KEYWORD_MNEMONICS
            .iter()
            .map(|w| keyword_from_mnemonic(w).expect("известное слово"))
            .collect();
        for (i, kind) in kinds.iter().enumerate() {
            assert!(
                kinds.iter().skip(i + 1).all(|o| o != kind),
                "два слова ведут в {kind:?}"
            );
        }
    }
}
