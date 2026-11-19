//! Парсер SGA — рекурсивный спуск.
//!
//! FUSION-ПРИМЕЧАНИЕ: объединяет грамматику типов/ownership (`[MUT] имя
//! [: тип]` в параметрах, `-> тип` для возврата, `: тип` в VarDecl) из
//! одной родительской ветки с грамматикой модулей и замыканий (`IMPORT
//! "путь";`, `FN(...) {...}` как выражение) из другой. Обе фичи требуют
//! знать, находимся ли мы на верхнем уровне программы (`top_level`):
//! `IMPORT` и именованный `FN имя(...)` синтаксически допустимы ТОЛЬКО
//! там, см. docs/COMPILER_SPEC.md.
//!
//! ПОЗИЦИИ (T017, M007): каждый узел AST несёт `ast::Span` — диапазон ИНДЕКСОВ
//! токенов (не символов). Координат строк/столбцов парсер не знает: их даёт
//! таблица `Lexer::tokenize_with_spans`, параллельная вектору токенов. Правила
//! диапазонов описаны у `ast::Span`; здесь они реализованы одним приёмом:
//! `let start = self.pos` на входе в функцию разбора узла и
//! `self.span_from(start)` после последнего потреблённого токена. `Token` и
//! `ParseError` при этом НЕ менялись (рост полей `Token` съедал запас стека
//! отладочной сборки, см. IMPLEMENTATION_LOG.md, T016/T017).

use crate::ast::{
    BinOp, Expr, ExprKind, MatchArm, Param, Pattern, Span, Stmt, StmtKind, TypeAnnotation, UnOp,
};
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
    /// T017: `true` — узлы получают `Span::NONE` (см. `Parser::detached`).
    detached: bool,
}

