# VM SPEC — SGA Bytecode & Virtual Machine v0.1

## Модель данных (`src/runtime/mod.rs::Value`)

```rust
enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Nil,
    Array(Rc<RefCell<Vec<Value>>>),  // ссылочная семантика, как и должно быть для мутируемых коллекций
    Closure(Rc<ClosureValue>),       // FN(...) {...} как значение — см. docs/LANGUAGE_SPEC.md §5.2
}

struct ClosureValue {
    params: Vec<String>,
    chunk: Rc<Chunk>,
    captured: HashMap<String, (Value, bool)>,  // снимок окружения на момент создания, ПО ЗНАЧЕНИЮ
}
```

`Int`/`Float`/`Bool`/`Str`/`Nil` — копируются по значению (Rust `Clone`,
дешёво для скаляров, `String`/`Vec` клонируются по необходимости).
`Array` использует `Rc<RefCell<...>>`, поэтому `arr2 = arr1;` создаёт два
имени, ссылающихся на один и тот же буфер (присваивание по ссылке —
осознанное решение для предсказуемой семантики `push`/индексного
присваивания).

## Формат чанка

```rust
struct Chunk {
    code: Vec<OpCode>,
    constants: Vec<Value>,
}

struct FunctionDef {
    params: Vec<String>,
    param_mut: Vec<bool>,  // MUT-флаг на каждый параметр, см. Ownership/Borrowing §5.1
    chunk: Chunk,
}
```

Переходы — **абсолютные индексы** в `code` того же чанка (функции/
замыкания не делятся чанками друг с другом). `Chunk`/`OpCode`/
`FunctionDef`/`CompiledProgram` определены в `src/runtime/mod.rs` (не в
`codegen` — см. `docs/COMPILER_SPEC.md` про цикл зависимостей,
вызванный `Value::Closure`); `codegen` ре-экспортирует их.

## Набор опкодов

| OpCode                  | Эффект на стеке (снизу вверх)                  |
|--------------------------|------------------------------------------------|
| `PushConst(idx)`         | `-> v`  (берёт `constants[idx]`)                |
| `LoadVar(name)`          | `-> v`                                          |
| `StoreVar(name)`         | `v -> v` (присваивает и оставляет значение)     |
| `DefineVar(name, mut)`   | `v ->`  (объявляет в текущей области видимости) |
| `Pop`                    | `v ->`                                          |
| `PushScope` / `PopScope` | не влияет на стек значений, только на стек областей видимости |
| `Add/Sub/Mul/Div/Mod`    | `a b -> r` (checked-арифметика для int, делене/0 — ошибка) |
| `Eq/NotEq/Lt/Gt/LtEq/GtEq` | `a b -> bool`                                 |
| `And` / `Or`             | `a b -> bool` (через `is_truthy`, не короткое замыкание на уровне байткода — обе стороны вычисляются заранее компилятором выражений) |
| `Neg` / `Not`            | `a -> r`                                        |
| `JumpIfFalse(addr)`      | `v ->` , `ip = addr` если `!is_truthy(v)`       |
| `Jump(addr)`             | `ip = addr`                                     |
| `Call(name, argc)`       | `a1..aN -> r` (статический вызов по имени — top-level функция или builtin) |
| `CallValue(argc)`        | `callee a1..aN -> r` (динамический вызов значения со стека — замыкание; `callee` снят ДО аргументов, в порядке компиляции, см. `codegen`) |
| `MakeClosure{params,body}` | `-> closure` (снимок видимых переменных как `captured`, см. `Value::Closure`) |
| `Print(argc)`            | `a1..aN ->` (печатает через `Display`, через пробел) |
| `MakeArray(n)`           | `a1..aN -> array`                               |
| `Index`                  | `arr idx -> v`                                  |
| `IndexAssign`            | `arr idx v -> v`                                |
| `Return(has_value)`      | завершает выполнение текущего `run_chunk`, возвращая `v` либо `Nil` |
| `MakeStruct{type_name,fields}` | `v1..vN -> struct` (значения полей в порядке `fields`, снизу вверх) |
| `GetField(name)`         | `struct -> v`                                   |
| `SetField(name)`         | `struct v -> v` (мутирует struct по reference-семантике, см. §6 `docs/LANGUAGE_SPEC.md`) |
| `CallMethod{method,argc}` | `obj a1..aN -> r` (разрешается как `{TypeName}_{method}` по фактическому типу `obj`, вызывается с `obj` как первым аргументом/`self`) |

