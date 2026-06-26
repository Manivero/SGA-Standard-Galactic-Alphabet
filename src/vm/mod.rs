//! Виртуальная машина SGA — стековый интерпретатор байткода.
//!
//! Безопасность (см. docs/SECURITY.md):
//!  - переполнение целых чисел проверяется (`checked_add/sub/mul`) — паника
//!    заменена управляемой `RuntimeError`, а не UB;
//!  - деление и взятие остатка от нуля — управляемая ошибка, а не crash;
//!  - выход за границы массива — управляемая ошибка;
//!  - неизменяемость переменных проверяется повторно на уровне VM
//!    (defense-in-depth, не только на этапе semantic-анализа);
//!  - Ownership/Borrowing на границе вызова функции (`MUT`-параметры)
//!    проверяется повторно здесь же, в `Vm::call` (defense-in-depth для
//!    `semantic::Analyzer::check_call_borrows`) — см. FUSION-ПРИМЕЧАНИЕ
//!    у `Vm::call` и MIGRATION_REPORT.md: это РЕГРЕССИЯ, найденная и
//!    исправленная при слиянии двух родительских веток, а не часть
//!    исходного кода ни одной из них.

use crate::codegen::{Chunk, CompiledProgram, FunctionDef, OpCode};
use crate::runtime::{call_builtin, is_builtin, ClosureValue, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug)]
pub struct RuntimeError(pub String);

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ошибка выполнения: {}", self.0)
    }
}

type RResult<T> = Result<T, RuntimeError>;

fn rt_err(msg: impl Into<String>) -> RuntimeError {
    RuntimeError(msg.into())
}

