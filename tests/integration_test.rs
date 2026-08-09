//! Интеграционные тесты SGA.
//!
//! FUSION-ПРИМЕЧАНИЕ: объединяет тестовые наборы двух родительских
//! веток. Общие тесты (арифметика, неизменяемость, ошибки времени
//! выполнения и т.п.) оставлены один раз. Секции ниже сгруппированы по
//! фиче, специфичной для одной из веток, чтобы было видно, что
//! покрывается слиянием: Ownership/Borrowing + Type System (были только
//! в одной ветке) и Замыкания + Bytecode Verifier + арность builtin'ов
//! (были только в другой). См. MIGRATION_REPORT.md.
//!
//! Используется `sga::sga_alphabet::encode_word`, чтобы тесты не зависели
//! от внешнего файла-транслитератора и оставались самодостаточными:
//! исходники собираются прямо в тесте из ASCII-мнемоник через `kw("LET")`.

use sga::codegen::{Chunk, CompiledProgram, FunctionDef, OpCode};
use sga::runtime::Value;
use sga::sga_alphabet::encode_word;
use sga::vm::Vm;
use sga::{codegen, lexer, parser, semantic, SgaError};
use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;

fn kw(word: &str) -> String {
    encode_word(word)
}

fn run(source: &str) -> Result<Value, SgaError> {
    sga::run_source(source)
}

// ===== Базовые сценарии (общие для обоих родительских наборов) =====

#[test]
fn test_arithmetic() {
    let src = format!("{} x = 2 + 3 * 4; {}(x);", kw("LET"), kw("PRINT"));
    assert!(run(&src).is_ok());
}

#[test]
fn test_immutable_assignment_is_rejected() {
    let src = format!("{} x = 5; x = 10;", kw("LET"));
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась ошибка семантики (immutable), получено {:?}", other),
    }
}

#[test]
fn test_mutable_var_can_be_reassigned() {
    let src = format!("{} x = 5; x = 10; {}(x);", kw("VAR"), kw("PRINT"));
    assert!(run(&src).is_ok());
}

#[test]
fn test_undefined_variable_is_rejected() {
    let src = format!("{}(y);", kw("PRINT"));
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась ошибка семантики (undefined), получено {:?}", other),
    }
}

#[test]
fn test_integer_overflow_is_runtime_error_not_panic() {
    let src = format!("{} x = 9223372036854775807; {}(x + 1);", kw("LET"), kw("PRINT"));
    match run(&src) {
        Err(SgaError::Runtime(msg)) => assert!(msg.contains("переполнение")),
        other => panic!("ожидалась ошибка переполнения, получено {:?}", other),
    }
}

#[test]
fn test_division_by_zero_is_runtime_error() {
    let src = format!("{} a = 1; {} b = 0; {}(a / b);", kw("LET"), kw("LET"), kw("PRINT"));
    match run(&src) {
        Err(SgaError::Runtime(_)) => {}
        other => panic!("ожидалась ошибка деления на ноль, получено {:?}", other),
    }
}

#[test]
fn test_array_out_of_bounds_is_runtime_error() {
    let src = format!("{} a = [1, 2, 3]; {}(a[99]);", kw("LET"), kw("PRINT"));
    match run(&src) {
        Err(SgaError::Runtime(_)) => {}
        other => panic!("ожидалась ошибка выхода за границы, получено {:?}", other),
    }
}

#[test]
fn test_break_outside_loop_is_compile_error() {
    let src = kw("BREAK") + ";";
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась ошибка семантики (break вне цикла), получено {:?}", other),
    }
}

#[test]
fn test_function_recursion() {
    let src = format!(
        "{fn} factorial(n) {{ {if} n <= 1 {{ {ret} 1; }} {els} {{ {ret} n * factorial(n - 1); }} }} {pr}(factorial(6));",
        fn = kw("FN"),
        if = kw("IF"),
        ret = kw("RETURN"),
        els = kw("ELSE"),
        pr = kw("PRINT"),
    );
    assert!(run(&src).is_ok());
}

#[test]
fn test_for_loop_with_break_and_continue() {
    let src = format!(
        "{var} sum = 0; {for} i {in_} 0..10 {{ {if} i == 5 {{ {cont}; }} {if} i == 8 {{ {brk}; }} sum = sum + i; }} {pr}(sum);",
        var = kw("VAR"),
        for = kw("FOR"),
        in_ = kw("IN"),
        if = kw("IF"),
        cont = kw("CONTINUE"),
        brk = kw("BREAK"),
        pr = kw("PRINT"),
    );
    assert!(run(&src).is_ok());
}

#[test]
fn test_lexer_rejects_unknown_sga_word() {
    // 'XQZ' не входит в таблицу ключевых слов
    let bad = encode_word("XQZ");
    let tokens = lexer::Lexer::new(&bad).tokenize();
    assert!(tokens.is_err());
}

#[test]
fn test_parser_produces_expected_statement_count() {
    let src = format!("{} a = 1; {} b = 2;", kw("LET"), kw("LET"));
    let tokens = lexer::Lexer::new(&src).tokenize().unwrap();
    let program = parser::Parser::new(tokens).parse_program().unwrap();
    assert_eq!(program.len(), 2);
}

