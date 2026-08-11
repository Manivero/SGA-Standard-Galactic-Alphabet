//! Семантический анализатор SGA. Делает проход по AST и проверяет:
//!  - использование неопределённых переменных/функций — ошибка компиляции;
//!  - повторное объявление переменной в той же области видимости — ошибка;
//!  - `break`/`continue` вне цикла — ошибка;
//!  - `return` вне функции — ошибка;
//!  - **Ownership/Borrowing (roadmap-пункт 2)**: параметры функций без
//!    `MUT` — immutable-заимствование (тело функции не может ни
//!    переприсвоить, ни мутировать содержимое); параметры с `MUT` —
//!    mutable-заимствование, и тогда аргумент на месте вызова обязан быть
//!    `var`-связанным именем, иначе ошибка компиляции. См. `check_call_borrows`.
//!  - **Замыкания/модули**: `Expr::Lambda` анализируется в области
//!    видимости, открытой к внешним переменным (для корректного захвата
//!    в рантайме, см. `runtime::ClosureValue`); `Stmt::Import` на этой
//!    стадии — defense-in-depth ошибка (см. ниже).
//!
//! Статическая проверка типов — в отдельном модуле `src/typechecker`.
//! Анализ владения реализован здесь в объёме, описанном выше; более
//! глубокий borrow-checking (несколько одновременных заимствований,
//! времена жизни — в т.ч. для замыканий, захватывающих окружение) НЕ
//! реализован — см. docs/ROADMAP.md.

use crate::ast::{Expr, Stmt};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct SemError {
    pub message: String,
}

impl std::fmt::Display for SemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ошибка семантики: {}", self.message)
    }
}

type SResult<T> = Result<T, SemError>;

struct Scope {
    vars: HashMap<String, bool>, // name -> is_mutable
}

/// Сигнатура функции, нужная семантическому анализатору: арность (для
/// проверки числа аргументов) и список флагов `mut` по каждому параметру
/// (для borrow-checking на месте вызова, см. `check_call_borrows`).
struct FnInfo {
    arity: usize,
    mut_params: Vec<bool>,
}