/// Bytecode verifier — статическая проверка `Chunk` перед исполнением.
///
/// Зачем это нужно: `Chunk`/`OpCode`/`CompiledProgram` — все публичные
/// типы (`pub`), а `Vm::run(&self, chunk: &Chunk)` — публичный метод.
/// Это значит, что внешний потребитель крейта `sga` (или будущий REPL/
/// сериализованный bytecode формат) может сконструировать `Chunk`
/// вручную, в обход `codegen::compile()`, который гарантирует
/// согласованность индексов по построению. Без верификации два класса
/// входных данных приводят к НЕУПРАВЛЯЕМОЙ Rust panic вместо
/// `RuntimeError`:
///   1. `OpCode::PushConst(idx)` с `idx >= chunk.constants.len()` —
///      прямая индексация `chunk.constants[*idx]` в `run_chunk` паникует.
///   2. `OpCode::Jump(target)` / `JumpIfFalse(target)` с `target`,
///      указывающим за пределы `chunk.code.len()` — текущий `while
///      ip < chunk.code.len()` сам по себе не паникует на следующей
///      итерации (просто завершает выполнение), но это всё равно
///      семантически некорректное, неверифицированное поведение,
///      которое заслуживает явной ошибки, а не молчаливого "программа
///      просто закончилась раньше времени".
///
/// `verify_chunk` гарантирует первый класс полностью (через `Result`,
/// а не panic) и явно отклоняет второй класс как `RuntimeError`, вместо
/// того чтобы давать программе тихо завершиться в неожиданном месте.
fn verify_chunk(chunk: &Chunk) -> RResult<()> {
    let code_len = chunk.code.len();
    let const_len = chunk.constants.len();
    for (i, op) in chunk.code.iter().enumerate() {
        match op {
            OpCode::PushConst(idx) => {
                if *idx >= const_len {
                    return Err(rt_err(format!(
                        "повреждённый байткод: PushConst({}) на позиции {} ссылается за пределы пула констант (размер {})",
                        idx, i, const_len
                    )));
                }
            }
            OpCode::Jump(target) | OpCode::JumpIfFalse(target) => {
                if *target > code_len {
                    return Err(rt_err(format!(
                        "повреждённый байткод: переход на позиции {} ведёт за пределы чанка (target={}, длина чанка={})",
                        i, target, code_len
                    )));
                }
            }
            OpCode::MakeClosure { body, .. } => {
                // Тело лямбды — ОТДЕЛЬНЫЙ Chunk, вложенный прямо в
                // опкод (а не зарегистрированный в CompiledProgram::functions,
                // т.к. у лямбды нет статического имени — см.
                // codegen::compile_expr, ветка Expr::Lambda). Без
                // рекурсивной проверки здесь повреждённый PushConst/Jump
                // внутри тела лямбды прошёл бы верификацию незамеченным
                // и дал бы panic при первом вызове этого конкретного
                // замыкания, а не сразу при Vm::new.
                verify_chunk(body)
                    .map_err(|e| rt_err(format!("в теле замыкания (MakeClosure на позиции {}): {}", i, e.0)))?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Рекурсивно верифицирует главный чанк программы и чанк каждой
/// определённой в ней функции. Вызывается один раз в `Vm::new`, перед
/// началом исполнения — стоимость линейна по размеру байткода и
/// несущественна по сравнению со стоимостью самого исполнения.
fn verify_program(main: &Chunk, functions: &HashMap<String, FunctionDef>) -> RResult<()> {
    verify_chunk(main)?;
    for (name, def) in functions {
        verify_chunk(&def.chunk).map_err(|e| rt_err(format!("в функции '{}': {}", name, e.0)))?;
    }
    Ok(())
}

struct Scope {
    vars: HashMap<String, (Value, bool)>, // name -> (value, mutable)
}

pub struct Vm {
    functions: HashMap<String, FunctionDef>,
    /// Лимит глубины рекурсии — простейшая защита от неконтролируемого
    /// исчерпания стека (sandbox/permission system в широком смысле,
    /// полноценная модель прав доступа — см. docs/ROADMAP.md).
    call_depth: usize,
    max_call_depth: usize,
}

impl Vm {
    /// Создаёт VM для выполнения `program`. Перед созданием выполняет
    /// `verify_program` — статическую проверку байткода (bounds-check
    /// для индексов пула констант и для целей переходов). Возвращает
    /// `Err`, если байткод повреждён/несогласован (что невозможно для
    /// программ, прошедших обычный `codegen::compile()`, но возможно
    /// при ручном конструировании `Chunk`/`CompiledProgram` в обход
    /// компилятора — оба типа публичны). См. `verify_program` выше для
    /// подробного обоснования.
    pub fn new(program: CompiledProgram) -> RResult<(Self, Chunk)> {
        verify_program(&program.main, &program.functions)?;
        Ok((Vm { functions: program.functions, call_depth: 0, max_call_depth: 200 }, program.main))
    }

    pub fn run(&mut self, chunk: &Chunk) -> RResult<Value> {
        let mut scopes = vec![Scope { vars: HashMap::new() }];
        self.run_chunk(chunk, &mut scopes)
    }

    fn run_chunk(&mut self, chunk: &Chunk, scopes: &mut Vec<Scope>) -> RResult<Value> {
        let mut stack: Vec<Value> = Vec::new();
        let mut ip = 0usize;
        while ip < chunk.code.len() {
            match &chunk.code[ip] {
                OpCode::PushConst(idx) => stack.push(chunk.constants[*idx].clone()),
                OpCode::LoadVar(name) => {
                    let v = self.lookup(scopes, name).ok_or_else(|| rt_err(format!("неопределённая переменная '{}'", name)))?;
                    stack.push(v);
                }
                OpCode::StoreVar(name) => {
                    let v = stack.pop().ok_or_else(|| rt_err("пустой стек при StoreVar"))?;
                    self.assign(scopes, name, v.clone())?;
                    stack.push(v);
                }
                OpCode::DefineVar(name, mutable) => {
                    let v = stack.pop().ok_or_else(|| rt_err("пустой стек при DefineVar"))?;
                    scopes.last_mut().unwrap().vars.insert(name.clone(), (v, *mutable));
                }
                OpCode::Pop => {
                    stack.pop();
                }
                OpCode::PushScope => scopes.push(Scope { vars: HashMap::new() }),
                OpCode::PopScope => {
                    scopes.pop();
                }
                OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Mod => {
                    let b = stack.pop().ok_or_else(|| rt_err("пустой стек в арифметике"))?;
                    let a = stack.pop().ok_or_else(|| rt_err("пустой стек в арифметике"))?;
                    stack.push(arith(&chunk.code[ip], a, b)?);
                }
                OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::Gt | OpCode::LtEq | OpCode::GtEq => {
                    let b = stack.pop().ok_or_else(|| rt_err("пустой стек в сравнении"))?;
                    let a = stack.pop().ok_or_else(|| rt_err("пустой стек в сравнении"))?;
                    stack.push(compare(&chunk.code[ip], a, b)?);
                }
                OpCode::And => {
                    let b = stack.pop().ok_or_else(|| rt_err("пустой стек в AND"))?;
                    let a = stack.pop().ok_or_else(|| rt_err("пустой стек в AND"))?;
                    stack.push(Value::Bool(a.is_truthy() && b.is_truthy()));
                }
                OpCode::Or => {
                    let b = stack.pop().ok_or_else(|| rt_err("пустой стек в OR"))?;
                    let a = stack.pop().ok_or_else(|| rt_err("пустой стек в OR"))?;
                    stack.push(Value::Bool(a.is_truthy() || b.is_truthy()));
                }
                OpCode::Neg => {
                    let a = stack.pop().ok_or_else(|| rt_err("пустой стек в NEG"))?;
                    stack.push(match a {
                        Value::Int(i) => Value::Int(i.checked_neg().ok_or_else(|| rt_err("переполнение int при унарном минусе"))?),
                        Value::Float(f) => Value::Float(-f),
                        other => return Err(rt_err(format!("унарный '-' не поддерживается для {}", other.type_name()))),
                    });
                }
                OpCode::Not => {
                    let a = stack.pop().ok_or_else(|| rt_err("пустой стек в NOT"))?;
                    stack.push(Value::Bool(!a.is_truthy()));
                }
                OpCode::JumpIfFalse(target) => {
                    let v = stack.pop().ok_or_else(|| rt_err("пустой стек в JumpIfFalse"))?;
                    if !v.is_truthy() {
                        ip = *target;
                        continue;
                    }
                }
                OpCode::Jump(target) => {
                    ip = *target;
                    continue;
                }
                OpCode::Call(name, argc) => {
                    let mut args = Vec::with_capacity(*argc);
                    for _ in 0..*argc {
                        args.push(stack.pop().ok_or_else(|| rt_err("пустой стек при вызове функции"))?);
                    }
                    args.reverse();
                    let result = self.call(name, args)?;
                    stack.push(result);
                }
                OpCode::CallValue(argc) => {
                    let mut args = Vec::with_capacity(*argc);
                    for _ in 0..*argc {
                        args.push(stack.pop().ok_or_else(|| rt_err("пустой стек при динамическом вызове"))?);
                    }
                    args.reverse();
                    let callee = stack.pop().ok_or_else(|| rt_err("пустой стек: нет callee для CallValue"))?;
                    let result = self.call_value(callee, args)?;
                    stack.push(result);
                }
                // FUSION: перенесено из ветки B. Вызов метода
                // `{TypeName}_{method}` разрешается по фактическому
                // (рантайм) типу объекта — нет статических типов structs
                // в v0.1, поэтому это не может быть статическим
                // `OpCode::Call`, как обычные top-level функции (см.
                // codegen::compile_expr, ветка Expr::MethodCall).
                // Уходит через тот же `Vm::call`, что и обычные функции,
                // — поэтому Ownership/Borrowing для параметра `self`
                // (если метод объявлен с `MUT self`) защищается тем же
                // механизмом `def.param_mut`, что и для обычных функций
                // (см. комментарий у `Vm::call` ниже). Статическая
                // проверка на месте ВЫЗОВА (что `obj` — `var`-связанное
                // имя, если метод требует `MUT self`) НЕ выполняется —
                // задокументированное ограничение v0.1, см.
                // docs/ROADMAP.md и docs/SECURITY.md.
                OpCode::CallMethod { method, argc } => {
                    let mut args = Vec::with_capacity(*argc);
                    for _ in 0..*argc {
                        args.push(stack.pop().ok_or_else(|| rt_err("пустой стек в CallMethod (аргументы)"))?);
                    }
                    args.reverse();
                    let obj = stack.pop().ok_or_else(|| rt_err("пустой стек в CallMethod (объект)"))?;
                    let type_name = obj.type_display_name();
                    let func_name = format!("{}_{}", type_name, method);
                    let mut all_args = vec![obj];
                    all_args.extend(args);
                    let result = self.call(&func_name, all_args)?;
                    stack.push(result);
                }
                OpCode::MakeClosure { params, body } => {
                    // Снимок видимых переменных в момент создания
                    // замыкания — захват ПО ЗНАЧЕНИЮ (см. подробное
                    // обоснование в `runtime::Value::Closure`). Обходим
                    // стек scope от самой ВНЕШНЕЙ к самой ВНУТРЕННЕЙ,
                    // чтобы более внутренние (более поздние) объявления
                    // того же имени корректно перекрывали внешние в
                    // плоской `captured`-карте — это даёт обычную
                    // лексическую семантику тени (shadowing). Сохраняем
                    // и значение, и mutable-флаг (см. `ClosureValue::captured`).
                    let mut captured = HashMap::new();
                    for scope in scopes.iter() {
                        for (name, (value, mutable)) in scope.vars.iter() {
                            captured.insert(name.clone(), (value.clone(), *mutable));
                        }
                    }
                    let closure = ClosureValue { params: params.clone(), chunk: body.clone(), captured };
                    stack.push(Value::Closure(Rc::new(closure)));
                }
                OpCode::Print(argc) => {
                    let mut parts = Vec::with_capacity(*argc);
                    for _ in 0..*argc {
                        parts.push(stack.pop().ok_or_else(|| rt_err("пустой стек при PRINT"))?);
                    }
                    parts.reverse();
                    let line = parts.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ");
                    println!("{}", line);
                }
                OpCode::MakeArray(n) => {
                    let mut items = Vec::with_capacity(*n);
                    for _ in 0..*n {
                        items.push(stack.pop().ok_or_else(|| rt_err("пустой стек при создании массива"))?);
                    }
                    items.reverse();
                    stack.push(Value::Array(Rc::new(RefCell::new(items))));
                }
                OpCode::Index => {
                    let idx = stack.pop().ok_or_else(|| rt_err("пустой стек при индексации"))?;
                    let target = stack.pop().ok_or_else(|| rt_err("пустой стек при индексации"))?;
                    stack.push(index_get(&target, &idx)?);
                }
                OpCode::IndexAssign => {
                    let value = stack.pop().ok_or_else(|| rt_err("пустой стек при IndexAssign"))?;
                    let idx = stack.pop().ok_or_else(|| rt_err("пустой стек при IndexAssign"))?;
                    let target = stack.pop().ok_or_else(|| rt_err("пустой стек при IndexAssign"))?;
                    index_set(&target, &idx, value.clone())?;
                    stack.push(value);
                }
                // FUSION: три опкода ниже перенесены из ветки B без
                // изменений в логике (ownership-проверки для FieldAssign
                // уже выполнены раньше, в semantic — см.
                // semantic::check_mutation_target, ветка FieldAccess).
                OpCode::MakeStruct { type_name, fields } => {
                    // Значения полей на стеке — в том же порядке, что
                    // имена в `fields`. Снимаем их и собираем HashMap.
                    let mut field_map = HashMap::new();
                    let mut values = Vec::with_capacity(fields.len());
                    for _ in 0..fields.len() {
                        values.push(stack.pop().ok_or_else(|| rt_err("пустой стек при MakeStruct"))?);
                    }
                    values.reverse();
                    for (name, value) in fields.iter().zip(values.into_iter()) {
                        field_map.insert(name.clone(), value);
                    }
                    stack.push(Value::Struct {
                        type_name: type_name.clone(),
                        fields: Rc::new(RefCell::new(field_map)),
                    });
                }
                OpCode::GetField(field) => {
                    let obj = stack.pop().ok_or_else(|| rt_err("пустой стек при GetField"))?;
                    match obj {
                        Value::Struct { fields, .. } => {
                            let val = fields.borrow().get(field).cloned().unwrap_or(Value::Nil);
                            stack.push(val);
                        }
                        other => return Err(rt_err(format!(
                            "доступ к полю '{}' не поддерживается для типа {}",
                            field, other.type_display_name()
                        ))),
                    }
                }
                OpCode::SetField(field) => {
                    let value = stack.pop().ok_or_else(|| rt_err("пустой стек при SetField (value)"))?;
                    let obj = stack.pop().ok_or_else(|| rt_err("пустой стек при SetField (obj)"))?;
                    match &obj {
                        Value::Struct { fields, .. } => {
                            fields.borrow_mut().insert(field.clone(), value.clone());
                            stack.push(value);
                        }
                        other => return Err(rt_err(format!(
                            "присваивание поля '{}' не поддерживается для типа {}",
                            field, other.type_display_name()
                        ))),
                    }
                }
                OpCode::Return(has_value) => {
                    return Ok(if *has_value { stack.pop().unwrap_or(Value::Nil) } else { Value::Nil });
                }
            }
            ip += 1;
        }
        Ok(Value::Nil)
    }

    fn lookup(&self, scopes: &[Scope], name: &str) -> Option<Value> {
        for scope in scopes.iter().rev() {
            if let Some((v, _)) = scope.vars.get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    fn assign(&self, scopes: &mut [Scope], name: &str, value: Value) -> RResult<()> {
        for scope in scopes.iter_mut().rev() {
            if let Some((v, mutable)) = scope.vars.get_mut(name) {
                if !*mutable {
                    return Err(rt_err(format!("невозможно изменить immutable-переменную '{}'", name)));
                }
                *v = value;
                return Ok(());
            }
        }
        Err(rt_err(format!("неопределённая переменная '{}'", name)))
    }

    /// FUSION-ПРИМЕЧАНИЕ (см. MIGRATION_REPORT.md, "Найденные при
    /// слиянии регрессии"): мутабельность параметра в его собственном
    /// кадре вызова берётся из сигнатуры функции (`def.param_mut`,
    /// `MUT` -> true, иначе false), а НЕ всегда `true`. Это VM-уровень
    /// defense-in-depth той же проверки, что уже выполнена в
    /// `src/semantic/mod.rs::check_call_borrows` на этапе компиляции.
    /// Хардкод `true` для всех параметров — это именно та дыра, которая
    /// в одной из родительских веток была закрыта (см. docs/SECURITY.md,
    /// "Ownership/Borrowing на границе вызова функции"), а затем
    /// случайно вновь открыта в другой ветке при добавлении замыканий
    /// (`Param` лишился поля `mutable`, `FunctionDef` лишился
    /// `param_mut`, и здесь стояло `(a, true)` для всех параметров без
    /// исключения). Слияние восстанавливает фикс.
    fn call(&mut self, name: &str, args: Vec<Value>) -> RResult<Value> {
        if is_builtin(name) {
            return call_builtin(name, args).map_err(rt_err);
        }
        self.call_depth += 1;
        if self.call_depth > self.max_call_depth {
            self.call_depth -= 1;
            return Err(rt_err("превышена максимальная глубина рекурсии (защита от stack overflow)"));
        }
        let def = self
            .functions
            .get(name)
            .cloned()
            .ok_or_else(|| rt_err(format!("неопределённая функция '{}'", name)))?;
        if def.params.len() != args.len() {
            self.call_depth -= 1;
            return Err(rt_err(format!("функция '{}' ожидает {} аргумент(ов), передано {}", name, def.params.len(), args.len())));
        }
        let mut base = HashMap::new();
        for ((p, a), m) in def.params.iter().zip(args.into_iter()).zip(def.param_mut.iter()) {
            base.insert(p.clone(), (a, *m));
        }
        let mut frame_scopes = vec![Scope { vars: base }];
        let result = self.run_chunk(&def.chunk, &mut frame_scopes);
        self.call_depth -= 1;
        result
    }

    /// Вызывает значение `callee` (ожидается `Value::Closure`) с
    /// аргументами `args` — используется для `OpCode::CallValue`, когда
    /// вызываемое выражение не было статически известной top-level
    /// функцией на этапе компиляции (см. `codegen::Compiler::known_functions`
    /// и комментарий у `Expr::Call` в `codegen/mod.rs`).
    ///
    /// Начальные scope-переменные кадра — это снимок `captured`
    /// (захваченное окружение на момент создания замыкания), поверх
    /// которого кладётся ОТДЕЛЬНАЯ scope с параметрами текущего вызова.
    /// Параметры замыкания (в отличие от параметров top-level функций)
    /// всегда mutable внутри своего кадра — у `Expr::Lambda` нет `MUT`-
    /// аннотаций (см. `ast::Expr::Lambda`), поэтому здесь нет аналога
    /// `param_mut` из `FunctionDef`.
    fn call_value(&mut self, callee: Value, args: Vec<Value>) -> RResult<Value> {
        let closure = match callee {
            Value::Closure(c) => c,
            other => {
                return Err(rt_err(format!(
                    "значение типа {} не является вызываемым (ожидалось замыкание)",
                    other.type_name()
                )))
            }
        };
        if closure.params.len() != args.len() {
            return Err(rt_err(format!(
                "замыкание ожидает {} аргумент(ов), передано {}",
                closure.params.len(),
                args.len()
            )));
        }
        self.call_depth += 1;
        if self.call_depth > self.max_call_depth {
            self.call_depth -= 1;
            return Err(rt_err("превышена максимальная глубина рекурсии (защита от stack overflow)"));
        }
        let captured_scope = Scope { vars: closure.captured.iter().map(|(k, (v, m))| (k.clone(), (v.clone(), *m))).collect() };
        let mut param_vars = HashMap::new();
        for (p, a) in closure.params.iter().zip(args.into_iter()) {
            param_vars.insert(p.clone(), (a, true));
        }
        let mut frame_scopes = vec![captured_scope, Scope { vars: param_vars }];
        let result = self.run_chunk(&closure.chunk, &mut frame_scopes);
        self.call_depth -= 1;
        result
    }
}

fn arith(op: &OpCode, a: Value, b: Value) -> RResult<Value> {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => Ok(Int(match op {
            OpCode::Add => x.checked_add(y).ok_or_else(|| rt_err("переполнение int при сложении"))?,
            OpCode::Sub => x.checked_sub(y).ok_or_else(|| rt_err("переполнение int при вычитании"))?,
            OpCode::Mul => x.checked_mul(y).ok_or_else(|| rt_err("переполнение int при умножении"))?,
            OpCode::Div => {
                if y == 0 {
                    return Err(rt_err("деление на ноль"));
                }
                x.checked_div(y).ok_or_else(|| rt_err("переполнение int при делении"))?
            }
            OpCode::Mod => {
                if y == 0 {
                    return Err(rt_err("деление на ноль (остаток)"));
                }
                x % y
            }
            _ => unreachable!(),
        })),
        (Float(x), Float(y)) => Ok(Float(match op {
            OpCode::Add => x + y,
            OpCode::Sub => x - y,
            OpCode::Mul => x * y,
            OpCode::Div => x / y,
            OpCode::Mod => x % y,
            _ => unreachable!(),
        })),
        (Int(x), Float(y)) => arith(op, Float(x as f64), Float(y)),
        (Float(x), Int(y)) => arith(op, Float(x), Float(y as f64)),
        (Str(x), Str(y)) => match op {
            OpCode::Add => Ok(Str(x + &y)),
            _ => Err(rt_err("для строк допустима только операция '+' (конкатенация)")),
        },
        (a, b) => Err(rt_err(format!("несовместимые типы в арифметической операции: {} и {}", a.type_name(), b.type_name()))),
    }
}

fn compare(op: &OpCode, a: Value, b: Value) -> RResult<Value> {
    use Value::*;
    let ordering = match (&a, &b) {
        (Int(x), Int(y)) => x.partial_cmp(y),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (Int(x), Float(y)) => (*x as f64).partial_cmp(y),
        (Float(x), Int(y)) => x.partial_cmp(&(*y as f64)),
        (Str(x), Str(y)) => x.partial_cmp(y),
        (Bool(x), Bool(y)) => x.partial_cmp(y),
        _ => None,
    };
    if matches!(op, OpCode::Eq | OpCode::NotEq) {
        let eq = values_equal(&a, &b);
        return Ok(Value::Bool(if matches!(op, OpCode::Eq) { eq } else { !eq }));
    }
    let ord = ordering.ok_or_else(|| rt_err(format!("несравнимые типы: {} и {}", a.type_name(), b.type_name())))?;
    Ok(Value::Bool(match op {
        OpCode::Lt => ord.is_lt(),
        OpCode::Gt => ord.is_gt(),
        OpCode::LtEq => ord.is_le(),
        OpCode::GtEq => ord.is_ge(),
        _ => unreachable!(),
    }))
}

fn values_equal(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => x == y,
        (Float(x), Float(y)) => x == y,
        (Int(x), Float(y)) => (*x as f64) == *y,
        (Float(x), Int(y)) => *x == (*y as f64),
        (Str(x), Str(y)) => x == y,
        (Bool(x), Bool(y)) => x == y,
        (Nil, Nil) => true,
        // FUSION/НАЙДЕНО ПРИ СЛИЯНИИ (см. MIGRATION_REPORT.md): до этого
        // исправления здесь стояло `_ => false` для всех остальных пар,
        // включая `(Array, Array)` — то есть `LET a=[1]; VAR b=a; PRINT(a==b);`
        // печатало `ложь`, хотя `a` и `b` — один и тот же объект
        // (`Rc::ptr_eq`). Это была реальная регрессия в ветке A: ветка B
        // (откуда перенесён Struct) уже содержала identity-equality для
        // Array, но в ветке A эта же логика для Array отсутствовала
        // (Array появился в A независимо от Struct, и ветка `_ => false`
        // никогда не обновлялась). Identity (pointer) equality —
        // стандартная семантика для reference-типов: `[1,2] == [1,2]`
        // даёт `ложь` (два разных объекта), но `let a=[1,2]; let b=a;
        // a==b` даёт `истина` (один объект). См. docs/LANGUAGE_SPEC.md.
        (Array(a), Array(b)) => Rc::ptr_eq(a, b),
        (Struct { fields: fa, .. }, Struct { fields: fb, .. }) => Rc::ptr_eq(fa, fb),
        _ => false,
    }
}

fn index_get(target: &Value, idx: &Value) -> RResult<Value> {
    match (target, idx) {
        (Value::Array(items), Value::Int(i)) => {
            let items = items.borrow();
            let i = normalize_index(*i, items.len())?;
            Ok(items[i].clone())
        }
        (Value::Str(s), Value::Int(i)) => {
            let chars: Vec<char> = s.chars().collect();
            let i = normalize_index(*i, chars.len())?;
            Ok(Value::Str(chars[i].to_string()))
        }
        (t, _) => Err(rt_err(format!("индексация не поддерживается для типа {}", t.type_name()))),
    }
}

fn index_set(target: &Value, idx: &Value, value: Value) -> RResult<()> {
    match (target, idx) {
        (Value::Array(items), Value::Int(i)) => {
            let mut items = items.borrow_mut();
            let i = normalize_index(*i, items.len())?;
            items[i] = value;
            Ok(())
        }
        (t, _) => Err(rt_err(format!("индексное присваивание не поддерживается для типа {}", t.type_name()))),
    }
}

fn normalize_index(i: i64, len: usize) -> RResult<usize> {
    if i < 0 || i as usize >= len {
        return Err(rt_err(format!("индекс {} выходит за границы массива длины {}", i, len)));
    }
    Ok(i as usize)
}