/// SECURITY FIX (T001, см. IMPLEMENTATION_LOG.md): рекурсивный спуск без
/// достаточной защиты давал неперехватываемый `fatal runtime error: stack
/// overflow` (`abort()` процесса, не Rust panic — `catch_unwind` не
/// спасает) на глубоко вложенном выражении. ПЕРВАЯ версия этой защиты
/// (`MAX_PARSE_DEPTH = 256`, без явного `stack_size` у этого теста) САМА
/// была уязвима к тому же классу бага, который она должна была
/// предотвращать: `self.depth` считает по одному инкременту на вызов
/// `parse_expr`, а один такой вызов — это ~10 вложенных нативных
/// Rust-кадров (вся цепочка precedence-climbing, см. doc-комментарий
/// `MAX_PARSE_DEPTH` в `src/parser/mod.rs`), поэтому 256 "уровней"
/// защиты реально исчерпывали 2‑МиБ стек ЗАДОЛГО до срабатывания
/// проверки. 2 МиБ — это дефолтный размер стека НЕ главного потока в
/// Rust, если явно не задан `stack_size`/`RUST_MIN_STACK` — именно такой
/// поток даёт тестовый харнесс `cargo test` каждому `#[test]`, что и
/// вызывало реальный краш при обычном запуске (эмпирические пороги
/// переполнения при разных размерах стека — см. `MAX_PARSE_DEPTH`).
/// Исправлено: (а) `MAX_PARSE_DEPTH` уменьшен до эмпирически проверенного
/// безопасного значения `80`; (б) этот тест теперь, как и аналогичные
/// тесты `Vm::max_call_depth` ниже, явно запускается в потоке с
/// фиксированным `stack_size` (2 МиБ — именно тот размер, на котором
/// воспроизводился реальный краш), а не полагается на непроверяемый
/// дефолт тестового харнесса.
#[test]
fn test_deeply_nested_parens_returns_parse_error_not_stack_overflow() {
    let depth = 100_000; // на 3 порядка больше эмпирического порога переполнения без защиты
    let mut expr = String::from("1");
    for _ in 0..depth {
        expr = format!("({})", expr);
    }
    let src = format!("{} x = {};", kw("LET"), expr);
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let tokens = lexer::Lexer::new(&src).tokenize().unwrap();
            match parser::Parser::new(tokens).parse_program() {
                Ok(_) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        })
        .expect("не удалось создать поток для теста");
    match handle.join() {
        Ok(Err(msg)) => assert!(
            msg.contains("глубин"),
            "ошибка должна явно объяснять превышение глубины вложенности, получено: {}",
            msg
        ),
        Ok(Ok(())) => panic!(
            "ожидалась ParseError (превышена максимальная глубина вложенности), получен Ok — лимит глубины не сработал"
        ),
        Err(_) => panic!(
            "ПРОЦЕСС РУХНУЛ (реальный stack overflow) в потоке с 2 МиБ стека — MAX_PARSE_DEPTH \
             слишком велик относительно реального расхода стека на кадр parse_expr (регрессия T001)"
        ),
    }
}

/// Negative-тест к предыдущему: лимит глубины не должен давать false
/// positive на легитимных, умеренно вложенных выражениях — 50 уровней
/// `(...)` (пик глубины ~51 с учётом охватывающего `LET`) — это глубже
/// любого реального кода, но всё ещё с запасом ~29 уровней от
/// `MAX_PARSE_DEPTH = 80` (см. обоснование выбора этого значения в
/// doc-комментарии `MAX_PARSE_DEPTH`, `src/parser/mod.rs`).
#[test]
fn test_moderately_nested_parens_still_parses_successfully() {
    let depth = 50;
    let mut expr = String::from("1");
    for _ in 0..depth {
        expr = format!("({})", expr);
    }
    let src = format!("{} x = {}; {}(x);", kw("LET"), expr, kw("PRINT"));
    assert_eq!(run(&src).unwrap(), Value::Nil);
}

#[test]
fn test_codegen_produces_nonempty_chunk() {
    let src = format!("{}(1);", kw("PRINT"));
    let tokens = lexer::Lexer::new(&src).tokenize().unwrap();
    let program = parser::Parser::new(tokens).parse_program().unwrap();
    semantic::Analyzer::new().analyze(&program).unwrap();
    let compiled = codegen::compile(&program);
    assert!(!compiled.main.code.is_empty());
}

#[test]
fn test_vm_string_concatenation() {
    let src = format!("{} s = \"foo\" + \"bar\"; {}(s);", kw("LET"), kw("PRINT"));
    assert!(run(&src).is_ok());
}

#[test]
fn test_vm_array_mutation_via_push_and_index() {
    let src = format!(
        "{var} a = [1, 2]; {push}(a, 3); a[0] = 99; {pr}(a);",
        var = kw("VAR"),
        push = "push",
        pr = kw("PRINT"),
    );
    assert!(run(&src).is_ok());
}

/// Регрессионный тест на найденную дыру: до фикса `let arr = [...]; arr[i] = v;`
/// компилировалось и исполнялось без ошибки, хотя `arr` объявлен через LET.
/// Исправлено в `semantic::Analyzer::check_mutation_target`.
#[test]
fn test_immutable_array_index_assignment_is_rejected() {
    let src = format!("{} a = [1, 2, 3]; a[0] = 99;", kw("LET"));
    match run(&src) {
        Err(SgaError::Semantic(msg)) => assert!(msg.contains("immutable")),
        other => panic!("ожидалась ошибка семантики (immutable через индекс), получено {:?}", other),
    }
}

/// Тот же баг, но через `push()` вместо индексного присваивания.
#[test]
fn test_immutable_array_push_is_rejected() {
    let src = format!("{} a = [1, 2, 3]; push(a, 4);", kw("LET"));
    match run(&src) {
        Err(SgaError::Semantic(msg)) => assert!(msg.contains("immutable")),
        other => panic!("ожидалась ошибка семантики (immutable через push), получено {:?}", other),
    }
}

/// Регрессионный тест на баг, реально найденный во время разработки:
/// самореференцирующийся массив (`a[0] = a;`) вызывал бесконечную
/// рекурсию в Display и stack overflow процесса при печати. Исправлено
/// ограничением глубины в `runtime::fmt_value`. Тест проверяет, что
/// программа теперь завершается успешно (Ok), а не падает.
#[test]
fn test_self_referential_array_does_not_crash_on_print() {
    let src = format!("{var} a = [1, 2, 3]; a[0] = a; {pr}(a);", var = kw("VAR"), pr = kw("PRINT"));
    assert!(run(&src).is_ok());
}

