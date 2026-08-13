//! Парсер SGA — рекурсивный спуск.
//!
//! FUSION-ПРИМЕЧАНИЕ: объединяет грамматику типов/ownership (`[MUT] имя
//! [: тип]` в параметрах, `-> тип` для возврата, `: тип` в VarDecl) из
//! одной родительской ветки с грамматикой модулей и замыканий (`IMPORT
//! "путь";`, `FN(...) {...}` как выражение) из другой. Обе фичи требуют
//! знать, находимся ли мы на верхнем уровне программы (`top_level`):
//! `IMPORT` и именованный `FN имя(...)` синтаксически допустимы ТОЛЬКО
//! там, см. docs/COMPILER_SPEC.md.

use crate::ast::{BinOp, Expr, MatchArm, Param, Pattern, Stmt, TypeAnnotation, UnOp};
use crate::lexer::token::{Token, TokenKind};

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}:{}] ошибка парсера: {}",
            self.line, self.col, self.message
        )
    }
}

/// Максимальная глубина рекурсии рекурсивного спуска (выражения и
/// вложенные блоки/операторы — см. `parse_expr`/`parse_stmt`).
///
/// БЕЗ ЭТОГО ЛИМИТА: `((((((...1...))))))` с несколькими сотнями
/// вложенных `(` (буквально пара КБ исходного текста) даёт
/// неперехватываемый `fatal runtime error: stack overflow` и `abort()`
/// процесса — НЕ Rust panic, поэтому `catch_unwind` не помогает, а
/// `Result`/`?` в `PResult` тоже не успевает сработать: стек уже
/// переполнен до того, как первый `Err` смог бы всплыть.
///
/// ВАЖНО (найдено и исправлено в T001, см. IMPLEMENTATION_LOG.md):
/// `self.depth` инкрементируется ОДИН раз за вызов `parse_expr`, но один
/// такой вызов — это ~10 вложенных нативных Rust-кадров (полная цепочка
/// precedence-climbing: `parse_expr -> parse_assignment -> parse_or ->
/// parse_and -> parse_equality -> parse_comparison -> parse_term ->
/// parse_factor -> parse_unary -> parse_postfix -> parse_primary ->
/// [ветка LParen] -> parse_expr`). То есть один "уровень" этого счётчика
/// стоит ЗАМЕТНО дороже нативного стека, чем один уровень `max_call_depth`
/// у VM (`vm/mod.rs`) — старое значение `256`, выбранное "симметрично" с
/// `max_call_depth=200` БЕЗ отдельного эмпирического измерения именно
/// этой цепочки, было ошибкой: оно давало реальный `abort()` ДО того, как
/// проверка `self.depth > MAX_PARSE_DEPTH` успевала сработать, при
/// разумных размерах стека.
///
/// Эмпирически измерено (debug-сборка, x86_64 Linux, той же методологией,
/// что и `Vm::max_call_depth` — см. `docs/SECURITY.md`; инструмент
/// измерения — временный, вне репозитория, результаты см. в
/// IMPLEMENTATION_LOG.md → TASK T001): порог реального переполнения по
/// количеству вложенных `(` (= количество вызовов `parse_expr`):
///
/// | Размер стека потока | Порог переполнения |
/// |---|---|
/// | 1 МиБ | между 50 и 60 |
/// | 2 МиБ (дефолтный размер стека НЕ-главного потока в Rust, если не задан явно `stack_size`/`RUST_MIN_STACK` — именно такой поток даёт тестовый харнесс `cargo test` каждому `#[test]`, что и вызывало реальный крах) | между 104 и 108 |
/// | 8 МиБ (типичный `ulimit -s` главного потока на Linux) | между 420 и 440 |
///
/// `80` выбрано осознанно между двумя границами: (а) достаточно ВЫШЕ
/// потребности легитimных программ — `test_moderately_nested_parens_still_parses_successfully`
/// требует пика глубины ~51 (50 вложенных `(` + 1 уровень на охватывающий
/// `LET`-statement), запас ~29 уровней; (б) достаточно НИЖЕ порога
/// переполнения даже на маленьком 2‑МиБ стеке (порог 104–108) — запас
/// ~25 уровней (~24%), сознательно не "впритык", т.к. в реальном
/// встраивании часть стека потока уже может быть занята вызывающим кодом
/// до вызова парсера. Как и для `max_call_depth`, эта защита — не
/// единственная линия обороны: встраивающим SGA в собственный сервис
/// по-прежнему рекомендуется запускать непроверенный код в потоке с явно
/// заданным, заведомо щедрым `stack_size` (см. `docs/SECURITY.md`).
const MAX_PARSE_DEPTH: usize = 80;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Текущая глубина рекурсии `parse_expr`/`parse_stmt` (общий
    /// счётчик для обоих — стек растёт от обоих видов рекурсии вместе,
    /// поэтому лимитировать их по отдельности было бы недостаточно).
    depth: usize,
}

