//! Кодогенератор SGA: AST -> байткод.
//!
//! Каждая функция компилируется в собственный `Chunk` (массив инструкций +
//! пул констант). Переходы (`Jump`/`JumpIfFalse`) — абсолютные индексы внутри
//! своего чанка. `break`/`continue` реализованы классическим backpatching:
//! при компиляции `break` эмиттируется placeholder-`Jump`, адрес которого
//! фиксируется в стеке контекстов циклов и патчится постфактум, когда стаёт
//! известен адрес конца цикла.

use crate::ast::{BinOp, Expr, Program, Stmt, UnOp};
use crate::runtime::{is_builtin, Value};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

// `OpCode`/`Chunk`/`FunctionDef`/`CompiledProgram` определены в
// `runtime` (а не здесь), чтобы избежать циклической зависимости
// `runtime -> codegen`, нужной для `Value::Closure { chunk: Rc<Chunk> }`
// — см. подробное объяснение в `src/runtime/mod.rs`. Ре-экспортируем их
// отсюда, чтобы `sga::codegen::Chunk` и т.п. продолжали работать для
// внешнего кода (обратная совместимость публичного API).
pub use crate::runtime::{Chunk, CompiledProgram, FunctionDef, OpCode};

struct LoopCtx {
    break_patches: Vec<usize>,
    continue_patches: Vec<usize>,
}

pub struct Compiler {
    chunk: Chunk,
    loop_stack: Vec<LoopCtx>,
    /// Имена top-level функций, известные на момент компиляции (собраны
    /// `compile()` первым проходом по `program`, как и в
    /// `semantic::Analyzer::analyze`). Нужно для `Expr::Call(name, args)`:
    /// если `name` входит сюда (или является builtin) — эмиттируется
    /// статический `OpCode::Call(name, argc)` (быстрый путь, поиск по
    /// строке в `Vm::functions`). Если `name` НЕ входит сюда — это
    /// обращение к переменной, предположительно содержащей
    /// `Value::Closure` (например, `let f = FN(x){...}; f(1);`), и
    /// эмиттируется `LoadVar(name)` + `OpCode::CallValue(argc)`,
    /// который снимает callee со стека и вызывает его динамически (см.
    /// `vm::run_chunk`, ветка `CallValue`). Разделяется через
    /// `Rc<HashSet<String>>`, а не клонируется заново для каждого
    /// вложенного `Compiler` (тело функции/лямбды) — набор имён общий
    /// для всей программы и не меняется во время компиляции.
    known_functions: Rc<HashSet<String>>,
}

