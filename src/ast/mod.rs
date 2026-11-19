//! Абстрактное синтаксическое дерево SGA.
//!
//! FUSION-ПРИМЕЧАНИЕ: этот файл объединяет два независимых направления
//! развития AST, существовавших в двух родительских ветках проекта:
//!   - градуальная система типов + ownership/borrowing (`TypeAnnotation`,
//!     `Param{ty, mutable}`, `VarDecl.ty`) — см. docs/LANGUAGE_SPEC.md §7;
//!   - замыкания и модульная система (`Expr::Lambda`, `Stmt::Import`) —
//!     см. docs/LANGUAGE_SPEC.md §5.2 и docs/COMPILER_SPEC.md.
//!
//! Это НЕ взаимоисключающие фичи: они трогают разные оси языка (типы
//! параметров vs анонимные функции) и физически не конфликтуют ни в
//! грамматике, ни в кодовом пространстве SGA-алфавита (см.
//! src/sga_alphabet.rs — это полный 26-буквенный алфавит, а не таблица
//! на 26 фиксированных ключевых слов, поэтому MUT и IMPORT — это два
//! независимых "слова", а не одна и та же кодовая позиция).
//!
//! ПОЗИЦИИ (T017, M007): каждый `Expr` и `Stmt` (а также `MatchArm` и `Param`)
//! несёт `Span` — диапазон токенов, из которых он построен. Прежние enum'ы
//! `Expr`/`Stmt` стали `ExprKind`/`StmtKind`; структуры `Expr { kind, span }` и
//! `Stmt { kind, span }` — то, чем узлы теперь являются. Равенство узлов
//! (`PartialEq`) СТРУКТУРНОЕ: `span` в нём не участвует. `Debug` `span` тоже не
//! показывает: вывод `sga fmt` (`{:#?}` по дереву) остаётся байт-в-байт прежним,
//! а `Debug` и `Eq` согласованы (то, что выглядит одинаково, и равно). Позиции
//! читают явно, через `.span`.

use crate::lexer::token::TokenSpan;

/// Диапазон исходного текста узла AST: индексы токенов `first..=last` в таблице
/// токенов, из которой построен узел (`tokens[first]` — первый токен узла,
/// `tokens[last]` — последний).
///
/// Это индексы, а не позиции в тексте: так диапазон занимает 8 байт (узлы
/// `Expr`/`Stmt` возвращаются парсером по значению, и каждый лишний байт
/// умножается на глубину рекурсии — см. `MAX_PARSE_DEPTH` в `parser`), а парсеру
/// не нужна таблица границ токенов. Позиции (строка/колонка начала первого токена
/// и конца последнего) даёт `Span::resolve` по таблице `TokenSpan`, которую строит
/// `Lexer::tokenize_with_spans` — ТОЙ ЖЕ самой последовательности токенов.
///
/// `Span::NONE` — «позиции нет»: синтезированный узел, узел из файла, разобранного
/// `Parser::detached` (импортируемые модули: их индексы относились бы к другой
/// таблице токенов).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Индекс первого токена узла.
    pub first: u32,
    /// Индекс последнего токена узла (включительно).
    pub last: u32,
}

impl Span {
    /// «Позиции нет» (см. описание типа).
    pub const NONE: Span = Span {
        first: u32::MAX,
        last: u32::MAX,
    };

    /// Диапазон токенов `first..=last`. Индексы, не помещающиеся в `u32` (больше
    /// четырёх миллиардов токенов), прижимаются к `u32::MAX - 1`, чтобы не
    /// совпасть с `NONE`.
    pub fn new(first: usize, last: usize) -> Span {
        let clamp =
            |index: usize| u32::try_from(index).map_or(u32::MAX - 1, |i| i.min(u32::MAX - 1));
        Span {
            first: clamp(first),
            last: clamp(last),
        }
    }

    /// Это `Span::NONE`?
    pub fn is_none(self) -> bool {
        self == Span::NONE
    }

