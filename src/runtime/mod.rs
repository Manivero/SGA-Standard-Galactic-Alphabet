//! Runtime-представление значений SGA, байткод и встроенные функции.
//!
//! Здесь реализовано РЕАЛЬНОЕ подмножество "стандартной библиотеки":
//! len, push, to_string, to_int, to_float. Полноценные std/io, std/fs,
//! std/net, std/http, std/crypto, std/json, std/thread, std/async — это
//! отдельная задача, требующая системного API и асинхронного рантайма;
//! статус — см. docs/ROADMAP.md. Здесь не делается вид, что они есть.
//!
//! АРХИТЕКТУРНАЯ ЗАМЕТКА (FUSION): `OpCode`/`Chunk`/`FunctionDef`/
//! `CompiledProgram` исторически определялись в `codegen`, а `Value` —
//! здесь, в `runtime`, с зависимостью `codegen -> runtime` (т.к.
//! `Chunk::constants: Vec<Value>`). Это работало, пока `Value` не нужно
//! было хранить внутри себя байткод. Введение `Value::Closure`
//! (замыкание = байткод тела + захваченное окружение) потребовало бы
//! обратной зависимости `runtime -> codegen`, что дало бы ЦИКЛ модулей.
//! Поэтому байткодовые типы ПЕРЕЕХАЛИ сюда, в `runtime`, а `codegen`
//! теперь их `pub use` ре-экспортирует — `sga::codegen::Chunk` и т.п.
//! продолжают работать для внешнего кода без изменений (обратная
//! совместимость публичного API сохранена), но "источник истины" для
//! этих типов теперь здесь.
//!
//! `FunctionDef::param_mut` (Ownership/Borrowing, roadmap-пункт 2) —
//! параллельный `params` массив той же длины, флаг `MUT` для каждого
//! параметра. Используется `Vm::call` (src/vm/mod.rs), чтобы привязать
//! параметр как mutable/immutable именно так, как объявлено в сигнатуре
//! функции, а не всегда как mutable — см. docs/SECURITY.md
//! ("Ownership/Borrowing на границе вызова функции").

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum OpCode {
    PushConst(usize),
    LoadVar(String),
    StoreVar(String),
    DefineVar(String, bool),
    Pop,
    PushScope,
    PopScope,
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
    Neg,
    Not,
    JumpIfFalse(usize),
    Jump(usize),
    Call(String, usize),
    /// Вызов значения, лежащего на вершине стека (а не функции по
    /// статически известному имени) — используется для вызова
    /// замыканий/значений-функций: `let f = FN(x) { ... }; f(1);`.
    /// На момент исполнения этого опкода на стеке (снизу вверх) лежат:
    /// callee (Value, обычно Value::Closure), затем `argc` аргументов.
    CallValue(usize),
    Print(usize),
    MakeArray(usize),
    Index,
    IndexAssign,
    Return(bool),
    /// Создаёт `Value::Closure` из чанка `body` (компилируется как
    /// отдельная мини-функция, как и обычные `FN`) и текущего
    /// окружения. См. `Value::Closure` и docs/LANGUAGE_SPEC.md, §5.2.
    MakeClosure {
        params: Vec<String>,
        body: Rc<Chunk>,
    },
    /// FUSION: четыре опкода ниже перенесены из ветки B (structs).
    /// Создаёт экземпляр struct. `fields` — упорядоченный список имён
    /// полей (порядок совпадает с порядком значений на стеке — снизу
    /// вверх: первое поле глубже всего). `type_name` — для сообщений об
    /// ошибках и `type_of()`.
    MakeStruct {
        type_name: String,
        fields: Vec<String>,
    },
    /// Загружает значение поля `field` из struct на вершине стека.
    GetField(String),
    /// Устанавливает значение поля `field` в struct. На стеке: (снизу)
    /// объект, (сверху) новое значение.
    SetField(String),
    /// Вызов метода `method` на объекте, лежащем под `argc` аргументами
    /// на стеке. VM разрешает имя функции как `{TypeName}_{method}` по
    /// фактическому типу объекта (`type_display_name()`), затем вызывает
    /// её с объектом как первым аргументом (`self`) — см. `vm::run_chunk`.
    CallMethod {
        method: String,
        argc: usize,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chunk {
    pub code: Vec<OpCode>,
    pub constants: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct FunctionDef {
    pub params: Vec<String>,
    /// Флаг `MUT` для каждого параметра (Ownership/Borrowing,
    /// roadmap-пункт 2) — параллельный `params` массив той же длины.
    /// См. заметку в шапке модуля.
    pub param_mut: Vec<bool>,
    pub chunk: Chunk,
}

pub struct CompiledProgram {
    pub main: Chunk,
    pub functions: HashMap<String, FunctionDef>,
}

/// FUSION-ПРИМЕЧАНИЕ: `PartialEq` добавлен сюда (и рекурсивно на
/// `ClosureValue`/`Chunk`/`OpCode` ниже) при слиянии веток — без него
/// `cargo test` не компилировался: тесты замыканий
/// (`tests/integration_test.rs`, секция "Замыкания") используют
/// `assert_eq!(run(&src).unwrap(), Value::Int(5))`, что требует
/// `Value: PartialEq`. Это не было замечено в исходной ветке, потому что
/// она была написана без доступа к `rustc`/`cargo` для реальной
/// компиляции (см. явное признание этого ограничения в собственных
/// комментариях той ветки) — см. MIGRATION_REPORT.md.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Nil,
    Array(Rc<RefCell<Vec<Value>>>),
    /// FUSION: перенесено из ветки B. Экземпляр struct — именованная
    /// коллекция полей. `type_name` — имя типа (для `type_of()` и
    /// сообщений об ошибках). `fields` хранятся в `Rc<RefCell<...>>` для
    /// reference-семантики — присваивание struct-значения переменной не
    /// копирует поля, обе переменные ссылаются на одни и те же данные
    /// (как массивы в SGA, см. `Value::Array` выше). Соответствует
    /// ожидаемой семантике ООП: объект передаётся "по ссылке". Именно
    /// поэтому мутация через `MUT self` в методе видна вызывающей
    /// стороне после возврата — см. docs/LANGUAGE_SPEC.md, §8.
    Struct {
        type_name: String,
        fields: Rc<RefCell<HashMap<String, Value>>>,
    },
    /// Замыкание: параметры + тело (байткод) + СНИМОК окружения на
    /// момент создания. См. подробное описание модели захвата в
    /// `docs/LANGUAGE_SPEC.md`, §5.2 ("Замыкания") — кратко: захват ПО
    /// ЗНАЧЕНИЮ (копия текущих видимых переменных и их mutable-флагов в
    /// момент `FN(...){}`), НЕ по ссылке. Мутация захваченной переменной
    /// снаружи после создания замыкания НЕ видна внутри него, и
    /// наоборот — это сознательное упрощение (полноценные mutable
    /// upvalues потребовали бы `Rc<RefCell<Value>>` per-variable во всей
    /// VM, что является отдельным, более крупным рефакторингом — см.
    /// ROADMAP). Присваивание захваченной `LET`/`CONST`-переменной
    /// внутри тела замыкания остаётся ошибкой (mutable-флаг сохраняется
    /// при захвате, см. `ClosureValue::captured`), как и для обычного
    /// кода вне замыканий.
    Closure(Rc<ClosureValue>),
}

#[derive(Debug, PartialEq)]
pub struct ClosureValue {
    pub params: Vec<String>,
    pub chunk: Rc<Chunk>,
    /// Снимок переменных, видимых в момент создания замыкания: имя ->
    /// (значение, была ли переменная mutable `VAR` или immutable
    /// `LET`/`CONST` в точке захвата). Плоская карта, а не стек
    /// `Scope`-ов — на момент создания замыкания все видимые имена уже
    /// единственны по построению (semantic-анализ запрещает теневые
    /// повторные объявления в одной области видимости, а сама плоская
    /// карта формируется обходом стека областей видимости от глобальной
    /// к самой внутренней, так что более внутренние имена естественно
    /// перекрывают внешние — корректно для лексической области видимости).
    pub captured: HashMap<String, (Value, bool)>,
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Str(_) => "string",
            Value::Nil => "nil",
            Value::Array(_) => "array",
            Value::Struct { .. } => "struct",
            Value::Closure(_) => "closure",
        }
    }

    /// FUSION: перенесено из ветки B. Для struct — возвращает имя
    /// конкретного типа (например, "Point"), а не просто "struct", для
    /// более полезных сообщений об ошибках и `type_of()`.
    pub fn type_display_name(&self) -> String {
        match self {
            Value::Struct { type_name, .. } => type_name.clone(),
            other => other.type_name().to_string(),
        }
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Nil => false,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Array(a) => !a.borrow().is_empty(),
            Value::Struct { .. } => true, // struct-объект всегда truthy
            // Замыкание всегда truthy (как функции в большинстве
            // динамических языков — JS, Python, Lua).
            Value::Closure(_) => true,
        }
    }
}