**Инвариант стека:** любое выражение (`Expr`) после компиляции оставляет
ровно одно значение на стеке; любой statement (`Stmt`) — ровно ноль. Это
проверено по построению в `codegen` и покрыто тестами в `tests/`.

> Примечание про `And`/`Or`: в текущей реализации оба операнда вычисляются
> до выполнения логической операции (нет short-circuit на уровне
> байткода). Если правый операнд вызывает побочный эффект (например,
> функцию с `print` внутри), он выполнится даже если левый операнд уже
> определяет результат. Это задокументированное ограничение v0.1, а не
> скрытая особенность — см. ROADMAP.

## Вызов функции и кадры

`Call(name, argc)` снимает `argc` значений со стека текущего чанка,
формирует новую область видимости с параметрами (с флагом `mutable` —
из `FunctionDef::param_mut`, см. Ownership/Borrowing, §5.1 в
`docs/LANGUAGE_SPEC.md`), и **рекурсивно** (на уровне Rust-стека)
вызывает `run_chunk` для чанка целевой функции — со своим независимым
стеком значений и независимым стеком областей видимости. Глубина
рекурсии ограничена (`max_call_depth = 200`) — защита от
неконтролируемого исчерпания системного стека. Значение откалибровано
эмпирически (не "на глаз") — см. `docs/SECURITY.md`, раздел про
`max_call_depth`, где показано, что унаследованное значение `2000`
реально переполняло системный стек ДО срабатывания этой защиты при
типичном для Linux размере стека потока (8 МиБ).

`CallValue(argc)` снимает `argc` значений и затем `callee`, ожидает
`Value::Closure`. Кадр вызова замыкания состоит из ДВУХ scope: снизу —
снимок `captured` (захваченное окружение на момент `MakeClosure`),
сверху — новая scope с параметрами текущего вызова (все mutable=true,
у замыканий нет `MUT`-аннотаций). Та же `max_call_depth` действует и
здесь.

## Bytecode verifier

`Chunk`/`OpCode`/`CompiledProgram` — все публичные типы, так что
потребитель крейта `sga` может сконструировать `Chunk` вручную, в
обход `codegen::compile()`. `Vm::new(program) -> Result<(Vm, Chunk), RuntimeError>`
проверяет ДО исполнения:
- каждый `PushConst(idx)` — что `idx < constants.len()`;
- каждый `Jump(target)`/`JumpIfFalse(target)` — что `target <= code.len()`;
- рекурсивно — то же самое для тела каждого `MakeClosure` и для чанка
  каждой функции в `CompiledProgram::functions`.

Без этого несогласованный вручную собранный `Chunk` приводил бы к
неуправляемой Rust panic (`index out of bounds`) вместо `RuntimeError`.
Программы, полученные через обычный `codegen::compile()`, всегда проходят
верификацию (индексы согласованы по построению) — стоимость проверки
линейна по размеру байткода и пренебрежимо мала по сравнению со
стоимостью самого исполнения.

## Builtin-функции (`src/runtime/mod.rs`)

25 функций — вызываются через `Call`, перехватываются `Vm::call()` до
поиска пользовательской функции:
- **Коллекции/общие:** `len`, `push`, `keys`, `range`, `to_string`,
  `to_int`, `to_float`, `type_of`
- **Числовые:** `sqrt`, `floor`, `ceil`, `abs`, `min`, `max`, `pow`
- **Строковые:** `str_split`, `str_contains`, `str_trim`,
  `str_starts_with`, `str_ends_with`, `str_replace`, `str_upper`,
  `str_lower`
- **std/json (T008, M003):** `json_stringify(v) -> string`,
  `json_parse(s) -> v` — ручной parser/serializer без внешних
  зависимостей (`src/json.rs`). `Struct` сериализуется по реальным
  полям (отсортированным по имени для детерминированного вывода); JSON-
  объект при парсинге становится `Array` из 2-элементных `[ключ,
  значение]` — у SGA v0.1 нет generic map-типа. `Closure` не
  сериализуется (`RuntimeError`).

`len`/`keys` также понимают `Value::Struct` (не только `Array`/`Str`).
Полный список и сигнатуры — `src/runtime/mod.rs::call_builtin`/
`is_builtin`.
