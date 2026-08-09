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
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Option<TypeAnnotation>,
    pub mutable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
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
    Lambda { params: Vec<String>, body: Vec<Stmt> },
    /// `TypeName { field1: expr, field2: expr }` — создание экземпляра
    /// struct. `type_name` — имя struct, объявленного через
    /// `Stmt::StructDecl`. Вычисляется в `Value::Struct(...)` рантайме.
    /// Перенесено из родительской ветки B при слиянии (см.
    /// MIGRATION_REPORT.md, раздел "Struct"): в ветке A структуры были
    /// объявлены как токен (`TokenKind::Struct`), но никогда не имели AST/
    /// парсера/кодогена — мёртвая фича. Реализация B (полный конвейер)
    /// перенесена как есть, без статической типизации (нет
    /// `TypeAnnotation::Struct` — см. ROADMAP.md).
    StructLit { type_name: String, fields: Vec<(String, Expr)> },
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// let/var/const name [: type] = expr  (is_mutable, is_const)
    VarDecl { name: String, value: Expr, mutable: bool, ty: Option<TypeAnnotation> },
    ExprStmt(Expr),
    Print(Vec<Expr>),
    If { cond: Expr, then_branch: Vec<Stmt>, else_branch: Option<Vec<Stmt>> },
    While { cond: Expr, body: Vec<Stmt> },
    ForIn { var: String, start: Expr, end: Expr, body: Vec<Stmt> },
    FnDecl { name: String, params: Vec<Param>, body: Vec<Stmt>, return_ty: Option<TypeAnnotation> },
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
    StructDecl { name: String, fields: Vec<String> },
}

pub type Program = Vec<Stmt>;