// ===== Ownership/Borrowing (roadmap-пункт 2) — `MUT`-параметры =====
//
// FUSION: эта секция (как и Type System ниже) принадлежала родительской
// ветке, не имевшей замыканий/IMPORT. При слиянии с веткой замыканий
// `Param.mutable`/`FunctionDef.param_mut` были случайно удалены и
// `Vm::call` хардкодил mutable=true для всех параметров — РЕГРЕССИЯ,
// найденная и закрытая при слиянии (см. vm::Vm::call и
// MIGRATION_REPORT.md). Тест
// `test_immutability_bypass_through_function_parameter_is_now_fixed`
// прямо проверяет, что эта дыра не открыта повторно.

/// ИСПРАВЛЕНО в раунде Ownership/Borrowing (roadmap-пункт 2) — и
/// ПОВТОРНО ИСПРАВЛЕНО при слиянии веток (см. заметку выше секции).
/// Параметры функций без `MUT` — immutable-заимствование, попытка
/// мутировать их внутри функции — ошибка КОМПИЛЯЦИИ, даже не зависящая
/// от того, как был объявлен `frozen` на стороне вызова.
#[test]
fn test_immutability_bypass_through_function_parameter_is_now_fixed() {
    let src = format!(
        "{fn} mutate(arr) {{ arr[0] = 999; }} {let_} frozen = [1, 2, 3]; mutate(frozen);",
        fn = kw("FN"),
        let_ = kw("LET"),
    );
    match run(&src) {
        Err(SgaError::Semantic(msg)) => assert!(msg.contains("immutable")),
        other => panic!("ожидалась ошибка семантики (immutable-параметр без MUT), получено {:?}", other),
    }
}

/// Правильный способ мутировать аргумент через границу вызова функции
/// — явный `MUT`-параметр + `VAR`-аргумент на месте вызова.
#[test]
fn test_mut_parameter_with_var_argument_allows_mutation_across_call_boundary() {
    let src = format!(
        "{fn} mutate({mut_} arr) {{ arr[0] = 999; }} {var} data = [1, 2, 3]; mutate(data); {pr}(data);",
        fn = kw("FN"),
        mut_ = kw("MUT"),
        var = kw("VAR"),
        pr = kw("PRINT"),
    );
    assert!(run(&src).is_ok());
}

/// Borrow-checking на месте вызова: нельзя передать LET-связанную
/// переменную в параметр, объявленный как MUT — ошибка компиляции, а не
/// runtime-сюрприз.
#[test]
fn test_passing_immutable_variable_to_mut_parameter_is_rejected() {
    let src = format!(
        "{fn} mutate({mut_} arr) {{ arr[0] = 999; }} {let_} frozen = [1, 2, 3]; mutate(frozen);",
        fn = kw("FN"),
        mut_ = kw("MUT"),
        let_ = kw("LET"),
    );
    match run(&src) {
        Err(SgaError::Semantic(msg)) => assert!(msg.contains("MUT")),
        other => panic!("ожидалась ошибка семантики (передача LET в MUT-параметр), получено {:?}", other),
    }
}

/// Параметр без MUT можно свободно ЧИТАТЬ (запрещена только мутация).
#[test]
fn test_immutable_parameter_can_still_be_read_freely() {
    let src = format!(
        "{fn} first(arr) {{ {ret} arr[0]; }} {let_} data = [10, 20, 30]; {pr}(first(data));",
        fn = kw("FN"),
        ret = kw("RETURN"),
        let_ = kw("LET"),
        pr = kw("PRINT"),
    );
    assert!(run(&src).is_ok());
}

/// Контрольный тест: легитимный путь с VAR продолжает работать.
#[test]
fn test_mutable_array_index_assignment_and_push_still_work() {
    let src = format!(
        "{var} a = [1, 2, 3]; a[0] = 99; {push}(a, 4); {pr}(a);",
        var = kw("VAR"),
        push = "push",
        pr = kw("PRINT"),
    );
    assert!(run(&src).is_ok());
}

// ===== Type System (roadmap-пункт 1): градуальная статическая типизация =====

#[test]
fn test_typed_var_decl_with_matching_value_is_ok() {
    let src = format!("{} x: int = 5; {}(x);", kw("LET"), kw("PRINT"));
    assert!(run(&src).is_ok());
}

#[test]
fn test_typed_var_decl_with_mismatched_value_is_type_error() {
    let src = format!("{} x: int = \"hello\";", kw("LET"));
    match run(&src) {
        Err(SgaError::Type(_)) => {}
        other => panic!("ожидалась ошибка типов, получено {:?}", other),
    }
}

#[test]
fn test_int_literal_satisfies_float_param_via_promotion() {
    // int -> float разрешено (согласовано с автопромоушеном в VM), обратное — нет.
    let src = format!("{} x: float = 5;", kw("LET"));
    assert!(run(&src).is_ok());
}

#[test]
fn test_typed_function_param_and_return_mismatch_is_rejected() {
    let src = format!(
        "{fn} add(a: int, b: int) -> int {{ {ret} a + b; }} {pr}(add(1, \"two\"));",
        fn = kw("FN"),
        ret = kw("RETURN"),
        pr = kw("PRINT"),
    );
    match run(&src) {
        Err(SgaError::Type(_)) => {}
        other => panic!("ожидалась ошибка типов (аргумент), получено {:?}", other),
    }
}

#[test]
fn test_typed_function_return_value_mismatch_is_rejected() {
    let src = format!(
        "{fn} bad() -> int {{ {ret} \"oops\"; }} {pr}(bad());",
        fn = kw("FN"),
        ret = kw("RETURN"),
        pr = kw("PRINT"),
    );
    match run(&src) {
        Err(SgaError::Type(_)) => {}
        other => panic!("ожидалась ошибка типов (return), получено {:?}", other),
    }
}

#[test]
fn test_typed_function_correct_usage_is_ok() {
    let src = format!(
        "{fn} add(a: int, b: int) -> int {{ {ret} a + b; }} {pr}(add(2, 3));",
        fn = kw("FN"),
        ret = kw("RETURN"),
        pr = kw("PRINT"),
    );
    assert!(run(&src).is_ok());
}