pub fn compile(program: &Program) -> CompiledProgram {
    let mut known_functions = HashSet::new();
    for stmt in program {
        if let Stmt::FnDecl { name, .. } = stmt {
            known_functions.insert(name.clone());
        }
    }
    let known_functions = Rc::new(known_functions);

    let mut functions = HashMap::new();
    let mut main_compiler = Compiler {
        chunk: Chunk::default(),
        loop_stack: Vec::new(),
        known_functions: known_functions.clone(),
    };

    // FUSION-ПРИМЕЧАНИЕ: значение ПОСЛЕДНЕГО top-level statement'а,
    // компилируемого в main-чанк (то есть последнего НЕ-FnDecl statement'а
    // в `program` — FnDecl компилируются отдельно, в `functions`, и не
    // попадают в поток инструкций main-чанка), если это `Stmt::ExprStmt`,
    // становится результатом выполнения всей программы (значением,
    // возвращаемым `Vm::run`/`sga::run_source`), а не отбрасывается
    // оператором `Pop`, как любой другой expression-statement.
    //
    // Без этого `let f = FN(x){...}; f(1);` как программа (где `f(1);`
    // — последний statement) ВСЕГДА возвращал бы `Value::Nil`, даже
    // если `f(1)` вычисляется в `Value::Int(...)` — единственный способ
    // получить ненулевое значение из `run_source()` был бы через явный
    // `RETURN` на верхнем уровне, что синтаксически невозможно (`RETURN`
    // вне функции — ошибка семантики, см. `semantic::check_stmt`).
    // Это реальный дефект, обнаруженный здесь эмпирически: тесты на
    // замыкания (`tests/integration_test.rs`, секция "Замыкания") писали
    // `assert_eq!(run(&src).unwrap(), Value::Int(5))` для программ вида
    // `let f = ...; f(1);`, рассчитывая именно на такую семантику "значение
    // последнего top-level выражения — результат скрипта" — естественную
    // для языка, где функции/блоки порой используются как REPL-подобные
    // скрипты. Подробности — см. MIGRATION_REPORT.md.
    let last_main_stmt_idx = program
        .iter()
        .rposition(|s| !matches!(s, Stmt::FnDecl { .. }));

    for (i, stmt) in program.iter().enumerate() {
        if let Stmt::FnDecl {
            name, params, body, ..
        } = stmt
        {
            let mut fc = Compiler {
                chunk: Chunk::default(),
                loop_stack: Vec::new(),
                known_functions: known_functions.clone(),
            };
            for s in body {
                fc.compile_stmt(s);
            }
            // если функция не завершилась явным return — неявно вернуть Nil
            fc.chunk.code.push(OpCode::Return(false));
            // Аннотации типов стираются здесь (type erasure) — VM работает
            // только с именами параметров, статическая проверка уже
            // выполнена раньше в пайплайне (src/typechecker/mod.rs).
            // `param_mut` (Ownership/Borrowing) НЕ стирается — это
            // runtime-релевантный флаг, нужный `Vm::call` для defense-in-
            // depth повторной проверки мутабельности на границе вызова
            // (см. docs/SECURITY.md).
            let param_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
            let param_mut: Vec<bool> = params.iter().map(|p| p.mutable).collect();
            functions.insert(
                name.clone(),
                FunctionDef {
                    params: param_names,
                    param_mut,
                    chunk: fc.chunk,
                },
            );
        } else if Some(i) == last_main_stmt_idx {
            main_compiler.compile_tail_stmt(stmt);
        } else {
            main_compiler.compile_stmt(stmt);
        }
    }
    // Безопасный fallback: если последний statement НЕ был `ExprStmt`
    // (например, `IF`/`WHILE`/`VarDecl`), `compile_tail_stmt` делегирует
    // в обычный `compile_stmt`, который не эмиттирует `Return`, — этот
    // `Return(false)` гарантирует, что main-чанк всегда корректно
    // завершается. Если последний statement БЫЛ `ExprStmt`,
    // `compile_tail_stmt` уже эмиттировал `OpCode::Return(true)`, и этот
    // код становится недостижимым (безвредный мёртвый байткод в самом
    // конце чанка — не влияет на верификатор, см. `vm::verify_chunk`).
    main_compiler.chunk.code.push(OpCode::Return(false));
    CompiledProgram {
        main: main_compiler.chunk,
        functions,
    }
}

impl Compiler {
    /// Компилирует ПОСЛЕДНИЙ statement главного потока программы. Для
    /// `Stmt::ExprStmt` — в отличие от обычного `compile_stmt` — НЕ
    /// эмиттирует `Pop` после вычисления выражения, а сразу эмиттирует
    /// `OpCode::Return(true)`, оставляя значение выражения результатом
    /// всей программы. Для любого другого варианта `Stmt` — поведение
    /// идентично обычному `compile_stmt` (без специальной обработки;
    /// программа в этом случае просто завершается с `Value::Nil`, как и
    /// раньше). См. подробное обоснование в `compile()`.
    fn compile_tail_stmt(&mut self, stmt: &Stmt) {
        if let Stmt::ExprStmt(e) = stmt {
            self.compile_expr(e);
            self.emit(OpCode::Return(true));
        } else {
            self.compile_stmt(stmt);
        }
    }

    fn emit(&mut self, op: OpCode) -> usize {
        self.chunk.code.push(op);
        self.chunk.code.len() - 1
    }

    fn here(&self) -> usize {
        self.chunk.code.len()
    }

    fn add_const(&mut self, v: Value) -> usize {
        self.chunk.constants.push(v);
        self.chunk.constants.len() - 1
    }

    fn patch_jump(&mut self, pos: usize, target: usize) {
        self.chunk.code[pos] = match self.chunk.code[pos] {
            OpCode::Jump(_) => OpCode::Jump(target),
            OpCode::JumpIfFalse(_) => OpCode::JumpIfFalse(target),
            ref other => other.clone(),
        };
    }