pub struct Analyzer {
    scopes: Vec<Scope>,
    functions: HashMap<String, FnInfo>,
    /// FUSION: имя struct -> множество объявленных полей. Перенесено из
    /// ветки B (структуры там не имели ownership-модели вообще). Нет
    /// статической типизации переменных по имени struct (см.
    /// typechecker) — таблица используется только для проверки
    /// `Expr::StructLit` (поле существует, нет дублей).
    structs: HashMap<String, std::collections::HashSet<String>>,
    loop_depth: u32,
    fn_depth: u32,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        Analyzer {
            scopes: vec![Scope {
                vars: HashMap::new(),
            }],
            functions: HashMap::new(),
            structs: HashMap::new(),
            loop_depth: 0,
            fn_depth: 0,
        }
    }

    fn err(msg: impl Into<String>) -> SemError {
        SemError {
            message: msg.into(),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope {
            vars: HashMap::new(),
        });
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare(&mut self, name: &str, mutable: bool) -> SResult<()> {
        let scope = self.scopes.last_mut().unwrap();
        if scope.vars.contains_key(name) {
            return Err(Self::err(format!(
                "переменная '{}' уже объявлена в этой области видимости",
                name
            )));
        }
        scope.vars.insert(name.to_string(), mutable);
        Ok(())
    }

    fn lookup(&self, name: &str) -> Option<bool> {
        for scope in self.scopes.iter().rev() {
            if let Some(m) = scope.vars.get(name) {
                return Some(*m);
            }
        }
        None
    }

    /// Находит "корневой" идентификатор сквозной цепочки индексации,
    /// например для `a[0][1]` вернёт `Some("a")`. Используется для
    /// проверки неизменяемости при глубокой мутации (`a[i] = v`,
    /// `push(a, v)`) — см. `check_mutation_target`.
    /// Если цель мутации — не простая переменная (например, результат
    /// вызова функции, `f()[0] = v`), возвращает `None`: в этом случае
    /// мы не можем статически определить владельца и не блокируем
    /// операцию (известное ограничение v0.1, см. docs/ROADMAP.md).
    fn mutation_root(expr: &Expr) -> Option<&str> {
        match expr {
            Expr::Ident(name) => Some(name.as_str()),
            Expr::Index(target, _) => Self::mutation_root(target),
            // FUSION: структуры (ветка B) — такие же reference-типы, как
            // массивы (Rc<RefCell<...>>), поэтому глубокая мутация через
            // цепочку доступа к полю (`p.x = 1`, `a.b.c = 1`) обязана
            // проходить ту же проверку неизменяемости корня, что и
            // индексное присваивание массива — иначе structs были бы
            // единственной "дырой" в ownership-модели, позволяющей
            // мутировать состояние через `LET`-переменную. См.
            // MIGRATION_REPORT.md.
            Expr::FieldAccess(target, _) => Self::mutation_root(target),
            _ => None,
        }
    }

    /// Проверяет, что цель глубокой мутации (индексное присваивание или
    /// мутирующий builtin вроде `push`) не привязана к immutable-имени
    /// (LET/CONST, либо immutable-параметр функции без `MUT`). Один и тот
    /// же механизм защищает и обычные переменные, и параметры функций —
    /// именно так Ownership/Borrowing (roadmap-пункт 2) переиспользует
    /// инфраструктуру неизменяемости, а не дублирует её.
    fn check_mutation_target(&self, target: &Expr, what: &str) -> SResult<()> {
        if let Some(name) = Self::mutation_root(target) {
            if let Some(false) = self.lookup(name) {
                return Err(Self::err(format!(
                    "невозможно изменить содержимое immutable-переменной '{}' через {} (объявлена через LET/CONST, либо параметр функции без MUT)",
                    name, what
                )));
            }
        }
        Ok(())
    }

    /// Borrow-checking на месте вызова (Ownership/Borrowing, roadmap-пункт
    /// 2): если параметр функции объявлен с `MUT` (mutable-заимствование),
    /// соответствующий аргумент — если это простое имя переменной — обязан
    /// сам быть mutable-связанным (`VAR`, либо `MUT`-параметр внешней
    /// функции). Если аргумент не простое имя (литерал, вызов функции) —
    /// проверка пропускается: это свежее значение без существующей
    /// immutable-привязки, заимствовать у него нечего.
    fn check_call_borrows(&self, fn_name: &str, args: &[Expr]) -> SResult<()> {
        if let Some(info) = self.functions.get(fn_name) {
            for (i, (arg, &is_mut_param)) in args.iter().zip(info.mut_params.iter()).enumerate() {
                if !is_mut_param {
                    continue;
                }
                if let Expr::Ident(arg_name) = arg {
                    if let Some(false) = self.lookup(arg_name) {
                        return Err(Self::err(format!(
                            "нельзя передать immutable-переменную '{}' в параметр {} функции '{}', объявленный как MUT (требуется VAR)",
                            arg_name, i + 1, fn_name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn analyze(&mut self, program: &[Stmt]) -> SResult<()> {
        // Первый проход: собрать сигнатуры функций, чтобы разрешить
        // взаимные/опережающие вызовы (forward references). Безопасно
        // обходить только `program` без рекурсии в тела блоков, так как
        // парсер (parse_stmt, top_level) гарантирует, что `FnDecl`
        // синтаксически не может появиться нигде, кроме верхнего уровня.
        for stmt in program {
            if let Stmt::FnDecl { name, params, .. } = stmt {
                if self.functions.contains_key(name) {
                    return Err(Self::err(format!("функция '{}' уже объявлена", name)));
                }
                let mut_params = params.iter().map(|p| p.mutable).collect();
                self.functions.insert(
                    name.clone(),
                    FnInfo {
                        arity: params.len(),
                        mut_params,
                    },
                );
            }
            if let Stmt::StructDecl { name, fields } = stmt {
                if self.structs.contains_key(name) {
                    return Err(Self::err(format!("тип '{}' уже объявлен", name)));
                }
                self.structs
                    .insert(name.clone(), fields.iter().cloned().collect());
            }
        }
        for stmt in program {
            self.check_stmt(stmt)?;
        }
        Ok(())
    }

    fn check_block(&mut self, stmts: &[Stmt]) -> SResult<()> {
        self.push_scope();
        for s in stmts {
            self.check_stmt(s)?;
        }
        self.pop_scope();
        Ok(())
    }

    fn check_stmt(&mut self, stmt: &Stmt) -> SResult<()> {
        match stmt {
            Stmt::VarDecl {
                name,
                value,
                mutable,
                ..
            } => {
                self.check_expr(value)?;
                self.declare(name, *mutable)?;
                Ok(())
            }
            Stmt::ExprStmt(e) => self.check_expr(e),
            Stmt::Print(args) => {
                for a in args {
                    self.check_expr(a)?;
                }
                Ok(())
            }
            Stmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.check_expr(cond)?;
                self.check_block(then_branch)?;
                if let Some(else_b) = else_branch {
                    self.check_block(else_b)?;
                }
                Ok(())
            }
            Stmt::While { cond, body } => {
                self.check_expr(cond)?;
                self.loop_depth += 1;
                let r = self.check_block(body);
                self.loop_depth -= 1;
                r
            }
            Stmt::ForIn {
                var,
                start,
                end,
                body,
            } => {
                self.check_expr(start)?;
                self.check_expr(end)?;
                self.push_scope();
                self.declare(var, true)?;
                self.loop_depth += 1;
                for s in body {
                    self.check_stmt(s)?;
                }
                self.loop_depth -= 1;
                self.pop_scope();
                Ok(())
            }
            Stmt::FnDecl {
                name: _,
                params,
                body,
                ..
            } => {
                // Сбрасываем loop_depth на время проверки тела функции.
                // На практике для Stmt::FnDecl это недостижимо иначе как
                // через 0 (FnDecl гарантированно top-level — см.
                // parser::parse_stmt, top_level — а на верхнем уровне
                // программы loop_depth всегда 0), но делаем это явно для
                // консистентности и защиты от регрессии, если инвариант
                // top-level-only когда-либо изменится.
                let saved_loop_depth = self.loop_depth;
                self.loop_depth = 0;
                self.push_scope();
                self.fn_depth += 1;
                for p in params {
                    // Ownership/Borrowing: без MUT параметр — immutable-
                    // заимствование (false), с MUT — mutable (true).
                    self.declare(&p.name, p.mutable)?;
                }
                let result = (|| {
                    for s in body {
                        self.check_stmt(s)?;
                    }
                    Ok(())
                })();
                self.fn_depth -= 1;
                self.pop_scope();
                self.loop_depth = saved_loop_depth;
                result
            }
            Stmt::Return(expr) => {
                if self.fn_depth == 0 {
                    return Err(Self::err("RETURN вне функции"));
                }
                if let Some(e) = expr {
                    self.check_expr(e)?;
                }
                Ok(())
            }
            Stmt::Break => {
                if self.loop_depth == 0 {
                    return Err(Self::err("BREAK вне цикла"));
                }
                Ok(())
            }
            Stmt::Continue => {
                if self.loop_depth == 0 {
                    return Err(Self::err("CONTINUE вне цикла"));
                }
                Ok(())
            }
            Stmt::Block(stmts) => self.check_block(stmts),
            Stmt::StructDecl { .. } => {
                // Уже обработано в первом проходе analyze() — здесь
                // только exhaustive match. Перенесено из ветки B.
                Ok(())
            }
            Stmt::Import(path) => {
                // В нормальном пайплайне (`run_source`/`run_source_file`,
                // см. src/lib.rs) до этой точки IMPORT уже либо отклонён
                // (`reject_imports`), либо полностью резолвлен
                // (`module_resolver::resolve_imports`) — `Stmt::Import`
                // физически не должен достигать семантического анализа.
                // Эта ветка — defense-in-depth для случая, когда кто-то
                // вызывает `Analyzer::analyze` напрямую на AST, полученном
                // из `parser` в обход `lib.rs`-пайплайна.
                Err(Self::err(format!(
                    "IMPORT \"{}\" не был резолвлен до семантического анализа — \
                     используйте sga::run_source_file() для файлов с IMPORT \
                     (Analyzer::analyze ожидает уже резолвленный AST)",
                    path
                )))
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr) -> SResult<()> {
        match expr {
            Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Nil => Ok(()),
            Expr::Ident(name) => {
                if self.lookup(name).is_none() {
                    return Err(Self::err(format!("неопределённая переменная '{}'", name)));
                }
                Ok(())
            }
            Expr::Array(items) => {
                for it in items {
                    self.check_expr(it)?;
                }
                Ok(())
            }
            Expr::Index(target, idx) => {
                self.check_expr(target)?;
                self.check_expr(idx)
            }
            Expr::Unary(_, e) => self.check_expr(e),
            Expr::Binary(_, l, r) => {
                self.check_expr(l)?;
                self.check_expr(r)
            }
            Expr::Call(name, args) => {
                let is_known_fn = self.functions.contains_key(name);
                let is_var = self.lookup(name).is_some();
                if !is_known_fn && !is_builtin(name) && !is_var {
                    return Err(Self::err(format!("неопределённая функция '{}'", name)));
                }
                if let Some(info) = self.functions.get(name) {
                    if info.arity != args.len() {
                        return Err(Self::err(format!(
                            "функция '{}' ожидает {} аргумент(ов), передано {}",
                            name,
                            info.arity,
                            args.len()
                        )));
                    }
                } else if let Some((min, max)) = builtin_arity(name) {
                    if args.len() < min || args.len() > max {
                        let expected = if min == max {
                            format!("{}", min)
                        } else {
                            format!("от {} до {}", min, max)
                        };
                        return Err(Self::err(format!(
                            "функция '{}' ожидает {} аргумент(ов), передано {}",
                            name,
                            expected,
                            args.len()
                        )));
                    }
                }
                // Если `name` — ни известная top-level функция, ни
                // builtin, а просто видимая переменная (`is_var`), её
                // арность НЕ проверяется здесь статически: semantic-
                // анализ не знает динамический тип значения переменной
                // (нет статических типов для замыканий — см. шапку
                // файла), поэтому не может знать, замыкание ли там и с
                // какой арностью. Несоответствие арности для вызова через
                // переменную — ошибка `RuntimeError`, обнаруживаемая в
                // VM в момент вызова (`vm::call_value`), а не ошибка
                // компиляции. По той же причине borrow-checking
                // (`check_call_borrows`) применяется только к статически
                // известным top-level функциям — у переменной нет
                // статической сигнатуры с `MUT`-параметрами.
                if is_known_fn {
                    self.check_call_borrows(name, args)?;
                }
                if name == "push" {
                    if let Some(first) = args.first() {
                        self.check_mutation_target(first, "push()")?;
                    }
                }
                for a in args {
                    self.check_expr(a)?;
                }
                Ok(())
            }
            Expr::CallExpr(callee, args) => {
                // Callee — произвольное выражение (не статическое имя),
                // поэтому здесь НЕТ ни проверки арности, ни
                // borrow-checking — оба полностью динамические и
                // проверяются в VM при исполнении (`vm::call_value`),
                // точно так же, как для `Expr::Call` в ветке `is_var`
                // выше (вызов через переменную). См. там подробный
                // комментарий о причине.
                self.check_expr(callee)?;
                for a in args {
                    self.check_expr(a)?;
                }
                Ok(())
            }
            Expr::Lambda { params, body } => {
                // Тело замыкания проверяется в НОВОЙ области видимости,
                // содержащей только его параметры — НЕ копию текущего
                // scope. Это осознанное решение: захват переменных
                // (Value::Closure::captured) происходит в РАНТАЙМЕ (см.
                // codegen::compile_expr -> OpCode::MakeClosure и
                // vm::run_chunk), а не на этапе semantic-анализа. Чтобы
                // semantic корректно разрешал ссылки на захватываемые
                // внешние переменные внутри тела лямбды (например,
                // `let x = 1; let f = FN() { return x; };` должно
                // пройти semantic, т.к. `x` видна лексически), мы
                // временно "открываем" видимость текущего стека
                // областей для проверки тела — физически НЕ копируя
                // scope, а просто не оборачивая в `push_scope`/`pop_scope`
                // ничего, кроме новой scope для параметров. Это даёт
                // корректную лексическую видимость без дублирования
                // структуры данных.
                //
                // ВАЖНО: `loop_depth` ОБНУЛЯЕТСЯ (не наследуется) на
                // время проверки тела лямбды, а затем восстанавливается.
                // Без этого `BREAK`/`CONTINUE` внутри тела лямбды,
                // созданной лексически внутри цикла (легальный код:
                // `while cond { let f = FN() { break; }; }`), ошибочно
                // считались бы разрешёнными — но `break` внутри
                // замыкания не может прервать внешний цикл: замыкание
                // выполняется в СВОЁМ собственном вызове функции (VM
                // executes `def.chunk` отдельным `run_chunk`), полностью
                // отдельно от текущего потока управления цикла, который
                // его создал. `fn_depth`, наоборот, не сбрасывается до 0,
                // а инкрементируется — `RETURN` внутри лямбды корректен
                // независимо от того, была ли лямбда создана внутри
                // объемлющей функции.
                //
                // Параметры лямбды всегда объявляются как mutable=true:
                // у `Expr::Lambda` нет `MUT`-аннотаций (см.
                // `ast::Expr::Lambda`), borrow-checking для замыканий не
                // реализован в v0.1 — см. docs/ROADMAP.md.
                let saved_loop_depth = self.loop_depth;
                self.loop_depth = 0;
                self.push_scope();
                self.fn_depth += 1;
                for p in params {
                    self.declare(p, true)?;
                }
                let result = (|| {
                    for s in body {
                        self.check_stmt(s)?;
                    }
                    Ok(())
                })();
                self.fn_depth -= 1;
                self.pop_scope();
                self.loop_depth = saved_loop_depth;
                result
            }
            Expr::Assign(name, value) => {
                match self.lookup(name) {
                    None => return Err(Self::err(format!("неопределённая переменная '{}'", name))),
                    Some(false) => {
                        return Err(Self::err(format!(
                            "невозможно изменить immutable-переменную '{}' (объявлена через LET/CONST, либо параметр функции без MUT)",
                            name
                        )))
                    }
                    Some(true) => {}
                }
                self.check_expr(value)
            }
            Expr::IndexAssign(target, idx, value) => {
                self.check_expr(target)?;
                self.check_mutation_target(target, "индексное присваивание")?;
                self.check_expr(idx)?;
                self.check_expr(value)
            }
            // FUSION: ниже — перенесено из ветки B. Структуры там не
            // имели ownership-модели вообще; здесь FieldAssign проходит
            // через `check_mutation_target` (см. `mutation_root`, ветка
            // FieldAccess, и комментарий там) — единственное реальное
            // изменение поведения относительно исходного кода ветки B.
            Expr::StructLit { type_name, fields } => {
                // Проверяем: тип объявлен, нет лишних полей, нет дублей.
                // Отсутствующие поля допустимы — инициализируются Nil.
                if !self.structs.contains_key(type_name) {
                    return Err(Self::err(format!("неизвестный тип '{}'", type_name)));
                }
                // Клонируем набор полей перед циклом — `self.check_expr`
                // нужен `&mut self`, а `self.structs.get(...)` держал бы
                // immutable borrow на всё время цикла (этот же класс
                // ошибки borrow checker'а был найден в исходном коде
                // ветки B при эмпирической проверке слияния — там он
                // был реальной ошибкой компиляции; здесь предотвращён
                // заранее, см. MIGRATION_REPORT.md).
                let declared = self.structs.get(type_name).unwrap().clone();
                let mut seen = std::collections::HashSet::new();
                for (field_name, field_expr) in fields {
                    if !declared.contains(field_name) {
                        return Err(Self::err(format!(
                            "тип '{}' не имеет поля '{}'",
                            type_name, field_name
                        )));
                    }
                    if !seen.insert(field_name) {
                        return Err(Self::err(format!(
                            "поле '{}' указано дважды в литерале '{}'",
                            field_name, type_name
                        )));
                    }
                    self.check_expr(field_expr)?;
                }
                Ok(())
            }
            Expr::FieldAccess(obj, _field) => {
                // Имя поля не проверяем статически — нет статических
                // типов структур (см. typechecker). Обращение к
                // несуществующему полю — RuntimeError (см. vm::GetField).
                self.check_expr(obj)
            }
            Expr::FieldAssign(obj, _field, value) => {
                self.check_expr(obj)?;
                self.check_mutation_target(obj, "присваивание полю struct")?;
                self.check_expr(value)
            }
            Expr::MethodCall(obj, method_name, args) => {
                // Метод = top-level функция `TypeName_method`, разрешаемая
                // в рантайме по фактическому типу `obj` (см. vm::CallMethod).
                // Статическая арность/borrow-checking для self-параметра
                // здесь НЕ выполняются: имя функции зависит от
                // динамического типа `obj`, которого semantic не знает
                // (структуры нетипизированы статически) — задокументированное
                // ограничение v0.1, см. docs/ROADMAP.md. Защита на уровне
                // тела метода всё равно действует: если `TypeName_method`
                // объявлен без `MUT self`, присваивание `self.field = ...`
                // внутри него будет отклонено той же проверкой, что и для
                // обычных immutable-параметров.
                let _ = method_name;
                self.check_expr(obj)?;
                for a in args {
                    self.check_expr(a)?;
                }
                Ok(())
            }
        }
    }
}

fn is_builtin(name: &str) -> bool {
    crate::runtime::is_builtin(name)
}

/// Допустимый диапазон количества аргументов (min, max) для каждой
/// builtin-функции. `push` — единственная исходно-builtin функция с
/// переменной арностью (см. FUSION-комментарий у `runtime::call_builtin`).
/// Остальные новые записи (`range`..`str_lower`) перенесены из ветки B —
/// её stdlib (см. MIGRATION_REPORT.md, раздел "Stdlib").
fn builtin_arity(name: &str) -> Option<(usize, usize)> {
    match name {
        "len" => Some((1, 1)),
        "push" => Some((1, 2)),
        "to_string" => Some((1, 1)),
        "to_int" => Some((1, 1)),
        "to_float" => Some((1, 1)),
        "type_of" => Some((1, 1)),
        "keys" => Some((1, 1)),
        "range" => Some((1, 2)),
        "sqrt" => Some((1, 1)),
        "floor" => Some((1, 1)),
        "ceil" => Some((1, 1)),
        "abs" => Some((1, 1)),
        "min" => Some((2, 2)),
        "max" => Some((2, 2)),
        "pow" => Some((2, 2)),
        "str_split" => Some((2, 2)),
        "str_contains" => Some((2, 2)),
        "str_trim" => Some((1, 1)),
        "str_starts_with" => Some((2, 2)),
        "str_ends_with" => Some((2, 2)),
        "str_replace" => Some((3, 3)),
        "str_upper" => Some((1, 1)),
        "str_lower" => Some((1, 1)),
        _ => None,
    }
}
