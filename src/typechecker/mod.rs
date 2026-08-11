//! Typechecker SGA — Type System (roadmap-пункт 1).
//!
//! ГРАДУАЛЬНАЯ типизация: проверяется только то, что явно аннотировано
//! пользователем (`let x: int = ...`, `fn f(a: int) -> int`). Любое имя
//! без аннотации трактуется как `Any` и НЕ проверяется — это гарантирует
//! 100% обратную совместимость с уже написанным untyped-кодом (все
//! существующие примеры и тесты не используют аннотации и продолжают
//! работать без единого изменения поведения).
//!
//! Типы полностью стираются после этого прохода (type erasure) — ни
//! `codegen`, ни `vm` ничего не знают про `TypeAnnotation`, см.
//! `src/codegen/mod.rs::compile` (комментарий "Аннотации типов стираются
//! здесь").
//!
//! НЕ реализовано в этой версии (см. docs/ROADMAP.md): типы элементов
//! массива (generics), типы полей структур (структур пока нет вообще),
//! union/intersection-типы, вывод типов для сложных выражений за пределами
//! литералов и бинарных операций.

use crate::ast::{BinOp, Expr, Program, Stmt, TypeAnnotation, UnOp};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct TypeError {
    pub message: String,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ошибка типов: {}", self.message)
    }
}

type TResult<T> = Result<T, TypeError>;

fn err(msg: impl Into<String>) -> TypeError {
    TypeError {
        message: msg.into(),
    }
}

#[derive(Debug, Clone)]
struct FnSig {
    params: Vec<TypeAnnotation>,
    ret: TypeAnnotation,
}

struct Scope {
    vars: HashMap<String, TypeAnnotation>,
}

pub struct Typechecker {
    scopes: Vec<Scope>,
    functions: HashMap<String, FnSig>,
    /// Тип возврата функции, тело которой сейчас проверяется (для
    /// проверки `RETURN` внутри неё). `None` — мы не внутри функции, либо
    /// функция не аннотировала возвращаемый тип (трактуется как `Any`).
    current_return_ty: Option<TypeAnnotation>,
}

impl Default for Typechecker {
    fn default() -> Self {
        Self::new()
    }
}