    fn compile_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::VarDecl {
                name,
                value,
                mutable,
                ..
            } => {
                self.compile_expr(value);
                self.emit(OpCode::DefineVar(name.clone(), *mutable));
            }
            Stmt::ExprStmt(e) => {
                self.compile_expr(e);
                self.emit(OpCode::Pop);
            }
            Stmt::Print(args) => {
                let n = args.len();
                for a in args {
                    self.compile_expr(a);
                }
                self.emit(OpCode::Print(n));
            }
            Stmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.compile_expr(cond);
                let jif = self.emit(OpCode::JumpIfFalse(0));
                self.emit(OpCode::PushScope);
                for s in then_branch {
                    self.compile_stmt(s);
                }
                self.emit(OpCode::PopScope);
                if let Some(else_b) = else_branch {
                    let jend = self.emit(OpCode::Jump(0));
                    let else_start = self.here();
                    self.patch_jump(jif, else_start);
                    self.emit(OpCode::PushScope);
                    for s in else_b {
                        self.compile_stmt(s);
                    }
                    self.emit(OpCode::PopScope);
                    let end = self.here();
                    self.patch_jump(jend, end);
                } else {
                    let end = self.here();
                    self.patch_jump(jif, end);
                }
            }
            Stmt::While { cond, body } => {
                let loop_start = self.here();
                self.compile_expr(cond);
                let jif = self.emit(OpCode::JumpIfFalse(0));
                self.loop_stack.push(LoopCtx {
                    break_patches: Vec::new(),
                    continue_patches: Vec::new(),
                });
                self.emit(OpCode::PushScope);
                for s in body {
                    self.compile_stmt(s);
                }
                self.emit(OpCode::PopScope);
                self.emit(OpCode::Jump(loop_start));
                let end = self.here();
                self.patch_jump(jif, end);
                let ctx = self.loop_stack.pop().unwrap();
                for p in ctx.break_patches {
                    self.patch_jump(p, end);
                }
                for p in ctx.continue_patches {
                    self.patch_jump(p, loop_start);
                }
            }
            Stmt::ForIn {
                var,
                start,
                end,
                body,
            } => {
                self.compile_expr(start);
                self.emit(OpCode::DefineVar(var.clone(), true));
                let cond_start = self.here();
                self.emit(OpCode::LoadVar(var.clone()));
                self.compile_expr(end);
                self.emit(OpCode::Lt);
                let jif = self.emit(OpCode::JumpIfFalse(0));
                self.emit(OpCode::PushScope);
                self.loop_stack.push(LoopCtx {
                    break_patches: Vec::new(),
                    continue_patches: Vec::new(),
                });
                for s in body {
                    self.compile_stmt(s);
                }
                self.emit(OpCode::PopScope);
                let incr_pos = self.here();
                self.emit(OpCode::LoadVar(var.clone()));
                let one = self.add_const(Value::Int(1));
                self.emit(OpCode::PushConst(one));
                self.emit(OpCode::Add);
                self.emit(OpCode::StoreVar(var.clone()));
                self.emit(OpCode::Pop);
                self.emit(OpCode::Jump(cond_start));
                let loop_end = self.here();
                self.patch_jump(jif, loop_end);
                let ctx = self.loop_stack.pop().unwrap();
                for p in ctx.break_patches {
                    self.patch_jump(p, loop_end);
                }
                for p in ctx.continue_patches {
                    self.patch_jump(p, incr_pos);
                }
            }
            Stmt::FnDecl { .. } => {
                // С учётом запрета вложенных FN на уровне парсера
                // (см. parser/mod.rs::parse_stmt, top_level) эта ветка
                // в codegen синтаксически недостижима для корректно
                // распарсенных программ. Оставлена как no-op
                // defense-in-depth на случай конструирования AST в обход
                // парсера. Верхнеуровневая компиляция функций
                // обрабатывается в `compile()` (см. выше).
            }
            Stmt::Return(expr) => match expr {
                Some(e) => {
                    self.compile_expr(e);
                    self.emit(OpCode::Return(true));
                }
                None => {
                    self.emit(OpCode::Return(false));
                }
            },
            Stmt::Break => {
                let pos = self.emit(OpCode::Jump(0));
                if let Some(ctx) = self.loop_stack.last_mut() {
                    ctx.break_patches.push(pos);
                }
            }
            Stmt::Continue => {
                let pos = self.emit(OpCode::Jump(0));
                if let Some(ctx) = self.loop_stack.last_mut() {
                    ctx.continue_patches.push(pos);
                }
            }
            Stmt::Block(stmts) => {
                self.emit(OpCode::PushScope);
                for s in stmts {
                    self.compile_stmt(s);
                }
                self.emit(OpCode::PopScope);
            }
            Stmt::Import(_) => {
                // Недостижимо в нормальном пайплайне: semantic-анализ
                // отклоняет нерезолвленный `Stmt::Import` раньше. No-op
                // defense-in-depth.
            }
            Stmt::StructDecl { .. } => {
                // FUSION: перенесено из ветки B. StructDecl не производит
                // байткода — pure-compile-time объявление. Тип
                // зарегистрирован в `semantic::Analyzer::structs` (первый
                // проход `analyze`) и используется при проверке
                // `Expr::StructLit`. В рантайме struct создаётся через
                // `OpCode::MakeStruct`.
            }
        }
    }

    fn compile_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Int(v) => {
                let idx = self.add_const(Value::Int(*v));
                self.emit(OpCode::PushConst(idx));
            }
            Expr::Float(v) => {
                let idx = self.add_const(Value::Float(*v));
                self.emit(OpCode::PushConst(idx));
            }
            Expr::Str(s) => {
                let idx = self.add_const(Value::Str(s.clone()));
                self.emit(OpCode::PushConst(idx));
            }
            Expr::Bool(b) => {
                let idx = self.add_const(Value::Bool(*b));
                self.emit(OpCode::PushConst(idx));
            }
            Expr::Nil => {
                let idx = self.add_const(Value::Nil);
                self.emit(OpCode::PushConst(idx));
            }
            Expr::Ident(name) => {
                self.emit(OpCode::LoadVar(name.clone()));
            }
            Expr::Array(items) => {
                let n = items.len();
                for it in items {
                    self.compile_expr(it);
                }
                self.emit(OpCode::MakeArray(n));
            }
            Expr::Index(target, idx) => {
                self.compile_expr(target);
                self.compile_expr(idx);
                self.emit(OpCode::Index);
            }
            Expr::Unary(op, e) => {
                self.compile_expr(e);
                self.emit(match op {
                    UnOp::Neg => OpCode::Neg,
                    UnOp::Not => OpCode::Not,
                });
            }
            Expr::Binary(op, l, r) => {
                self.compile_expr(l);
                self.compile_expr(r);
                self.emit(match op {
                    BinOp::Add => OpCode::Add,
                    BinOp::Sub => OpCode::Sub,
                    BinOp::Mul => OpCode::Mul,
                    BinOp::Div => OpCode::Div,
                    BinOp::Mod => OpCode::Mod,
                    BinOp::Eq => OpCode::Eq,
                    BinOp::NotEq => OpCode::NotEq,
                    BinOp::Lt => OpCode::Lt,
                    BinOp::Gt => OpCode::Gt,
                    BinOp::LtEq => OpCode::LtEq,
                    BinOp::GtEq => OpCode::GtEq,
                    BinOp::And => OpCode::And,
                    BinOp::Or => OpCode::Or,
                });
            }
            Expr::Call(name, args) => {
                let n = args.len();
                if self.known_functions.contains(name) || is_builtin(name) {
                    // Статически известная top-level функция или builtin
                    // — быстрый путь: имя зашито прямо в опкод,
                    // `Vm::call` ищет его в `self.functions` (HashMap
                    // <String, FunctionDef>) или диспетчеризует в
                    // `call_builtin`.
                    for a in args {
                        self.compile_expr(a);
                    }
                    self.emit(OpCode::Call(name.clone(), n));
                } else {
                    // `name` не входит в множество top-level функций и
                    // не builtin — это обращение к переменной (semantic
                    // уже гарантировала, что переменная с таким именем
                    // видна — см. `semantic::check_expr`, ветка
                    // `Expr::Call`, `is_var`). Загружаем её значение и
                    // вызываем динамически через `CallValue`: VM в
                    // рантайме проверит, что это действительно
                    // `Value::Closure` (см. `vm::run_chunk`, ветка
                    // `CallValue`), и даст понятную `RuntimeError`, если
                    // нет (например, `let x = 5; x();`).
                    self.emit(OpCode::LoadVar(name.clone()));
                    for a in args {
                        self.compile_expr(a);
                    }
                    self.emit(OpCode::CallValue(n));
                }
            }
            Expr::CallExpr(callee, args) => {
                // Общий случай: callee — произвольное выражение, не
                // статическое имя. Компилируется так же, как
                // вызов-через-переменную выше (`LoadVar` + `CallValue`),
                // только вместо `LoadVar(name)` — полноценная компиляция
                // произвольного выражения (например, тела лямбды для
                // IIFE: `OpCode::MakeClosure`, см. `Expr::Lambda` ниже).
                self.compile_expr(callee);
                let n = args.len();
                for a in args {
                    self.compile_expr(a);
                }
                self.emit(OpCode::CallValue(n));
            }
            Expr::Lambda { params, body } => {
                // Тело лямбды компилируется в ОТДЕЛЬНЫЙ Chunk новым
                // Compiler'ом — точно так же, как тело top-level FnDecl
                // компилируется в compile() — а не инлайнится в текущий
                // chunk. Получившийся Chunk не добавляется в
                // CompiledProgram::functions (у лямбды нет статического
                // имени), а упаковывается в OpCode::MakeClosure прямо
                // здесь, в потоке инструкций текущей функции — VM при
                // исполнении этого опкода создаст Value::Closure,
                // содержащий Rc<Chunk> и снимок текущего видимого
                // окружения (см. vm::run_chunk, ветка MakeClosure).
                let mut lambda_compiler = Compiler {
                    chunk: Chunk::default(),
                    loop_stack: Vec::new(),
                    known_functions: self.known_functions.clone(),
                };
                for s in body {
                    lambda_compiler.compile_stmt(s);
                }
                lambda_compiler.chunk.code.push(OpCode::Return(false));
                self.emit(OpCode::MakeClosure {
                    params: params.clone(),
                    body: Rc::new(lambda_compiler.chunk),
                });
            }
            Expr::Assign(name, value) => {
                self.compile_expr(value);
                self.emit(OpCode::StoreVar(name.clone()));
            }
            Expr::IndexAssign(target, idx, value) => {
                self.compile_expr(target);
                self.compile_expr(idx);
                self.compile_expr(value);
                self.emit(OpCode::IndexAssign);
            }
            // FUSION: ниже — перенесено из ветки B без изменений
            // (codegen для structs не пересекается с ownership-моделью —
            // та проверка выполнена раньше, в semantic).
            Expr::StructLit { type_name, fields } => {
                // Компилируем значения полей в порядке объявления поля в
                // литерале (не порядке объявления в STRUCT), чтобы
                // MakeStruct в VM мог сопоставить позицию на стеке с
                // именем поля. Поля, не упомянутые в литерале, получают
                // значение Nil (см. vm::OpCode::MakeStruct).
                let field_names: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
                for (_, expr) in fields {
                    self.compile_expr(expr);
                }
                self.emit(OpCode::MakeStruct {
                    type_name: type_name.clone(),
                    fields: field_names,
                });
            }
            Expr::FieldAccess(obj, field) => {
                self.compile_expr(obj);
                self.emit(OpCode::GetField(field.clone()));
            }
            Expr::FieldAssign(obj, field, value) => {
                self.compile_expr(obj);
                self.compile_expr(value);
                self.emit(OpCode::SetField(field.clone()));
            }
            Expr::MethodCall(obj, method_name, args) => {
                // Компилируем объект (self) и аргументы на стек.
                // Разрешение имени функции (`TypeName_method`) происходит
                // в рантайме через OpCode::CallMethod, которое знает
                // фактический тип объекта.
                self.compile_expr(obj);
                for a in args {
                    self.compile_expr(a);
                }
                self.emit(OpCode::CallMethod {
                    method: method_name.clone(),
                    argc: args.len(),
                });
            }
        }
    }
}