    /// Позиции в тексте: от начала токена `first` до конца токена `last` по
    /// таблице границ токенов (1-based, колонки в символах, конец исключающий).
    /// `None` — для `Span::NONE`, при `first > last` и если индекс выходит за
    /// таблицу (таблица от другого текста).
    pub fn resolve(self, table: &[TokenSpan]) -> Option<TokenSpan> {
        if self.is_none() || self.first > self.last {
            return None;
        }
        let first = table.get(self.first as usize)?;
        let last = table.get(self.last as usize)?;
        Some(TokenSpan {
            line: first.line,
            col: first.col,
            end_line: last.end_line,
            end_col: last.end_col,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

/// Аннотация типа (Type System, roadmap-пункт 1). Аннотации опциональны
/// везде, где встречаются в грамматике — отсутствие аннотации эквивалентно
/// `Any` (полностью динамическое поведение). Градуальная типизация:
/// проверяется только то, что явно аннотировано пользователем, см.
/// docs/LANGUAGE_SPEC.md, §7.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeAnnotation {
    Int,
    Float,
    Bool,
    String,
    Array,
    /// Тип значения-замыкания (`FN(...) {...}` как выражение). В v0.1
    /// сигнатура замыкания (типы параметров/возврата) НЕ проверяется
    /// typechecker'ом — см. `src/typechecker/mod.rs`, ветка
    /// `Expr::Lambda`, и docs/ROADMAP.md. Аннотация существует, чтобы
    /// `let f: closure = FN(x) { return x; };` хотя бы синтаксически
    /// разрешалось и не требовало писать `any`.
    Closure,
    Nil,
    Any,
}

/// Параметр функции. `mutable=false` (по умолчанию, без `MUT`) — параметр
/// получает immutable-заимствование: тело функции не может ни
/// переприсвоить имя параметра, ни мутировать его содержимое (массив)
/// — проверяется тем же механизмом, что и обычные `LET`-переменные, см.
/// `src/semantic/mod.rs`. `mutable=true` (явный `MUT` перед именем) —
/// mutable-заимствование: вызывающая сторона обязана передать
/// `var`-связанный аргумент, иначе ошибка компиляции (Ownership/Borrowing,
/// roadmap-пункт 2). ВАЖНО (см. MIGRATION_REPORT.md): эта проверка
/// действует только для статически объявленных top-level функций
/// (`Stmt::FnDecl`) — параметры анонимных функций/замыканий
/// (`Expr::Lambda`) всегда mutable=true, захват происходит по значению,
/// см. `runtime::ClosureValue`.
#[derive(Clone)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeAnnotation>,
    pub mutable: bool,
    /// `[MUT] имя [: тип]` целиком (T017).
    pub span: Span,
}

/// Равенство структурное: `span` не сравнивается (см. шапку модуля).
impl PartialEq for Param {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.ty == other.ty && self.mutable == other.mutable
    }
}

/// `Debug` без `span` (см. шапку модуля).
impl std::fmt::Debug for Param {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Param")
            .field("name", &self.name)
            .field("ty", &self.ty)
            .field("mutable", &self.mutable)
            .finish()
    }
}

/// Выражение: вариант (`kind`) и диапазон токенов, из которых оно построено
/// (T017). Равенство структурное — `span` не сравнивается.
#[derive(Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }

    /// Узел без позиции (`Span::NONE`): собранный вручную (тесты) или
    /// синтезированный.
    pub fn unspanned(kind: ExprKind) -> Expr {
        Expr {
            kind,
            span: Span::NONE,
        }
    }
}