impl Typechecker {
    pub fn new() -> Self {
        let mut functions = HashMap::new();
        // Сигнатуры встроенных функций (src/runtime/mod.rs) — без них
        // typechecker трактовал бы их как Any/Any, что тоже безопасно,
        // но менее полезно: например, `let s: string = len([1,2,3]);`
        // не поймал бы реальную ошибку без этой таблицы.
        functions.insert(
            "len".into(),
            FnSig {
                params: vec![TypeAnnotation::Any],
                ret: TypeAnnotation::Int,
            },
        );
        functions.insert(
            "push".into(),
            FnSig {
                params: vec![TypeAnnotation::Any, TypeAnnotation::Any],
                ret: TypeAnnotation::Nil,
            },
        );
        functions.insert(
            "to_string".into(),
            FnSig {
                params: vec![TypeAnnotation::Any],
                ret: TypeAnnotation::String,
            },
        );
        functions.insert(
            "to_int".into(),
            FnSig {
                params: vec![TypeAnnotation::Any],
                ret: TypeAnnotation::Int,
            },
        );
        functions.insert(
            "to_float".into(),
            FnSig {
                params: vec![TypeAnnotation::Any],
                ret: TypeAnnotation::Float,
            },
        );
        Typechecker {
            scopes: vec![Scope {
                vars: HashMap::new(),
            }],
            functions,
            current_return_ty: None,
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

    fn declare(&mut self, name: &str, ty: TypeAnnotation) {
        self.scopes
            .last_mut()
            .unwrap()
            .vars
            .insert(name.to_string(), ty);
    }

    fn lookup(&self, name: &str) -> TypeAnnotation {
        for scope in self.scopes.iter().rev() {
            if let Some(t) = scope.vars.get(name) {
                return t.clone();
            }
        }
        // Неизвестное имя — не наша забота (semantic уже проверил
        // существование), безопасный дефолт — Any.
        TypeAnnotation::Any
    }

    pub fn analyze(&mut self, program: &Program) -> TResult<()> {
        // Первый проход: собрать сигнатуры функций верхнего уровня —
        // как и semantic-analyzer, чтобы поддержать опережающие вызовы.
        for stmt in program {
            if let Stmt::FnDecl {
                name,
                params,
                return_ty,
                ..
            } = stmt
            {
                let sig = FnSig {
                    params: params
                        .iter()
                        .map(|p| p.ty.clone().unwrap_or(TypeAnnotation::Any))
                        .collect(),
                    ret: return_ty.clone().unwrap_or(TypeAnnotation::Any),
                };
                self.functions.insert(name.clone(), sig);
            }
        }
        for stmt in program {
            self.check_stmt(stmt)?;
        }
        Ok(())
    }

    fn check_block(&mut self, stmts: &[Stmt]) -> TResult<()> {
        self.push_scope();
        for s in stmts {
            self.check_stmt(s)?;
        }
        self.pop_scope();
        Ok(())
    }

    fn check_stmt(&mut self, stmt: &Stmt) -> TResult<()> {
        match stmt {
            Stmt::VarDecl { name, value, ty, .. } => {
                let value_ty = self.infer(value)?;
                match ty {
                    Some(declared) => {
                        if !compatible(declared, &value_ty) {
                            return Err(err(format!(
                                "переменная '{}' объявлена с типом {}, но присвоено значение типа {}",
                                name,
                                show(declared),
                                show(&value_ty)
                            )));
                        }
                        // Доверяем явной аннотации пользователя для дальнейших проверок,
                        // даже если значение инициализатора само было Any.
                        self.declare(name, declared.clone());
                    }
                    None => {
                        // Без аннотации — переменная всегда Any, независимо от
                        // типа инициализатора. Это сознательное решение:
                        // делать вывод типа автоматическим даже для VAR без
                        // аннотации сломало бы существующий динамический код
                        // вроде `var x = 5; x = "текст";` — см. docs/ROADMAP.md.
                        self.declare(name, TypeAnnotation::Any);
                    }
                }
                Ok(())
            }
            Stmt::ExprStmt(e) => {
                self.infer(e)?;
                Ok(())
            }
            Stmt::Print(args) => {
                for a in args {
                    self.infer(a)?;
                }
                Ok(())
            }
            Stmt::If { cond, then_branch, else_branch } => {
                self.infer(cond)?;
                self.check_block(then_branch)?;
                if let Some(else_b) = else_branch {
                    self.check_block(else_b)?;
                }
                Ok(())
            }
            Stmt::While { cond, body } => {
                self.infer(cond)?;
                self.check_block(body)
            }
            Stmt::ForIn { var, start, end, body } => {
                let start_ty = self.infer(start)?;
                let end_ty = self.infer(end)?;
                if !compatible(&start_ty, &end_ty) {
                    return Err(err(format!(
                        "границы диапазона FOR..IN имеют несовместимые типы: {} и {}",
                        show(&start_ty),
                        show(&end_ty)
                    )));
                }
                self.push_scope();
                let loop_var_ty = if start_ty == TypeAnnotation::Any { TypeAnnotation::Any } else { start_ty };
                self.declare(var, loop_var_ty);
                for s in body {
                    self.check_stmt(s)?;
                }
                self.pop_scope();
                Ok(())
            }
            Stmt::FnDecl { params, body, return_ty, .. } => {
                self.push_scope();
                for p in params {
                    self.declare(&p.name, p.ty.clone().unwrap_or(TypeAnnotation::Any));
                }
                let prev_return = self.current_return_ty.take();
                self.current_return_ty = Some(return_ty.clone().unwrap_or(TypeAnnotation::Any));
                for s in body {
                    self.check_stmt(s)?;
                }
                self.current_return_ty = prev_return;
                self.pop_scope();
                Ok(())
            }
            Stmt::Return(expr) => {
                let actual = match expr {
                    Some(e) => self.infer(e)?,
                    None => TypeAnnotation::Nil,
                };
                if let Some(expected) = self.current_return_ty.clone() {
                    if !compatible(&expected, &actual) {
                        return Err(err(format!(
                            "функция объявлена с возвращаемым типом {}, но RETURN возвращает {}",
                            show(&expected),
                            show(&actual)
                        )));
                    }
                }
                Ok(())
            }
            Stmt::Break | Stmt::Continue => Ok(()),
            Stmt::Block(stmts) => self.check_block(stmts),
            // FUSION: `Stmt::Import` не должен достигать typechecker'а в
            // нормальном пайплайне (см. semantic::check_stmt, та же
            // defense-in-depth логика) — резолвится `module_resolver`
            // раньше. Явная ошибка вместо игнорирования варианта.
            Stmt::Import(path) => Err(err(format!(
                "IMPORT \"{}\" не был резолвлен до проверки типов — используйте sga::run_source_file()",
                path
            ))),
            // FUSION: structs (ветка B) не участвуют в градуальной системе
            // типов — нет TypeAnnotation::Struct в v0.1 (см.
            // docs/ROADMAP.md). StructDecl — pure compile-time, не несёт
            // проверяемой информации для typechecker'а.
            Stmt::StructDecl { .. } => Ok(()),
        }
    }

    /// Выводит (статически известный) тип выражения. `Any` означает
    /// "статически неизвестно/не проверяем" — не ошибка.
    fn infer(&mut self, expr: &Expr) -> TResult<TypeAnnotation> {
        use TypeAnnotation::*;
        Ok(match expr {
            Expr::Int(_) => Int,
            Expr::Float(_) => Float,
            Expr::Str(_) => String,
            Expr::Bool(_) => Bool,
            Expr::Nil => Nil,
            Expr::Ident(name) => self.lookup(name),
            Expr::Array(items) => {
                for it in items {
                    self.infer(it)?;
                }
                Array
            }
            Expr::Index(target, idx) => {
                let tt = self.infer(target)?;
                let it = self.infer(idx)?;
                if tt != Any && tt != Array && tt != String {
                    return Err(err(format!(
                        "индексация '[...]' не определена для типа {}",
                        show(&tt)
                    )));
                }
                if it != Any && it != Int {
                    return Err(err(format!(
                        "индекс массива должен быть int, получено {}",
                        show(&it)
                    )));
                }
                Any
            }
            Expr::Unary(op, e) => {
                let t = self.infer(e)?;
                match op {
                    UnOp::Not => Bool,
                    UnOp::Neg => match t {
                        Any | Int | Float => t,
                        other => {
                            return Err(err(format!(
                                "унарный '-' не определён для {}",
                                show(&other)
                            )))
                        }
                    },
                }
            }
            Expr::Binary(op, l, r) => {
                let lt = self.infer(l)?;
                let rt = self.infer(r)?;
                infer_binary(op, &lt, &rt)?
            }
            Expr::Call(name, args) => {
                let arg_tys: Vec<TypeAnnotation> =
                    args.iter().map(|a| self.infer(a)).collect::<TResult<_>>()?;
                let sig = self.functions.get(name).cloned();
                if let Some(sig) = sig {
                    for (i, (declared, actual)) in sig.params.iter().zip(arg_tys.iter()).enumerate()
                    {
                        if !compatible(declared, actual) {
                            return Err(err(format!(
                                "функция '{}': аргумент {} должен быть {}, передан {}",
                                name,
                                i + 1,
                                show(declared),
                                show(actual)
                            )));
                        }
                    }
                    sig.ret
                } else {
                    // Неизвестная функция — не наша забота (semantic уже отловил).
                    Any
                }
            }
            Expr::CallExpr(callee, args) => {
                // Callee — произвольное выражение, не статическое имя
                // функции, поэтому у него нет записи в `self.functions`
                // (та таблица сигнатур заполняется только из
                // `Stmt::FnDecl`, см. `Typechecker::new`/`declare_fn`).
                // Ожидаем `Closure` либо `Any` — если объявлено что-то
                // явно несовместимое (например, `let x: int = 5; x(1);`,
                // что синтаксически даёт `CallExpr` через постфиксный
                // вызов в `parser::parse_postfix`), это ошибка типов
                // здесь и сейчас, а не только RuntimeError позже в VM.
                let callee_ty = self.infer(callee)?;
                if callee_ty != Any && callee_ty != Closure {
                    return Err(err(format!(
                        "вызов значения типа {} как функции невозможен (ожидался closure)",
                        show(&callee_ty)
                    )));
                }
                for a in args {
                    self.infer(a)?;
                }
                // Сигнатура замыкания не типизирована (см.
                // `Expr::Lambda` выше) — результат вызова через
                // вычисляемый callee статически неизвестен.
                Any
            }
            Expr::Assign(name, value) => {
                let value_ty = self.infer(value)?;
                let declared = self.lookup(name);
                if !compatible(&declared, &value_ty) {
                    return Err(err(format!(
                        "невозможно присвоить значение типа {} переменной '{}' (объявлена как {})",
                        show(&value_ty),
                        name,
                        show(&declared)
                    )));
                }
                if declared == Any {
                    value_ty
                } else {
                    declared
                }
            }
            Expr::IndexAssign(target, idx, value) => {
                let tt = self.infer(target)?;
                let it = self.infer(idx)?;
                if tt != Any && tt != Array {
                    return Err(err(format!(
                        "индексное присваивание не определено для типа {}",
                        show(&tt)
                    )));
                }
                if it != Any && it != Int {
                    return Err(err(format!(
                        "индекс массива должен быть int, получено {}",
                        show(&it)
                    )));
                }
                self.infer(value)?
            }
            Expr::Lambda { params, body } => {
                // FUSION: типы параметров/возврата замыкания НЕ
                // аннотируются в грамматике (см. ast::Expr::Lambda) —
                // каждый параметр трактуется как `Any`, тело проверяется
                // в новой области видимости поверх текущей (для
                // корректного вывода типов внутри тела, если оно
                // ссылается на захватываемые внешние переменные с уже
                // известными типами). `current_return_ty` внутри тела —
                // `Any` (RETURN не ограничен), сохраняется/восстанавливается
                // вокруг прохода. Само выражение `FN(...) {...}` имеет
                // тип `Closure`. См. docs/ROADMAP.md — типизация сигнатур
                // замыканий (generics над arity/типами параметров) не
                // реализована в этой версии.
                self.push_scope();
                for p in params {
                    self.declare(p, Any);
                }
                let prev_return = self.current_return_ty.take();
                self.current_return_ty = Some(Any);
                let result = (|| {
                    for s in body {
                        self.check_stmt(s)?;
                    }
                    Ok(())
                })();
                self.current_return_ty = prev_return;
                self.pop_scope();
                result?;
                Closure
            }
            // FUSION: structs (ветка B) — все операции типизируются как
            // `Any`, нет статических типов полей/struct-имён в v0.1 (см.
            // комментарий у Stmt::StructDecl выше). Поля литерала всё же
            // рекурсивно проверяются — это даёт раннее обнаружение ошибок
            // типов ВНУТРИ значений полей (например, `Point{x: 1+"a"}`),
            // даже если сама struct нетипизирована.
            Expr::StructLit { fields, .. } => {
                for (_, e) in fields {
                    self.infer(e)?;
                }
                Any
            }
            Expr::FieldAccess(obj, _field) => {
                self.infer(obj)?;
                Any
            }
            Expr::FieldAssign(obj, _field, value) => {
                self.infer(obj)?;
                self.infer(value)?
            }
            Expr::MethodCall(obj, _method, args) => {
                self.infer(obj)?;
                for a in args {
                    self.infer(a)?;
                }
                Any
            }
        })
    }
}

fn infer_binary(op: &BinOp, lt: &TypeAnnotation, rt: &TypeAnnotation) -> TResult<TypeAnnotation> {
    use TypeAnnotation::*;
    match op {
        BinOp::Eq
        | BinOp::NotEq
        | BinOp::Lt
        | BinOp::Gt
        | BinOp::LtEq
        | BinOp::GtEq
        | BinOp::And
        | BinOp::Or => Ok(Bool),
        BinOp::Add => {
            if *lt == Any || *rt == Any {
                return Ok(Any);
            }
            match (lt, rt) {
                (Int, Int) => Ok(Int),
                (Float, Float) | (Int, Float) | (Float, Int) => Ok(Float),
                (String, String) => Ok(String),
                _ => Err(err(format!(
                    "оператор '+' не определён для {} и {}",
                    show(lt),
                    show(rt)
                ))),
            }
        }
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            if *lt == Any || *rt == Any {
                return Ok(Any);
            }
            match (lt, rt) {
                (Int, Int) => Ok(Int),
                (Float, Float) | (Int, Float) | (Float, Int) => Ok(Float),
                _ => Err(err(format!(
                    "арифметический оператор не определён для {} и {}",
                    show(lt),
                    show(rt)
                ))),
            }
        }
    }
}

/// Совместимость "объявленного" типа с "фактическим". `Any` совместим со
/// всем (граница градуальной типизации). `Int` неявно повышается до
/// `Float` — согласовано с автоматическим повышением в `src/vm/mod.rs::arith`.
fn compatible(declared: &TypeAnnotation, actual: &TypeAnnotation) -> bool {
    use TypeAnnotation::*;
    if *declared == Any || *actual == Any {
        return true;
    }
    if declared == actual {
        return true;
    }
    matches!((declared, actual), (Float, Int))
}

fn show(t: &TypeAnnotation) -> &'static str {
    use TypeAnnotation::*;
    match t {
        Int => "int",
        Float => "float",
        Bool => "bool",
        String => "string",
        Array => "array",
        Closure => "closure",
        Nil => "nil",
        Any => "any",
    }
}