const MAX_DISPLAY_DEPTH: usize = 64;

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_value(self, 0, f)
    }
}

/// Печать значения с ограничением глубины рекурсии. Без этого ограничения
/// самореференцирующийся массив (например, `arr[0] = arr;`) приводил к
/// неконтролируемому stack overflow и аварийному завершению процесса —
/// это было реально обнаружено при тестировании (см. docs/SECURITY.md),
/// а не гипотетический сценарий, и исправлено здесь, а не только описано.
fn fmt_value(v: &Value, depth: usize, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match v {
        Value::Int(i) => write!(f, "{}", i),
        Value::Float(v) => write!(f, "{}", v),
        Value::Bool(b) => write!(f, "{}", if *b { "истина" } else { "ложь" }),
        Value::Str(s) => write!(f, "{}", s),
        Value::Nil => write!(f, "nil"),
        Value::Array(items) => {
            if depth >= MAX_DISPLAY_DEPTH {
                return write!(f, "[...]");
            }
            let items = items.borrow();
            write!(f, "[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                fmt_value(item, depth + 1, f)?;
            }
            write!(f, "]")
        }
        Value::Closure(c) => write!(f, "<closure/{}>", c.params.len()),
        // FUSION/НАЙДЕНО ПРИ СЛИЯНИИ: исходный код этой ветки в B
        // (`src/runtime/mod.rs`) содержал `write!(f, "{}: ")?;` без
        // аргумента для `{}` — `error: 1 positional argument in format
        // string, but no arguments were given`. Ветка B не компилировалась
        // вообще (см. MIGRATION_REPORT.md) — исправлено здесь явной
        // передачей `k`.
        Value::Struct { type_name, fields } => {
            if depth >= MAX_DISPLAY_DEPTH {
                return write!(f, "{} {{...}}", type_name);
            }
            let fields = fields.borrow();
            write!(f, "{} {{", type_name)?;
            let mut sorted: Vec<_> = fields.iter().collect();
            sorted.sort_by_key(|(k, _)| k.as_str());
            for (i, (k, v)) in sorted.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}: ", k)?;
                fmt_value(v, depth + 1, f)?;
            }
            write!(f, "}}")
        }
    }
}