impl PartialEq for Expr {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

/// `Debug` прозрачен: печатает только `kind`, как печатал прежний enum `Expr`
/// (см. шапку модуля).
impl std::fmt::Debug for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.kind, f)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Nil,
    Ident(String),
    Array(Vec<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// Статический вызов: `name` — top-level функция или builtin,
    /// разрешённая на этапе компиляции (`codegen::Compiler::known_functions`).
    /// Вызов через переменную, хранящую замыкание (`f(1)`, где `f` —
    /// `LET`/`VAR`), парсится в этот же вариант (имя идентификатора), а
    /// различение "статическая функция или значение-замыкание"
    /// происходит позже, в `semantic`/`codegen` — см. там подробные
    /// комментарии. AST не делает это различение явным, чтобы парсер не
    /// зависел от таблицы имён функций (однопроходный парсер).
    Call(String, Vec<Expr>),
    /// Вызов ПРОИЗВОЛЬНОГО выражения как функции: `(expr)(args)`. В
    /// отличие от `Call(name, args)` (вызов по статическому имени —
    /// единственный путь, доступный из исходного грамматического
    /// разбора идентификатора в `parser::parse_primary`), сюда попадает
    /// любой другой "вызываемый" постфикс: непосредственный вызов
    /// литерала замыкания (IIFE, `FN(x){...}(21)`), вызов результата
    /// предыдущего вызова (`f()()`), вызов элемента массива (`fns[0]()`)
    /// и т.п. — везде, где `(args)` следует за чем-то, что не является
    /// просто именем. См. `parser::parse_postfix`.
    ///
    /// FUSION/НАЙДЕНО ПРИ СЛИЯНИИ: до введения этого варианта парсер
    /// успешно разбирал Lambda-литерал в IIFE-позиции, но молча НЕ
    /// потреблял следующий за ним `(args)` (грамматическая дыра —
    /// `parse_postfix` обрабатывал только `[idx]`), из-за чего
    /// `FN(x){...}(21);` тихо превращался в ДВА отдельных statement'а:
    /// создание и немедленное отбрасывание замыкания, затем независимое
    /// вычисление `(21)` как отдельного выражения. Баг был скрыт до тех
    /// пор, пока программа не начала возвращать значение последнего
    /// statement'а (см. `codegen::compile`, `compile_tail_stmt`) — после
    /// этого исправления баг стал детектируемым по неверному результату
    /// теста, а не только "случайно похожим на правильный" из-за
    /// совпадения отброшенного и реального значений. См.
    /// MIGRATION_REPORT.md.
    CallExpr(Box<Expr>, Vec<Expr>),
    Assign(String, Box<Expr>),
    IndexAssign(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `FN(a, b) { ... }` как ВЫРАЖЕНИЕ (не `Stmt::FnDecl`) — анонимная
    /// функция/замыкание. Отличие от `Stmt::FnDecl`: это `Expr`, может
    /// встречаться где угодно, где допустимо выражение (включая
    /// аргумент вызова, элемент массива, правую часть присваивания), и
    /// при компиляции захватывает текущее видимое окружение по значению
    /// (см. `runtime::Value::Closure`). Параметры лямбды НЕ принимают
    /// `MUT`/тип — это сознательное упрощение v0.1 (см.
    /// docs/ROADMAP.md): ownership-модель для замыканий, захватывающих
    /// произвольное окружение, требует отдельного анализа времён жизни,
    /// не реализованного в этой версии. См. docs/LANGUAGE_SPEC.md, §5.2.
    Lambda {
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    /// `TypeName { field1: expr, field2: expr }` — создание экземпляра
    /// struct. `type_name` — имя struct, объявленного через
    /// `Stmt::StructDecl`. Вычисляется в `Value::Struct(...)` рантайме.
    /// Перенесено из родительской ветки B при слиянии (см.
    /// MIGRATION_REPORT.md, раздел "Struct"): в ветке A структуры были
    /// объявлены как токен (`TokenKind::Struct`), но никогда не имели AST/
    /// парсера/кодогена — мёртвая фича. Реализация B (полный конвейер)
    /// перенесена как есть, без статической типизации (нет
    /// `TypeAnnotation::Struct` — см. ROADMAP.md).
    StructLit {
        type_name: String,
        fields: Vec<(String, Expr)>,
    },
    /// `expr.field` — доступ к полю struct по имени.
    FieldAccess(Box<Expr>, String),
    /// `expr.field = value` — присваивание полю struct по имени.
    /// ВАЖНО (FUSION, отличие от ветки B): в отличие от исходной ветки B
    /// (где ownership/borrowing не существовал вообще), здесь это
    /// присваивание проходит через ТУ ЖЕ проверку неизменяемости, что и
    /// `IndexAssign` для массивов — см. `semantic::check_mutation_target`
    /// и `semantic::mutation_root`, ветка `FieldAccess`. Без этого
    /// расширения структуры были бы единственным reference-типом,
    /// мутируемым через `LET`-переменную в обход всей ownership-модели —
    /// см. MIGRATION_REPORT.md.
    FieldAssign(Box<Expr>, String, Box<Expr>),
    /// `method_expr.method(args)` — вызов метода на экземпляре struct.
    /// Методы — обычные top-level `Stmt::FnDecl` с именем по конвенции
    /// `TypeName_method(self, ...)`; `self` — обычный `Param` (может быть
    /// `MUT self`, если метод должен мутировать поля через присваивание
    /// `self.field = ...` — см. docs/LANGUAGE_SPEC.md, §8).
    MethodCall(Box<Expr>, String, Vec<Expr>),
    /// `MATCH scrutinee { паттерн -> выражение, ... }` — T006 (M002).
    /// Выражение (не statement), как и `Lambda`: результат — значение
    /// тела ПЕРВОГО совпавшего `arm`. Последний `arm` в списке ОБЯЗАН
    /// быть catch-all (`Pattern::Wildcard` или `Pattern::Bind`) — это
    /// проверяется `semantic::Analyzer`, не парсером (см.
    /// `docs/LANGUAGE_SPEC.md`, §9 — раздел про MATCH). Только
    /// литеральные паттерны в v0.1 — без структурной деструктуризации
    /// struct/массивов (см. docs/ROADMAP.md).
    Match(Box<Expr>, Vec<MatchArm>),
}

/// Паттерн одного `arm` в `Expr::Match` (T006, M002). Только литералы и
/// два вида catch-all — см. `docs/LANGUAGE_SPEC.md`, §9.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Nil,
    /// `_` — catch-all, совпадает с чем угодно, не связывает имя.
    Wildcard,
    /// Простой идентификатор в позиции паттерна — catch-all, совпадает с
    /// чем угодно И связывает значение scrutinee с этим именем как
    /// новую immutable-переменную, видимую только в теле своего `arm`.
    Bind(String),
}

/// Один пункт `MATCH`: `паттерн -> тело`. `span` — от начала паттерна до конца
/// тела (T017); в равенстве не участвует.
#[derive(Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

impl PartialEq for MatchArm {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern && self.body == other.body
    }
}

/// `Debug` без `span` (см. шапку модуля).
impl std::fmt::Debug for MatchArm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatchArm")
            .field("pattern", &self.pattern)
            .field("body", &self.body)
            .finish()
    }
}

/// Оператор: вариант (`kind`) и диапазон токенов, из которых он построен
/// (T017), включая необязательную `;` в конце. Равенство структурное — `span`
/// не сравнивается.
#[derive(Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

impl Stmt {
    pub fn new(kind: StmtKind, span: Span) -> Stmt {
        Stmt { kind, span }
    }