type PResult<T> = Result<T, ParseError>;

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser {
            tokens,
            pos: 0,
            depth: 0,
            detached: false,
        }
    }

    /// Парсер, который НЕ ставит позиций: каждый узел получает `Span::NONE`.
    ///
    /// Нужен там, где AST попадает в программу из ДРУГОГО текста, чем тот, по
    /// которому будет строиться таблица позиций: индексы токенов импортируемого
    /// модуля, подставленные в программу главного файла, указывали бы на чужие
    /// токены и давали бы ложные `[строка:столбец]` (см. `parse_full_flat`).
    /// Структура дерева и поведение разбора от режима не зависят — `Span` не
    /// участвует ни в сравнении узлов, ни в кодогенерации.
    pub fn detached(tokens: Vec<Token>) -> Self {
        Parser {
            detached: true,
            ..Parser::new(tokens)
        }
    }

    /// Диапазон токенов `start..=pos-1`: всё, что потреблено с тех пор, как
    /// `self.pos` была равна `start`. Вызывается ПОСЛЕ последнего токена узла.
    /// Ни один узел не потребляет `Eof`, поэтому `pos - 1` — всегда реальный
    /// токен (`advance` на `Eof` не сдвигает `pos`, но и не вызывается).
    fn span_from(&self, start: usize) -> Span {
        if self.detached {
            Span::NONE
        } else {
            Span::new(start, self.pos.saturating_sub(1))
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
        let start = self.pos;
        let kind = self.guarded_recursion("блоков/операторов", |p| {
            p.parse_stmt_inner(top_level)
        })?;
        // Диапазон оператора включает завершающую `;`, если она есть: её
        // потребляет `parse_stmt_inner`.
        Ok(Stmt::new(kind, self.span_from(start)))
    }

    /// `top_level=true` только для statement'ов непосредственно в теле
    /// программы (`parse_program`). Внутри любого блока (`if`/`while`/
    /// `for`/`fn`-тело) — `top_level=false`. Используется, чтобы явно
    /// запретить вложенные `FN`/`IMPORT` синтаксически, вместо того
    /// чтобы давать им молча пройти semantic-анализ (см.
    /// docs/COMPILER_SPEC.md — функции и импорты только верхнего уровня).
    fn parse_stmt_inner(&mut self, top_level: bool) -> PResult<StmtKind> {
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
                let args = self.parse_args()?;
                self.expect(TokenKind::RParen, "')'")?;
                self.match_kind(&TokenKind::Semicolon);
                Ok(StmtKind::Print(args))
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
                Ok(StmtKind::If {
                    cond,
                    then_branch,
                    else_branch,
                })
            }
            TokenKind::While => {
                self.advance();
                let cond = self.parse_expr()?;
                let body = self.parse_block()?;
                Ok(StmtKind::While { cond, body })
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
                Ok(StmtKind::ForIn {
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
                Ok(StmtKind::StructDecl { name, fields })
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
                Ok(StmtKind::Import(path))
            }
            TokenKind::Fn => {
                // Неоднозначность грамматики: `FN` начинает ОБЕ формы —
                // именованное top-level объявление (`FN name(...) {...}`,
                // даёт `StmtKind::FnDecl`) И анонимную функцию-выражение
                // (`FN(...) {...}`, даёт `ExprKind::Lambda` через
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
                    Ok(StmtKind::FnDecl {
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
                    Ok(StmtKind::ExprStmt(expr))
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
                Ok(StmtKind::Return(value))
            }
            TokenKind::Break => {
                self.advance();
                self.match_kind(&TokenKind::Semicolon);
                Ok(StmtKind::Break)
            }
            TokenKind::Continue => {
                self.advance();
                self.match_kind(&TokenKind::Semicolon);
                Ok(StmtKind::Continue)
            }
            TokenKind::LBrace => Ok(StmtKind::Block(self.parse_block()?)),
            _ => {
                let expr = self.parse_expr()?;
                self.match_kind(&TokenKind::Semicolon);
                Ok(StmtKind::ExprStmt(expr))
            }
        }
    }

    fn parse_var_decl(&mut self, mutable_is_false: bool) -> PResult<StmtKind> {
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
        Ok(StmtKind::VarDecl {
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
    /// Параметры анонимных функций/замыканий (`ExprKind::Lambda`) НЕ
    /// проходят через этот метод — см. `parse_primary`, ветка
    /// `TokenKind::Fn`, и комментарий у `ast::ExprKind::Lambda` о том, почему
    /// MUT/типы для замыканий не поддерживаются в v0.1.
    fn parse_param(&mut self) -> PResult<Param> {
        let start = self.pos;
        let mutable = self.match_kind(&TokenKind::Mut);
        let name = self.parse_ident_name()?;
        let ty = if self.match_kind(&TokenKind::Colon) {
            Some(self.parse_type_name()?)
        } else {
            None
        };
        Ok(Param {
            name,
            ty,
            mutable,
            span: self.span_from(start),
        })
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
        let start = self.pos;
        let expr = self.parse_or()?;
        if self.check(&TokenKind::Assign) {
            self.advance();
            // T016 (M007): цепочка `a = b = c = ...` рекурсивна по
            // `parse_assignment`, минуя `parse_expr`, поэтому без
            // собственной защиты глубины тысячи `=` подряд давали
            // `stack overflow` и abort процесса (языковой сервер парсит
            // любой текст редактора и не может на этом падать).
            let value =
                self.guarded_recursion("присваиваний", |p| p.parse_assignment())?;
            // Диапазон присваивания: от первого токена цели (вместе с её
            // скобками) до последнего токена значения.
            let span = self.span_from(start);
            let kind = match expr.kind {
                ExprKind::Ident(name) => ExprKind::Assign(name, Box::new(value)),
                ExprKind::Index(target, idx) => {
                    ExprKind::IndexAssign(target, idx, Box::new(value))
                }
                ExprKind::FieldAccess(obj, field) => {
                    ExprKind::FieldAssign(obj, field, Box::new(value))
                }
                _ => return Err(self.err("недопустимая левая часть присваивания (ожидалось имя переменной, индексное выражение или доступ к полю)")),
            };
            return Ok(Expr::new(kind, span));
        }
        Ok(expr)
    }

    /// Бинарный узел `left op right`. `start` — токен, с которого началось
    /// разбираемое ВЫРАЖЕНИЕ УРОВНЯ (вместе с открывающей скобкой, если левый
    /// операнд взят в скобки); конец — последний потреблённый токен (вместе с
    /// закрывающей скобкой правого операнда).
    fn binary(&self, start: usize, op: BinOp, left: Expr, right: Expr) -> Expr {
        Expr::new(
            ExprKind::Binary(op, Box::new(left), Box::new(right)),
            self.span_from(start),
        )
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let start = self.pos;
        let mut left = self.parse_and()?;
        while self.check(&TokenKind::Or) {
            self.advance();
            let right = self.parse_and()?;
            left = self.binary(start, BinOp::Or, left, right);
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let start = self.pos;
        let mut left = self.parse_equality()?;
        while self.check(&TokenKind::And) {
            self.advance();
            let right = self.parse_equality()?;
            left = self.binary(start, BinOp::And, left, right);
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> PResult<Expr> {
        let start = self.pos;
        let mut left = self.parse_comparison()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Eq => BinOp::Eq,
                TokenKind::NotEq => BinOp::NotEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison()?;
            left = self.binary(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let start = self.pos;
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
            left = self.binary(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> PResult<Expr> {
        let start = self.pos;
        let mut left = self.parse_factor()?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_factor()?;
            left = self.binary(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_factor(&mut self) -> PResult<Expr> {
        let start = self.pos;
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
            left = self.binary(start, op, left, right);
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        match self.peek_kind() {
            TokenKind::Minus | TokenKind::Not => self.parse_prefix_operator(),
            _ => self.parse_postfix(),
        }
    }

    /// Унарный `-` / `NOT` и его операнд.
    ///
    /// T016 (M007): унарные операторы рекурсивны по `parse_unary`, минуя
    /// `parse_expr`, поэтому цепочка `- - - ... x` / `NOT NOT ... x` обязана
    /// проходить через `guarded_recursion` — иначе несколько тысяч операторов
    /// подряд давали `stack overflow` и abort процесса (языковой сервер
    /// парсит любой текст редактора и не может на этом падать).
    ///
    /// Ветка вынесена из `parse_unary` в отдельную функцию намеренно:
    /// `parse_unary` стоит в кадре КАЖДОГО уровня вложенных скобок, а в
    /// отладочной сборке кадр содержит временные значения всех веток сразу.
    /// Тонкий `parse_unary` + отдельная функция для редкой ветки не только
    /// не добавляют стека на пути скобок (см. `MAX_PARSE_DEPTH`), но и
    /// уменьшают его относительно прежней формы с двумя встроенными ветками.
    fn parse_prefix_operator(&mut self) -> PResult<Expr> {
        let start = self.pos;
        let op = if self.check(&TokenKind::Minus) {
            UnOp::Neg
        } else {
            UnOp::Not
        };
        self.advance();
        let operand =
            self.guarded_recursion("унарных операторов", |p| p.parse_unary())?;
        Ok(Expr::new(
            ExprKind::Unary(op, Box::new(operand)),
            self.span_from(start),
        ))
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        // `start` — первый токен ВСЕГО постфиксного выражения (вместе с
        // открывающей скобкой, если операнд взят в скобки): от него считается
        // диапазон каждого `Index`/`FieldAccess`/`MethodCall`/`CallExpr`.
        let start = self.pos;
        let mut expr = self.parse_primary()?;
        loop {
            expr = if self.check(&TokenKind::LBracket) {
                self.parse_index_suffix(expr, start)?
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
                self.parse_member_suffix(expr, start)?
            } else if self.check(&TokenKind::LParen) {
                // Общий постфиксный вызов: применяется к ЛЮБОМУ
                // выражению, полученному на данный момент — литералу
                // замыкания (IIFE), результату индексации (`fns[0]()`),
                // результату предыдущего вызова (`f()()`) и т.п. Вызов
                // по статическому ИМЕНИ (`Ident` сразу за которым `(`)
                // уже полностью разобран раньше, в `parse_primary`
                // (даёт `ExprKind::Call`, не доходя до этой ветки с тем же
                // `(` сразу после простого идентификатора) — сюда
                // попадает только всё остальное, поэтому используется
                // отдельный AST-вариант `ExprKind::CallExpr`, а не `Call`,
                // у которого callee — `String`, а не произвольное
                // выражение. См. docstring у `ast::ExprKind::CallExpr`.
                self.parse_call_suffix(expr, start)?
            } else {
                return Ok(expr);
            };
        }
    }

    // Суффиксы вынесены из тела цикла `parse_postfix` в отдельные функции
    // намеренно: `parse_postfix` стоит в кадре КАЖДОГО уровня вложенных скобок
    // (см. `MAX_PARSE_DEPTH`), а в отладочной сборке кадр содержит временные
    // значения всех веток сразу. Диапазоны узлов добавили бы к этому кадру
    // ещё по два временных значения на ветку.

    /// `target[индекс]`: `[` ещё не потреблена.
    fn parse_index_suffix(&mut self, target: Expr, start: usize) -> PResult<Expr> {
        self.advance();
        let idx = self.parse_expr()?;
        self.expect(TokenKind::RBracket, "']'")?;
        Ok(Expr::new(
            ExprKind::Index(Box::new(target), Box::new(idx)),
            self.span_from(start),
        ))
    }

    /// `object.поле` или `object.метод(аргументы)`: `.` ещё не потреблена.
    fn parse_member_suffix(&mut self, object: Expr, start: usize) -> PResult<Expr> {
        self.advance();
        let field = self.parse_ident_name()?;
        if self.check(&TokenKind::LParen) {
            self.advance();
            let args = self.parse_args()?;
            self.expect(TokenKind::RParen, "')'")?;
            Ok(Expr::new(
                ExprKind::MethodCall(Box::new(object), field, args),
                self.span_from(start),
            ))
        } else {
            Ok(Expr::new(
                ExprKind::FieldAccess(Box::new(object), field),
                self.span_from(start),
            ))
        }
    }

    /// `callee(аргументы)` для произвольного `callee`: `(` ещё не потреблена.
    fn parse_call_suffix(&mut self, callee: Expr, start: usize) -> PResult<Expr> {
        self.advance();
        let args = self.parse_args()?;
        self.expect(TokenKind::RParen, "')'")?;
        Ok(Expr::new(
            ExprKind::CallExpr(Box::new(callee), args),
            self.span_from(start),
        ))
    }

    /// Ноль или больше выражений через запятую — до закрывающей `)`, которую
    /// потребляет вызывающий (аргументы `PRINT`, вызова функции, метода и
    /// произвольного вызова).
    fn parse_args(&mut self) -> PResult<Vec<Expr>> {
        let mut args = Vec::new();
        if !self.check(&TokenKind::RParen) {
            args.push(self.parse_expr()?);
            while self.match_kind(&TokenKind::Comma) {
                args.push(self.parse_expr()?);
            }
        }
        Ok(args)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        // Редкие ветки (`FN`, `MATCH`, имя, `[`) вынесены в отдельные функции
        // по той же причине, что и суффиксы в `parse_postfix`: на пути
        // вложенных скобок `parse_primary` — самый тяжёлый кадр цепочки.
        let start = self.pos;
        let kind = match self.peek_kind().clone() {
            TokenKind::IntLit(v) => ExprKind::Int(v),
            TokenKind::FloatLit(v) => ExprKind::Float(v),
            TokenKind::StringLit(s) => ExprKind::Str(s),
            TokenKind::True => ExprKind::Bool(true),
            TokenKind::False => ExprKind::Bool(false),
            TokenKind::Nil => ExprKind::Nil,
            TokenKind::Fn => return self.parse_lambda(),
            TokenKind::Match => return self.parse_match(),
            TokenKind::Ident(name) => return self.parse_ident_expr(name),
            TokenKind::LBracket => return self.parse_array_literal(),
            TokenKind::LParen => {
                // Скобки собственного узла не создают: результат — внутреннее
                // выражение со своим (узким) диапазоном; скобки входят только
                // в диапазоны ОХВАТЫВАЮЩИХ узлов (через их `start`).
                self.advance();
                let e = self.parse_expr()?;
                self.expect(TokenKind::RParen, "')'")?;
                return Ok(e);
            }
            other => return Err(self.err(&format!("неожиданный токен в выражении: {:?}", other))),
        };
        // Литерал — ровно один токен.
        self.advance();
        Ok(Expr::new(kind, self.span_from(start)))
    }

    /// `FN(params) { body }` как ВЫРАЖЕНИЕ (анонимная функция /
    /// замыкание) — отличается от `StmtKind::FnDecl` ("FN имя(...)
    /// {...}"), которая обрабатывается раньше, в parse_stmt, и
    /// сюда, в parse_primary, никогда не доходит (см. развилку
    /// `next_is_ident` там). Здесь FN всегда сразу следует за
    /// "(" (без имени). Параметры лямбды — простые имена, без
    /// MUT/типов (см. `ast::ExprKind::Lambda`).
    fn parse_lambda(&mut self) -> PResult<Expr> {
        let start = self.pos;
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
        Ok(Expr::new(
            ExprKind::Lambda { params, body },
            self.span_from(start),
        ))
    }

    /// MATCH scrutinee { паттерн -> выражение, ... } — T006
    /// (M002). Выражение (не statement), как и Lambda/FN.
    /// Обязательность catch-all последним пунктом парсер НЕ
    /// проверяет (только форму паттернов) — это делает
    /// semantic::Analyzer, см. ast::Pattern/ast::MatchArm.
    fn parse_match(&mut self) -> PResult<Expr> {
        let start = self.pos;
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
        Ok(Expr::new(
            ExprKind::Match(Box::new(scrutinee), arms),
            self.span_from(start),
        ))
    }

    /// Выражение, начинающееся с имени: переменная, вызов по имени
    /// `имя(аргументы)` или struct-литерал `Имя { поле: значение }`. Токен
    /// имени ещё не потреблён (`name` — его копия).
    fn parse_ident_expr(&mut self, name: String) -> PResult<Expr> {
        let start = self.pos;
        self.advance();
        let kind = if self.check(&TokenKind::LParen) {
            self.advance();
            let args = self.parse_args()?;
            self.expect(TokenKind::RParen, "')'")?;
            ExprKind::Call(name, args)
        } else if self.check(&TokenKind::LBrace) && self.is_struct_lit_lookahead() {
            // TypeName { field: expr, ... } — struct-литерал.
            // Перенесено из ветки B. Лукахед (`is_struct_lit_lookahead`)
            // нужен, чтобы не путать с `Ident` сразу перед блоком
            // `{...}` где-то ещё в грамматике — в текущей грамматике
            // SGA такого места нет (блок `{}` как самостоятельное
            // выражение не существует), но лукахед сохранён из
            // ветки B как защита на будущее и для пустого литерала
            // `TypeName {}` (см. правило ниже).
            let fields = self.parse_struct_lit_fields()?;
            ExprKind::StructLit {
                type_name: name,
                fields,
            }
        } else {
            ExprKind::Ident(name)
        };
        Ok(Expr::new(kind, self.span_from(start)))
    }

    /// Поля struct-литерала: `{ поле: значение, ... }` (с необязательной
    /// запятой в конце). `{` ещё не потреблена.
    fn parse_struct_lit_fields(&mut self) -> PResult<Vec<(String, Expr)>> {
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
        Ok(fields)
    }

    /// Литерал массива `[a, b, c]`: `[` ещё не потреблена.
    fn parse_array_literal(&mut self) -> PResult<Expr> {
        let start = self.pos;
        self.advance();
        let mut items = Vec::new();
        if !self.check(&TokenKind::RBracket) {
            items.push(self.parse_expr()?);
            while self.match_kind(&TokenKind::Comma) {
                items.push(self.parse_expr()?);
            }
        }
        self.expect(TokenKind::RBracket, "']'")?;
        Ok(Expr::new(ExprKind::Array(items), self.span_from(start)))
    }

    /// Один пункт MATCH: `паттерн -> выражение`. См. `parse_pattern`.
    fn parse_match_arm(&mut self) -> PResult<MatchArm> {
        let start = self.pos;
        let pattern = self.parse_pattern()?;
        self.expect(TokenKind::Arrow, "'->' (после паттерна в MATCH)")?;
        let body = self.parse_expr()?;
        Ok(MatchArm {
            pattern,
            body,
            span: self.span_from(start),
        })
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

/// Прямые unit-тесты парсера (до T016 парсер покрывался только сквозными
/// тестами `tests/integration_test.rs`). Здесь — защита глубины для цепочек
/// унарных операторов и присваиваний, добавленная в T016 (M007).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::sga_alphabet::encode_word as kw;

    /// Разбирает `src` в потоке со стеком в 2 МиБ — столько получает поток
    /// тестового харнесса; если защита глубины не сработает, процесс тестов
    /// упадёт на `stack overflow`, а не вернёт `Err`.
    fn parse_on_small_stack(src: String) -> Result<Vec<Stmt>, ParseError> {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let tokens = Lexer::new(&src).tokenize().expect("лексинг");
                Parser::new(tokens).parse_program()
            })
            .expect("поток")
            .join()
            .expect("поток не должен падать (stack overflow?)")
    }

    fn let_value(src: String) -> Result<Expr, ParseError> {
        let program = parse_on_small_stack(src)?;
        match program.into_iter().next() {
            Some(Stmt {
                kind: StmtKind::VarDecl { value, .. },
                ..
            }) => Ok(value),
            other => panic!("ожидалось объявление переменной, получено {other:?}"),
        }
    }

    // Ожидаемые деревья строятся БЕЗ позиций (`unspanned`): равенство узлов
    // структурное и `Span` в нём не участвует (см. `ast::Expr`), а сами
    // диапазоны проверяются отдельно — `tests/ast_spans_test.rs`.

    fn int(v: i64) -> Expr {
        Expr::unspanned(ExprKind::Int(v))
    }

    fn ident(name: &str) -> Expr {
        Expr::unspanned(ExprKind::Ident(name.into()))
    }

    fn neg(inner: Expr) -> Expr {
        Expr::unspanned(ExprKind::Unary(UnOp::Neg, Box::new(inner)))
    }

    fn not(inner: Expr) -> Expr {
        Expr::unspanned(ExprKind::Unary(UnOp::Not, Box::new(inner)))
    }

    fn assign(name: &str, value: Expr) -> Expr {
        Expr::unspanned(ExprKind::Assign(name.into(), Box::new(value)))
    }

    fn expr_stmt(expr: Expr) -> Stmt {
        Stmt::unspanned(StmtKind::ExprStmt(expr))
    }

    // ── структура дерева (поведение без изменений) ───────────────────

    #[test]
    fn unary_operators_nest_to_the_right() {
        let src = format!("{} x = - {} - y;", kw("LET"), kw("NOT"));
        assert_eq!(let_value(src).unwrap(), neg(not(neg(ident("y")))));
        let src = format!("{} x = ----1;", kw("LET"));
        assert_eq!(let_value(src).unwrap(), neg(neg(neg(neg(int(1))))));
    }

    #[test]
    fn unary_binds_tighter_than_binary_operators() {
        let src = format!("{} x = -a + -b;", kw("LET"));
        assert_eq!(
            let_value(src).unwrap(),
            Expr::unspanned(ExprKind::Binary(
                BinOp::Add,
                Box::new(neg(ident("a"))),
                Box::new(neg(ident("b"))),
            ))
        );
    }

    #[test]
    fn assignments_nest_to_the_right() {
        let program = parse_on_small_stack("a = b = 1;".to_string()).unwrap();
        assert_eq!(program, vec![expr_stmt(assign("a", assign("b", int(1))))]);
    }

    #[test]
    fn index_and_field_targets_still_chain() {
        let program = parse_on_small_stack("a[0] = b.c = d = 2;".to_string()).unwrap();
        assert_eq!(
            program,
            vec![expr_stmt(Expr::unspanned(ExprKind::IndexAssign(
                Box::new(ident("a")),
                Box::new(int(0)),
                Box::new(Expr::unspanned(ExprKind::FieldAssign(
                    Box::new(ident("b")),
                    "c".into(),
                    Box::new(assign("d", int(2)))
                )))
            )))]
        );
    }

    #[test]
    fn invalid_assignment_target_is_still_an_error_after_the_value_is_parsed() {
        let err = parse_on_small_stack("1 = 2;".to_string()).unwrap_err();
        assert!(
            err.message.contains("недопустимая левая часть"),
            "{}",
            err.message
        );
    }

    // ── граница защиты глубины ───────────────────────────────────────

    /// `LET x = <n операторов>1;`: охватывающий оператор и `parse_expr`
    /// занимают два уровня, каждый унарный оператор — ещё один.
    #[test]
    fn unary_chain_is_accepted_up_to_exactly_the_depth_limit() {
        let longest_ok = MAX_PARSE_DEPTH - 2;
        for n in [1, 2, 50, longest_ok - 1, longest_ok] {
            let src = format!("{} x = {}1;", kw("LET"), "-".repeat(n));
            assert!(let_value(src).is_ok(), "{n} операторов должны разбираться");
        }
        for n in [longest_ok + 1, longest_ok + 2, 1000] {
            let src = format!("{} x = {}1;", kw("LET"), "-".repeat(n));
            let err = let_value(src).expect_err("цепочка длиннее предела");
            assert!(
                err.message.contains("глубин") && err.message.contains("унарных операторов"),
                "{n} операторов: {}",
                err.message
            );
        }
    }

    #[test]
    fn not_chain_is_limited_like_the_minus_chain() {
        let not_kw = kw("NOT");
        let longest_ok = MAX_PARSE_DEPTH - 2;
        let src = format!(
            "{} x = {}y;",
            kw("LET"),
            format!("{not_kw} ").repeat(longest_ok)
        );
        assert!(let_value(src).is_ok());
        let src = format!(
            "{} x = {}y;",
            kw("LET"),
            format!("{not_kw} ").repeat(longest_ok + 1)
        );
        assert!(let_value(src).is_err());
    }

    #[test]
    fn assignment_chain_is_accepted_up_to_exactly_the_depth_limit() {
        let longest_ok = MAX_PARSE_DEPTH - 2;
        for n in [1, 2, 50, longest_ok - 1, longest_ok] {
            let src = format!("{}1;", "a = ".repeat(n));
            assert!(
                parse_on_small_stack(src).is_ok(),
                "{n} присваиваний должны разбираться"
            );
        }
        for n in [longest_ok + 1, longest_ok + 2, 1000] {
            let src = format!("{}1;", "a = ".repeat(n));
            let err = parse_on_small_stack(src).expect_err("цепочка длиннее предела");
            assert!(
                err.message.contains("глубин") && err.message.contains("присваиваний"),
                "{n} присваиваний: {}",
                err.message
            );
        }
    }

    #[test]
    fn the_depth_error_points_at_a_real_token_of_the_chain() {
        let src = format!("{} x = {}1;", kw("LET"), "-".repeat(500));
        let err = let_value(src).unwrap_err();
        assert_eq!(err.line, 1);
        // `LET x = ` — 8 символов; ошибка — на одном из минусов или на токене
        // после них, но не раньше начала цепочки и не после конца текста
        assert!(
            (9..=9 + 500 + 2).contains(&err.col),
            "колонка ошибки {} вне цепочки",
            err.col
        );
    }

    // ── устойчивость на маленьком стеке ──────────────────────────────

    #[test]
    fn very_long_chains_are_a_parse_error_not_a_stack_overflow() {
        let not_kw = kw("NOT");
        let inputs = [
            format!("{} x = {}1;", kw("LET"), "-".repeat(200_000)),
            format!(
                "{} x = {}y;",
                kw("LET"),
                format!("{not_kw} ").repeat(100_000)
            ),
            format!("{}1;", "a = ".repeat(200_000)),
            // вперемешку: унарные внутри значения присваивания
            format!("{}{}1;", "a = ".repeat(60), "- ".repeat(100)),
        ];
        for src in inputs {
            let err = parse_on_small_stack(src).expect_err("ожидалась ParseError");
            assert!(err.message.contains("глубин"), "{}", err.message);
        }
    }

    #[test]
    fn mixed_unary_and_paren_nesting_shares_one_depth_budget() {
        // Каждая пара `-(` стоит два уровня: унарный оператор и `parse_expr`
        // внутри скобок. Бюджет глубины общий: 30 пар (62 уровня) проходят,
        // 60 пар (122 уровня) — нет.
        let n = 30;
        let ok = format!("{} x = {}1{};", kw("LET"), "-(".repeat(n), ")".repeat(n));
        assert!(let_value(ok).is_ok());
        let too_deep = format!("{} x = {}1{};", kw("LET"), "-(".repeat(60), ")".repeat(60));
        let err = let_value(too_deep).expect_err("слишком глубоко");
        assert!(err.message.contains("глубин"), "{}", err.message);
    }
}