pub fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "len"
            | "push"
            | "to_string"
            | "to_int"
            | "to_float"
            | "type_of"
            | "keys"
            | "sqrt"
            | "floor"
            | "ceil"
            | "abs"
            | "min"
            | "max"
            | "pow"
            | "str_split"
            | "str_contains"
            | "str_trim"
            | "str_starts_with"
            | "str_ends_with"
            | "str_replace"
            | "str_upper"
            | "str_lower"
            | "range"
    )
}

/// FUSION: расширено при слиянии — исходно (ветка A) здесь было только
/// 5 функций (len/push/to_string/to_int/to_float). Остальные
/// (type_of..str_lower) перенесены из ветки B (см. MIGRATION_REPORT.md,
/// раздел "Stdlib"), с двумя изменениями относительно исходного кода:
/// (1) `len`/`keys` теперь также понимают `Value::Struct` (поддержка
/// structs из ветки B расширена на builtin'ы из самой B — в исходном
/// коде B это уже было так, сохранено как есть); (2) сообщения об
/// ошибках везде используют `type_display_name()` вместо `type_name()`,
/// чтобы ошибка для struct показывала реальное имя типа (например,
/// "Point"), а не общее "struct".
pub fn call_builtin(name: &str, args: Vec<Value>) -> Result<Value, String> {
    match name {
        // ── коллекции ────────────────────────────────────────────────
        "len" => match args.first() {
            Some(Value::Str(s)) => Ok(Value::Int(s.chars().count() as i64)),
            Some(Value::Array(a)) => Ok(Value::Int(a.borrow().len() as i64)),
            Some(Value::Struct { fields, .. }) => Ok(Value::Int(fields.borrow().len() as i64)),
            Some(v) => Err(format!(
                "len() не поддерживает тип {}",
                v.type_display_name()
            )),
            None => Err("len() ожидает 1 аргумент, передано 0".to_string()),
        },
        "push" => match args.first() {
            Some(Value::Array(a)) => {
                a.borrow_mut()
                    .push(args.get(1).cloned().unwrap_or(Value::Nil));
                Ok(Value::Nil)
            }
            Some(v) => Err(format!(
                "push() ожидает array первым аргументом, получено {}",
                v.type_display_name()
            )),
            None => Err("push() ожидает минимум 1 аргумент, передано 0".to_string()),
        },
        // Возвращает массив имён полей struct или индексов массива (как int).
        "keys" => match args.first() {
            Some(Value::Struct { fields, .. }) => {
                let mut ks: Vec<Value> = fields
                    .borrow()
                    .keys()
                    .map(|k| Value::Str(k.clone()))
                    .collect();
                ks.sort_by(|a, b| match (a, b) {
                    (Value::Str(sa), Value::Str(sb)) => sa.cmp(sb),
                    _ => std::cmp::Ordering::Equal,
                });
                Ok(Value::Array(Rc::new(RefCell::new(ks))))
            }
            Some(Value::Array(a)) => {
                let n = a.borrow().len();
                let ks: Vec<Value> = (0..n).map(|i| Value::Int(i as i64)).collect();
                Ok(Value::Array(Rc::new(RefCell::new(ks))))
            }
            Some(v) => Err(format!(
                "keys() не поддерживает тип {}",
                v.type_display_name()
            )),
            None => Err("keys() ожидает 1 аргумент, передано 0".to_string()),
        },
        // Создаёт массив целых чисел [start, end). Без аргументов start —
        // диапазон [0, end).
        "range" => {
            const MAX_RANGE: i64 = 10_000_000;
            match (args.first(), args.get(1)) {
                (Some(Value::Int(end)), None) => {
                    let n = *end;
                    if n < 0 {
                        return Err(format!("range() получил отрицательный конец: {}", n));
                    }
                    if n > MAX_RANGE {
                        return Err(format!(
                            "range({}) превышает максимальный размер {}",
                            n, MAX_RANGE
                        ));
                    }
                    Ok(Value::Array(Rc::new(RefCell::new(
                        (0..n).map(Value::Int).collect(),
                    ))))
                }
                (Some(Value::Int(start)), Some(Value::Int(end))) => {
                    let (s, e) = (*start, *end);
                    if s >= e {
                        return Ok(Value::Array(Rc::new(RefCell::new(vec![]))));
                    }
                    if e - s > MAX_RANGE {
                        return Err(format!(
                            "range({}, {}) превышает максимальный размер {}",
                            s, e, MAX_RANGE
                        ));
                    }
                    Ok(Value::Array(Rc::new(RefCell::new(
                        (s..e).map(Value::Int).collect(),
                    ))))
                }
                _ => Err("range(end) или range(start, end) ожидают int-аргументы".to_string()),
            }
        }

        // ── преобразование типов ─────────────────────────────────────
        "to_string" => match args.first() {
            Some(v) => Ok(Value::Str(v.to_string())),
            None => Err("to_string() ожидает 1 аргумент, передано 0".to_string()),
        },
        "to_int" => match args.first() {
            Some(Value::Int(i)) => Ok(Value::Int(*i)),
            Some(Value::Float(f)) => Ok(Value::Int(*f as i64)),
            Some(Value::Str(s)) => s
                .trim()
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| format!("невозможно преобразовать '{}' в int", s)),
            Some(Value::Bool(b)) => Ok(Value::Int(if *b { 1 } else { 0 })),
            Some(v) => Err(format!(
                "to_int() не поддерживает тип {}",
                v.type_display_name()
            )),
            None => Err("to_int() ожидает 1 аргумент, передано 0".to_string()),
        },
        "to_float" => match args.first() {
            Some(Value::Int(i)) => Ok(Value::Float(*i as f64)),
            Some(Value::Float(f)) => Ok(Value::Float(*f)),
            Some(Value::Str(s)) => s
                .trim()
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| format!("невозможно преобразовать '{}' в float", s)),
            Some(v) => Err(format!(
                "to_float() не поддерживает тип {}",
                v.type_display_name()
            )),
            None => Err("to_float() ожидает 1 аргумент, передано 0".to_string()),
        },
        // Возвращает строку с именем типа значения (для struct — реальное
        // имя типа, например "Point", см. type_display_name()).
        "type_of" => match args.first() {
            Some(v) => Ok(Value::Str(v.type_display_name())),
            None => Err("type_of() ожидает 1 аргумент, передано 0".to_string()),
        },

        // ── математика ───────────────────────────────────────────────
        "sqrt" => match args.first() {
            Some(Value::Float(f)) => Ok(Value::Float(f.sqrt())),
            Some(Value::Int(i)) => Ok(Value::Float((*i as f64).sqrt())),
            Some(v) => Err(format!(
                "sqrt() ожидает число, получено {}",
                v.type_display_name()
            )),
            None => Err("sqrt() ожидает 1 аргумент, передано 0".to_string()),
        },
        "floor" => match args.first() {
            Some(Value::Float(f)) => Ok(Value::Int(f.floor() as i64)),
            Some(Value::Int(i)) => Ok(Value::Int(*i)),
            Some(v) => Err(format!(
                "floor() ожидает число, получено {}",
                v.type_display_name()
            )),
            None => Err("floor() ожидает 1 аргумент, передано 0".to_string()),
        },
        "ceil" => match args.first() {
            Some(Value::Float(f)) => Ok(Value::Int(f.ceil() as i64)),
            Some(Value::Int(i)) => Ok(Value::Int(*i)),
            Some(v) => Err(format!(
                "ceil() ожидает число, получено {}",
                v.type_display_name()
            )),
            None => Err("ceil() ожидает 1 аргумент, передано 0".to_string()),
        },
        "abs" => match args.first() {
            Some(Value::Int(i)) => Ok(Value::Int(i.abs())),
            Some(Value::Float(f)) => Ok(Value::Float(f.abs())),
            Some(v) => Err(format!(
                "abs() ожидает число, получено {}",
                v.type_display_name()
            )),
            None => Err("abs() ожидает 1 аргумент, передано 0".to_string()),
        },
        "min" => match (args.first(), args.get(1)) {
            (Some(Value::Int(a)), Some(Value::Int(b))) => Ok(Value::Int(*a.min(b))),
            (Some(Value::Float(a)), Some(Value::Float(b))) => Ok(Value::Float(a.min(*b))),
            (Some(Value::Int(a)), Some(Value::Float(b))) => Ok(Value::Float((*a as f64).min(*b))),
            (Some(Value::Float(a)), Some(Value::Int(b))) => Ok(Value::Float(a.min(*b as f64))),
            (Some(a), Some(b)) => Err(format!(
                "min() ожидает два числа, получено {} и {}",
                a.type_display_name(),
                b.type_display_name()
            )),
            _ => Err("min(a, b) ожидает 2 аргумента".to_string()),
        },
        "max" => match (args.first(), args.get(1)) {
            (Some(Value::Int(a)), Some(Value::Int(b))) => Ok(Value::Int(*a.max(b))),
            (Some(Value::Float(a)), Some(Value::Float(b))) => Ok(Value::Float(a.max(*b))),
            (Some(Value::Int(a)), Some(Value::Float(b))) => Ok(Value::Float((*a as f64).max(*b))),
            (Some(Value::Float(a)), Some(Value::Int(b))) => Ok(Value::Float(a.max(*b as f64))),
            (Some(a), Some(b)) => Err(format!(
                "max() ожидает два числа, получено {} и {}",
                a.type_display_name(),
                b.type_display_name()
            )),
            _ => Err("max(a, b) ожидает 2 аргумента".to_string()),
        },
        "pow" => match (args.first(), args.get(1)) {
            (Some(Value::Int(base)), Some(Value::Int(exp))) => {
                if *exp < 0 {
                    return Err("pow() с целым отрицательным экспонентом не поддерживается; используйте to_float() для обоих аргументов".to_string());
                }
                if *exp > u32::MAX as i64 {
                    return Err(format!("pow() экспонента {} слишком велика", exp));
                }
                let result = base
                    .checked_pow(*exp as u32)
                    .ok_or_else(|| format!("переполнение int в pow({}, {})", base, exp))?;
                Ok(Value::Int(result))
            }
            (Some(a), Some(b)) => {
                let af = match a {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => *f,
                    v => {
                        return Err(format!(
                            "pow() ожидает числа, получено {}",
                            v.type_display_name()
                        ))
                    }
                };
                let bf = match b {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => *f,
                    v => {
                        return Err(format!(
                            "pow() ожидает числа, получено {}",
                            v.type_display_name()
                        ))
                    }
                };
                Ok(Value::Float(af.powf(bf)))
            }
            _ => Err("pow(base, exp) ожидает 2 аргумента".to_string()),
        },

        // ── строки ──────────────────────────────────────────────────
        "str_split" => match (args.first(), args.get(1)) {
            (Some(Value::Str(s)), Some(Value::Str(sep))) => {
                let parts: Vec<Value> = s
                    .split(sep.as_str())
                    .map(|p| Value::Str(p.to_string()))
                    .collect();
                Ok(Value::Array(Rc::new(RefCell::new(parts))))
            }
            (Some(a), Some(b)) => Err(format!(
                "str_split(str, sep) ожидает два string, получено {} и {}",
                a.type_display_name(),
                b.type_display_name()
            )),
            _ => Err("str_split(str, sep) ожидает 2 аргумента".to_string()),
        },
        "str_contains" => match (args.first(), args.get(1)) {
            (Some(Value::Str(s)), Some(Value::Str(sub))) => {
                Ok(Value::Bool(s.contains(sub.as_str())))
            }
            (Some(a), Some(b)) => Err(format!(
                "str_contains(str, sub) ожидает два string, получено {} и {}",
                a.type_display_name(),
                b.type_display_name()
            )),
            _ => Err("str_contains(str, sub) ожидает 2 аргумента".to_string()),
        },
        "str_trim" => match args.first() {
            Some(Value::Str(s)) => Ok(Value::Str(s.trim().to_string())),
            Some(v) => Err(format!(
                "str_trim() ожидает string, получено {}",
                v.type_display_name()
            )),
            None => Err("str_trim() ожидает 1 аргумент, передано 0".to_string()),
        },
        "str_starts_with" => match (args.first(), args.get(1)) {
            (Some(Value::Str(s)), Some(Value::Str(pre))) => {
                Ok(Value::Bool(s.starts_with(pre.as_str())))
            }
            (Some(a), Some(b)) => Err(format!(
                "str_starts_with(str, prefix) ожидает два string, получено {} и {}",
                a.type_display_name(),
                b.type_display_name()
            )),
            _ => Err("str_starts_with(str, prefix) ожидает 2 аргумента".to_string()),
        },
        "str_ends_with" => match (args.first(), args.get(1)) {
            (Some(Value::Str(s)), Some(Value::Str(suf))) => {
                Ok(Value::Bool(s.ends_with(suf.as_str())))
            }
            (Some(a), Some(b)) => Err(format!(
                "str_ends_with(str, suffix) ожидает два string, получено {} и {}",
                a.type_display_name(),
                b.type_display_name()
            )),
            _ => Err("str_ends_with(str, suffix) ожидает 2 аргумента".to_string()),
        },
        "str_replace" => match (args.first(), args.get(1), args.get(2)) {
            (Some(Value::Str(s)), Some(Value::Str(from)), Some(Value::Str(to))) => {
                Ok(Value::Str(s.replace(from.as_str(), to.as_str())))
            }
            _ => Err("str_replace(str, from, to) ожидает 3 string-аргумента".to_string()),
        },
        "str_upper" => match args.first() {
            Some(Value::Str(s)) => Ok(Value::Str(s.to_uppercase())),
            Some(v) => Err(format!(
                "str_upper() ожидает string, получено {}",
                v.type_display_name()
            )),
            None => Err("str_upper() ожидает 1 аргумент, передано 0".to_string()),
        },
        "str_lower" => match args.first() {
            Some(Value::Str(s)) => Ok(Value::Str(s.to_lowercase())),
            Some(v) => Err(format!(
                "str_lower() ожидает string, получено {}",
                v.type_display_name()
            )),
            None => Err("str_lower() ожидает 1 аргумент, передано 0".to_string()),
        },
        _ => Err(format!("неизвестная встроенная функция '{}'", name)),
    }
}