    /// Узел без позиции (`Span::NONE`): собранный вручную (тесты) или
    /// синтезированный.
    pub fn unspanned(kind: StmtKind) -> Stmt {
        Stmt {
            kind,
            span: Span::NONE,
        }
    }
}

impl PartialEq for Stmt {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

/// `Debug` прозрачен: печатает только `kind`, как печатал прежний enum `Stmt`
/// (см. шапку модуля).
impl std::fmt::Debug for Stmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.kind, f)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// let/var/const name [: type] = expr  (is_mutable, is_const)
    VarDecl {
        name: String,
        value: Expr,
        mutable: bool,
        ty: Option<TypeAnnotation>,
    },
    ExprStmt(Expr),
    Print(Vec<Expr>),
    If {
        cond: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Option<Vec<Stmt>>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    ForIn {
        var: String,
        start: Expr,
        end: Expr,
        body: Vec<Stmt>,
    },
    FnDecl {
        name: String,
        params: Vec<Param>,
        body: Vec<Stmt>,
        return_ty: Option<TypeAnnotation>,
    },
    Return(Option<Expr>),
    Break,
    Continue,
    Block(Vec<Stmt>),
    /// `IMPORT "путь/к/файлу.sga";` — статически резолвится отдельным
    /// проходом (`module_resolver`) до семантического анализа: импорт
    /// разрешается с диска, его top-level `Stmt`-ы инлайнятся в текущую
    /// программу. После резолвинга этот вариант не должен встречаться
    /// в AST, передаваемом в `semantic`/`typechecker`/`codegen` — см.
    /// `src/module_resolver.rs` и docs/COMPILER_SPEC.md.
    Import(String),
    /// `STRUCT TypeName { field1, field2, ... }` — объявление нового
    /// номинального типа. Только top-level (как `FnDecl`/`Import`). Поля
    /// задаются именами без аннотаций типов — структуры НЕ участвуют в
    /// градуальной системе типов v0.1 (нет `TypeAnnotation::Struct`,
    /// см. typechecker — поля и литералы структур типизируются как
    /// `Any`). Методы — отдельные `FnDecl` с конвенцией именования
    /// `TypeName_method(self, ...)`, без синтаксиса `impl` (см.
    /// `ast::Expr::MethodCall`). Перенесено из родительской ветки B.
    StructDecl {
        name: String,
        fields: Vec<String>,
    },
}

pub type Program = Vec<Stmt>;