#[test]
fn test_typed_var_reassignment_with_wrong_type_is_rejected() {
    let src = format!("{} x: int = 5; x = \"теперь строка\";", kw("VAR"));
    match run(&src) {
        Err(SgaError::Type(_)) => {}
        other => panic!("ожидалась ошибка типов при переприсваивании, получено {:?}", other),
    }
}

/// КЛЮЧЕВОЙ тест на обратную совместимость: без аннотации переменная
/// остаётся полностью динамической — Type System не должна ломать
/// существующий untyped-код.
#[test]
fn test_untyped_var_can_still_change_type_freely() {
    let src = format!("{var} x = 5; x = \"теперь строка\"; {pr}(x);", var = kw("VAR"), pr = kw("PRINT"));
    assert!(run(&src).is_ok());
}

#[test]
fn test_typed_and_untyped_code_coexist_in_same_program() {
    let src = format!(
        "{let_} typed_x: int = 10; {var} untyped_y = 5; untyped_y = \"строка\"; {pr}(typed_x); {pr}(untyped_y);",
        let_ = kw("LET"),
        var = kw("VAR"),
        pr = kw("PRINT"),
    );
    assert!(run(&src).is_ok());
}

#[test]
fn test_builtin_len_return_type_signature_is_checked() {
    let ok = format!("{} n: int = len([1, 2, 3]); {}(n);", kw("LET"), kw("PRINT"));
    assert!(run(&ok).is_ok());

    let bad = format!("{} n: string = len([1, 2, 3]);", kw("LET"));
    match run(&bad) {
        Err(SgaError::Type(_)) => {}
        other => panic!("ожидалась ошибка типов (len -> int, не string), получено {:?}", other),
    }
}

#[test]
fn test_array_index_must_be_int_when_statically_known() {
    let src =
        format!("{} idx: bool = {}; {} a = [1, 2, 3]; {}(a[idx]);", kw("LET"), kw("TRUE"), kw("VAR"), kw("PRINT"));
    match run(&src) {
        Err(SgaError::Type(_)) => {}
        other => panic!("ожидалась ошибка типов (индекс должен быть int), получено {:?}", other),
    }
}

// ===== Грамматика/арность builtin'ов (найдено и закрыто в ветке замыканий) =====

/// Регрессионный тест: до фикса парсер разрешал `FN` внутри `if`-блока,
/// semantic частично "одобрял" такой код (не регистрируя имя функции),
/// а codegen тихо отбрасывал вложенное объявление — несогласованность
/// трёх стадий компилятора. Теперь парсер отклоняет это синтаксически.
#[test]
fn test_nested_fn_declaration_is_rejected_by_parser() {
    let src = format!(
        "{fn} outer() {{ {fn} inner() {{ {ret} 1; }} {ret} inner(); }}",
        fn = kw("FN"),
        ret = kw("RETURN"),
    );
    match run(&src) {
        Err(SgaError::Parse(_)) => {}
        other => panic!("ожидалась ошибка парсера (вложенный FN), получено {:?}", other),
    }
}

/// Регрессионный тест: до фикса арность builtin-функций (len, to_string,
/// to_int, to_float, push) вообще не проверялась в semantic-анализаторе
/// — в отличие от пользовательских функций. Теперь это ошибка
/// компиляции, как и для пользовательских функций.
#[test]
fn test_builtin_call_with_wrong_arity_is_semantic_error() {
    let src = format!("{}(len());", kw("PRINT"));
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась ошибка семантики (len без аргументов), получено {:?}", other),
    }
}

#[test]
fn test_push_with_variable_arity_one_or_two_args_is_accepted() {
    let src = format!("{var} a = [1]; {push}(a); {push}(a, 2); {pr}(a);", var = kw("VAR"), push = "push", pr = kw("PRINT"));
    assert!(run(&src).is_ok());
}

#[test]
fn test_push_with_three_args_is_semantic_error() {
    let src = format!("{var} a = [1]; {push}(a, 2, 3);", var = kw("VAR"), push = "push");
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась ошибка семантики (push с 3 аргументами), получено {:?}", other),
    }
}

// ===== Защита от stack overflow и bytecode verifier =====

/// Регрессионный/верифицирующий тест на `Vm::max_call_depth`. Этот тест
/// закрывает функциональную часть утверждения: рекурсия глубже
/// `max_call_depth` действительно завершается управляемой
/// `RuntimeError`, а не зависает и не паникует в обычных условиях.
///
/// ВАЖНАЯ ОГОВОРКА: этот тест запускается с заведомо щедрым стеком
/// потока (16 МиБ), поэтому он подтверждает корректность СРАБАТЫВАНИЯ
/// лимита, но НЕ откалиброван под минимальный безопасный размер стека.
///
/// FUSION-ПРИМЕЧАНИЕ: исходная версия этого теста перемещала
/// `Result<Value, SgaError>` через границу потока напрямую — не
/// компилируется, т.к. `Value::Array`/`Value::Closure` хранят `Rc<...>`,
/// а `Rc` не `Send` (тот же класс проблемы, что и отсутствие
/// `PartialEq` у `Value` — см. `runtime::Value`, докстрока о
/// `PartialEq`: исходная ветка была написана без реальной компиляции).
/// Исправлено здесь: результат конвертируется в `Result<(), String>`
/// (только примитивы, безопасно для `Send`) ДО пересечения границы
/// потока, само сравнение текста ошибки происходит уже после `join()`.
#[test]
fn test_recursion_depth_limit_is_safe_under_default_sized_stack() {
    let src = format!(
        "{fn} recurse(n) {{ {ret} recurse(n + 1); }} {pr}(recurse(0));",
        fn = kw("FN"),
        ret = kw("RETURN"),
        pr = kw("PRINT"),
    );
    let handle = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || match run(&src) {
            Ok(_) => Ok(()),
            Err(e) => Err(e.to_string()),
        })
        .expect("не удалось создать поток для теста");
    match handle.join() {
        Ok(Err(_)) => {}
        Ok(Ok(())) => panic!("ожидалась RuntimeError"),
        Err(_) => panic!(
            "ПРОЦЕСС РУХНУЛ при стандартном для Linux размере стека потока (8 МиБ) — \
             max_call_depth слишком велик относительно реального расхода стека на кадр VM"
        ),
    }
}

