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
обход `codegen::compile()`. `Vm::new(program, sandbox_root) -> Result<(Vm, Chunk), RuntimeError>`
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

32 функции — вызываются через `Call`, перехватываются `Vm::call()` до
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
- **std/io (T009, M003):** `read_file(path) -> string` — один из шести
  builtin'ов из 32 (остальные — `args`, T012, `read_line`/`read_stdin`,
  T013, `time_monotonic_ms`, T014, и `write_file`, T015), реализованных НЕ в
  `runtime::call_builtin`, а перехватываемых напрямую в `Vm::call()`
  (нужен доступ к состоянию VM — здесь `Vm::sandbox_root`, которого нет у
  свободной функции `call_builtin`;
  `runtime::is_builtin`/`semantic::builtin_arity` всё равно знают о нём
  — нужно codegen'у для статического `OpCode::Call`). Sandboxed:
  переиспользует confinement-границу `IMPORT`
  (`module_resolver::is_within_root`/`compute_confinement_root`), см.
  `docs/SECURITY.md`. Только чтение (запись — `write_file`, ниже); `run_source`
  (без файлового пути) → `RuntimeError` при вызове.
- **std/process (T012, M006):** `args() -> array` — аргументы командной
  строки программы как массив `string` в порядке передачи, БЕЗ пути к
  самой программе (пустой массив, если аргументов нет). Арность (0, 0)
  проверяет `semantic::builtin_arity`; в рантайме — перехват в
  `Vm::call()` (`call_args`, нужен `Vm::args`), в `call_builtin` стоит
  `unreachable!`, как у `read_file`. Аргументы задаются
  `Vm::set_args(Vec<String>)` между `Vm::new` и `Vm::run` (сигнатура
  `Vm::new(program, sandbox_root)` не менялась). Каждый вызов возвращает
  НОВЫЙ массив: мутация результата не влияет на последующие вызовы.
  Источник аргументов — `RunOptions` (`sga::run_source_with_options`,
  `sga::run_source_file_with_options`); CLI `sga run файл арг1 арг2`
  передаёт всё после пути файла дословно.
- **std/io — стандартный ввод (T013, M006):** `read_line() -> string | nil`
  и `read_stdin() -> string`. Арность (0, 0) у обоих
  (`semantic::builtin_arity`); в рантайме — перехват в `Vm::call()`
  (`call_read_line`/`call_read_stdin`, нужен читатель `Vm::stdin`), в
  `call_builtin` стоит `unreachable!`, как у `read_file`/`args`. Читатель —
  `Box<dyn BufRead>`, по умолчанию пустой ввод; задаётся
  `Vm::set_stdin(impl BufRead + 'static)` между `Vm::new` и `Vm::run`
  (сигнатура `Vm::new(program, sandbox_root)` не менялась). Позиция чтения
  принадлежит VM: повторный `run` на той же VM продолжает с того места,
  где остановился предыдущий. `read_line()` читает байты до первого `\n`
  включительно (или до конца ввода); ни одного байта → `nil`; иначе
  отбрасывается ОДИН завершающий `\n` и ОДИН `\r` непосредственно перед
  ним (только если `\n` был): одиночный `\r` внутри строки и `\r` на конце
  ввода без `\n` остаются данными; `""` — пустая строка, а не конец ввода.
  `read_stdin()` читает ВСЕ оставшиеся байты, переводы строк не
  нормализуются; после вызова ввод исчерпан (`read_line()` → `nil`,
  повторный `read_stdin()` → `""`). Декодирование строгое: не-UTF-8 →
  `RuntimeError` (сообщение называет UTF-8 и число валидных байт;
  прочитанные байты потреблены), подстановки U+FFFD нет; ошибка чтения
  читателя тоже `RuntimeError`, не panic. Чтение ленивое и инкрементальное
  (`read_line()` не ждёт конца ввода). Разбор — `src/stdin.rs`
  (`read_line`/`read_all` над `&mut dyn BufRead`, 14 unit-тестов). Источник
  ввода — `RunOptions` (`with_stdin_bytes`, `with_process_stdin`; по
  умолчанию пустой ввод); CLI `sga run` подключает stdin процесса.
- **std/time — время (T014, M006):** `time_now_ms() -> int` и
  `time_monotonic_ms() -> int`. Арность (0, 0) у обоих
  (`semantic::builtin_arity`). `time_now_ms()` — миллисекунды с
  1970-01-01T00:00:00Z (эпоха Unix, UTC) по `SystemTime`: состояния VM не
  требует, реализован в `runtime::call_builtin`; следует за системными
  часами (может скакнуть при их переводе). `time_monotonic_ms()` —
  миллисекунды, прошедшие с создания VM, по `Instant`: не убывает и не
  зависит от перевода системных часов; нужен момент старта, поэтому
  перехват в `Vm::call()` (`call_time_monotonic_ms`; поле
  `Vm::started: Instant` фиксируется в `Vm::new` и НЕ сбрасывается `run` —
  повторный `run` на той же VM продолжает отсчёт); в `call_builtin` стоит
  `unreachable!`, как у `read_file`/`args`/`read_line`/`read_stdin`. Оба
  результата — `int` (целые миллисекунды, округление вниз); время до
  1970-01-01 и значение за пределом `int` (i64) — `RuntimeError` (без
  усечения и panic), «`now` раньше `start`» у монотонных часов
  сатурируется в 0. Арифметика — `src/clock.rs`: чистые функции над
  инъектируемыми `SystemTime`/`Instant` (`unix_millis`, `elapsed_millis`;
  11 unit-тестов без ожидания реального времени). Разрешений и флагов CLI
  нет. Оптимизатор вызовы не вычисляет и не удаляет. Часы настоящие и не
  подменяются встраивающим кодом (TD-014).
- **std/io — запись файла (T015, M006):** `write_file(path, content) -> nil`.
  Оба аргумента — `string`. Арность (2, 2) проверяет
  `semantic::builtin_arity`; в рантайме — перехват в `Vm::call()`
  (`call_write_file`: нужны `Vm::sandbox_root` и `Vm::allow_write`), в
  `call_builtin` стоит `unreachable!`, как у остальных builtin'ов с
  состоянием VM. Запись запрещена по умолчанию: `Vm::allow_write = false`,
  включается `Vm::set_allow_write(true)` между `Vm::new` и `Vm::run`
  (сигнатура `Vm::new(program, sandbox_root)` не менялась); источник
  разрешения — `RunOptions::with_allow_write(true)` и флаг CLI
  `sga run --allow-write <файл>` (только ДО пути файла). Порядок проверок:
  арность (compile-time) → оба аргумента `string` → разрешение → файловый
  контекст (`sandbox_root`; без него — `RuntimeError` даже при разрешении) →
  правила пути (`src/fs_sandbox.rs::resolve_write_target`) → запись
  (`write_target`). Путь относителен корню песочницы (каталог входного
  файла). Правила пути: пустой путь, NUL, абсолютный путь и путь на каталог
  (хвостовой разделитель, `.`, `..`) отклоняются; родительский каталог
  обязан существовать (каталоги не создаются) и после `canonicalize` лежать
  внутри канонизированного корня (`..` и symlink'и разрешаются физически);
  цель — обычный файл (перезаписывается целиком: `truncate`) или новый файл
  (`create_new`); symlink (даже висячий), каталог, fifo/сокет/устройство в
  роли цели отклоняются. Содержимое записывается как байты UTF-8
  дословно: перевод строки не добавляется и не нормализуется; пустая строка
  даёт пустой файл. Результат — `nil`. Запись не атомарна и не вызывает
  `fsync`; пределы (гонки TOCTOU, жёсткие ссылки, нет лимита размера) — в
  `docs/SECURITY.md` и TD-016. Оптимизатор вызов не вычисляет и не удаляет
  (побочный эффект).

`len`/`keys` также понимают `Value::Struct` (не только `Array`/`Str`).
Полный список и сигнатуры — `src/runtime/mod.rs::call_builtin`/
`is_builtin`.