/// Глубина поддерева `stmt` больше `limit`?
///
/// Глубина считает и операторы, и выражения: корень — 1, каждый вложенный
/// `Stmt`/`Expr` — на единицу глубже своего родителя (`MatchArm` отдельным
/// уровнем не считается). Обход ИТЕРАТИВНЫЙ (явный стек в куче): функция
/// предназначена для проверки ДО рекурсивных проходов (`semantic`,
/// `typechecker`), которым глубокое дерево стоит неограниченного стека, поэтому
/// сама на глубоком дереве упасть не может. Парсер не ограничивает глубину AST:
/// левоассоциативные цепочки (`1+1+…+1`) и постфиксы (`a[0][0]…`) он строит
/// циклом, и `MAX_PARSE_DEPTH` их не видит.
///
/// Все варианты `ExprKind` и `StmtKind` перечислены явно (без `_`): новый вариант
/// не скомпилируется, пока не будет учтён здесь.
pub fn exceeds_depth(stmt: &Stmt, limit: usize) -> bool {
    enum Node<'a> {
        Stmt(&'a Stmt),
        Expr(&'a Expr),
    }
    let mut work = vec![(Node::Stmt(stmt), 1_usize)];
    while let Some((node, depth)) = work.pop() {
        if depth > limit {
            return true;
        }
        let below = depth + 1;
        match node {
            Node::Stmt(stmt) => match &stmt.kind {
                StmtKind::VarDecl { value, .. } => work.push((Node::Expr(value), below)),
                StmtKind::ExprStmt(expr) => work.push((Node::Expr(expr), below)),
                StmtKind::Print(args) => {
                    work.extend(args.iter().map(|arg| (Node::Expr(arg), below)))
                }
                StmtKind::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    work.push((Node::Expr(cond), below));
                    work.extend(then_branch.iter().map(|s| (Node::Stmt(s), below)));
                    if let Some(else_branch) = else_branch {
                        work.extend(else_branch.iter().map(|s| (Node::Stmt(s), below)));
                    }
                }
                StmtKind::While { cond, body } => {
                    work.push((Node::Expr(cond), below));
                    work.extend(body.iter().map(|s| (Node::Stmt(s), below)));
                }
                StmtKind::ForIn {
                    start, end, body, ..
                } => {
                    work.push((Node::Expr(start), below));
                    work.push((Node::Expr(end), below));
                    work.extend(body.iter().map(|s| (Node::Stmt(s), below)));
                }
                StmtKind::FnDecl { body, .. } => {
                    work.extend(body.iter().map(|s| (Node::Stmt(s), below)))
                }
                StmtKind::Return(value) => {
                    if let Some(value) = value {
                        work.push((Node::Expr(value), below));
                    }
                }
                StmtKind::Block(stmts) => work.extend(stmts.iter().map(|s| (Node::Stmt(s), below))),
                StmtKind::Break
                | StmtKind::Continue
                | StmtKind::Import(_)
                | StmtKind::StructDecl { .. } => {}
            },
            Node::Expr(expr) => match &expr.kind {
                ExprKind::Int(_)
                | ExprKind::Float(_)
                | ExprKind::Str(_)
                | ExprKind::Bool(_)
                | ExprKind::Nil
                | ExprKind::Ident(_) => {}
                ExprKind::Array(items) => work.extend(items.iter().map(|e| (Node::Expr(e), below))),
                ExprKind::Index(target, index) => {
                    work.push((Node::Expr(target), below));
                    work.push((Node::Expr(index), below));
                }
                ExprKind::Unary(_, operand) => work.push((Node::Expr(operand), below)),
                ExprKind::Binary(_, left, right) => {
                    work.push((Node::Expr(left), below));
                    work.push((Node::Expr(right), below));
                }
                ExprKind::Call(_, args) => work.extend(args.iter().map(|e| (Node::Expr(e), below))),
                ExprKind::CallExpr(callee, args) => {
                    work.push((Node::Expr(callee), below));
                    work.extend(args.iter().map(|e| (Node::Expr(e), below)));
                }
                ExprKind::Assign(_, value) => work.push((Node::Expr(value), below)),
                ExprKind::IndexAssign(target, index, value) => {
                    work.push((Node::Expr(target), below));
                    work.push((Node::Expr(index), below));
                    work.push((Node::Expr(value), below));
                }
                ExprKind::Lambda { body, .. } => {
                    work.extend(body.iter().map(|s| (Node::Stmt(s), below)))
                }
                ExprKind::StructLit { fields, .. } => {
                    work.extend(fields.iter().map(|(_, e)| (Node::Expr(e), below)))
                }
                ExprKind::FieldAccess(object, _) => work.push((Node::Expr(object), below)),
                ExprKind::FieldAssign(object, _, value) => {
                    work.push((Node::Expr(object), below));
                    work.push((Node::Expr(value), below));
                }
                ExprKind::MethodCall(object, _, args) => {
                    work.push((Node::Expr(object), below));
                    work.extend(args.iter().map(|e| (Node::Expr(e), below)));
                }
                ExprKind::Match(scrutinee, arms) => {
                    work.push((Node::Expr(scrutinee), below));
                    work.extend(arms.iter().map(|arm| (Node::Expr(&arm.body), below)));
                }
            },
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> Expr {
        Expr::unspanned(ExprKind::Int(n))
    }

    fn ex(kind: ExprKind) -> Expr {
        Expr::unspanned(kind)
    }

    fn st(kind: StmtKind) -> Stmt {
        Stmt::unspanned(kind)
    }

    fn boxed(expr: Expr) -> Box<Expr> {
        Box::new(expr)
    }

    fn token(line: usize, col: usize, end_line: usize, end_col: usize) -> TokenSpan {
        TokenSpan {
            line,
            col,
            end_line,
            end_col,
        }
    }

    // ── Span ────────────────────────────────────────────────────────────

    #[test]
    fn none_is_none_and_real_ranges_are_not() {
        assert!(Span::NONE.is_none());
        assert!(!Span::new(0, 0).is_none());
        assert!(!Span::new(3, 7).is_none());
        assert_eq!(Span::new(3, 7), Span { first: 3, last: 7 });
    }

    #[test]
    fn new_clamps_indexes_that_do_not_fit_and_never_produces_none() {
        let huge = Span::new(usize::MAX, usize::MAX);
        assert!(!huge.is_none());
        assert_eq!(huge.first, u32::MAX - 1);
        assert_eq!(huge.last, u32::MAX - 1);
        // u32::MAX сам по себе тоже зарезервирован под NONE
        assert!(!Span::new(u32::MAX as usize, 0).is_none());
        assert_eq!(Span::new(u32::MAX as usize, 0).first, u32::MAX - 1);
    }

    #[test]
    fn resolve_joins_the_start_of_the_first_and_the_end_of_the_last_token() {
        let table = [
            token(1, 1, 1, 4),
            token(1, 5, 1, 6),
            token(2, 3, 3, 8),
            token(3, 8, 3, 8),
        ];
        assert_eq!(Span::new(0, 1).resolve(&table), Some(token(1, 1, 1, 6)));
        assert_eq!(Span::new(1, 2).resolve(&table), Some(token(1, 5, 3, 8)));
        assert_eq!(Span::new(0, 3).resolve(&table), Some(token(1, 1, 3, 8)));
    }

    #[test]
    fn resolve_of_a_single_token_is_that_token() {
        let table = [token(1, 1, 1, 4), token(2, 2, 2, 9)];
        assert_eq!(Span::new(1, 1).resolve(&table), Some(table[1]));
    }

    #[test]
    fn resolve_is_none_for_none_reversed_and_out_of_table_ranges() {
        let table = [token(1, 1, 1, 2), token(1, 3, 1, 4)];
        assert_eq!(Span::NONE.resolve(&table), None);
        assert_eq!(Span::new(1, 0).resolve(&table), None);
        assert_eq!(Span::new(0, 2).resolve(&table), None);
        assert_eq!(Span::new(2, 2).resolve(&table), None);
        assert_eq!(Span::new(0, 0).resolve(&[]), None);
    }

    // ── равенство узлов ─────────────────────────────────────────────────

    #[test]
    fn node_equality_is_structural_and_ignores_spans() {
        let here = Expr::new(ExprKind::Int(1), Span::new(0, 0));
        let there = Expr::new(ExprKind::Int(1), Span::new(5, 9));
        assert_eq!(here, there);
        assert_ne!(here, int(2));
        assert_ne!(here.span, there.span);

        let nested_a = ex(ExprKind::Binary(
            BinOp::Add,
            boxed(Expr::new(ExprKind::Int(1), Span::new(0, 0))),
            boxed(Expr::new(ExprKind::Int(2), Span::new(2, 2))),
        ));
        let nested_b = ex(ExprKind::Binary(
            BinOp::Add,
            boxed(int(1)),
            boxed(Expr::new(ExprKind::Int(2), Span::new(9, 9))),
        ));
        assert_eq!(nested_a, nested_b);
        let different_op = ex(ExprKind::Binary(BinOp::Sub, boxed(int(1)), boxed(int(2))));
        assert_ne!(nested_a, different_op);
    }

    #[test]
    fn statement_arm_and_param_equality_ignore_spans_too() {
        let a = Stmt::new(StmtKind::Break, Span::new(1, 2));
        let b = Stmt::new(StmtKind::Break, Span::new(7, 8));
        assert_eq!(a, b);
        assert_ne!(a, st(StmtKind::Continue));

        let arm = |span| MatchArm {
            pattern: Pattern::Wildcard,
            body: int(1),
            span,
        };
        assert_eq!(arm(Span::new(0, 3)), arm(Span::NONE));
        let other_body = MatchArm {
            pattern: Pattern::Wildcard,
            body: int(2),
            span: Span::NONE,
        };
        assert_ne!(arm(Span::NONE), other_body);

        let param = |span| Param {
            name: "x".into(),
            ty: None,
            mutable: false,
            span,
        };
        assert_eq!(param(Span::new(0, 0)), param(Span::new(4, 4)));
        let mutable = Param {
            mutable: true,
            ..param(Span::NONE)
        };
        assert_ne!(param(Span::NONE), mutable);
    }

    // ── Debug не показывает позиции (вывод `sga fmt` прежний) ───────────

    #[test]
    fn debug_output_is_the_one_of_the_old_enums_and_hides_spans() {
        let stmt = Stmt::new(
            StmtKind::ExprStmt(Expr::new(ExprKind::Int(1), Span::new(2, 2))),
            Span::new(2, 3),
        );
        // ровно то, что печатал прежний `Stmt::ExprStmt(Expr::Int(1))`
        assert_eq!(
            format!("{stmt:#?}"),
            "ExprStmt(\n    Int(\n        1,\n    ),\n)"
        );
        assert_eq!(format!("{stmt:?}"), "ExprStmt(Int(1))");
        assert!(!format!("{stmt:#?}").contains("span"));
    }

    #[test]
    fn debug_of_params_and_arms_hides_spans_and_keeps_field_order() {
        let param = Param {
            name: "x".into(),
            ty: Some(TypeAnnotation::Int),
            mutable: true,
            span: Span::new(1, 4),
        };
        assert_eq!(
            format!("{param:?}"),
            "Param { name: \"x\", ty: Some(Int), mutable: true }"
        );
        let arm = MatchArm {
            pattern: Pattern::Wildcard,
            body: int(7),
            span: Span::new(0, 9),
        };
        assert_eq!(
            format!("{arm:?}"),
            "MatchArm { pattern: Wildcard, body: Int(7) }"
        );
        let decl = st(StmtKind::FnDecl {
            name: "f".into(),
            params: vec![param],
            body: vec![],
            return_ty: None,
        });
        assert_eq!(
            format!("{decl:?}"),
            "FnDecl { name: \"f\", params: [Param { name: \"x\", ty: Some(Int), mutable: true }], body: [], return_ty: None }"
        );
    }

    // ── exceeds_depth: КАЖДАЯ дочерняя позиция каждого варианта обходится ─

    /// Выражение, вложенное `levels` раз через `wrap` вокруг литерала.
    fn expr_chain(levels: usize, wrap: &dyn Fn(Expr) -> Expr) -> Expr {
        let mut expr = int(0);
        for _ in 0..levels {
            expr = wrap(expr);
        }
        expr
    }

    fn stmt_chain(levels: usize, wrap: &dyn Fn(Stmt) -> Stmt) -> Stmt {
        let mut stmt = st(StmtKind::Break);
        for _ in 0..levels {
            stmt = wrap(stmt);
        }
        stmt
    }

    /// Способ поместить выражение в родительское выражение / оператор (таблицы
    /// теста глубины перебирают все дочерние позиции).
    type ExprWrapper = Box<dyn Fn(Expr) -> Expr>;
    type ExprParent = Box<dyn Fn(Expr) -> Stmt>;
    type StmtWrapper = Box<dyn Fn(Stmt) -> Stmt>;

    /// Дерево глубины ровно `depth` проходит предел `depth` и не проходит
    /// предел `depth - 1`.
    fn assert_depth(stmt: &Stmt, depth: usize, what: &str) {
        assert!(
            !exceeds_depth(stmt, depth),
            "{what}: глубина {depth} в пределе"
        );
        assert!(
            exceeds_depth(stmt, depth - 1),
            "{what}: глубина {depth} не замечена (дочерняя позиция не обойдена?)"
        );
    }

    #[test]
    fn depth_is_counted_through_every_expression_child_position() {
        let wrappers: Vec<(&str, ExprWrapper)> = vec![
            ("Array", Box::new(|e| ex(ExprKind::Array(vec![int(0), e])))),
            (
                "Index.target",
                Box::new(|e| ex(ExprKind::Index(boxed(e), boxed(int(0))))),
            ),
            (
                "Index.index",
                Box::new(|e| ex(ExprKind::Index(boxed(int(0)), boxed(e)))),
            ),
            (
                "Unary",
                Box::new(|e| ex(ExprKind::Unary(UnOp::Neg, boxed(e)))),
            ),
            (
                "Binary.left",
                Box::new(|e| ex(ExprKind::Binary(BinOp::Add, boxed(e), boxed(int(0))))),
            ),
            (
                "Binary.right",
                Box::new(|e| ex(ExprKind::Binary(BinOp::Add, boxed(int(0)), boxed(e)))),
            ),
            (
                "Call.args",
                Box::new(|e| ex(ExprKind::Call("f".into(), vec![int(0), e]))),
            ),
            (
                "CallExpr.callee",
                Box::new(|e| ex(ExprKind::CallExpr(boxed(e), vec![]))),
            ),
            (
                "CallExpr.args",
                Box::new(|e| ex(ExprKind::CallExpr(boxed(int(0)), vec![e]))),
            ),
            (
                "Assign",
                Box::new(|e| ex(ExprKind::Assign("a".into(), boxed(e)))),
            ),
            (
                "IndexAssign.target",
                Box::new(|e| {
                    ex(ExprKind::IndexAssign(
                        boxed(e),
                        boxed(int(0)),
                        boxed(int(0)),
                    ))
                }),
            ),
            (
                "IndexAssign.index",
                Box::new(|e| {
                    ex(ExprKind::IndexAssign(
                        boxed(int(0)),
                        boxed(e),
                        boxed(int(0)),
                    ))
                }),
            ),
            (
                "IndexAssign.value",
                Box::new(|e| {
                    ex(ExprKind::IndexAssign(
                        boxed(int(0)),
                        boxed(int(0)),
                        boxed(e),
                    ))
                }),
            ),
            (
                "Lambda.body",
                Box::new(|e| {
                    ex(ExprKind::Lambda {
                        params: vec![],
                        body: vec![st(StmtKind::ExprStmt(e))],
                    })
                }),
            ),
            (
                "StructLit.field",
                Box::new(|e| {
                    ex(ExprKind::StructLit {
                        type_name: "P".into(),
                        fields: vec![("x".into(), e)],
                    })
                }),
            ),
            (
                "FieldAccess",
                Box::new(|e| ex(ExprKind::FieldAccess(boxed(e), "x".into()))),
            ),
            (
                "FieldAssign.object",
                Box::new(|e| ex(ExprKind::FieldAssign(boxed(e), "x".into(), boxed(int(0))))),
            ),
            (
                "FieldAssign.value",
                Box::new(|e| ex(ExprKind::FieldAssign(boxed(int(0)), "x".into(), boxed(e)))),
            ),
            (
                "MethodCall.object",
                Box::new(|e| ex(ExprKind::MethodCall(boxed(e), "m".into(), vec![]))),
            ),
            (
                "MethodCall.args",
                Box::new(|e| ex(ExprKind::MethodCall(boxed(int(0)), "m".into(), vec![e]))),
            ),
            (
                "Match.scrutinee",
                Box::new(|e| ex(ExprKind::Match(boxed(e), vec![]))),
            ),
            (
                "Match.arm body",
                Box::new(|e| {
                    ex(ExprKind::Match(
                        boxed(int(0)),
                        vec![MatchArm {
                            pattern: Pattern::Wildcard,
                            body: e,
                            span: Span::NONE,
                        }],
                    ))
                }),
            ),
        ];
        for (name, wrap) in &wrappers {
            let levels = 25;
            let root = st(StmtKind::ExprStmt(expr_chain(levels, wrap.as_ref())));
            // Тело замыкания — список ОПЕРАТОРОВ, поэтому каждый уровень
            // `Lambda.body` стоит два уровня глубины: само замыкание и
            // оператор-выражение внутри него. Остальные обёртки — по одному.
            let per_level = if *name == "Lambda.body" { 2 } else { 1 };
            // ExprStmt (1) + `levels` обёрток + литерал
            assert_depth(&root, levels * per_level + 2, name);
        }
    }

    #[test]
    fn depth_is_counted_through_every_statement_child_position() {
        let expr_parents: Vec<(&str, ExprParent)> = vec![
            (
                "VarDecl.value",
                Box::new(|e| {
                    st(StmtKind::VarDecl {
                        name: "x".into(),
                        value: e,
                        mutable: false,
                        ty: None,
                    })
                }),
            ),
            ("ExprStmt", Box::new(|e| st(StmtKind::ExprStmt(e)))),
            ("Print", Box::new(|e| st(StmtKind::Print(vec![int(0), e])))),
            (
                "If.cond",
                Box::new(|e| {
                    st(StmtKind::If {
                        cond: e,
                        then_branch: vec![],
                        else_branch: None,
                    })
                }),
            ),
            (
                "While.cond",
                Box::new(|e| {
                    st(StmtKind::While {
                        cond: e,
                        body: vec![],
                    })
                }),
            ),
            (
                "ForIn.start",
                Box::new(|e| {
                    st(StmtKind::ForIn {
                        var: "i".into(),
                        start: e,
                        end: int(0),
                        body: vec![],
                    })
                }),
            ),
            (
                "ForIn.end",
                Box::new(|e| {
                    st(StmtKind::ForIn {
                        var: "i".into(),
                        start: int(0),
                        end: e,
                        body: vec![],
                    })
                }),
            ),
            ("Return", Box::new(|e| st(StmtKind::Return(Some(e))))),
        ];
        for (name, parent) in &expr_parents {
            let chain = expr_chain(25, &|e| ex(ExprKind::Unary(UnOp::Neg, boxed(e))));
            // оператор (1) + 25 унарных + литерал
            assert_depth(&parent(chain), 27, name);
        }

        let stmt_wrappers: Vec<(&str, StmtWrapper)> = vec![
            (
                "If.then",
                Box::new(|s| {
                    st(StmtKind::If {
                        cond: int(0),
                        then_branch: vec![s],
                        else_branch: None,
                    })
                }),
            ),
            (
                "If.else",
                Box::new(|s| {
                    st(StmtKind::If {
                        cond: int(0),
                        then_branch: vec![],
                        else_branch: Some(vec![s]),
                    })
                }),
            ),
            (
                "While.body",
                Box::new(|s| {
                    st(StmtKind::While {
                        cond: int(0),
                        body: vec![s],
                    })
                }),
            ),
            (
                "ForIn.body",
                Box::new(|s| {
                    st(StmtKind::ForIn {
                        var: "i".into(),
                        start: int(0),
                        end: int(1),
                        body: vec![s],
                    })
                }),
            ),
            (
                "FnDecl.body",
                Box::new(|s| {
                    st(StmtKind::FnDecl {
                        name: "f".into(),
                        params: vec![],
                        body: vec![s],
                        return_ty: None,
                    })
                }),
            ),
            ("Block", Box::new(|s| st(StmtKind::Block(vec![s])))),
        ];
        for (name, wrap) in &stmt_wrappers {
            // 25 обёрток + `BREAK`
            assert_depth(&stmt_chain(25, wrap.as_ref()), 26, name);
        }
    }

    #[test]
    fn leaves_and_siblings_do_not_add_depth() {
        // широкое, но неглубокое дерево: 10 000 братьев на одном уровне
        let wide = st(StmtKind::Print(vec![int(0); 10_000]));
        assert_depth(&wide, 2, "широкий PRINT");
        for leaf in [
            st(StmtKind::Break),
            st(StmtKind::Continue),
            st(StmtKind::Import("m.sga".into())),
            st(StmtKind::StructDecl {
                name: "P".into(),
                fields: vec![],
            }),
            st(StmtKind::Return(None)),
        ] {
            assert!(!exceeds_depth(&leaf, 1), "{leaf:?}");
            assert!(exceeds_depth(&leaf, 0), "{leaf:?}");
        }
    }

    #[test]
    fn a_very_deep_tree_is_measured_without_recursion() {
        // 400 000 уровней — рекурсивный обход упал бы на стеке потока тестов
        // (2 МиБ); освобождение такого дерева тоже рекурсивно, поэтому сначала
        // «разбираем» его в цикле, чтобы не упасть в `Drop`.
        let mut expr = int(0);
        for _ in 0..400_000 {
            expr = ex(ExprKind::Binary(BinOp::Add, boxed(expr), boxed(int(1))));
        }
        let root = st(StmtKind::ExprStmt(expr));
        assert!(exceeds_depth(&root, 1_000));
        assert!(exceeds_depth(&root, 399_999));
        let StmtKind::ExprStmt(mut expr) = root.kind else {
            unreachable!()
        };
        while let ExprKind::Binary(_, left, _) = expr.kind {
            expr = *left;
        }
    }
}