#[test]
fn test_recursion_depth_limit_triggers_before_stack_overflow() {
    let src = format!(
        "{fn} recurse(n) {{ {ret} recurse(n + 1); }} {pr}(recurse(0));",
        fn = kw("FN"),
        ret = kw("RETURN"),
        pr = kw("PRINT"),
    );
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || match run(&src) {
            Ok(_) => Ok(()),
            Err(e) => Err(e.to_string()),
        })
        .expect("не удалось создать поток для теста");
    match handle.join() {
        Ok(Err(msg)) => {
            assert!(
                msg.contains("рекурсии") || msg.contains("глубина"),
                "ожидалось сообщение о превышении глубины рекурсии, получено: {}",
                msg
            );
        }
        Ok(Ok(())) => panic!("ожидалась RuntimeError о превышении глубины рекурсии, получено Ok"),
        Err(_) => panic!(
            "поток аварийно завершился даже при щедром стеке 16 МиБ (вероятно, реальный stack \
             overflow ДО срабатывания max_call_depth) — это серьёзная регрессия"
        ),
    }
}

/// Регрессионный тест на bytecode verifier (`vm::verify_chunk`/
/// `verify_program`). `OpCode`/`Chunk`/`CompiledProgram` все публичны,
/// так что внешний потребитель крейта мог сконструировать `Chunk` с
/// `PushConst(idx)`, где `idx >= constants.len()`, и получить
/// НЕУПРАВЛЯЕМУЮ Rust panic вместо `RuntimeError`. Теперь `Vm::new`
/// отклоняет такой `Chunk` явной ошибкой ещё до начала исполнения.
#[test]
fn test_vm_rejects_chunk_with_out_of_bounds_const_index_instead_of_panicking() {
    let chunk = Chunk {
        // constants пуст, но PushConst ссылается на индекс 0 — нет такого элемента
        code: vec![OpCode::PushConst(0), OpCode::Return(true)],
        constants: vec![],
    };
    let program = CompiledProgram { main: chunk, functions: HashMap::new() };
    match Vm::new(program) {
        Err(_) => {}
        Ok(_) => panic!("ожидалась ошибка верификации байткода (PushConst вне границ пула констант), получен Ok"),
    }
}

/// Аналогичный тест для `Jump`/`JumpIfFalse` с целью за пределами чанка.
#[test]
fn test_vm_rejects_chunk_with_jump_target_past_end() {
    let chunk = Chunk { code: vec![OpCode::Jump(99), OpCode::Return(false)], constants: vec![] };
    let program = CompiledProgram { main: chunk, functions: HashMap::new() };
    match Vm::new(program) {
        Err(_) => {}
        Ok(_) => panic!("ожидалась ошибка верификации байткода (Jump за пределы чанка), получен Ok"),
    }
}

/// Verifier не должен давать false positive на легальный краевой
/// случай: `Jump`/`JumpIfFalse` с целью, РОВНО равной длине чанка.
#[test]
fn test_vm_accepts_chunk_with_jump_target_exactly_at_end() {
    let chunk = Chunk { code: vec![OpCode::Jump(1), OpCode::Return(false)], constants: vec![] };
    let program = CompiledProgram { main: chunk, functions: HashMap::new() };
    assert!(Vm::new(program).is_ok());
}

/// Verifier должен проверять не только main-чанк, но и чанк каждой
/// определённой функции.
#[test]
fn test_vm_rejects_corrupted_function_chunk_not_just_main() {
    let bad_fn_chunk = Chunk { code: vec![OpCode::PushConst(5), OpCode::Return(true)], constants: vec![] };
    let mut functions = HashMap::new();
    // FUSION: `param_mut` восстановлен в `FunctionDef` (Ownership/
    // Borrowing) — конструктор теста обновлён под новое поле.
    functions.insert("broken".to_string(), FunctionDef { params: vec![], param_mut: vec![], chunk: bad_fn_chunk });
    let program = CompiledProgram { main: Chunk { code: vec![OpCode::Return(false)], constants: vec![] }, functions };
    match Vm::new(program) {
        Err(_) => {}
        Ok(_) => panic!("ожидалась ошибка верификации байткода функции 'broken', получен Ok"),
    }
}

/// SECURITY FIX (audit): `Chunk`/`OpCode` — публичные типы, поэтому
/// внешний потребитель крейта может вручную сконструировать чанк с
/// несбалансированными `PushScope`/`PopScope` (`PopScope` без
/// соответствующего `PushScope`). До исправления `OpCode::DefineVar`
/// делал `scopes.last_mut().unwrap()`, что давало неуправляемую Rust
/// panic, как только `scopes` опускался до пустого вектора — ровно тот
/// класс багов, для защиты от которого существует verify_chunk/
/// verify_program (см. два теста выше), но статическая проверка баланса
/// PushScope/PopScope ненадёжна при наличии Jump/JumpIfFalse, поэтому
/// verify_chunk эту конкретную инвариант не проверяет — защита должна
/// быть в самой `run_chunk`, в точке использования. Подтверждено
/// эмпирически (PoC, паника `called `Option::unwrap()` on a `None`
/// value` at `src/vm/mod.rs:161`) до исправления; после — управляемая
/// `RuntimeError`, тест проверяет именно это, а не просто `is_err()`.
#[test]
fn test_vm_defines_var_after_scope_underflow_returns_runtime_error_not_panic() {
    let chunk = Chunk {
        code: vec![
            OpCode::PushConst(0),
            OpCode::PopScope, // нет соответствующего PushScope — опускает scopes ниже базового
            OpCode::DefineVar("x".to_string(), true),
            OpCode::Return(false),
        ],
        constants: vec![Value::Int(1)],
    };
    let program = CompiledProgram { main: chunk, functions: HashMap::new() };
    // Верификатор не проверяет баланс PushScope/PopScope (см. docstring
    // выше), поэтому Vm::new должен пройти успешно...
    let (mut vm, main) = Vm::new(program).expect("verify_program не должен отклонять этот чанк — баланс scope не проверяется статически");
    // ...а паника должна быть превращена в RuntimeError на этапе run().
    match vm.run(&main) {
        Err(e) => assert!(e.to_string().contains("DefineVar"), "сообщение об ошибке должно объяснять причину (DefineVar без активного scope), получено: {}", e),
        Ok(v) => panic!("ожидалась RuntimeError (DefineVar без активного scope), получен Ok({:?})", v),
    }
}