type PResult<T> = Result<T, ParseError>;

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            depth: 0,
        }
    }

    /// Входная точка в рекурсию с проверкой глубины. Увеличивает
    /// `self.depth`, выполняет `f`, затем гарантированно уменьшает
    /// обратно (в том числе на пути ошибки). Без RAII-guard, потому что
    /// держать `&mut self.depth` живым одновременно с `&mut self`,
    /// нужным внутри `f`, не проходит borrow checker — вместо этого
    /// декремент выполняется явно после вызова `f`, последовательно.
    fn guarded_recursion<T>(
        &mut self,
        what: &str,
        f: impl FnOnce(&mut Self) -> PResult<T>,
    ) -> PResult<T> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            self.depth -= 1;
            return Err(self.err(&format!(
                "превышена максимальная глубина вложенности {} ({}) — защита от stack overflow",
                what, MAX_PARSE_DEPTH
            )));
        }
        let result = f(self);
        self.depth -= 1;
        result
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn check(&self, kind: &TokenKind) -> bool {
        self.peek_kind() == kind
    }

    fn err(&self, msg: &str) -> ParseError {
        ParseError {
            message: msg.into(),
            line: self.peek().line,
            col: self.peek().col,
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> PResult<Token> {
        if self.check(&kind) {
            Ok(self.advance())
        } else {
            Err(self.err(&format!(
                "ожидался {}, получено {:?}",
                what,
                self.peek_kind()
            )))
        }
    }

    fn match_kind(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Лукахед на один токен вперёд: текущий токен — `Dot`, проверяет,
    /// является ли СЛЕДУЮЩИЙ токен тоже `Dot` (то есть мы стоим на первом
    /// `.` диапазона `..`, а не на одиночном `.` доступа к полю). См.
    /// `parse_postfix`, ветка `TokenKind::Dot`, для объяснения, почему
    /// это необходимо.
    fn next_is_dot(&self) -> bool {
        matches!(
            self.tokens.get(self.pos + 1).map(|t| &t.kind),
            Some(TokenKind::Dot)
        )
    }

    /// Лукахед: текущая позиция — на `{`, и непосредственно перед ней —
    /// `Ident` (вызывающая сторона уже проверила это через `check`).
    /// Определяет, является ли конструкция struct-литералом
    /// (`TypeName { field: expr }`) — пустой `{}` или `Ident :` сразу
    /// после `{`. Перенесено из ветки B без изменений.
    fn is_struct_lit_lookahead(&self) -> bool {
        match self.tokens.get(self.pos + 1).map(|t| &t.kind) {
            Some(TokenKind::RBrace) => true,
            Some(TokenKind::Ident(_)) => {
                matches!(
                    self.tokens.get(self.pos + 2).map(|t| &t.kind),
                    Some(TokenKind::Colon)
                )
            }
            _ => false,
        }
    }

    pub fn parse_program(&mut self) -> PResult<Vec<Stmt>> {
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::Eof) {
            stmts.push(self.parse_stmt(true)?);
        }
        Ok(stmts)
    }

    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(TokenKind::LBrace, "'{'")?;
        let mut stmts = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            stmts.push(self.parse_stmt(false)?);
        }
        self.expect(TokenKind::RBrace, "'}'")?;
        Ok(stmts)
    }

    fn parse_stmt(&mut self, top_level: bool) -> PResult<Stmt> {
        self.guarded_recursion("блоков/операторов", |p| {
            p.parse_stmt_inner(top_level)
        })
    }

    /// `top_level=true` только для statement'ов непосредственно в теле
    /// программы (`parse_program`). Внутри любого блока (`if`/`while`/
    /// `for`/`fn`-тело) — `top_level=false`. Используется, чтобы явно
    /// запретить вложенные `FN`/`IMPORT` синтаксически, вместо того
    /// чтобы давать им молча пройти semantic-анализ (см.
    /// docs/COMPILER_SPEC.md — функции и импорты только верхнего уровня).
    fn parse_stmt_inner(&mut self, top_level: bool) -> PResult<Stmt> {
        match self.peek_kind().clone() {
            TokenKind::Let => {
                self.advance();
                self.parse_var_decl(true)
            }
            TokenKind::Var => {
                self.advance();
                self.parse_var_decl(false)
            }
            TokenKind::Const => {
                self.advance();
                self.parse_var_decl(true)
            }
            TokenKind::Print => {
                self.advance();
                self.expect(TokenKind::LParen, "'('")?;
                let mut args = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    args.push(self.parse_expr()?);
                    while self.match_kind(&TokenKind::Comma) {
                        args.push(self.parse_expr()?);
                    }
                }
                self.expect(TokenKind::RParen, "')'")?;
                self.match_kind(&TokenKind::Semicolon);
                Ok(Stmt::Print(args))
            }
            TokenKind::If => {
                self.advance();
                let cond = self.parse_expr()?;
                let then_branch = self.parse_block()?;
                let else_branch = if self.match_kind(&TokenKind::Else) {
                    if self.check(&TokenKind::If) {
                        Some(vec![self.parse_stmt(top_level)?])
                    } else {
                        Some(self.parse_block()?)
                    }
                } else {
                    None
                };
                Ok(Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                })
            }
            TokenKind::While => {
                self.advance();
                let cond = self.parse_expr()?;
                let body = self.parse_block()?;
                Ok(Stmt::While { cond, body })
            }
            TokenKind::For => {
                self.advance();
                let var = self.parse_ident_name()?;
                self.expect(TokenKind::In, "'IN' (SGA)")?;
                let start = self.parse_expr()?;
                self.expect(TokenKind::Dot, "'.'")?;
                self.expect(TokenKind::Dot, "'.' (диапазон '..')")?;
                let end = self.parse_expr()?;
                let body = self.parse_block()?;
                Ok(Stmt::ForIn {
                    var,
                    start,
                    end,
                    body,
                })
            }
            TokenKind::Struct => {
                // FUSION: перенесено из родительской ветки B — в ветке A
                // токен Struct был объявлен в лексере, но не имел
                // парсера (мёртвая фича). Top-level-only — как Import и
                // именованный FN, по тем же причинам (см. комментарий
                // выше у TokenKind::Fn).
                if !top_level {
                    return Err(self.err("STRUCT допустим только на верхнем уровне программы"));
                }
                self.advance();
                let name = self.parse_ident_name()?;
                self.expect(TokenKind::LBrace, "'{'")?;
                let mut fields = Vec::new();
                if !self.check(&TokenKind::RBrace) {
                    fields.push(self.parse_ident_name()?);
                    while self.match_kind(&TokenKind::Comma) {
                        if self.check(&TokenKind::RBrace) {
                            break; // trailing comma
                        }
                        fields.push(self.parse_ident_name()?);
                    }
                }
                self.expect(TokenKind::RBrace, "'}'")?;
                Ok(Stmt::StructDecl { name, fields })
            }
            TokenKind::Import => {
                if !top_level {
                    return Err(self.err(
                        "IMPORT допустим только на верхнем уровне программы (как и именованный FN) — импорт внутри блока/функции не поддерживается",
                    ));
                }
                self.advance();
                let path = match self.peek_kind().clone() {
                    TokenKind::StringLit(s) => {
                        self.advance();
                        s
                    }
                    other => {
                        return Err(self.err(&format!(
                            "ожидался строковый литерал с путём после IMPORT, получено {:?}",
                            other
                        )))
                    }
                };
                self.match_kind(&TokenKind::Semicolon);
                Ok(Stmt::Import(path))
            }
            TokenKind::Fn => {
                // Неоднозначность грамматики: `FN` начинает ОБЕ формы —
                // именованное top-level объявление (`FN name(...) {...}`,
                // даёт `Stmt::FnDecl`) И анонимную функцию-выражение
                // (`FN(...) {...}`, даёт `Expr::Lambda` через
                // `parse_primary`). Различаем по тому, идёт ли сразу за
                // `FN` идентификатор или сразу `(` — оба токена видны
                // через прямой доступ `self.tokens[self.pos + 1]`, без
                // backtracking.
                //
                // Без этой развилки `FN(x) { print(x); }(5);` как
                // statement верхнего уровня (IIFE) безусловно проваливался
                // в ветку именованной формы и падал с ParseError на
                // токене `(` — грамматическая дыра, найденная и закрытая
                // здесь.
                let next_is_ident = matches!(
                    self.tokens.get(self.pos + 1).map(|t| &t.kind),
                    Some(TokenKind::Ident(_))
                );
                if next_is_ident {
                    if !top_level {
                        return Err(self.err(
                            "вложенные объявления функций (FN имя(...)) не поддерживаются — функции объявляются только на верхнем уровне программы; для анонимной функции внутри блока используйте 'FN(...) { ... }' без имени",
                        ));
                    }
                    self.advance();
                    let name = self.parse_ident_name()?;
                    self.expect(TokenKind::LParen, "'('")?;
                    let mut params = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        params.push(self.parse_param()?);
                        while self.match_kind(&TokenKind::Comma) {
                            params.push(self.parse_param()?);
                        }
                    }
                    self.expect(TokenKind::RParen, "')'")?;
                    let return_ty = if self.match_kind(&TokenKind::Arrow) {
                        Some(self.parse_type_name()?)
                    } else {
                        None
                    };
                    let body = self.parse_block()?;
                    Ok(Stmt::FnDecl {
                        name,
                        params,
                        body,
                        return_ty,
                    })
                } else {
                    // Анонимная форма как statement — делегируем в
                    // обычный expression-parsing (parse_primary уже умеет
                    // `FN(...) {...}`), результат оборачиваем как обычный
                    // `ExprStmt` (тот же путь, что и любое другое
                    // выражение, использованное как самостоятельный
                    // statement, например голый вызов функции `f();`).
                    let expr = self.parse_expr()?;
                    self.match_kind(&TokenKind::Semicolon);
                    Ok(Stmt::ExprStmt(expr))
                }
            }
            TokenKind::Return => {
                self.advance();
                let value = if self.check(&TokenKind::Semicolon) || self.check(&TokenKind::RBrace) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                self.match_kind(&TokenKind::Semicolon);
                Ok(Stmt::Return(value))
            }
            TokenKind::Break => {
                self.advance();
                self.match_kind(&TokenKind::Semicolon);
                Ok(Stmt::Break)
            }
            TokenKind::Continue => {
                self.advance();
                self.match_kind(&TokenKind::Semicolon);
                Ok(Stmt::Continue)
            }
            TokenKind::LBrace => Ok(Stmt::Block(self.parse_block()?)),
            _ => {
                let expr = self.parse_expr()?;
                self.match_kind(&TokenKind::Semicolon);
                Ok(Stmt::ExprStmt(expr))
            }
        }
    }

    fn parse_var_decl(&mut self, mutable_is_false: bool) -> PResult<Stmt> {
        // mutable_is_false=true означает immutable (LET/CONST), false означает VAR (mutable)
        let name = self.parse_ident_name()?;
        let ty = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_type_name()?)
        } else {
            None
        };
        self.expect(TokenKind::Assign, "'='")?;
        let value = self.parse_expr()?;
        self.match_kind(&TokenKind::Semicolon);
        Ok(Stmt::VarDecl {
            name,
            value,
            mutable: !mutable_is_false,
            ty,
        })
    }

    fn parse_ident_name(&mut self) -> PResult<String> {
        match self.peek_kind().clone() {
            TokenKind::Ident(s) => {
                self.advance();
                Ok(s)
            }
            other => Err(self.err(&format!("ожидался идентификатор, получено {:?}", other))),
        }
    }

    /// Параметр именованной top-level функции: `[MUT] имя [: тип]`.
    /// Параметры анонимных функций/замыканий (`Expr::Lambda`) НЕ
    /// проходят через этот метод — см. `parse_primary`, ветка
    /// `TokenKind::Fn`, и комментарий у `ast::Expr::Lambda` о том, почему
    /// MUT/типы для замыканий не поддерживаются в v0.1.
    fn parse_param(&mut self) -> PResult<Param> {
        let mutable = self.match_kind(&TokenKind::Mut);
        let name = self.parse_ident_name()?;
        let ty = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_type_name()?)
        } else {
            None
        };
        Ok(Param { name, ty, mutable })
    }

    /// Имя типа в позиции аннотации (после `:` или `->`). Имена типов —
    /// обычные ASCII-идентификаторы (`int`/`float`/`bool`/`string`/
    /// `array`/`any`/`closure`), НЕ новые SGA-ключевые слова — осознанное
    /// решение, чтобы не трогать лексер/алфавит/расширение VS Code для
    /// этой фичи (см. docs/LANGUAGE_SPEC.md, §7). Исключение — `NIL`:
    /// тип "nil" представлен уже существующим SGA-токеном `Nil`, а не
    /// ASCII-словом.
    fn parse_type_name(&mut self) -> PResult<TypeAnnotation> {
        match self.peek_kind().clone() {
            TokenKind::Nil => {
                self.advance();
                Ok(TypeAnnotation::Nil)
            }
            TokenKind::Ident(s) => {
                self.advance();
                match s.to_ascii_lowercase().as_str() {
                    "int" => Ok(TypeAnnotation::Int),
                    "float" => Ok(TypeAnnotation::Float),
                    "bool" => Ok(TypeAnnotation::Bool),
                    "string" => Ok(TypeAnnotation::String),
                    "array" => Ok(TypeAnnotation::Array),
                    "any" => Ok(TypeAnnotation::Any),
                    "closure" => Ok(TypeAnnotation::Closure),
                    other => Err(self.err(&format!(
                        "неизвестное имя типа '{}'. Допустимые: int, float, bool, string, array, closure, any, nil",
                        other
                    ))),
                }
            }
            other => Err(self.err(&format!("ожидалось имя типа, получено {:?}", other))),
        }
    }

    // --- выражения, по приоритетам ---

    fn parse_expr(&mut self) -> PResult<Expr> {
        self.guarded_recursion("выражения", |p| p.parse_assignment())
    }

    fn parse_assignment(&mut self) -> PResult<Expr> {
        let expr = self.parse_or()?;
        if self.check(&TokenKind::Assign) {
            self.advance();
            let value = self.parse_assignment()?;
            return match expr {
                Expr::Ident(name) => Ok(Expr::Assign(name, Box::new(value))),
                Expr::Index(target, idx) => Ok(Expr::IndexAssign(target, idx, Box::new(value))),
                Expr::FieldAccess(obj, field) => Ok(Expr::FieldAssign(obj, field, Box::new(value))),
                _ => Err(self.err("недопустимая левая часть присваивания (ожидалось имя переменной, индексное выражение или доступ к полю)")),
            };
        }
        Ok(expr)
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let mut left = self.parse_and()?;
        while self.check(&TokenKind::Or) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let mut left = self.parse_equality()?;
        while self.check(&TokenKind::And) {
            self.advance();
            let right = self.parse_equality()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> PResult<Expr> {
        let mut left = self.parse_comparison()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Eq => BinOp::Eq,
                TokenKind::NotEq => BinOp::NotEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let mut left = self.parse_term()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Lt => BinOp::Lt,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::LtEq => BinOp::LtEq,
                TokenKind::GtEq => BinOp::GtEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_term()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> PResult<Expr> {
        let mut left = self.parse_factor()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_factor()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_factor(&mut self) -> PResult<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        match self.peek_kind() {
            TokenKind::Minus => {
                self.advance();
                Ok(Expr::Unary(UnOp::Neg, Box::new(self.parse_unary()?)))
            }
            TokenKind::Not => {
                self.advance();
                Ok(Expr::Unary(UnOp::Not, Box::new(self.parse_unary()?)))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.check(&TokenKind::LBracket) {
                self.advance();
                let idx = self.parse_expr()?;
                self.expect(TokenKind::RBracket, "']'")?;
                expr = Expr::Index(Box::new(expr), Box::new(idx));
            } else if self.check(&TokenKind::Dot) && !self.next_is_dot() {
                // FUSION/НАЙДЕНО ПРИ СЛИЯНИИ (см. MIGRATION_REPORT.md):
                // доступ к полю/вызов метода struct перенесён из ветки B,
                // где `Dot` безусловно запускал FieldAccess в этом месте
                // грамматики. Это РЕГРЕССИЯ для `FOR var IN a..b { ... }`
                // (общая для A и B фича, см. parse_stmt::TokenKind::For):
                // `b..b` начинается с того же `Dot`, и без лукахеда здесь
                // `parse_expr()` для границы диапазона `start` жадно
                // потреблял бы первый `.` как начало FieldAccess, а затем
                // падал на втором `.`, ожидая идентификатор поля. Баг
                // присутствовал в исходном коде ветки B (там `parse_postfix`
                // и `Stmt::For` обе существуют, но никогда не
                // тестировались вместе — собственный тестовый набор ветки
                // B не компилировался, см. MIGRATION_REPORT.md), и был бы
                // незаметно перенесён сюда без этой проверки. Различаем:
                // одиночный `.` (за которым НЕ следует второй `.`) — это
                // FieldAccess; `..` (два `Dot` подряд) — оставляем
                // нетронутым для `Stmt::For`, которое потребляет оба через
                // явный `expect(TokenKind::Dot)` дважды.
                self.advance();
                let field = self.parse_ident_name()?;
                if self.check(&TokenKind::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        args.push(self.parse_expr()?);
                        while self.match_kind(&TokenKind::Comma) {
                            args.push(self.parse_expr()?);
                        }
                    }
                    self.expect(TokenKind::RParen, "')'")?;
                    expr = Expr::MethodCall(Box::new(expr), field, args);
                } else {
                    expr = Expr::FieldAccess(Box::new(expr), field);
                }
            } else if self.check(&TokenKind::LParen) {
                // Общий постфиксный вызов: применяется к ЛЮБОМУ
                // выражению, полученному на данный момент — литералу
                // замыкания (IIFE), результату индексации (`fns[0]()`),
                // результату предыдущего вызова (`f()()`) и т.п. Вызов
                // по статическому ИМЕНИ (`Ident` сразу за которым `(`)
                // уже полностью разобран раньше, в `parse_primary`
                // (даёт `Expr::Call`, не доходя до этой ветки с тем же
                // `(` сразу после простого идентификатора) — сюда
                // попадает только всё остальное, поэтому используется
                // отдельный AST-вариант `Expr::CallExpr`, а не `Call`,
                // у которого callee — `String`, а не произвольное
                // выражение. См. docstring у `ast::Expr::CallExpr`.
                self.advance();
                let mut args = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    args.push(self.parse_expr()?);
                    while self.match_kind(&TokenKind::Comma) {
                        args.push(self.parse_expr()?);
                    }
                }
                self.expect(TokenKind::RParen, "')'")?;
                expr = Expr::CallExpr(Box::new(expr), args);
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        match self.peek_kind().clone() {
            TokenKind::IntLit(v) => {
                self.advance();
                Ok(Expr::Int(v))
            }
            TokenKind::FloatLit(v) => {
                self.advance();
                Ok(Expr::Float(v))
            }
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            TokenKind::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            TokenKind::Nil => {
                self.advance();
                Ok(Expr::Nil)
            }
            TokenKind::Fn => {
                // FN(params) { body } как ВЫРАЖЕНИЕ (анонимная функция /
                // замыкание) — отличается от Stmt::FnDecl ("FN имя(...)
                // {...}"), которая обрабатывается раньше, в parse_stmt, и
                // сюда, в parse_primary, никогда не доходит (см. развилку
                // `next_is_ident` там). Здесь FN всегда сразу следует за
                // "(" (без имени). Параметры лямбды — простые имена, без
                // MUT/типов (см. ast::Expr::Lambda).
                self.advance();
                self.expect(
                    TokenKind::LParen,
                    "'(' (анонимная функция: 'FN(параметры) { тело }', без имени)",
                )?;
                let mut params = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    params.push(self.parse_ident_name()?);
                    while self.match_kind(&TokenKind::Comma) {
                        params.push(self.parse_ident_name()?);
                    }
                }
                self.expect(TokenKind::RParen, "')'")?;
                if self.match_kind(&TokenKind::Arrow) {
                    self.parse_type_name()?; // тип возврата лямбды — парсится и отбрасывается (см. ROADMAP)
                }
                let body = self.parse_block()?;
                Ok(Expr::Lambda { params, body })
            }
            TokenKind::Match => {
                // MATCH scrutinee { паттерн -> выражение, ... } — T006
                // (M002). Выражение (не statement), как и Lambda/FN.
                // Обязательность catch-all последним пунктом парсер НЕ
                // проверяет (только форму паттернов) — это делает
                // semantic::Analyzer, см. ast::Pattern/ast::MatchArm.
                self.advance();
                let scrutinee = self.parse_expr()?;
                self.expect(TokenKind::LBrace, "'{' (после выражения в MATCH)")?;
                let mut arms = Vec::new();
                if !self.check(&TokenKind::RBrace) {
                    arms.push(self.parse_match_arm()?);
                    while self.match_kind(&TokenKind::Comma) {
                        if self.check(&TokenKind::RBrace) {
                            break;
                        }
                        arms.push(self.parse_match_arm()?);
                    }
                }
                self.expect(TokenKind::RBrace, "'}' (конец MATCH)")?;
                Ok(Expr::Match(Box::new(scrutinee), arms))
            }
            TokenKind::Ident(name) => {
                self.advance();
                if self.check(&TokenKind::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        args.push(self.parse_expr()?);
                        while self.match_kind(&TokenKind::Comma) {
                            args.push(self.parse_expr()?);
                        }
                    }
                    self.expect(TokenKind::RParen, "')'")?;
                    Ok(Expr::Call(name, args))
                } else if self.check(&TokenKind::LBrace) && self.is_struct_lit_lookahead() {
                    // TypeName { field: expr, ... } — struct-литерал.
                    // Перенесено из ветки B. Лукахед (`is_struct_lit_lookahead`)
                    // нужен, чтобы не путать с `Ident` сразу перед блоком
                    // `{...}` где-то ещё в грамматике — в текущей грамматике
                    // SGA такого места нет (блок `{}` как самостоятельное
                    // выражение не существует), но лукахед сохранён из
                    // ветки B как защита на будущее и для пустого литерала
                    // `TypeName {}` (см. правило ниже).
                    self.advance();
                    let mut fields = Vec::new();
                    if !self.check(&TokenKind::RBrace) {
                        let field_name = self.parse_ident_name()?;
                        self.expect(TokenKind::Colon, "':' (в литерале struct: field: value)")?;
                        let field_val = self.parse_expr()?;
                        fields.push((field_name, field_val));
                        while self.match_kind(&TokenKind::Comma) {
                            if self.check(&TokenKind::RBrace) {
                                break;
                            }
                            let field_name = self.parse_ident_name()?;
                            self.expect(TokenKind::Colon, "':'")?;
                            let field_val = self.parse_expr()?;
                            fields.push((field_name, field_val));
                        }
                    }
                    self.expect(TokenKind::RBrace, "'}'")?;
                    Ok(Expr::StructLit {
                        type_name: name,
                        fields,
                    })
                } else {
                    Ok(Expr::Ident(name))
                }
            }
            TokenKind::LBracket => {
                self.advance();
                let mut items = Vec::new();
                if !self.check(&TokenKind::RBracket) {
                    items.push(self.parse_expr()?);
                    while self.match_kind(&TokenKind::Comma) {
                        items.push(self.parse_expr()?);
                    }
                }
                self.expect(TokenKind::RBracket, "']'")?;
                Ok(Expr::Array(items))
            }
            TokenKind::LParen => {
                self.advance();
                let e = self.parse_expr()?;
                self.expect(TokenKind::RParen, "')'")?;
                Ok(e)
            }
            other => Err(self.err(&format!("неожиданный токен в выражении: {:?}", other))),
        }
    }

    /// Один пункт MATCH: `паттерн -> выражение`. См. `parse_pattern`.
    fn parse_match_arm(&mut self) -> PResult<MatchArm> {
        let pattern = self.parse_pattern()?;
        self.expect(TokenKind::Arrow, "'->' (после паттерна в MATCH)")?;
        let body = self.parse_expr()?;
        Ok(MatchArm { pattern, body })
    }

    /// Паттерн в v0.1 — только литералы (включая отрицательные числа
    /// через unary `-`, т.к. лексер не сворачивает их в единый токен) и
    /// два вида catch-all: `_` (wildcard, не связывает имя) и простой
    /// идентификатор (bind — связывает значение scrutinee с этим именем,
    /// проверяется в `semantic::Analyzer`). Никакой структурной
    /// struct-деструктуризации в v0.1 (см. docs/ROADMAP.md).
    fn parse_pattern(&mut self) -> PResult<Pattern> {
        match self.peek_kind().clone() {
            TokenKind::IntLit(v) => {
                self.advance();
                Ok(Pattern::Int(v))
            }
            TokenKind::FloatLit(v) => {
                self.advance();
                Ok(Pattern::Float(v))
            }
            TokenKind::StringLit(s) => {
                self.advance();
                Ok(Pattern::Str(s))
            }
            TokenKind::True => {
                self.advance();
                Ok(Pattern::Bool(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(Pattern::Bool(false))
            }
            TokenKind::Nil => {
                self.advance();
                Ok(Pattern::Nil)
            }
            TokenKind::Minus => {
                self.advance();
                match self.peek_kind().clone() {
                    TokenKind::IntLit(v) => {
                        self.advance();
                        Ok(Pattern::Int(-v))
                    }
                    TokenKind::FloatLit(v) => {
                        self.advance();
                        Ok(Pattern::Float(-v))
                    }
                    other => Err(self.err(&format!(
                        "после '-' в паттерне MATCH ожидалось число, получено {:?}",
                        other
                    ))),
                }
            }
            TokenKind::Ident(name) if name == "_" => {
                self.advance();
                Ok(Pattern::Wildcard)
            }
            TokenKind::Ident(name) => {
                self.advance();
                Ok(Pattern::Bind(name))
            }
            other => Err(self.err(&format!(
                "ожидался паттерн (литерал, '_' или имя-привязка) в MATCH, получено {:?}",
                other
            ))),
        }
    }
}
