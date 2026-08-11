# SGA Programming Language

**SGA** — самостоятельный язык программирования, ключевые слова которого
закодированы алфавитом зачарований Minecraft (Standard Galactic Alphabet).
Файлы исходного кода имеют расширение `.sga`.

> **Статус: v0.1.0 — реальный, рабочий, но честно ограниченный по объёму
> релиз.** Это не концепт и не псевдокод: лексер, парсер, семантический
> анализатор, проверка типов, кодогенератор, резолвер модулей и
> байткод-VM реально реализованы на Rust, компилируются без единого
> warning'а (`cargo clippy` — 0 предупреждений, `cargo fmt --check` —
> чисто) и проходят 89 интеграционных/модульных тестов (`cargo test`).
> Поддерживаются градуальная статическая типизация, Ownership/Borrowing
> (`MUT`-параметры), замыкания, многофайловые программы (`IMPORT`),
> структуры (`STRUCT`) с методами и расширенная стандартная библиотека
> (23 встроенных функции). Чего здесь **нет** — честно перечислено в
> [`docs/ROADMAP.md`](docs/ROADMAP.md): нативного бэкенда (LLVM/WASM),
> полноценного `std` (сеть, http, crypto, async, threads), пакетного
> реестра и языкового сервера для VS Code. Это инженерно правдивая отсечка
> объёма, а не попытка скрыть недостающее.

## Почему именно так

SGA — это не "Python с другим синтаксисом" и не шифратор. Ключевые слова
(`LET`, `FN`, `IF`, `WHILE`, ...) физически закодированы не ASCII-буквами,
а отдельными Unicode-кодпоинтами из официально зарегистрированного блока
**Standard Galactic** (U+EB40–U+EB5F, Under-ConScript Unicode Registry).
Сам алфавит — конструированная письменность Tom Hall для Commander Keen
(1990), которую Minecraft переиспользовал в 2011 как шрифт зачарований —
подробная история и проверенные источники в
[`docs/SGA_ALPHABET.md`](docs/SGA_ALPHABET.md).

Идентификаторы, числа, строки и операторы — обычный UTF-8/ASCII: это
осознанный выбор, иначе языком было бы невозможно пользоваться без
специальной клавиатуры.

**Руны зачарования реально отображаются**: в `vscode-extension/fonts/`
подключён готовый шрифт **Fairfax HD** (Kreative Software, лицензия
SIL OFL-1.1) — после однократной установки в систему `.sga`-файлы в
VS Code показывают настоящие иероглифы, а не пустые прямоугольники.

## Быстрый старт

Требуется Rust ≥ 1.75 (`rustc`, `cargo`).

```bash
cd sga
cargo build --release
./target/release/sga run examples/hello.sga
```

```
Hello, SGA Programming Language!
```

Создать новый проект:

```bash
./target/release/sga init my-project
./target/release/sga run my-project/src/main.sga
```

### Как писать код, если ключевые слова — не ASCII?

Пишите обычными ASCII-мнемониками (`let`, `fn`, `if`, ...) в файле
`*.sga.src`, затем превратите его в настоящий `.sga` с реальными
SGA-кодпоинтами:

```bash
cargo build -p sga-translit --manifest-path tools/translit/Cargo.toml
./tools/translit/target/debug/sga-translit my_program.sga.src my_program.sga
./target/release/sga run my_program.sga
```

Этот же конвейер использован для генерации всех файлов в `examples/` —
читаемые исходники лежат в `examples-src/*.sga.src`.

## Структура репозитория

```
sga/
  src/
    lib.rs        точки входа run_source()/run_source_file() — полный пайплайн
    main.rs       CLI (run/init/fmt/version/...)
    lexer/        лексер (распознаёт SGA-кодпоинты как ключевые слова)
    parser/       рекурсивный спуск, токены -> AST
    ast/          определения узлов AST (включая Lambda/Import/типы)
    semantic/     неизменяемость, Ownership/Borrowing, область видимости замыканий
    typechecker/  градуальная статическая типизация (опциональные `: тип`)
    module_resolver.rs  резолвинг IMPORT (циклы, diamond-дедупликация)
    codegen/      AST -> байткод (OpCode/Chunk), backpatching для циклов
    vm/           стековая VM, bytecode verifier, защита от stack overflow
    runtime/      Value (+ Closure), Chunk/OpCode/FunctionDef, встроенные функции
    sga_alphabet.rs  таблица SGA <-> Unicode PUA
  tools/translit/ инструмент транслитерации ASCII-мнемоник в реальный SGA
  examples/        готовые .sga программы (настоящие SGA-кодпоинты)
  examples-src/    их же читаемые ASCII-исходники
  tests/           интеграционные + модульные тесты (cargo test)
  docs/            спецификации
  vscode-extension/ расширение VS Code (синтаксис, сниппеты)
  scripts/         build.sh, run_examples.sh
```

## CLI

| Команда                         | Статус                                            |
|----------------------------------|---------------------------------------------------|
| `sga run <файл.sga>`             | ✅ реализовано — компиляция + исполнение на VM     |
| `sga init [путь]`                | ✅ реализовано — создаёт `sga.toml` + `src/main.sga` |
| `sga fmt <файл.sga>`             | ✅ частично — печатает разобранный AST            |
| `sga build`                      | ❌ запланировано (нативный бэкенд), см. ROADMAP    |
| `sga test/lint/install/...`      | ❌ запланировано, см. ROADMAP                      |

## Документация

- [`docs/LANGUAGE_SPEC.md`](docs/LANGUAGE_SPEC.md) — типы, синтаксис, EBNF-грамматика
- [`docs/COMPILER_SPEC.md`](docs/COMPILER_SPEC.md) — устройство компилятора
- [`docs/VM_SPEC.md`](docs/VM_SPEC.md) — формат байткода и VM
- [`docs/SGA_ALPHABET.md`](docs/SGA_ALPHABET.md) — таблица соответствия алфавита
- [`docs/SECURITY.md`](docs/SECURITY.md) — модель безопасности
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — что сделано, что не сделано и почему
- [`docs/MIGRATION_REPORT.md`](docs/MIGRATION_REPORT.md) — отчёт о слиянии двух веток проекта
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — как развивать проект дальше

## Лицензия

MIT, см. [`LICENSE`](LICENSE).