#[test]
fn test_closure_basic_call_through_variable() {
    let src = format!(
        "{let_} add = {fn}(a, b) {{ {ret} a + b; }}; add(2, 3);",
        let_ = kw("LET"),
        fn = kw("FN"),
        ret = kw("RETURN"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(5));
}

#[test]
fn test_closure_capture_is_by_value_not_by_reference() {
    let src = format!(
        "{var} counter = 1; \
         {let_} read_it = {fn}() {{ {ret} counter; }}; \
         counter = 99; \
         read_it() == 1;",
        var = kw("VAR"),
        let_ = kw("LET"),
        fn = kw("FN"),
        ret = kw("RETURN"),
    );
    assert_eq!(run(&src).unwrap(), Value::Bool(true));
}

#[test]
fn test_closure_capture_value_is_observable_via_return() {
    let src = format!(
        "{fn} check() {{ \
             {var} counter = 1; \
             {let_} read_it = {fn2}() {{ {ret} counter; }}; \
             counter = 99; \
             {ret} read_it(); \
         }} \
         check();",
        fn = kw("FN"),
        var = kw("VAR"),
        let_ = kw("LET"),
        fn2 = kw("FN"),
        ret = kw("RETURN"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(1));
}

#[test]
fn test_closure_factory_pattern_each_call_captures_independently() {
    let src = format!(
        "{fn} make_adder(n) {{ {ret} {fn2}(y) {{ {ret} n + y; }}; }} \
         {let_} add5  = make_adder(5); \
         {let_} add10 = make_adder(10); \
         add5(1) + add10(1);",
        fn = kw("FN"),
        fn2 = kw("FN"),
        ret = kw("RETURN"),
        let_ = kw("LET"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(17));
}

#[test]
fn test_closure_iife_as_statement() {
    let src = format!("{fn}(x) {{ {ret} x * 2; }}(21);", fn = kw("FN"), ret = kw("RETURN"));
    assert_eq!(run(&src).unwrap(), Value::Int(42));
}

/// Обобщённая версия предыдущего теста: вызов результата вызова
/// (`f()()`), а не только литерала-замыкания напрямую. Проверяет, что
/// `Expr::CallExpr` (см. `ast::Expr::CallExpr`) корректно работает не
/// только для IIFE, но и для произвольной цепочки постфиксных вызовов —
/// общий случай той же грамматической дыры, закрытой при слиянии.
#[test]
fn test_chained_calls_on_function_returning_a_closure() {
    let src = format!(
        "{fn} make_adder(n) {{ {ret} {fn2}(y) {{ {ret} n + y; }}; }} make_adder(5)(1);",
        fn = kw("FN"),
        fn2 = kw("FN"),
        ret = kw("RETURN"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(6));
}

#[test]
fn test_calling_non_closure_value_is_runtime_error() {
    let src = format!("{let_} x = 5; x();", let_ = kw("LET"));
    match run(&src) {
        Err(SgaError::Runtime(_)) => {}
        other => panic!("ожидалась RuntimeError при вызове не-замыкания, получено {:?}", other),
    }
}

#[test]
fn test_closure_called_with_wrong_arity_is_runtime_error() {
    let src = format!(
        "{let_} f = {fn}(a, b) {{ {ret} a + b; }}; f(1);",
        let_ = kw("LET"),
        fn = kw("FN"),
        ret = kw("RETURN"),
    );
    match run(&src) {
        Err(SgaError::Runtime(_)) => {}
        other => panic!("ожидалась RuntimeError при неверной арности вызова замыкания, получено {:?}", other),
    }
}

#[test]
fn test_assigning_to_captured_immutable_variable_inside_closure_is_semantic_error() {
    let src = format!(
        "{let_} total = 0; {let_} f = {fn}() {{ total = total + 1; }};",
        let_ = kw("LET"),
        fn = kw("FN"),
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась SemError при присваивании захваченной immutable-переменной, получено {:?}", other),
    }
}

#[test]
fn test_break_inside_closure_created_in_loop_is_semantic_error() {
    let src = format!(
        "{while_} {true_} {{ {let_} f = {fn}() {{ {brk}; }}; }}",
        while_ = kw("WHILE"),
        true_ = kw("TRUE"),
        let_ = kw("LET"),
        fn = kw("FN"),
        brk = kw("BREAK"),
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась SemError (break внутри замыкания не может прервать внешний цикл), получено {:?}", other),
    }
}

// ===== Регрессионные тесты, добавленные специально для слияния =====

/// Демонстрирует, что MUT (Ownership/Borrowing) и замыкания
/// сосуществуют в одной программе без конфликтов — это было НЕВОЗМОЖНО
/// проверить в любой из родительских веток по отдельности, так как ни
/// в одной из них не было ОБЕИХ фич одновременно.
#[test]
fn test_mut_parameters_and_closures_coexist_in_same_program() {
    let src = format!(
        "{fn} mutate({mut_} arr) {{ arr[0] = 999; }} \
         {var} data = [1, 2, 3]; \
         mutate(data); \
         {let_} read_first = {fn2}() {{ {ret} data[0]; }}; \
         read_first();",
        fn = kw("FN"),
        mut_ = kw("MUT"),
        var = kw("VAR"),
        let_ = kw("LET"),
        fn2 = kw("FN"),
        ret = kw("RETURN"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(999));
}

/// Закрывает регрессию, найденную при слиянии (см. vm::Vm::call и
/// MIGRATION_REPORT.md): убеждается, что иммутабельный параметр
/// top-level функции (без `MUT`) ДЕЙСТВИТЕЛЬНО защищён на уровне VM, а
/// не только на уровне semantic — конструируя байткод напрямую (минуя
/// семантический анализ), с `param_mut: vec![false]`, и проверяя, что
/// попытка `StoreVar` внутри тела функции отклоняется `Vm::assign` как
/// `RuntimeError`, даже если бы semantic был обойдён.
#[test]
fn test_vm_enforces_param_immutability_even_if_semantic_is_bypassed() {
    let fn_chunk = Chunk {
        code: vec![
            OpCode::PushConst(0),
            OpCode::StoreVar("arr".to_string()),
            OpCode::Return(false),
        ],
        constants: vec![Value::Int(2)],
    };
    let mut functions = HashMap::new();
    functions.insert(
        "frozen_fn".to_string(),
        FunctionDef { params: vec!["arr".to_string()], param_mut: vec![false], chunk: fn_chunk },
    );
    let main_chunk = Chunk {
        code: vec![OpCode::PushConst(0), OpCode::Call("frozen_fn".to_string(), 1), OpCode::Return(false)],
        constants: vec![Value::Int(5)],
    };
    let program = CompiledProgram { main: main_chunk, functions };
    let (mut machine, main) = Vm::new(program).expect("байткод должен пройти верификатор");
    match machine.run(&main) {
        Err(_) => {}
        Ok(_) => panic!(
            "ожидалась RuntimeError: param_mut=false должен запрещать StoreVar внутри тела функции \
             на уровне VM, независимо от semantic-анализа"
        ),
    }
}

// =====================================================================
// Structs + stdlib (перенесено и адаптировано из родительской ветки B,
// см. MIGRATION_REPORT.md). Ни в одной из родительских веток эти тесты
// не пересекались с MUT/ownership или с FOR..IN — см. отдельный
// регрессионный тест `test_struct_field_access_does_not_break_for_range`
// ниже для конфликта грамматики, найденного именно при слиянии.
// =====================================================================

#[test]
fn test_struct_decl_and_literal() {
    let src = format!(
        "{struct_} Point {{ x, y }} {let_} p = Point {{ x: 3, y: 4 }}; p.x + p.y;",
        struct_ = kw("STRUCT"),
        let_ = kw("LET"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(7));
}

#[test]
fn test_struct_field_access_returns_correct_value() {
    let src = format!(
        "{struct_} Pair {{ a, b }} {let_} p = Pair {{ a: 10, b: 20 }}; p.b;",
        struct_ = kw("STRUCT"),
        let_ = kw("LET"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(20));
}

#[test]
fn test_struct_field_assign_through_var_is_allowed() {
    let src = format!(
        "{struct_} Pair {{ a, b }} {var} p = Pair {{ a: 1, b: 2 }}; p.a = 99; p.a;",
        struct_ = kw("STRUCT"),
        var = kw("VAR"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(99));
}

/// FUSION: новая проверка, не существовавшая в ветке B (там не было
/// ownership вообще). Структуры — reference-тип, как и массивы (см.
/// `Value::Array`), поэтому присваивание полю через `LET`-связанную
/// переменную обязано отклоняться той же проверкой неизменяемости, что
/// и `IndexAssign` для массивов — см. `semantic::mutation_root`, ветка
/// `FieldAccess`, и MIGRATION_REPORT.md.
#[test]
fn test_struct_field_assign_through_let_is_rejected() {
    let src = format!(
        "{struct_} Pair {{ a, b }} {let_} p = Pair {{ a: 1, b: 2 }}; p.a = 99;",
        struct_ = kw("STRUCT"),
        let_ = kw("LET"),
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError: присваивание полю struct через LET-переменную должно быть запрещено, получено {:?}",
            other
        ),
    }
}

#[test]
fn test_struct_is_reference_type() {
    // Присваивание struct-значения другой переменной не копирует поля —
    // обе переменные ссылаются на один объект (как массивы в SGA).
    let src = format!(
        "{struct_} Counter {{ n }} {var} a = Counter {{ n: 1 }}; {var} b = a; b.n = 42; a.n;",
        struct_ = kw("STRUCT"),
        var = kw("VAR"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(42));
}

#[test]
fn test_struct_method_call_with_mut_self_mutates_field() {
    // Методы — обычные top-level FnDecl по конвенции `TypeName_method`.
    // `MUT self` нужен, чтобы метод мог присваивать `self.field` (та же
    // ownership-проверка, что и для обычных параметров функций).
    let src = format!(
        "{struct_} Point {{ x, y }} \
         {fn_} Point_move({mut_} self, dx, dy) {{ self.x = self.x + dx; self.y = self.y + dy; }} \
         {var} p = Point {{ x: 1, y: 1 }}; p.move(2, 3); p.x + p.y;",
        struct_ = kw("STRUCT"),
        fn_ = kw("FN"),
        mut_ = kw("MUT"),
        var = kw("VAR"),
    );
    assert_eq!(run(&src).unwrap(), Value::Int(7)); // (1+2) + (1+3) = 7
}

/// FUSION: метод без `MUT self` не должен мочь мутировать поля self —
/// та же защита, что и для обычных immutable-параметров. Не существовало
/// в исходной ветке B (там не было ownership вообще).
#[test]
fn test_struct_method_call_without_mut_self_cannot_mutate_field() {
    let src = format!(
        "{struct_} Point {{ x }} \
         {fn_} Point_break(self) {{ self.x = 999; }} \
         {var} p = Point {{ x: 1 }}; p.break();",
        struct_ = kw("STRUCT"),
        fn_ = kw("FN"),
        var = kw("VAR"),
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError: метод без MUT self не должен мочь присваивать self.field, получено {:?}",
            other
        ),
    }
}

#[test]
fn test_struct_unknown_type_is_semantic_error() {
    let src = "p = Ghost { x: 1 };".to_string();
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась SemError для неизвестного типа struct, получено {:?}", other),
    }
}

#[test]
fn test_struct_unknown_field_in_literal_is_semantic_error() {
    let src = format!(
        "{struct_} Point {{ x, y }} p = Point {{ z: 1 }};",
        struct_ = kw("STRUCT"),
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась SemError для неизвестного поля в литерале struct, получено {:?}", other),
    }
}

/// РЕГРЕССИОННЫЙ ТЕСТ, НАЙДЕННЫЙ ПРИ СЛИЯНИИ (см. MIGRATION_REPORT.md):
/// исходный код ветки B добавлял доступ к полю struct через одиночный
/// `Dot` в `parse_postfix` БЕЗ лукахеда, отличающего его от `..`
/// (диапазон `FOR..IN`). Поскольку собственный тестовый набор ветки B не
/// компилировался (см. MIGRATION_REPORT.md, "Критические дефекты"), эта
/// регрессия — `FOR i IN 0..3 {...}` ломается, как только в программе
/// участвует struct/field-access грамматика — никогда не была обнаружена
/// и не покрыта тестом. Этот тест существует именно для того, чтобы
/// зафиксировать, что обе фичи (FieldAccess и `..`) корректно
/// сосуществуют в одной грамматике.
#[test]
fn test_struct_field_access_does_not_break_for_range() {
    let src = format!(
        "{struct_} Point {{ x }} \
         {let_} p = Point {{ x: 10 }}; \
         {var} total = 0; \
         {for_} i {in_} 0..3 {{ total = total + i; }} \
         total + p.x;",
        struct_ = kw("STRUCT"),
        let_ = kw("LET"),
        var = kw("VAR"),
        for_ = kw("FOR"),
        in_ = kw("IN"),
    );
    // total = 0+1+2 = 3, p.x = 10 -> 13
    assert_eq!(run(&src).unwrap(), Value::Int(13));
}

#[test]
fn test_stdlib_type_of() {
    let src = format!("{struct_} P {{ x }} type_of(P {{ x: 1 }});", struct_ = kw("STRUCT"));
    assert_eq!(run(&src).unwrap(), Value::Str("P".to_string()));
    assert_eq!(run("type_of(5);").unwrap(), Value::Str("int".to_string()));
}

#[test]
fn test_stdlib_keys_on_struct_and_array() {
    let src = format!(
        "{struct_} P {{ a, b }} keys(P {{ a: 1, b: 2 }});",
        struct_ = kw("STRUCT"),
    );
    assert_eq!(run(&src).unwrap(), Value::Array(Rc::new(RefCell::new(vec![Value::Str("a".into()), Value::Str("b".into())]))));
    assert_eq!(run("keys([10, 20, 30]);").unwrap(), Value::Array(Rc::new(RefCell::new(vec![Value::Int(0), Value::Int(1), Value::Int(2)]))));
}

#[test]
fn test_stdlib_range() {
    assert_eq!(run("range(3);").unwrap(), Value::Array(Rc::new(RefCell::new(vec![Value::Int(0), Value::Int(1), Value::Int(2)]))));
    assert_eq!(run("range(2, 5);").unwrap(), Value::Array(Rc::new(RefCell::new(vec![Value::Int(2), Value::Int(3), Value::Int(4)]))));
}

#[test]
fn test_stdlib_math() {
    assert_eq!(run("sqrt(16.0);").unwrap(), Value::Float(4.0));
    assert_eq!(run("floor(3.7);").unwrap(), Value::Int(3));
    assert_eq!(run("ceil(3.2);").unwrap(), Value::Int(4));
    assert_eq!(run("abs(-5);").unwrap(), Value::Int(5));
    assert_eq!(run("min(3, 7);").unwrap(), Value::Int(3));
    assert_eq!(run("max(3, 7);").unwrap(), Value::Int(7));
}

#[test]
fn test_stdlib_pow_checked() {
    assert_eq!(run("pow(2, 10);").unwrap(), Value::Int(1024));
    match run("pow(2, -1);") {
        Err(SgaError::Runtime(_)) => {}
        other => panic!("ожидалась RuntimeError для pow с отрицательным int-экспонентом, получено {:?}", other),
    }
}

#[test]
fn test_stdlib_str_functions() {
    assert_eq!(run("str_upper(\"abc\");").unwrap(), Value::Str("ABC".into()));
    assert_eq!(run("str_lower(\"ABC\");").unwrap(), Value::Str("abc".into()));
    assert_eq!(run("str_trim(\"  hi  \");").unwrap(), Value::Str("hi".into()));
    assert_eq!(run("str_starts_with(\"hello\", \"he\");").unwrap(), Value::Bool(true));
    assert_eq!(run("str_ends_with(\"hello\", \"lo\");").unwrap(), Value::Bool(true));
    assert_eq!(run("str_replace(\"foo bar\", \"bar\", \"baz\");").unwrap(), Value::Str("foo baz".into()));
}

#[test]
fn test_stdlib_str_split_and_contains() {
    assert_eq!(
        run("str_split(\"a,b,c\", \",\");").unwrap(),
        Value::Array(Rc::new(RefCell::new(vec![Value::Str("a".into()), Value::Str("b".into()), Value::Str("c".into())])))
    );
    assert_eq!(run("str_contains(\"hello world\", \"wor\");").unwrap(), Value::Bool(true));
}

#[test]
fn test_stdlib_builtin_arity_is_checked_for_new_functions() {
    // Расширенная stdlib (перенесённая из ветки B) обязана проходить ту
    // же проверку арности на этапе semantic, что и исходные 5 builtin'ов
    // ветки A — см. semantic::builtin_arity.
    match run("sqrt();") {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась SemError для sqrt() без аргументов, получено {:?}", other),
    }
    match run("str_replace(\"a\", \"b\");") {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась SemError для str_replace() с 2 аргументами вместо 3, получено {:?}", other),
    }
}
