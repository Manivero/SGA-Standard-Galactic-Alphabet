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

/// Таблица соответствия декодированной ASCII-мнемоники SGA-слова -> ключевое слово.
pub fn keyword_from_mnemonic(word: &str) -> Option<TokenKind> {
    Some(match word {
        "LET" => TokenKind::Let,
        "VAR" => TokenKind::Var,
        "CONST" => TokenKind::Const,
        "FN" => TokenKind::Fn,
        "RETURN" => TokenKind::Return,
        "IF" => TokenKind::If,
        "ELSE" => TokenKind::Else,
        "WHILE" => TokenKind::While,
        "FOR" => TokenKind::For,
        "IN" => TokenKind::In,
        "TRUE" => TokenKind::True,
        "FALSE" => TokenKind::False,
        "STRUCT" => TokenKind::Struct,
        "PRINT" => TokenKind::Print,
        "BREAK" => TokenKind::Break,
        "CONTINUE" => TokenKind::Continue,
        "AND" => TokenKind::And,
        "OR" => TokenKind::Or,
        "NOT" => TokenKind::Not,
        "NIL" => TokenKind::Nil,
        "MUT" => TokenKind::Mut,
        "IMPORT" => TokenKind::Import,
        _ => return None,
    })
}
