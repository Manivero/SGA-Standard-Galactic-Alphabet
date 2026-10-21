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
use sga::{codegen, lexer, parser, semantic, RunOptions, SgaError};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

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
        other => panic!(
            "ожидалась ошибка семантики (immutable), получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась ошибка семантики (undefined), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_integer_overflow_is_runtime_error_not_panic() {
    let src = format!(
        "{} x = 9223372036854775807; {}(x + 1);",
        kw("LET"),
        kw("PRINT")
    );
    match run(&src) {
        Err(SgaError::Runtime(msg)) => assert!(msg.contains("переполнение")),
        other => panic!("ожидалась ошибка переполнения, получено {:?}", other),
    }
}

#[test]
fn test_division_by_zero_is_runtime_error() {
    let src = format!(
        "{} a = 1; {} b = 0; {}(a / b);",
        kw("LET"),
        kw("LET"),
        kw("PRINT")
    );
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
        other => panic!(
            "ожидалась ошибка семантики (break вне цикла), получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась ошибка семантики (immutable через индекс), получено {:?}",
            other
        ),
    }
}

/// Тот же баг, но через `push()` вместо индексного присваивания.
#[test]
fn test_immutable_array_push_is_rejected() {
    let src = format!("{} a = [1, 2, 3]; push(a, 4);", kw("LET"));
    match run(&src) {
        Err(SgaError::Semantic(msg)) => assert!(msg.contains("immutable")),
        other => panic!(
            "ожидалась ошибка семантики (immutable через push), получено {:?}",
            other
        ),
    }
}

/// Регрессионный тест на баг, реально найденный во время разработки:
/// самореференцирующийся массив (`a[0] = a;`) вызывал бесконечную
/// рекурсию в Display и stack overflow процесса при печати. Исправлено
/// ограничением глубины в `runtime::fmt_value`. Тест проверяет, что
/// программа теперь завершается успешно (Ok), а не падает.
#[test]
fn test_self_referential_array_does_not_crash_on_print() {
    let src = format!(
        "{var} a = [1, 2, 3]; a[0] = a; {pr}(a);",
        var = kw("VAR"),
        pr = kw("PRINT")
    );
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
        other => panic!(
            "ожидалась ошибка семантики (immutable-параметр без MUT), получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась ошибка семантики (передача LET в MUT-параметр), получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась ошибка типов при переприсваивании, получено {:?}",
            other
        ),
    }
}

/// КЛЮЧЕВОЙ тест на обратную совместимость: без аннотации переменная
/// остаётся полностью динамической — Type System не должна ломать
/// существующий untyped-код.
#[test]
fn test_untyped_var_can_still_change_type_freely() {
    let src = format!(
        "{var} x = 5; x = \"теперь строка\"; {pr}(x);",
        var = kw("VAR"),
        pr = kw("PRINT")
    );
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
        other => panic!(
            "ожидалась ошибка типов (len -> int, не string), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_array_index_must_be_int_when_statically_known() {
    let src = format!(
        "{} idx: bool = {}; {} a = [1, 2, 3]; {}(a[idx]);",
        kw("LET"),
        kw("TRUE"),
        kw("VAR"),
        kw("PRINT")
    );
    match run(&src) {
        Err(SgaError::Type(_)) => {}
        other => panic!(
            "ожидалась ошибка типов (индекс должен быть int), получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась ошибка парсера (вложенный FN), получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась ошибка семантики (len без аргументов), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_push_with_variable_arity_one_or_two_args_is_accepted() {
    let src = format!(
        "{var} a = [1]; {push}(a); {push}(a, 2); {pr}(a);",
        var = kw("VAR"),
        push = "push",
        pr = kw("PRINT")
    );
    assert!(run(&src).is_ok());
}

#[test]
fn test_push_with_three_args_is_semantic_error() {
    let src = format!(
        "{var} a = [1]; {push}(a, 2, 3);",
        var = kw("VAR"),
        push = "push"
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась ошибка семантики (push с 3 аргументами), получено {:?}",
            other
        ),
    }
}

// ===== Защита от stack overflow и bytecode verifier =====

/// Регрессионный/верифицирующий тест на `Vm::max_call_depth`. Этот тест
/// закрывает функциональную часть утверждения: рекурсия глубже
/// `max_call_depth` действительно завершается управляемой
/// `RuntimeError`, а не зависает и не паникует в обычных условиях.
///
/// ВАЖНАЯ ОГОВОРКА: этот тест запускается с заведомо щедрым стеком
/// потока (8 МиБ), поэтому он подтверждает корректность СРАБАТЫВАНИЯ
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
            "поток аварийно завершился даже при щедром стеке 64 МиБ (вероятно, реальный stack \
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
    let program = CompiledProgram {
        main: chunk,
        functions: HashMap::new(),
    };
    match Vm::new(program, None) {
        Err(_) => {}
        Ok(_) => panic!("ожидалась ошибка верификации байткода (PushConst вне границ пула констант), получен Ok"),
    }
}

/// Аналогичный тест для `Jump`/`JumpIfFalse` с целью за пределами чанка.
#[test]
fn test_vm_rejects_chunk_with_jump_target_past_end() {
    let chunk = Chunk {
        code: vec![OpCode::Jump(99), OpCode::Return(false)],
        constants: vec![],
    };
    let program = CompiledProgram {
        main: chunk,
        functions: HashMap::new(),
    };
    match Vm::new(program, None) {
        Err(_) => {}
        Ok(_) => {
            panic!("ожидалась ошибка верификации байткода (Jump за пределы чанка), получен Ok")
        }
    }
}

/// Verifier не должен давать false positive на легальный краевой
/// случай: `Jump`/`JumpIfFalse` с целью, РОВНО равной длине чанка.
#[test]
fn test_vm_accepts_chunk_with_jump_target_exactly_at_end() {
    let chunk = Chunk {
        code: vec![OpCode::Jump(1), OpCode::Return(false)],
        constants: vec![],
    };
    let program = CompiledProgram {
        main: chunk,
        functions: HashMap::new(),
    };
    assert!(Vm::new(program, None).is_ok());
}

/// Verifier должен проверять не только main-чанк, но и чанк каждой
/// определённой функции.
#[test]
fn test_vm_rejects_corrupted_function_chunk_not_just_main() {
    let bad_fn_chunk = Chunk {
        code: vec![OpCode::PushConst(5), OpCode::Return(true)],
        constants: vec![],
    };
    let mut functions = HashMap::new();
    // FUSION: `param_mut` восстановлен в `FunctionDef` (Ownership/
    // Borrowing) — конструктор теста обновлён под новое поле.
    functions.insert(
        "broken".to_string(),
        FunctionDef {
            params: vec![],
            param_mut: vec![],
            chunk: bad_fn_chunk,
        },
    );
    let program = CompiledProgram {
        main: Chunk {
            code: vec![OpCode::Return(false)],
            constants: vec![],
        },
        functions,
    };
    match Vm::new(program, None) {
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
    let program = CompiledProgram {
        main: chunk,
        functions: HashMap::new(),
    };
    // Верификатор не проверяет баланс PushScope/PopScope (см. docstring
    // выше), поэтому Vm::new должен пройти успешно...
    let (mut vm, main) = Vm::new(program, None).expect(
        "verify_program не должен отклонять этот чанк — баланс scope не проверяется статически",
    );
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
        other => panic!(
            "ожидалась RuntimeError при вызове не-замыкания, получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась RuntimeError при неверной арности вызова замыкания, получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась SemError при присваивании захваченной immutable-переменной, получено {:?}",
            other
        ),
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
        FunctionDef {
            params: vec!["arr".to_string()],
            param_mut: vec![false],
            chunk: fn_chunk,
        },
    );
    let main_chunk = Chunk {
        code: vec![
            OpCode::PushConst(0),
            OpCode::Call("frozen_fn".to_string(), 1),
            OpCode::Return(false),
        ],
        constants: vec![Value::Int(5)],
    };
    let program = CompiledProgram {
        main: main_chunk,
        functions,
    };
    let (mut machine, main) = Vm::new(program, None).expect("байткод должен пройти верификатор");
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
        other => panic!(
            "ожидалась SemError для неизвестного типа struct, получено {:?}",
            other
        ),
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
        other => panic!(
            "ожидалась SemError для неизвестного поля в литерале struct, получено {:?}",
            other
        ),
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
    let src = format!(
        "{struct_} P {{ x }} type_of(P {{ x: 1 }});",
        struct_ = kw("STRUCT")
    );
    assert_eq!(run(&src).unwrap(), Value::Str("P".to_string()));
    assert_eq!(run("type_of(5);").unwrap(), Value::Str("int".to_string()));
}

#[test]
fn test_stdlib_keys_on_struct_and_array() {
    let src = format!(
        "{struct_} P {{ a, b }} keys(P {{ a: 1, b: 2 }});",
        struct_ = kw("STRUCT"),
    );
    assert_eq!(
        run(&src).unwrap(),
        Value::Array(Rc::new(RefCell::new(vec![
            Value::Str("a".into()),
            Value::Str("b".into())
        ])))
    );
    assert_eq!(
        run("keys([10, 20, 30]);").unwrap(),
        Value::Array(Rc::new(RefCell::new(vec![
            Value::Int(0),
            Value::Int(1),
            Value::Int(2)
        ])))
    );
}

#[test]
fn test_stdlib_range() {
    assert_eq!(
        run("range(3);").unwrap(),
        Value::Array(Rc::new(RefCell::new(vec![
            Value::Int(0),
            Value::Int(1),
            Value::Int(2)
        ])))
    );
    assert_eq!(
        run("range(2, 5);").unwrap(),
        Value::Array(Rc::new(RefCell::new(vec![
            Value::Int(2),
            Value::Int(3),
            Value::Int(4)
        ])))
    );
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
        other => panic!(
            "ожидалась RuntimeError для pow с отрицательным int-экспонентом, получено {:?}",
            other
        ),
    }
}

#[test]
fn test_stdlib_str_functions() {
    assert_eq!(
        run("str_upper(\"abc\");").unwrap(),
        Value::Str("ABC".into())
    );
    assert_eq!(
        run("str_lower(\"ABC\");").unwrap(),
        Value::Str("abc".into())
    );
    assert_eq!(
        run("str_trim(\"  hi  \");").unwrap(),
        Value::Str("hi".into())
    );
    assert_eq!(
        run("str_starts_with(\"hello\", \"he\");").unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        run("str_ends_with(\"hello\", \"lo\");").unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        run("str_replace(\"foo bar\", \"bar\", \"baz\");").unwrap(),
        Value::Str("foo baz".into())
    );
}

#[test]
fn test_stdlib_str_split_and_contains() {
    assert_eq!(
        run("str_split(\"a,b,c\", \",\");").unwrap(),
        Value::Array(Rc::new(RefCell::new(vec![
            Value::Str("a".into()),
            Value::Str("b".into()),
            Value::Str("c".into())
        ])))
    );
    assert_eq!(
        run("str_contains(\"hello world\", \"wor\");").unwrap(),
        Value::Bool(true)
    );
}

#[test]
fn test_stdlib_builtin_arity_is_checked_for_new_functions() {
    // Расширенная stdlib (перенесённая из ветки B) обязана проходить ту
    // же проверку арности на этапе semantic, что и исходные 5 builtin'ов
    // ветки A — см. semantic::builtin_arity.
    match run("sqrt();") {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError для sqrt() без аргументов, получено {:?}",
            other
        ),
    }
    match run("str_replace(\"a\", \"b\");") {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError для str_replace() с 2 аргументами вместо 3, получено {:?}",
            other
        ),
    }
}

// ===== MATCH-выражение (T006, M002) =====
//
// Feature Contract — см. PROJECT_STATUS.md. Кратко: MATCH — ВЫРАЖЕНИЕ
// (не statement), только литеральные паттерны + два вида catch-all
// (`_` — wildcard, идентификатор — bind), последний arm ОБЯЗАН быть
// catch-all (ошибка компиляции иначе — не runtime error).
//
// ВАЖНО: `_` — единственный паттерн, который пишется как обычный ASCII
// символ подчёркивания (это не SGA-ключевое слово, а обычный
// идентификатор на уровне лексера, см. src/lexer/mod.rs). ЛЮБОЙ другой
// SGA-keyword в исходнике (MATCH, TRUE, FALSE, NIL, LET, VAR, FN, ...)
// обязан идти через `kw(...)` — иначе он лексится как обычный Ident, а
// не как соответствующий TokenKind, и тест проверяет не то, что должен.

#[test]
fn test_match_literal_patterns_all_types() {
    // AC-02: int/float/string/bool/nil, включая отрицательные числа.
    let arms = format!(
        "1 -> \"one\", 2 -> \"two\", -3 -> \"neg-three\", 3.5 -> \"three-half\", \
         -2.5 -> \"neg-two-half\", \"hello\" -> \"greeting\", {true_} -> \"yes\", \
         {false_} -> \"no\", {nil_} -> \"nothing\", _ -> \"other\"",
        true_ = kw("TRUE"),
        false_ = kw("FALSE"),
        nil_ = kw("NIL")
    );
    let cases: &[(&str, &str)] = &[
        ("1", "one"),
        ("2", "two"),
        ("-3", "neg-three"),
        ("3.5", "three-half"),
        ("-2.5", "neg-two-half"),
        ("\"hello\"", "greeting"),
        (&kw("TRUE"), "yes"),
        (&kw("FALSE"), "no"),
        (&kw("NIL"), "nothing"),
        ("12345", "other"),
    ];
    for (scrutinee, expected) in cases {
        let src = format!(
            "{m} {s} {{ {arms} }};",
            m = kw("MATCH"),
            s = scrutinee,
            arms = arms
        );
        match run(&src) {
            Ok(Value::Str(s)) => assert_eq!(s, *expected, "scrutinee = {scrutinee}"),
            other => {
                panic!("scrutinee = {scrutinee}: ожидался Str({expected:?}), получено {other:?}")
            }
        }
    }
}

#[test]
fn test_match_wildcard_catches_unmatched_and_binds_nothing() {
    // AC-03: `_` — catch-all, срабатывает, когда ни один литерал не
    // подошёл, и НЕ вводит новое имя (в отличие от Bind).
    let src = format!(
        "{let_} x = {m} 999 {{ 1 -> \"one\", _ -> \"fallback\" }}; x;",
        let_ = kw("LET"),
        m = kw("MATCH")
    );
    assert_eq!(run(&src).unwrap(), Value::Str("fallback".into()));
}

#[test]
fn test_match_bind_pattern_captures_scrutinee_value() {
    // AC-04 (часть 1): Bind-паттерн связывает СКРУТИНИ (не что-то ещё) с
    // новым именем, видимым в теле arm'а.
    let src = format!(
        "{let_} result = {m} 42 {{ 0 -> \"zero\", n -> n * 2 }}; result;",
        let_ = kw("LET"),
        m = kw("MATCH")
    );
    assert_eq!(run(&src).unwrap(), Value::Int(84));
}

#[test]
fn test_match_bind_name_reassignment_is_rejected() {
    // AC-04 (часть 2): Bind-имя immutable, как любое LET-имя — попытка
    // присвоить ему новое значение внутри тела arm'а — ошибка семантики.
    // (Тело arm'а — ОДНО выражение, не блок — присваивание `n = 10`
    // само по себе валидное выражение, отдельный `{...}`-блок не нужен.)
    let src = format!(
        "{let_} r = {m} 5 {{ n -> n = 10 }};",
        let_ = kw("LET"),
        m = kw("MATCH")
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError: Bind-имя 'n' должно быть immutable, получено {:?}",
            other
        ),
    }
}

#[test]
fn test_match_bind_name_scoped_only_to_its_own_arm() {
    // Bind-имя не должно "протекать" за пределы своего arm'а — если бы
    // scope не закрывался (PopScope), обращение к `n` после MATCH было
    // бы (ошибочно) видно. Проверяем, что ссылка на `n` СНАРУЖИ MATCH —
    // неопределённая переменная.
    let src = format!(
        "{let_} x = {m} 1 {{ n -> n }}; n;",
        let_ = kw("LET"),
        m = kw("MATCH")
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError: 'n' не должно быть видно после MATCH, получено {:?}",
            other
        ),
    }
}

#[test]
fn test_match_missing_catch_all_is_semantic_error() {
    // AC-05: последний arm не catch-all — ошибка КОМПИЛЯЦИИ, не runtime.
    let src = format!(
        "{let_} x = {m} 5 {{ 1 -> \"a\", 2 -> \"b\" }};",
        let_ = kw("LET"),
        m = kw("MATCH")
    );
    match run(&src) {
        Err(SgaError::Semantic(msg)) => {
            assert!(
                msg.contains("catch-all"),
                "сообщение должно объяснять отсутствие catch-all, получено: {msg}"
            );
        }
        other => panic!(
            "ожидалась SemError (нет catch-all последним), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_match_catch_all_not_last_is_semantic_error_wildcard() {
    // AC-06: `_` раньше последней позиции — недостижимый код.
    let src = format!(
        "{let_} x = {m} 5 {{ _ -> \"a\", 1 -> \"b\" }};",
        let_ = kw("LET"),
        m = kw("MATCH")
    );
    match run(&src) {
        Err(SgaError::Semantic(msg)) => {
            assert!(
                msg.contains("недостижим"),
                "сообщение должно объяснять недостижимость, получено: {msg}"
            );
        }
        other => panic!("ожидалась SemError, получено {:?}", other),
    }
}

#[test]
fn test_match_catch_all_not_last_is_semantic_error_bind() {
    // AC-06, вторая форма catch-all (Bind, не только Wildcard).
    let src = format!(
        "{let_} x = {m} 5 {{ n -> n, 1 -> \"b\" }};",
        let_ = kw("LET"),
        m = kw("MATCH")
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError (Bind не последним), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_match_empty_is_semantic_error() {
    // AC-07: пустой MATCH — нет arm'ов вовсе, нет catch-all по построению.
    let src = format!("{let_} x = {m} 5 {{}};", let_ = kw("LET"), m = kw("MATCH"));
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!("ожидалась SemError для пустого MATCH, получено {:?}", other),
    }
}

#[test]
fn test_match_scrutinee_evaluated_exactly_once() {
    // AC-08: если бы scrutinee вычислялось заново для КАЖДОГО сравнения
    // паттерна (баг реализации без кеширования в скрытой переменной),
    // побочный эффект (push в MUT-массив) произошёл бы несколько раз.
    // `counter` — VAR (не LET): передаётся в `bump(MUT c)`, требующий
    // var-bound аргумента (см. semantic::check_call_borrows).
    let src = format!(
        "{var_} counter = []; \
         {fn_} bump({mut_} c) {{ push(c, 1); {return_} len(c); }} \
         {let_} result = {m} bump(counter) {{ \
             1 -> \"matched-first-call\", \
             2 -> \"matched-second-call\", \
             _ -> \"other\" \
         }}; \
         [result, len(counter)];",
        var_ = kw("VAR"),
        let_ = kw("LET"),
        fn_ = kw("FN"),
        mut_ = kw("MUT"),
        return_ = kw("RETURN"),
        m = kw("MATCH")
    );
    match run(&src) {
        Ok(Value::Array(items)) => {
            let items = items.borrow();
            assert_eq!(
                items[0],
                Value::Str("matched-first-call".into()),
                "bump(counter) должен вернуть 1 (первый и единственный вызов push)"
            );
            assert_eq!(
                items[1],
                Value::Int(1),
                "counter должен вырасти РОВНО на 1 элемент — scrutinee вычислен ровно один раз, \
                 а не по разу на каждый паттерн"
            );
        }
        other => panic!("ожидался Array([Str, Int]), получено {:?}", other),
    }
}

#[test]
fn test_match_is_an_expression_assignable_and_usable_in_arithmetic() {
    // AC-09 (часть 1): результат MATCH участвует в арифметике напрямую,
    // без промежуточной переменной.
    let src = format!("1 + {m} 5 {{ 5 -> 10, _ -> 0 }};", m = kw("MATCH"));
    assert_eq!(run(&src).unwrap(), Value::Int(11));
}

#[test]
fn test_match_can_be_passed_as_function_argument() {
    // AC-09 (часть 2): результат MATCH — обычное значение, передаваемое
    // как аргумент вызова.
    let src = format!(
        "{fn_} double(x) {{ {return_} x * 2; }} double({m} 3 {{ 3 -> 21, _ -> 0 }});",
        fn_ = kw("FN"),
        return_ = kw("RETURN"),
        m = kw("MATCH")
    );
    assert_eq!(run(&src).unwrap(), Value::Int(42));
}

#[test]
fn test_match_can_be_nested() {
    // AC-09 (часть 3): MATCH внутри тела arm'а другого MATCH — проверяет
    // и грамматику (парсер), и уникальность скрытых имён scrutinee для
    // вложенных MATCH в одном chunk'е (см. Compiler::match_counter).
    let src = format!(
        "{m} 1 {{ 1 -> {m} 2 {{ 2 -> \"inner-matched\", _ -> \"inner-fallback\" }}, \
         _ -> \"outer-fallback\" }};",
        m = kw("MATCH")
    );
    assert_eq!(run(&src).unwrap(), Value::Str("inner-matched".into()));
}

#[test]
fn test_match_first_matching_arm_wins_not_last() {
    // Порядок arm'ов значим: первый совпавший литерал побеждает.
    let src = format!(
        "{m} 1 {{ 1 -> \"first\", 1 -> \"unreachable-duplicate\", _ -> \"fallback\" }};",
        m = kw("MATCH")
    );
    assert_eq!(run(&src).unwrap(), Value::Str("first".into()));
}

#[test]
fn test_match_inside_loop_recompiles_correctly_each_iteration() {
    // Регрессионный тест на PushScope/PopScope-баланс: MATCH внутри
    // WHILE-тела не должен "накапливать" незакрытые scope между
    // итерациями (это привело бы к растущему стеку scope и, в конечном
    // счёте, к неверному разрешению имён или падению).
    let src = format!(
        "{var_} i = 0; {var2_} total = 0; \
         {while_} i < 5 {{ \
             total = total + {m} i {{ 0 -> 100, n -> n }}; \
             i = i + 1; \
         }} \
         total;",
        var_ = kw("VAR"),
        var2_ = kw("VAR"),
        while_ = kw("WHILE"),
        m = kw("MATCH")
    );
    // i=0 -> 100, i=1..4 -> n (1+2+3+4=10) => 100+10=110
    assert_eq!(run(&src).unwrap(), Value::Int(110));
}

// ===== std/json: json_stringify / json_parse (T008, M003) =====
//
// Feature Contract — см. PROJECT_STATUS.md. Value <-> JSON: Nil<->null,
// Bool<->true/false, Int/Float<->число, Str<->строка, Array<->массив,
// Struct (только stringify)->JSON-объект из реальных полей, JSON-объект
// (только parse)->Array из 2-элементных [ключ,значение] (нет map-типа
// в v0.1). Closure->ошибка при stringify.

#[test]
fn test_json_stringify_scalars() {
    let cases: &[(&str, &str)] = &[
        (&kw("NIL"), "null"),
        (&kw("TRUE"), "true"),
        (&kw("FALSE"), "false"),
        ("42", "42"),
        ("-7", "-7"),
        ("\"hello\"", "\"hello\""),
    ];
    for (expr, expected_json) in cases {
        let src = format!("json_stringify({});", expr);
        match run(&src) {
            Ok(Value::Str(s)) => assert_eq!(s, *expected_json, "expr = {expr}"),
            other => panic!("expr = {expr}: ожидался Str({expected_json:?}), получено {other:?}"),
        }
    }
}

#[test]
fn test_json_stringify_array_nested() {
    let src = format!(
        "json_stringify([1, \"two\", [3, 4], {true_}, {nil_}]);",
        true_ = kw("TRUE"),
        nil_ = kw("NIL")
    );
    assert_eq!(
        run(&src).unwrap(),
        Value::Str("[1,\"two\",[3,4],true,null]".into())
    );
}

#[test]
fn test_json_stringify_struct_uses_real_field_names() {
    // AC-01: Struct -> JSON-объект из РЕАЛЬНЫХ полей (не array-of-pairs
    // — это только для parse, см. AC-02/асимметрию в контракте).
    let src = format!(
        "{struct_} Point {{ x, y }} {let_} p = Point {{ x: 1, y: 2 }}; json_stringify(p);",
        struct_ = kw("STRUCT"),
        let_ = kw("LET")
    );
    // Поля сортируются по имени для детерминированного вывода (HashMap
    // внутри Value::Struct не гарантирует порядок) — см. src/json.rs.
    assert_eq!(run(&src).unwrap(), Value::Str("{\"x\":1,\"y\":2}".into()));
}

#[test]
fn test_json_stringify_closure_is_runtime_error() {
    // AC-05
    let src = format!(
        "json_stringify({fn_}(x) {{ {return_} x; }});",
        fn_ = kw("FN"),
        return_ = kw("RETURN")
    );
    match run(&src) {
        Err(SgaError::Runtime(msg)) => {
            assert!(msg.contains("closure"), "получено: {msg}");
        }
        other => panic!("ожидалась RuntimeError, получено {:?}", other),
    }
}

#[test]
fn test_json_parse_scalars() {
    let cases: &[(&str, Value)] = &[
        ("null", Value::Nil),
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("42", Value::Int(42)),
        ("-7", Value::Int(-7)),
        ("3.5", Value::Float(3.5)),
        ("\\\"hi\\\"", Value::Str("hi".into())),
    ];
    for (json_lit, expected) in cases {
        let src = format!("json_parse(\"{}\");", json_lit);
        assert_eq!(run(&src).unwrap(), *expected, "json_lit = {json_lit}");
    }
}

#[test]
fn test_json_parse_array() {
    let src = "json_parse(\"[1, 2, 3]\");";
    match run(src) {
        Ok(Value::Array(items)) => {
            let items = items.borrow();
            assert_eq!(*items, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        }
        other => panic!("ожидался Array, получено {:?}", other),
    }
}

#[test]
fn test_json_parse_object_becomes_array_of_key_value_pairs() {
    // AC-02: JSON-объект -> Array из 2-элементных Array [ключ, значение]
    let src = "json_parse(\"{\\\"a\\\": 1, \\\"b\\\": 2}\");";
    match run(src) {
        Ok(Value::Array(items)) => {
            let items = items.borrow();
            assert_eq!(items.len(), 2);
            match &items[0] {
                Value::Array(pair) => {
                    let pair = pair.borrow();
                    assert_eq!(pair[0], Value::Str("a".into()));
                    assert_eq!(pair[1], Value::Int(1));
                }
                other => panic!("ожидалась пара [ключ, значение], получено {:?}", other),
            }
        }
        other => panic!(
            "ожидался Array (представление объекта), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_json_parse_invalid_is_runtime_error() {
    // AC-04
    for bad in ["\"not json\"", "\"[1, 2,]\"", "\"{\\\"a\\\": }\"", "\"\""] {
        let src = format!("json_parse({});", bad);
        match run(&src) {
            Err(SgaError::Runtime(_)) => {}
            other => panic!("bad = {bad}: ожидалась RuntimeError, получено {:?}", other),
        }
    }
}

#[test]
fn test_json_roundtrip_via_sga_code() {
    // AC-03: полный round-trip внутри самого SGA-кода (не только в
    // Rust-unit-тестах src/json.rs), через реальный VM pipeline.
    //
    // ВАЖНО: сравнение делается на уровне Rust (Value: PartialEq,
    // структурное сравнение через Rc<RefCell<Vec<Value>>>), а НЕ через
    // оператор `==` внутри самого SGA-кода — у SGA `==` для массивов
    // это identity-equality (`Rc::ptr_eq`, см. src/vm/mod.rs,
    // values_equal), осознанная и задокументированная семантика
    // "reference-типа" (LANGUAGE_SPEC.md §2): `restored` и `original` —
    // заведомо РАЗНЫЕ Rc-аллокации с одинаковым содержимым, поэтому
    // `restored == original` внутри SGA корректно даёт `FALSE`, даже
    // при идеальном round-trip. Это не баг json.rs — это поведение
    // языка, и данный тест проверяет содержимое, а не identity.
    let src = format!(
        "{let_} original = [1, \"two\", {true_}, {nil_}, 3.5]; \
         json_parse(json_stringify(original));",
        let_ = kw("LET"),
        true_ = kw("TRUE"),
        nil_ = kw("NIL")
    );
    match run(&src) {
        Ok(Value::Array(items)) => {
            let items = items.borrow();
            assert_eq!(
                *items,
                vec![
                    Value::Int(1),
                    Value::Str("two".into()),
                    Value::Bool(true),
                    Value::Nil,
                    Value::Float(3.5),
                ]
            );
        }
        other => panic!("ожидался Array, получено {:?}", other),
    }
}

#[test]
fn test_json_wrong_arity_is_semantic_error() {
    // AC-06
    match run("json_stringify();") {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError для json_stringify() без аргументов, получено {:?}",
            other
        ),
    }
    match run("json_parse(\"1\", \"2\");") {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError для json_parse() с 2 аргументами, получено {:?}",
            other
        ),
    }
}

// ===== std/io: read_file (T009, M003) =====
//
// Feature Contract — см. PROJECT_STATUS.md. read_file — sandboxed чтение
// файлов, только ЧТЕНИЕ, переиспользует confinement-границу IMPORT.
// В отличие от большинства тестов файла, эти тесты используют
// РЕАЛЬНЫЕ файлы на диске и run_source_file (не run_source) — read_file
// требует файловый контекст.

/// Создаёт временную директорию с уникальным именем (PID процесса +
/// монотонный счётчик) для теста. Без внешнего крейта `tempfile` —
/// проект остаётся без внешних зависимостей (тот же принцип, что и в
/// src/json.rs).
fn temp_test_dir(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sga_test_{}_{}_{}", std::process::id(), name, n));
    std::fs::create_dir_all(&dir)
        .expect("не удалось создать временную директорию для теста read_file");
    dir
}

#[test]
fn test_read_file_reads_sibling_file_within_root() {
    // AC-01
    let dir = temp_test_dir("read_ok");
    std::fs::write(dir.join("data.txt"), "hello from file").unwrap();
    let entry = dir.join("main.sga");
    let src = format!(
        "{let_} content = read_file(\"data.txt\"); content;",
        let_ = kw("LET")
    );
    std::fs::write(&entry, &src).unwrap();
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(result.unwrap(), Value::Str("hello from file".into()));
}

#[test]
fn test_read_file_absolute_path_is_rejected() {
    // AC-02: абсолютный путь запрещён безусловно, даже указывая на
    // реально существующий, доступный файл.
    let dir = temp_test_dir("read_abs");
    let secret = dir.join("secret.txt");
    std::fs::write(&secret, "should not be readable via absolute path").unwrap();
    let entry = dir.join("main.sga");
    let src = format!("read_file(\"{}\");", secret.to_string_lossy());
    std::fs::write(&entry, &src).unwrap();
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&dir).ok();
    match result {
        Err(SgaError::Runtime(msg)) => {
            assert!(msg.contains("абсолютн"), "получено: {msg}");
        }
        other => panic!(
            "ожидалась RuntimeError (абсолютный путь), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_read_file_parent_escape_is_rejected() {
    // AC-03: '..', выходящий за пределы root, запрещён.
    let outer = temp_test_dir("read_escape_outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(outer.join("secret.txt"), "outside root").unwrap();
    let entry = inner.join("main.sga");
    let src = format!(
        "read_file(\"{}\");",
        std::path::Path::new("..")
            .join("secret.txt")
            .to_string_lossy()
    );
    std::fs::write(&entry, &src).unwrap();
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&outer).ok();
    match result {
        Err(SgaError::Runtime(msg)) => {
            assert!(msg.contains("выходит за пределы"), "получено: {msg}");
        }
        other => panic!(
            "ожидалась RuntimeError (выход за root), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_read_file_without_file_context_is_runtime_error() {
    // AC-04: run_source (не run_source_file) — нет файлового контекста.
    let src = "read_file(\"anything.txt\");";
    match run(src) {
        Err(SgaError::Runtime(msg)) => {
            assert!(msg.contains("файлового контекста"), "получено: {msg}");
        }
        other => panic!(
            "ожидалась RuntimeError (нет файлового контекста), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_read_file_nonexistent_file_is_runtime_error_not_panic() {
    // AC-05
    let dir = temp_test_dir("read_missing");
    let entry = dir.join("main.sga");
    let src = "read_file(\"does_not_exist.txt\");";
    std::fs::write(&entry, src).unwrap();
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&dir).ok();
    match result {
        Err(SgaError::Runtime(_)) => {}
        other => panic!(
            "ожидалась RuntimeError (файл не существует), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_read_file_wrong_arity_is_semantic_error() {
    // AC-06
    match run("read_file();") {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError для read_file() без аргументов, получено {:?}",
            other
        ),
    }
    match run("read_file(\"a\", \"b\");") {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError для read_file() с 2 аргументами, получено {:?}",
            other
        ),
    }
}

#[test]
fn test_read_file_wrong_type_argument_is_runtime_error() {
    let dir = temp_test_dir("read_wrongtype");
    let entry = dir.join("main.sga");
    let src = "read_file(42);";
    std::fs::write(&entry, src).unwrap();
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&dir).ok();
    match result {
        Err(SgaError::Runtime(msg)) => assert!(msg.contains("string"), "получено: {msg}"),
        other => panic!("ожидалась RuntimeError (не string), получено {:?}", other),
    }
}

#[test]
fn test_read_file_in_subdirectory_within_root_works() {
    // Поддиректория ВНУТРИ root — не то же самое, что выход за root
    // ('..') — должна работать нормально.
    let dir = temp_test_dir("read_subdir");
    let sub = dir.join("subdir");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("nested.txt"), "nested content").unwrap();
    let entry = dir.join("main.sga");
    let src = format!(
        "read_file(\"{}\");",
        std::path::Path::new("subdir")
            .join("nested.txt")
            .to_string_lossy()
    );
    std::fs::write(&entry, &src).unwrap();
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(result.unwrap(), Value::Str("nested content".into()));
}

// ===== Optimizer: constant folding + DCE (T010, M004) =====
//
// Feature Contract — см. PROJECT_STATUS.md. Полное покрытие самого
// сворачивания/DCE — в src/optimizer.rs (unit-тесты, работают с AST
// напрямую). Здесь — тесты через ПОЛНЫЙ pipeline (run_source), которые
// unit-тесты в изоляции принципиально не могут проверить: (а) что
// оптимизатор реально подключён и не ломает работающие программы, (б)
// САМОЕ ВАЖНОЕ — что семантические ошибки в НЕДОСТИЖИМОМ (после DCE)
// коде по-прежнему ловятся КОМПИЛЯЦИЕЙ, а не тихо исчезают вместе с
// удаляемым кодом (AC-11) — это проверяет ПОРЯДОК прохода в pipeline
// (semantic/typechecker ДО optimizer), а не сам optimizer.rs в изоляции.

#[test]
fn test_optimizer_folds_arithmetic_through_full_pipeline() {
    let src = format!("{let_} x = 2 * (3 + 4); x;", let_ = kw("LET"));
    assert_eq!(run(&src).unwrap(), Value::Int(14));
}

#[test]
fn test_optimizer_overflow_still_errors_identically_through_full_pipeline() {
    let src_overflow = format!("{let_} x = 9223372036854775807 + 1; x;", let_ = kw("LET"));
    let src_non_const = format!(
        "{let_} a = 9223372036854775807; {let_} x = a + 1; x;",
        let_ = kw("LET")
    );
    let r1 = run(&src_overflow);
    let r2 = run(&src_non_const);
    assert!(
        matches!(r1, Err(SgaError::Runtime(_))),
        "получено: {:?}",
        r1
    );
    assert!(
        matches!(r2, Err(SgaError::Runtime(_))),
        "получено: {:?}",
        r2
    );
}

#[test]
fn test_optimizer_division_by_zero_still_errors_through_full_pipeline() {
    let src = format!("{let_} x = 1 / 0; x;", let_ = kw("LET"));
    match run(&src) {
        Err(SgaError::Runtime(_)) => {}
        other => panic!(
            "ожидалась RuntimeError (деление на ноль), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_optimizer_dce_does_not_hide_semantic_error_in_dead_if_branch() {
    let src = format!(
        "{if_} {false_} {{ undefined_variable_xyz; }}",
        if_ = kw("IF"),
        false_ = kw("FALSE")
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError даже в недостижимой ветке (DCE не должен прятать ошибки), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_optimizer_dce_does_not_hide_semantic_error_in_while_false_body() {
    let src = format!(
        "{while_} {false_} {{ undefined_variable_xyz; }}",
        while_ = kw("WHILE"),
        false_ = kw("FALSE")
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError даже в WHILE FALSE (DCE не должен прятать ошибки), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_optimizer_unreachable_code_after_return_does_not_hide_semantic_error() {
    let src = format!(
        "{fn_} f() {{ {return_} 1; undefined_variable_xyz; }} f();",
        fn_ = kw("FN"),
        return_ = kw("RETURN")
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError для кода после RETURN (DCE не должен прятать ошибки), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_optimizer_if_true_dce_preserves_scoping() {
    let src = format!(
        "{if_} {true_} {{ {let_} y = 5; }} y;",
        if_ = kw("IF"),
        true_ = kw("TRUE"),
        let_ = kw("LET")
    );
    match run(&src) {
        Err(SgaError::Semantic(_)) => {}
        other => panic!(
            "ожидалась SemError: 'y' не должно быть видно снаружи свёрнутого IF, получено {:?}",
            other
        ),
    }
}

#[test]
fn test_optimizer_while_false_removed_body_never_executes() {
    let src = format!(
        "{var_} counter = 0; \
         {while_} {false_} {{ counter = counter + 1; }} \
         counter;",
        var_ = kw("VAR"),
        while_ = kw("WHILE"),
        false_ = kw("FALSE")
    );
    assert_eq!(run(&src).unwrap(), Value::Int(0));
}

#[test]
fn test_optimizer_string_and_logical_folding_through_full_pipeline() {
    let src = format!(
        "{let_} s = \"foo\" + \"bar\"; {let_} b = {true_} {and_} {false_}; [s, b];",
        let_ = kw("LET"),
        true_ = kw("TRUE"),
        and_ = kw("AND"),
        false_ = kw("FALSE")
    );
    match run(&src) {
        Ok(Value::Array(items)) => {
            let items = items.borrow();
            assert_eq!(
                *items,
                vec![Value::Str("foobar".into()), Value::Bool(false)]
            );
        }
        other => panic!("ожидался Array, получено {:?}", other),
    }
}

// ===== Package Manager: path-зависимости (T011, M005) =====
//
// Feature Contract — см. PROJECT_STATUS.md. Только path-зависимости
// (`name = { path = "..." }` в sga.toml), `IMPORT "pkg:имя/файл.sga";`.
// Реальные временные директории на диске — как и для read_file (T009).

fn write_test_file(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn double_fn_src() -> String {
    format!(
        "{fn_} double(x) {{ {return_} x * 2; }}",
        fn_ = kw("FN"),
        return_ = kw("RETURN")
    )
}

#[test]
fn test_pkg_import_resolves_file_from_declared_path_dependency() {
    // AC-02
    let root = temp_test_dir("pkg_ok");
    let project = root.join("project");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"../mylib\" }\n",
    );
    write_test_file(&root.join("mylib").join("utils.sga"), &double_fn_src());
    let entry = project.join("main.sga");
    write_test_file(
        &entry,
        &format!(
            "{import_} \"pkg:mylib/utils.sga\"; double(21);",
            import_ = kw("IMPORT")
        ),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(result.unwrap(), Value::Int(42));
}

#[test]
fn test_pkg_import_resolves_nested_subdirectory_inside_package() {
    let root = temp_test_dir("pkg_nested");
    let project = root.join("project");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"../mylib\" }\n",
    );
    write_test_file(
        &root.join("mylib").join("sub").join("deep.sga"),
        &double_fn_src(),
    );
    let entry = project.join("main.sga");
    write_test_file(
        &entry,
        &format!(
            "{import_} \"pkg:mylib/sub/deep.sga\"; double(5);",
            import_ = kw("IMPORT")
        ),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(result.unwrap(), Value::Int(10));
}

#[test]
fn test_pkg_import_same_file_twice_is_deduplicated() {
    // Повторный IMPORT того же pkg-файла не должен дублировать
    // определения (иначе "функция уже объявлена" — тот же механизм
    // visited, что и для обычных импортов).
    let root = temp_test_dir("pkg_dedup");
    let project = root.join("project");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"../mylib\" }\n",
    );
    write_test_file(&root.join("mylib").join("utils.sga"), &double_fn_src());
    let entry = project.join("main.sga");
    write_test_file(
        &entry,
        &format!(
            "{import_} \"pkg:mylib/utils.sga\"; {import_} \"pkg:mylib/utils.sga\"; double(4);",
            import_ = kw("IMPORT")
        ),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(result.unwrap(), Value::Int(8));
}

#[test]
fn test_pkg_import_undeclared_package_is_clear_import_error() {
    // AC-03
    let root = temp_test_dir("pkg_undeclared");
    write_test_file(&root.join("sga.toml"), "[dependencies]\n");
    let entry = root.join("main.sga");
    write_test_file(
        &entry,
        &format!("{import_} \"pkg:nope/x.sga\";", import_ = kw("IMPORT")),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    match result {
        Err(SgaError::Import(msg)) => {
            assert!(msg.contains("не объявлен"), "получено: {msg}");
            assert!(msg.contains("nope"), "получено: {msg}");
        }
        other => panic!("ожидалась ImportError, получено {:?}", other),
    }
}

#[test]
fn test_pkg_bare_version_dependency_is_clear_import_error() {
    // AC-04: name = "1.0.0" без path — явная ошибка, не паника. Ошибка
    // манифеста всплывает на pkg:-импорте (ленивая валидация — см. тест
    // `test_malformed_manifest_does_not_affect_programs_without_pkg_imports`),
    // поэтому программа обязана содержать pkg:-импорт.
    let root = temp_test_dir("pkg_bare_version");
    write_test_file(
        &root.join("sga.toml"),
        "[dependencies]\nmylib = \"1.0.0\"\n",
    );
    let entry = root.join("main.sga");
    write_test_file(
        &entry,
        &format!("{import_} \"pkg:mylib/x.sga\";", import_ = kw("IMPORT")),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    match result {
        Err(SgaError::Import(msg)) => {
            assert!(msg.contains("path-зависимости"), "получено: {msg}");
        }
        other => panic!("ожидалась ImportError, получено {:?}", other),
    }
}

#[test]
fn test_pkg_import_dotdot_escaping_package_root_is_rejected() {
    // AC-05 (форма 1): sub_path в самой pkg:-строке выходит за корень пакета.
    let root = temp_test_dir("pkg_escape1");
    let project = root.join("project");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"../mylib\" }\n",
    );
    write_test_file(&root.join("mylib").join("ok.sga"), "1;");
    write_test_file(&root.join("secret.sga"), "2;");
    let entry = project.join("main.sga");
    write_test_file(
        &entry,
        &format!(
            "{import_} \"pkg:mylib/../secret.sga\";",
            import_ = kw("IMPORT")
        ),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    match result {
        Err(SgaError::Import(msg)) => {
            assert!(msg.contains("выходит за пределы"), "получено: {msg}");
        }
        other => panic!("ожидалась ImportError, получено {:?}", other),
    }
}

#[test]
fn test_pkg_internal_import_confined_to_package_root_not_main_project_root() {
    // AC-05 (форма 2 — ключевая): пакет лежит ВНУТРИ каталога основного
    // проекта (vendor/mylib), и его файл пытается импортировать
    // ../../main_helper.sga — файл, лежащий внутри корня ОСНОВНОГО
    // проекта, но ВНЕ корня пакета. Если бы confinement внутри пакета
    // использовал корень основного проекта, это было бы разрешено —
    // отказ доказывает, что у пакета СВОЙ собственный root.
    let project = temp_test_dir("pkg_own_root");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"vendor/mylib\" }\n",
    );
    write_test_file(&project.join("main_helper.sga"), "1;");
    write_test_file(
        &project.join("vendor").join("mylib").join("utils.sga"),
        &format!(
            "{import_} \"../../main_helper.sga\";",
            import_ = kw("IMPORT")
        ),
    );
    let entry = project.join("main.sga");
    write_test_file(
        &entry,
        &format!("{import_} \"pkg:mylib/utils.sga\";", import_ = kw("IMPORT")),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&project).ok();
    match result {
        Err(SgaError::Import(msg)) => {
            assert!(msg.contains("выходит за пределы"), "получено: {msg}");
        }
        other => panic!("ожидалась ImportError, получено {:?}", other),
    }
}

#[test]
fn test_pkg_transitive_pkg_import_inside_package_is_rejected() {
    // AC-06
    let root = temp_test_dir("pkg_transitive");
    let project = root.join("project");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"../mylib\" }\nother = { path = \"../other\" }\n",
    );
    write_test_file(&root.join("other").join("x.sga"), "1;");
    write_test_file(
        &root.join("mylib").join("utils.sga"),
        &format!("{import_} \"pkg:other/x.sga\";", import_ = kw("IMPORT")),
    );
    let entry = project.join("main.sga");
    write_test_file(
        &entry,
        &format!("{import_} \"pkg:mylib/utils.sga\";", import_ = kw("IMPORT")),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    match result {
        Err(SgaError::Import(msg)) => {
            assert!(msg.contains("транзитивные"), "получено: {msg}");
        }
        other => panic!("ожидалась ImportError, получено {:?}", other),
    }
}

#[test]
fn test_pkg_import_without_manifest_says_not_declared_not_manifest_missing() {
    // AC-07: нет sga.toml вообще — pkg:-импорт даёт "не объявлен", а не
    // "manifest не найден" (обратная совместимость: программы без
    // зависимостей не обязаны иметь sga.toml).
    // Примечание: поиск манифеста идёт ВВЕРХ по дереву, поэтому тест
    // предполагает, что в окружении нет посторонних /tmp/sga.toml и
    // /sga.toml (случайный чужой манифест выше изменил бы результат).
    let root = temp_test_dir("pkg_no_manifest");
    let entry = root.join("main.sga");
    write_test_file(
        &entry,
        &format!("{import_} \"pkg:mylib/x.sga\";", import_ = kw("IMPORT")),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    match result {
        Err(SgaError::Import(msg)) => {
            assert!(msg.contains("не объявлен"), "получено: {msg}");
            assert!(!msg.to_lowercase().contains("manifest"), "получено: {msg}");
        }
        other => panic!("ожидалась ImportError, получено {:?}", other),
    }
}

#[test]
fn test_regular_relative_import_still_works_without_any_manifest() {
    // AC-07 (вторая половина): обычные (не pkg:) импорты не затронуты —
    // обратная совместимость.
    let root = temp_test_dir("pkg_regular_still_works");
    write_test_file(&root.join("helper.sga"), &double_fn_src());
    let entry = root.join("main.sga");
    write_test_file(
        &entry,
        &format!(
            "{import_} \"helper.sga\"; double(7);",
            import_ = kw("IMPORT")
        ),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(result.unwrap(), Value::Int(14));
}

#[test]
fn test_sga_init_generates_commented_path_dependency_example() {
    // AC-08: `sga init` генерирует sga.toml с закомментированным примером
    // path-зависимости (и этот пример НЕ ломает разбор манифеста —
    // строки на '#' игнорируются парсером).
    let dir = temp_test_dir("pkg_init");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("init")
        .arg(&dir)
        .output()
        .expect("не удалось запустить sga init");
    assert!(
        status.status.success(),
        "sga init завершился ошибкой: {:?}",
        status
    );
    let toml = std::fs::read_to_string(dir.join("sga.toml")).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        toml.contains("# mylib = { path = \"../mylib\" }"),
        "ожидался закомментированный пример, получено:\n{toml}"
    );
    // и сгенерированный манифест парсится без ошибок (пустая карта)
    assert!(sga::manifest::load_dependencies(&dir).unwrap().is_empty());
}

#[test]
fn test_malformed_manifest_does_not_affect_programs_without_pkg_imports() {
    // Обратная совместимость (шире AC-07): sga.toml читается ЛИШЬ ради
    // pkg:-импортов. Поиск идёт вверх по дереву, "ближайший" манифест
    // может быть посторонним, — программа без pkg:-импортов обязана вести
    // себя ровно как до T011, что бы в нём ни лежало.
    let root = temp_test_dir("pkg_bad_manifest_unused");
    write_test_file(
        &root.join("sga.toml"),
        "[dependencies]\nmylib = \"1.0.0\"\nnot even toml\n",
    );
    write_test_file(&root.join("helper.sga"), &double_fn_src());
    let entry = root.join("main.sga");
    write_test_file(
        &entry,
        &format!(
            "{import_} \"helper.sga\"; double(8);",
            import_ = kw("IMPORT")
        ),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(result.unwrap(), Value::Int(16));
}

#[test]
fn test_pkg_manifest_in_parent_of_entry_dir_is_found_and_paths_are_relative_to_manifest() {
    // AC-10: раскладка, которую создаёт `sga init` — sga.toml в корне
    // проекта, запускаемый файл в src/. Путь "../mylib" считается от
    // каталога МАНИФЕСТА (project/), а не от каталога входного файла
    // (project/src/): от project/src он указывал бы на project/mylib,
    // которого нет, — тест упал бы, если бы путь считался от входного файла.
    let root = temp_test_dir("pkg_parent_manifest");
    let project = root.join("project");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"../mylib\" }\n",
    );
    write_test_file(&root.join("mylib").join("utils.sga"), &double_fn_src());
    let entry = project.join("src").join("main.sga");
    write_test_file(
        &entry,
        &format!(
            "{import_} \"pkg:mylib/utils.sga\"; double(21);",
            import_ = kw("IMPORT")
        ),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(result.unwrap(), Value::Int(42));
}

#[test]
fn test_pkg_nearest_manifest_wins_over_ancestor_manifest() {
    // AC-10: два манифеста по пути вверх — побеждает ближайший. Внешний
    // даёт double = x*2, внутренний (ближайший) — double = x*3.
    let root = temp_test_dir("pkg_nearest");
    write_test_file(
        &root.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"outer_lib\" }\n",
    );
    write_test_file(&root.join("outer_lib").join("utils.sga"), &double_fn_src());
    let project = root.join("project");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"../inner_lib\" }\n",
    );
    write_test_file(
        &root.join("inner_lib").join("utils.sga"),
        &format!(
            "{fn_} double(x) {{ {return_} x * 3; }}",
            fn_ = kw("FN"),
            return_ = kw("RETURN")
        ),
    );
    let entry = project.join("src").join("main.sga");
    write_test_file(
        &entry,
        &format!(
            "{import_} \"pkg:mylib/utils.sga\"; double(10);",
            import_ = kw("IMPORT")
        ),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(result.unwrap(), Value::Int(30));
}

#[test]
fn test_pkg_nearest_manifest_is_not_merged_with_ancestor_manifest() {
    // AC-10: ближайший манифест побеждает ЦЕЛИКОМ, без слияния с внешним:
    // зависимость, объявленная только во внешнем sga.toml, из вложенного
    // проекта недоступна.
    let root = temp_test_dir("pkg_no_merge");
    write_test_file(
        &root.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"mylib\" }\n",
    );
    write_test_file(&root.join("mylib").join("utils.sga"), &double_fn_src());
    let project = root.join("project");
    write_test_file(&project.join("sga.toml"), "[dependencies]\n");
    let entry = project.join("main.sga");
    write_test_file(
        &entry,
        &format!("{import_} \"pkg:mylib/utils.sga\";", import_ = kw("IMPORT")),
    );
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&root).ok();
    match result {
        Err(SgaError::Import(msg)) => {
            assert!(msg.contains("не объявлен"), "получено: {msg}");
        }
        other => panic!("ожидалась ImportError, получено {:?}", other),
    }
}

#[test]
fn test_sga_init_scaffold_runs_with_path_dependency_end_to_end_via_cli() {
    // AC-11: сквозной CLI-сценарий на раскладке, которую создаёт сам
    // `sga init`: init -> раскомментировать пример зависимости ->
    // `sga run <проект>/src/main.sga`. Три формы пути запускаемого файла:
    // абсолютный; относительный из каталога проекта (`src/main.sga`);
    // голое имя файла из каталога src/ (`main.sga` — родитель
    // относительного пути пуст). До исправления дефекта (манифест
    // искался только рядом с запускаемым файлом) все три давали "пакет
    // 'mylib' не объявлен в [dependencies] sga.toml".
    let root = temp_test_dir("pkg_cli_e2e");
    let project = root.join("proj");
    let bin = env!("CARGO_BIN_EXE_sga");
    let init = std::process::Command::new(bin)
        .arg("init")
        .arg(&project)
        .output()
        .expect("не удалось запустить sga init");
    assert!(init.status.success(), "sga init: {:?}", init);

    let toml_path = project.join("sga.toml");
    let toml = std::fs::read_to_string(&toml_path).unwrap();
    let enabled = toml.replace(
        "# mylib = { path = \"../mylib\" }",
        "mylib = { path = \"../mylib\" }",
    );
    assert_ne!(
        enabled, toml,
        "в sga.toml не найден закомментированный пример"
    );
    std::fs::write(&toml_path, enabled).unwrap();
    write_test_file(&root.join("mylib").join("utils.sga"), &double_fn_src());
    let main_sga = project.join("src").join("main.sga");
    std::fs::write(
        &main_sga,
        format!(
            "{import_} \"pkg:mylib/utils.sga\"; {print_}(double(21));",
            import_ = kw("IMPORT"),
            print_ = kw("PRINT")
        ),
    )
    .unwrap();

    let absolute = std::process::Command::new(bin)
        .arg("run")
        .arg(&main_sga)
        .output()
        .unwrap();
    let relative = std::process::Command::new(bin)
        .current_dir(&project)
        .arg("run")
        .arg("src/main.sga")
        .output()
        .unwrap();
    let bare_name = std::process::Command::new(bin)
        .current_dir(project.join("src"))
        .arg("run")
        .arg("main.sga")
        .output()
        .unwrap();
    std::fs::remove_dir_all(&root).ok();

    for (label, out) in [
        ("абсолютный путь", absolute),
        ("относительный путь из каталога проекта", relative),
        ("голое имя файла из src/", bare_name),
    ] {
        assert!(
            out.status.success(),
            "{label}: stderr = {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42", "{label}");
    }
}

// ===== std/process: args() (T012, M006) =====
//
// Feature Contract — см. PROJECT_STATUS.md. `args()` возвращает аргументы
// командной строки ПРОГРАММЫ (без пути к ней) как массив string.
// Параметры запуска передаются через `RunOptions` (builder), CLI
// передаёт программе всё, что стоит после пути файла, дословно.
//
// Значения в тестах наблюдаются через значение последнего выражения
// программы (`run*` его возвращает); сквозные тесты через настоящий
// бинарник читают stdout.

#[test]
fn test_args_without_options_is_empty_array() {
    // AC-01 (библиотека): `run_source`/`run_source_file` запускают
    // программу с параметрами по умолчанию — аргументов нет.
    assert_eq!(run("len(args());").unwrap(), Value::Int(0));
    assert_eq!(
        run("json_stringify(args());").unwrap(),
        Value::Str("[]".into())
    );

    let dir = temp_test_dir("args_default_file");
    let entry = dir.join("main.sga");
    write_test_file(&entry, "len(args());");
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(result.unwrap(), Value::Int(0));
}

#[test]
fn test_args_returns_passed_strings_in_order_as_strings() {
    // AC-02: порядок сохраняется, пустая строка и строки, похожие на флаги,
    // не теряются и не интерпретируются; каждый элемент — string.
    let options = RunOptions::new().with_args(["a", "b c", "--flag", ""]);
    let result = sga::run_source_with_options("json_stringify(args());", options);
    assert_eq!(
        result.unwrap(),
        Value::Str("[\"a\",\"b c\",\"--flag\",\"\"]".into())
    );

    let options = RunOptions::new().with_args(["5", "true"]);
    let result = sga::run_source_with_options(
        "json_stringify([type_of(args()[0]), type_of(args()[1])]);",
        options,
    );
    assert_eq!(
        result.unwrap(),
        Value::Str("[\"string\",\"string\"]".into()),
        "\"5\" и \"true\" должны остаться строками, а не превратиться в int/bool"
    );
}

#[test]
fn test_args_preserves_unicode_arguments() {
    let options = RunOptions::new().with_args(["日本語 🦀", "привет"]);
    let result = sga::run_source_with_options("args()[0];", options.clone());
    assert_eq!(result.unwrap(), Value::Str("日本語 🦀".into()));
    let result = sga::run_source_with_options("len(args()[1]);", options);
    assert_eq!(
        result.unwrap(),
        Value::Int(6),
        "len() считает символы, не байты"
    );
}

#[test]
fn test_run_source_file_with_options_passes_args() {
    // AC-02 (файловый вариант запуска).
    let dir = temp_test_dir("args_file_opts");
    let entry = dir.join("main.sga");
    write_test_file(&entry, "json_stringify(args());");
    let options = RunOptions::new().with_args(["x", "y z"]);
    let result = sga::run_source_file_with_options(&entry, options);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(result.unwrap(), Value::Str("[\"x\",\"y z\"]".into()));
}

#[test]
fn test_args_each_call_returns_independent_array() {
    // AC-03: мутация результата одного вызова не видна следующему
    // вызову `args()` (Value::Array — общий Rc<RefCell<..>>, поэтому
    // отдавать один и тот же экземпляр было бы ошибкой).
    let src = format!(
        "{var_} a = args(); push(a, \"mutated\"); json_stringify([len(a), len(args())]);",
        var_ = kw("VAR")
    );
    let options = RunOptions::new().with_args(["one", "two"]);
    let result = sga::run_source_with_options(&src, options);
    assert_eq!(result.unwrap(), Value::Str("[3,2]".into()));
}

#[test]
fn test_args_is_available_inside_functions() {
    // `args()` — вызов builtin, обрабатываемый VM в любом кадре, не
    // только в главном блоке.
    let src = format!(
        "{fn_} count() {{ {return_} len(args()); }} count();",
        fn_ = kw("FN"),
        return_ = kw("RETURN")
    );
    let options = RunOptions::new().with_args(["a", "b"]);
    let result = sga::run_source_with_options(&src, options);
    assert_eq!(result.unwrap(), Value::Int(2));
}

#[test]
fn test_args_index_out_of_range_without_arguments_is_runtime_error() {
    // Типичная ошибка скрипта — прочитать `args()[0]`, когда аргумент не
    // передали: это управляемая RuntimeError, не panic.
    match run("args()[0];") {
        Err(SgaError::Runtime(_)) => {}
        other => panic!(
            "ожидалась RuntimeError (индекс вне границ пустого массива), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_variable_and_parameter_named_args_are_unaffected_by_builtin() {
    // Обратная совместимость: имя `args` занято ТОЛЬКО как имя вызова
    // `args(..)`. Переменная и параметр с таким именем работают как раньше.
    let src = format!(
        "{fn_} count(args) {{ {return_} len(args); }} {let_} args = [1, 2, 3]; count(args) + len(args);",
        fn_ = kw("FN"),
        return_ = kw("RETURN"),
        let_ = kw("LET")
    );
    assert_eq!(run(&src).unwrap(), Value::Int(6));
}

#[test]
fn test_args_with_arguments_is_semantic_error() {
    // AC-04 (статическая часть): арность (0, 0) проверяется на этапе
    // семантического анализа, до исполнения.
    match run("args(1);") {
        Err(SgaError::Semantic(msg)) => {
            assert!(msg.contains("'args'"), "получено: {msg}");
            assert!(msg.contains("ожидает 0"), "получено: {msg}");
        }
        other => panic!("ожидалась SemError для args(1), получено {:?}", other),
    }
}

#[test]
fn test_vm_args_call_with_arguments_via_handbuilt_bytecode_is_runtime_error() {
    // AC-04 (рантайм-часть, defense-in-depth): VM можно запустить на
    // вручную собранном байткоде в обход semantic — `Call("args", 1)`
    // обязан дать RuntimeError, а не panic или молчаливое игнорирование.
    let chunk = Chunk {
        code: vec![
            OpCode::PushConst(0),
            OpCode::Call("args".to_string(), 1),
            OpCode::Return(true),
        ],
        constants: vec![Value::Int(1)],
    };
    let program = CompiledProgram {
        main: chunk,
        functions: HashMap::new(),
    };
    let (mut machine, main) = Vm::new(program, None).expect("байткод должен пройти верификатор");
    match machine.run(&main) {
        Err(e) => assert!(e.to_string().contains("args()"), "получено: {e}"),
        Ok(v) => panic!(
            "ожидалась RuntimeError (args с аргументом), получено {:?}",
            v
        ),
    }
}

#[test]
fn test_vm_set_args_is_visible_to_args_builtin() {
    // Уровень VM без lib.rs: `Vm::set_args` между `Vm::new` и `run`.
    // `Vm::new(program, None)` при этом остаётся прежней сигнатуры (AC-07).
    let chunk = Chunk {
        code: vec![OpCode::Call("args".to_string(), 0), OpCode::Return(true)],
        constants: vec![],
    };
    let program = CompiledProgram {
        main: chunk,
        functions: HashMap::new(),
    };
    let (mut machine, main) = Vm::new(program, None).expect("байткод должен пройти верификатор");
    machine.set_args(vec!["first".to_string(), "second".to_string()]);
    match machine.run(&main) {
        Ok(Value::Array(items)) => {
            let items = items.borrow();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0], Value::Str("first".into()));
            assert_eq!(items[1], Value::Str("second".into()));
        }
        other => panic!("ожидался массив из двух строк, получено {:?}", other),
    }
}

#[test]
fn test_cli_run_passes_arguments_after_file_verbatim_to_program() {
    // AC-01 (CLI) и AC-05: сквозной тест через НАСТОЯЩИЙ бинарник. Всё
    // после пути файла — аргументы программы, дословно: пробелы внутри
    // аргумента, пустая строка, то, что выглядит как флаги самого sga
    // (`--version`, `-v`, `--help`), и `--`.
    let dir = temp_test_dir("args_cli");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &format!("{print_}(json_stringify(args()));", print_ = kw("PRINT")),
    );
    let bin = env!("CARGO_BIN_EXE_sga");

    let without = std::process::Command::new(bin)
        .arg("run")
        .arg(&entry)
        .output()
        .unwrap();
    let with = std::process::Command::new(bin)
        .arg("run")
        .arg(&entry)
        .args(["a", "b c", "--flag", "--", "-v", "--version", "--help", ""])
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        without.status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&without.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&without.stdout).trim(), "[]");
    assert!(
        with.status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&with.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&with.stdout).trim(),
        "[\"a\",\"b c\",\"--flag\",\"--\",\"-v\",\"--version\",\"--help\",\"\"]"
    );
}

#[cfg(unix)]
#[test]
fn test_cli_run_with_non_utf8_argument_is_clear_error_not_panic() {
    // AC-06: строки SGA — UTF-8, поэтому не-UTF-8 аргумент передать
    // программе нельзя. Раньше `env::args()` в этом случае паниковал
    // (код возврата 101); теперь — понятное сообщение и обычный код
    // неуспеха 1.
    use std::os::unix::ffi::OsStrExt;
    let dir = temp_test_dir("args_non_utf8");
    let entry = dir.join("main.sga");
    write_test_file(&entry, "len(args());");
    let bad = std::ffi::OsStr::from_bytes(b"\xff\xfe");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(&entry)
        .arg(bad)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr = {stderr}");
    assert!(stderr.contains("UTF-8"), "получено: {stderr}");
    assert!(!stderr.contains("panicked"), "получено: {stderr}");
}

// ===== std/io: read_line() / read_stdin() (T013, M006) =====
//
// Feature Contract — см. PROJECT_STATUS.md. `read_line()` — следующая
// строка стандартного ввода без `\n`/`\r\n` (`nil` на EOF; пустая строка
// отличима от EOF), `read_stdin()` — весь остаток ввода дословно. Ввод
// задаётся через `RunOptions::with_stdin_bytes` (детерминированные тесты без
// процесса); CLI подключает stdin ПРОЦЕССА — его проверяют сквозные тесты
// через настоящий pipe в конце секции. Разбор формата ввода (границы
// `\r\n`/EOF/UTF-8, ошибка читателя) дополнительно покрыт юнит-тестами в
// `src/stdin.rs`.
//
// Значения наблюдаются через значение последнего выражения программы;
// stdout — только в сквозных CLI-тестах.

/// Исходник из шаблона: `@LET`, `@VAR`, … заменяются SGA-кодировкой
/// соответствующих ключевых слов (`kw`) — так тесты читаются как обычные
/// программы. Остальной текст шаблона остаётся как есть (в шаблонах нет
/// строковых литералов, содержащих `@`).
fn kw_src(template: &str) -> String {
    let mut out = template.to_string();
    for word in [
        "LET", "VAR", "FN", "RETURN", "IF", "ELSE", "WHILE", "BREAK", "NIL", "PRINT", "TRUE",
    ] {
        out = out.replace(&format!("@{word}"), &kw(word));
    }
    out
}

/// Запускает программу с заданными байтами стандартного ввода.
fn run_with_stdin(source: &str, input: impl Into<Vec<u8>>) -> Result<Value, SgaError> {
    sga::run_source_with_options(source, RunOptions::new().with_stdin_bytes(input))
}

/// Запускает программу с произвольным читателем на месте stdin (путь
/// «текст -> токены -> AST -> байткод -> `Vm::set_stdin` -> `run`» без
/// `RunOptions`): нужен, чтобы подставить читатель с буфером в 1 байт.
fn run_with_reader(
    source: &str,
    reader: impl std::io::BufRead + 'static,
) -> Result<Value, SgaError> {
    let tokens = lexer::Lexer::new(source)
        .tokenize()
        .map_err(|e| SgaError::Lex(e.to_string()))?;
    let program = parser::Parser::new(tokens)
        .parse_program()
        .map_err(|e| SgaError::Parse(e.to_string()))?;
    let compiled = codegen::compile(&program);
    let (mut machine, main) =
        Vm::new(compiled, None).map_err(|e| SgaError::Runtime(e.to_string()))?;
    machine.set_stdin(reader);
    machine
        .run(&main)
        .map_err(|e| SgaError::Runtime(e.to_string()))
}

fn string_value(text: &str) -> Value {
    Value::Str(text.to_string())
}

/// Программа «читать до nil и сложить числа»: типичное использование.
fn sum_lines_src() -> String {
    kw_src(
        "@VAR total = 0; \
         @WHILE @TRUE { \
             @LET line = read_line(); \
             @IF line == @NIL { @BREAK; } \
             total = total + to_int(line); \
         } \
         total;",
    )
}

#[test]
fn test_read_line_returns_lines_one_by_one_then_nil() {
    // AC-01: два вызова сверх ввода — nil и снова nil (EOF «залипает»).
    let src = "json_stringify([read_line(), read_line(), read_line(), read_line()]);";
    assert_eq!(
        run_with_stdin(src, "a\nb\n").unwrap(),
        string_value("[\"a\",\"b\",null,null]")
    );
    // Последняя строка без завершающего перевода строки — всё равно строка.
    assert_eq!(
        run_with_stdin(src, "a\nb").unwrap(),
        string_value("[\"a\",\"b\",null,null]")
    );
    // Пустой ввод: сразу EOF.
    assert_eq!(
        run_with_stdin(src, "").unwrap(),
        string_value("[null,null,null,null]")
    );
}

#[test]
fn test_read_line_handles_crlf_and_distinguishes_empty_line_from_eof() {
    // AC-02: \r\n и \n дают одно и то же; "" — настоящая пустая строка.
    let src = "json_stringify([read_line(), read_line(), read_line(), read_line()]);";
    let expected = string_value("[\"a\",\"\",\"b\",null]");
    assert_eq!(run_with_stdin(src, "a\r\n\r\nb\n").unwrap(), expected);
    assert_eq!(run_with_stdin(src, "a\n\nb\n").unwrap(), expected);

    // Пустая строка не равна nil, а EOF не равен "".
    let probe = kw_src("@LET line = read_line(); json_stringify([line == @NIL, line == \"\"]);");
    assert_eq!(
        run_with_stdin(&probe, "\n").unwrap(),
        string_value("[false,true]")
    );
    assert_eq!(
        run_with_stdin(&probe, "").unwrap(),
        string_value("[true,false]")
    );
}

#[test]
fn test_read_line_keeps_lone_carriage_returns_as_data() {
    // AC-02: отбрасывается ОДИН \r и только перед \n; остальные \r — данные.
    assert_eq!(
        run_with_stdin("read_line();", "a\rb\n").unwrap(),
        string_value("a\rb")
    );
    assert_eq!(
        run_with_stdin("read_line();", "a\r").unwrap(),
        string_value("a\r"),
        "\\r в самом конце ввода без \\n — данные, не перевод строки"
    );
    assert_eq!(
        run_with_stdin("read_line();", "a\r\r\n").unwrap(),
        string_value("a\r")
    );
}

#[test]
fn test_read_stdin_returns_the_remainder_after_read_line_verbatim() {
    // AC-03: остаток дословно, включая \r\n; без read_line — весь ввод.
    assert_eq!(
        run_with_stdin("read_line(); read_stdin();", "first\r\nsecond\r\nthird").unwrap(),
        string_value("second\r\nthird")
    );
    assert_eq!(
        run_with_stdin("read_stdin();", "a\r\nb\n\nc\n").unwrap(),
        string_value("a\r\nb\n\nc\n")
    );
    // Пустой ввод — пустая строка (не nil).
    assert_eq!(
        run_with_stdin("read_stdin();", "").unwrap(),
        string_value("")
    );
}

#[test]
fn test_input_is_exhausted_after_read_stdin() {
    // AC-03: после read_stdin() ввод исчерпан — read_line() даёт nil, повторный
    // read_stdin() — "".
    let src = "read_stdin(); json_stringify([read_line(), read_stdin(), read_line()]);";
    assert_eq!(
        run_with_stdin(src, "x\ny\n").unwrap(),
        string_value("[null,\"\",null]")
    );
}

#[test]
fn test_read_line_and_read_stdin_share_one_stream() {
    // Оба builtin'а работают над ОДНИМ потоком: прочитанное одним не
    // видно другому.
    let src = "json_stringify([read_line(), read_line(), read_stdin(), read_line()]);";
    assert_eq!(
        run_with_stdin(src, "1\n2\n3\n4").unwrap(),
        string_value("[\"1\",\"2\",\"3\\n4\",null]")
    );
}

#[test]
fn test_read_line_invalid_utf8_is_runtime_error_not_panic_or_lossy() {
    // AC-04: строки SGA — UTF-8; подстановка U+FFFD исказила бы данные молча.
    match run_with_stdin("read_line();", b"ab\xffcd\n") {
        Err(SgaError::Runtime(msg)) => {
            assert!(msg.contains("read_line()"), "получено: {msg}");
            assert!(msg.contains("UTF-8"), "получено: {msg}");
            assert!(!msg.contains('\u{FFFD}'), "получено: {msg}");
        }
        other => panic!(
            "ожидалась RuntimeError (не-UTF-8 ввод), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_read_stdin_invalid_utf8_is_runtime_error_not_panic_or_lossy() {
    // AC-04 для read_stdin: ошибка даже если невалидно только окончание.
    match run_with_stdin("read_stdin();", b"fine\n\xff\xfe") {
        Err(SgaError::Runtime(msg)) => {
            assert!(msg.contains("read_stdin()"), "получено: {msg}");
            assert!(msg.contains("UTF-8"), "получено: {msg}");
        }
        other => panic!(
            "ожидалась RuntimeError (не-UTF-8 ввод), получено {:?}",
            other
        ),
    }
}

#[test]
fn test_invalid_utf8_only_fails_the_line_that_contains_it() {
    // Декодирование ленивое: валидные строки до испорченной читаются, ошибка
    // возникает именно при чтении испорченной.
    assert_eq!(
        run_with_stdin("read_line();", b"ok\n\xff\n").unwrap(),
        string_value("ok")
    );
    match run_with_stdin("read_line(); read_line();", b"ok\n\xff\n") {
        Err(SgaError::Runtime(_)) => {}
        other => panic!(
            "ожидалась RuntimeError на второй (испорченной) строке, получено {:?}",
            other
        ),
    }
}

#[test]
fn test_read_line_and_read_stdin_with_arguments_are_semantic_errors() {
    // AC-05 (статическая часть): арность (0, 0) проверяется до исполнения.
    for (call, quoted_name) in [
        ("read_line(1);", "'read_line'"),
        ("read_stdin(\"x\");", "'read_stdin'"),
    ] {
        match run(call) {
            Err(SgaError::Semantic(msg)) => {
                assert!(msg.contains(quoted_name), "получено: {msg}");
                assert!(msg.contains("ожидает 0"), "получено: {msg}");
            }
            other => panic!("ожидалась SemError для {call}, получено {:?}", other),
        }
    }
}

#[test]
fn test_vm_stdin_calls_with_arguments_via_handbuilt_bytecode_are_runtime_errors() {
    // AC-05 (рантайм-часть, defense-in-depth): VM можно запустить на вручную
    // собранном байткоде в обход semantic — вызов с аргументом обязан дать
    // RuntimeError, а не panic и не молчаливое игнорирование.
    for name in ["read_line", "read_stdin"] {
        let chunk = Chunk {
            code: vec![
                OpCode::PushConst(0),
                OpCode::Call(name.to_string(), 1),
                OpCode::Return(true),
            ],
            constants: vec![Value::Int(1)],
        };
        let program = CompiledProgram {
            main: chunk,
            functions: HashMap::new(),
        };
        let (mut machine, main) =
            Vm::new(program, None).expect("байткод должен пройти верификатор");
        machine.set_stdin(std::io::Cursor::new(b"unused\n".to_vec()));
        match machine.run(&main) {
            Err(e) => assert!(
                e.to_string().contains(&format!("{name}()")),
                "получено: {e}"
            ),
            Ok(v) => panic!(
                "ожидалась RuntimeError ({name} с аргументом), получено {:?}",
                v
            ),
        }
    }
}

#[test]
fn test_stdin_is_empty_by_default() {
    // AC-06 (а): `run_source`, `run_source_file` и `RunOptions::new()` —
    // пустой ввод (программа не читает stdin процесса, пока её не попросили).
    assert_eq!(run("read_line();").unwrap(), Value::Nil);
    assert_eq!(run("read_stdin();").unwrap(), string_value(""));
    assert_eq!(
        sga::run_source_with_options("read_line();", RunOptions::new()).unwrap(),
        Value::Nil
    );

    let dir = temp_test_dir("stdin_default_file");
    let entry = dir.join("main.sga");
    write_test_file(&entry, "json_stringify([read_line(), read_stdin()]);");
    let result = sga::run_source_file(&entry);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(result.unwrap(), string_value("[null,\"\"]"));
}

#[test]
fn test_run_source_file_with_options_passes_stdin_bytes() {
    // AC-06 (б), файловый вариант запуска.
    let dir = temp_test_dir("stdin_file_opts");
    let entry = dir.join("main.sga");
    write_test_file(&entry, "json_stringify([read_line(), read_stdin()]);");
    let options = RunOptions::new().with_stdin_bytes("one\ntwo\nthree\n");
    let result = sga::run_source_file_with_options(&entry, options);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        result.unwrap(),
        string_value("[\"one\",\"two\\nthree\\n\"]")
    );
}

#[test]
fn test_stdin_bytes_are_read_from_the_start_by_each_run() {
    // AC-06 (в): состояние чтения не разделяется между запусками —
    // `RunOptions` (и его клон) можно переиспользовать.
    let options = RunOptions::new().with_stdin_bytes("one\ntwo\n");
    let first = sga::run_source_with_options("read_line(); read_line();", options.clone());
    let second = sga::run_source_with_options("read_line();", options);
    assert_eq!(first.unwrap(), string_value("two"));
    assert_eq!(
        second.unwrap(),
        string_value("one"),
        "второй запуск обязан читать ввод с начала"
    );
}

#[test]
fn test_stdin_source_builder_last_call_wins_and_accepts_common_byte_types() {
    // Повторный вызов заменяет источник; принимаются &str, String, &[u8],
    // Vec<u8>, &[u8; N].
    let options = RunOptions::new()
        .with_stdin_bytes("first")
        .with_stdin_bytes("second");
    assert_eq!(
        sga::run_source_with_options("read_stdin();", options).unwrap(),
        string_value("second")
    );

    let variants = [
        RunOptions::new().with_stdin_bytes("x"),
        RunOptions::new().with_stdin_bytes(String::from("x")),
        RunOptions::new().with_stdin_bytes(&b"x"[..]),
        RunOptions::new().with_stdin_bytes(vec![b'x']),
        RunOptions::new().with_stdin_bytes(b"x"),
    ];
    for options in variants {
        assert_eq!(
            sga::run_source_with_options("read_stdin();", options).unwrap(),
            string_value("x")
        );
    }
}

#[test]
fn test_stdin_and_args_options_are_independent() {
    // Параметры запуска не затирают друг друга, в каком бы порядке ни
    // задавались.
    let src = "args()[0] + \":\" + read_line();";
    let a = RunOptions::new()
        .with_args(["--flag"])
        .with_stdin_bytes("text\n");
    let b = RunOptions::new()
        .with_stdin_bytes("text\n")
        .with_args(["--flag"]);
    assert_eq!(
        sga::run_source_with_options(src, a).unwrap(),
        string_value("--flag:text")
    );
    assert_eq!(
        sga::run_source_with_options(src, b).unwrap(),
        string_value("--flag:text")
    );
}

#[test]
fn test_vm_set_stdin_is_visible_to_read_line_and_vm_new_signature_is_unchanged() {
    // AC-06 (г): уровень VM без lib.rs — `Vm::set_stdin` между `Vm::new` и
    // `run`; `Vm::new(program, None)` остаётся прежней сигнатуры.
    let chunk = Chunk {
        code: vec![
            OpCode::Call("read_line".to_string(), 0),
            OpCode::Return(true),
        ],
        constants: vec![],
    };
    let program = CompiledProgram {
        main: chunk,
        functions: HashMap::new(),
    };
    let (mut machine, main) = Vm::new(program, None).expect("байткод должен пройти верификатор");
    machine.set_stdin(std::io::Cursor::new(b"from vm\nnext\n".to_vec()));
    assert_eq!(machine.run(&main).unwrap(), string_value("from vm"));
    // Позиция чтения принадлежит VM: повторный `run` продолжает с того же места.
    assert_eq!(machine.run(&main).unwrap(), string_value("next"));
    assert_eq!(machine.run(&main).unwrap(), Value::Nil);
}

#[test]
fn test_vm_without_set_stdin_has_empty_input() {
    let chunk = Chunk {
        code: vec![
            OpCode::Call("read_stdin".to_string(), 0),
            OpCode::Return(true),
        ],
        constants: vec![],
    };
    let program = CompiledProgram {
        main: chunk,
        functions: HashMap::new(),
    };
    let (mut machine, main) = Vm::new(program, None).expect("байткод должен пройти верификатор");
    assert_eq!(machine.run(&main).unwrap(), string_value(""));
}

#[test]
fn test_unicode_input_is_read_without_distortion() {
    // AC-07: кириллица, CJK и эмодзи; len() считает символы, не байты.
    let text = "привет, 日本語 🦀";
    assert_eq!(
        run_with_stdin("read_line();", format!("{text}\n")).unwrap(),
        string_value(text)
    );
    assert_eq!(
        run_with_stdin("len(read_line());", format!("{text}\r\n")).unwrap(),
        Value::Int(text.chars().count() as i64)
    );
    let two = format!("{text}\n{text}");
    assert_eq!(
        run_with_stdin("read_stdin();", two.clone()).unwrap(),
        string_value(&two)
    );
}

#[test]
fn test_unicode_input_survives_a_reader_with_one_byte_buffer() {
    // AC-07: граница внутреннего буфера читателя падает ВНУТРЬ каждого
    // многобайтного символа — строка должна собраться и декодироваться целиком.
    let text = "привет, 日本語 🦀";
    let input = format!("{text}\r\n{text}\nхвост");
    let src = "json_stringify([read_line() == read_line(), read_stdin()]);";
    let reader = std::io::BufReader::with_capacity(1, std::io::Cursor::new(input.into_bytes()));
    assert_eq!(
        run_with_reader(src, reader).unwrap(),
        string_value("[true,\"хвост\"]")
    );
}

#[test]
fn test_very_long_line_is_read_whole() {
    // Строка в миллион символов без перевода строки читается целиком и за
    // линейное время (read_until накапливает байты, а не перечитывает).
    let long = "я".repeat(1_000_000);
    assert_eq!(
        run_with_stdin("len(read_line());", long.clone()).unwrap(),
        Value::Int(1_000_000)
    );
    assert_eq!(
        run_with_stdin("len(read_stdin());", long).unwrap(),
        Value::Int(1_000_000)
    );
}

#[test]
fn test_discarded_stdin_calls_still_consume_input() {
    // AC-09: оптимизатор не удаляет вызов с побочным эффектом, даже если его
    // результат отброшен (`read_line();`) или присвоен неиспользуемой
    // переменной.
    let skip_header = kw_src("read_line(); @LET unused = read_line(); read_line();");
    assert_eq!(
        run_with_stdin(&skip_header, "h1\nh2\ndata\n").unwrap(),
        string_value("data")
    );
    assert_eq!(
        run_with_stdin("read_stdin(); read_line();", "a\nb\n").unwrap(),
        Value::Nil,
        "отброшенный read_stdin() обязан исчерпать ввод"
    );
}

#[test]
fn test_stdin_builtins_work_inside_functions_and_closures() {
    // AC-10: читатель принадлежит VM, а не кадру — доступен в любом.
    let in_function = kw_src("@FN next() { @RETURN read_line(); } next() + \"/\" + next();");
    assert_eq!(
        run_with_stdin(&in_function, "a\nb\n").unwrap(),
        string_value("a/b")
    );
    let in_closure = kw_src(
        "@LET f = @FN() { @RETURN read_line(); }; \
         @LET g = @FN() { @RETURN read_stdin(); }; \
         f() + \"|\" + g();",
    );
    assert_eq!(
        run_with_stdin(&in_closure, "x\ny\nz").unwrap(),
        string_value("x|y\nz")
    );
}

#[test]
fn test_reading_until_nil_sums_numbers_from_input() {
    // AC-10: типичная программа — «читать до конца ввода».
    assert_eq!(
        run_with_stdin(&sum_lines_src(), "1\r\n2\n3\n40\n").unwrap(),
        Value::Int(46)
    );
    assert_eq!(
        run_with_stdin(&sum_lines_src(), "").unwrap(),
        Value::Int(0),
        "пустой ввод — цикл не выполняется ни разу"
    );
}

#[test]
fn test_reading_many_lines_is_correct() {
    // 20 000 строк: порядок и полнота (сумма 1..=n), без потерь на границах
    // внутренних буферов.
    let n: i64 = 20_000;
    let input: String = (1..=n).map(|i| format!("{i}\n")).collect();
    assert_eq!(
        run_with_stdin(&sum_lines_src(), input).unwrap(),
        Value::Int(n * (n + 1) / 2)
    );
}

#[test]
fn test_variable_and_parameter_named_like_stdin_builtins_are_unaffected() {
    // AC-11: имена заняты ТОЛЬКО как имена вызова `read_line(..)` /
    // `read_stdin(..)`; переменная и параметр с таким именем работают как раньше.
    let src = kw_src(
        "@FN count(read_line) { @RETURN len(read_line); } \
         @LET read_stdin = [1, 2, 3]; \
         count(read_stdin) + len(read_stdin);",
    );
    assert_eq!(run(&src).unwrap(), Value::Int(6));
}

// ----- Сквозные тесты через НАСТОЯЩИЙ бинарник и pipe (AC-08) -----

/// Ждёт завершения процесса не дольше `limit`. По истечении убивает его и
/// возвращает `None` — тест должен упасть с понятным сообщением, а не
/// зависнуть навсегда, если реализация вдруг начнёт ждать EOF.
fn wait_exit_within(
    child: &mut std::process::Child,
    limit: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        if started.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Запускает `sga run <entry> [args…]`, подаёт `input` в настоящий pipe на
/// stdin и закрывает его. Запись идёт в отдельном потоке: иначе программа,
/// печатающая много вывода, и тест, пишущий много ввода, могли бы заблокировать
/// друг друга (оба pipe ограничены буфером ОС).
fn cli_run_with_stdin(
    entry: &std::path::Path,
    args: &[&str],
    input: Vec<u8>,
) -> std::process::Output {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(entry)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        // Ошибка записи (Broken pipe) допустима: программа могла завершиться,
        // не дочитав ввод до конца.
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    out
}

#[test]
fn test_cli_run_reads_lines_from_piped_stdin() {
    // AC-08 (а): построчное чтение из настоящего pipe; пустая строка и \r\n
    // обрабатываются так же, как в библиотеке; программа видит конец ввода.
    let dir = temp_test_dir("stdin_cli_lines");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &kw_src(
            "@VAR n = 0; \
             @WHILE @TRUE { \
                 @LET line = read_line(); \
                 @IF line == @NIL { @BREAK; } \
                 n = n + 1; \
                 @PRINT(to_string(n) + \": \" + line); \
             } \
             @PRINT(\"всего\", n);",
        ),
    );
    let out = cli_run_with_stdin(
        &entry,
        &[],
        "первая\nвторая\r\n\nчетвёртая".as_bytes().to_vec(),
    );
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        out.status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "1: первая\n2: вторая\n3: \n4: четвёртая\nвсего 4\n"
    );
}

#[test]
fn test_cli_run_read_stdin_gets_whole_input_verbatim() {
    // read_stdin() через pipe: длина в символах (CRLF не нормализуется).
    let dir = temp_test_dir("stdin_cli_all");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &kw_src("@LET first = read_line(); @PRINT(first); @PRINT(len(read_stdin()));"),
    );
    // "привет" + CRLF уходит в read_line(); остаток "мир\r\nещё" = 3+2+3 = 8 символов.
    let out = cli_run_with_stdin(&entry, &[], "привет\r\nмир\r\nещё".as_bytes().to_vec());
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        out.status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "привет\n8\n");
}

#[test]
fn test_cli_run_reads_large_piped_input_without_deadlock() {
    // 50 000 строк (≈340 КБ) заведомо больше буфера pipe (64 КиБ): ввод
    // идёт кусками, программа читает инкрементально, сумма сходится.
    let n: i64 = 50_000;
    let input: String = (1..=n).map(|i| format!("{i}\n")).collect();
    let dir = temp_test_dir("stdin_cli_large");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &kw_src(
            "@VAR total = 0; \
             @WHILE @TRUE { \
                 @LET line = read_line(); \
                 @IF line == @NIL { @BREAK; } \
                 total = total + to_int(line); \
             } \
             @PRINT(total);",
        ),
    );
    let out = cli_run_with_stdin(&entry, &[], input.into_bytes());
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        out.status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        (n * (n + 1) / 2).to_string()
    );
}

#[test]
fn test_cli_run_read_line_does_not_wait_for_eof_on_open_pipe() {
    // AC-08 (б): read_line() потребляет ОДНУ строку и возвращается. В pipe
    // записана одна строка, но он НЕ закрыт: реализация, ждущая EOF (или
    // зачитывающая весь stdin заранее), здесь зависла бы — тест убьёт её по
    // тайм-ауту и упадёт с понятным сообщением.
    use std::io::Write;
    use std::process::{Command, Stdio};
    let dir = temp_test_dir("stdin_cli_lazy");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &kw_src("@PRINT(\"got:\", read_line());"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(&entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all("первая\n".as_bytes()).unwrap();
    stdin.flush().unwrap();
    let status = wait_exit_within(&mut child, std::time::Duration::from_secs(20));
    drop(stdin); // pipe закрывается только ПОСЛЕ проверки
    std::fs::remove_dir_all(&dir).ok();
    let status = status.expect(
        "sga не завершился за 20 с при открытом pipe: read_line() ждёт EOF вместо одной строки",
    );
    let out = child.wait_with_output().unwrap();
    assert!(
        status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "got: первая");
}

#[test]
fn test_cli_run_program_that_does_not_read_stdin_does_not_block_on_open_pipe() {
    // AC-08 (в): stdin процесса не трогается, пока программа сама не вызовет
    // read_line()/read_stdin(). Pipe открыт и пуст — программа обязана
    // завершиться, а не ждать ввода.
    use std::process::{Command, Stdio};
    let dir = temp_test_dir("stdin_cli_untouched");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &kw_src("@PRINT(\"ok\");"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(&entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap(); // держим открытым, ничего не пишем
    let status = wait_exit_within(&mut child, std::time::Duration::from_secs(20));
    drop(stdin);
    std::fs::remove_dir_all(&dir).ok();
    let status = status.expect(
        "sga не завершился за 20 с: программа, не читающая stdin, ждёт ввода (stdin читается заранее?)",
    );
    let out = child.wait_with_output().unwrap();
    assert!(
        status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
}

#[test]
fn test_cli_run_invalid_utf8_on_stdin_is_clear_error_not_panic() {
    // AC-08 (г): первая строка валидна и печатается; вторая — нет: сообщение
    // про UTF-8, обычный код неуспеха 1 (не 101 от panic).
    let dir = temp_test_dir("stdin_cli_bad_utf8");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &kw_src("@PRINT(read_line()); @PRINT(read_line()); @PRINT(\"недостижимо\");"),
    );
    let out = cli_run_with_stdin(&entry, &[], b"ok\n\xff\xfe\n".to_vec());
    std::fs::remove_dir_all(&dir).ok();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr = {stderr}");
    assert!(stderr.contains("read_line()"), "получено: {stderr}");
    assert!(stderr.contains("UTF-8"), "получено: {stderr}");
    assert!(!stderr.contains("panicked"), "получено: {stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
}

#[test]
fn test_cli_run_with_null_or_empty_stdin_is_eof() {
    // AC-08 (д): нулевое устройство и сразу закрытый pipe — это EOF, а не
    // ошибка и не зависание.
    use std::process::{Command, Stdio};
    let dir = temp_test_dir("stdin_cli_eof");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &kw_src("@PRINT(read_line()); @PRINT(len(read_stdin()));"),
    );

    let null_stdin = Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(&entry)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let empty_pipe = cli_run_with_stdin(&entry, &[], Vec::new());
    std::fs::remove_dir_all(&dir).ok();

    for (label, out) in [("null", null_stdin), ("empty pipe", empty_pipe)] {
        assert!(
            out.status.success(),
            "{label}: stderr = {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "nil\n0\n",
            "{label}: EOF"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_cli_run_with_closed_stdin_descriptor_is_eof() {
    // AC-08 (д), unix: `<&-` закрывает дескриптор 0 совсем (не /dev/null).
    // Стандартная библиотека Rust трактует EBADF на stdin как EOF — программа
    // не должна падать.
    let dir = temp_test_dir("stdin_cli_closed_fd");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &kw_src("@PRINT(read_line());"));
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("exec \"$0\" run \"$1\" <&-")
        .arg(env!("CARGO_BIN_EXE_sga"))
        .arg(&entry)
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        out.status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "nil\n");
}

#[cfg(unix)]
#[test]
fn test_cli_run_stdin_io_error_is_runtime_error_not_panic() {
    // Настоящая ошибка ввода-вывода читателя: stdin — каталог, read(2) даёт
    // EISDIR. Ожидаем RuntimeError (код 1, сообщение про stdin), а не panic.
    use std::process::{Command, Stdio};
    let dir = temp_test_dir("stdin_cli_io_error");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &kw_src("@PRINT(read_line());"));
    let stdin_dir = std::fs::File::open(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(&entry)
        .stdin(Stdio::from(stdin_dir))
        .output()
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr = {stderr}");
    assert!(stderr.contains("read_line()"), "получено: {stderr}");
    assert!(stderr.contains("stdin"), "получено: {stderr}");
    assert!(!stderr.contains("panicked"), "получено: {stderr}");
}

#[test]
fn test_cli_run_passes_both_arguments_and_stdin() {
    // args() (T012) и stdin (T013) работают вместе; аргументы после файла
    // по-прежнему дословные, ввод идёт из pipe.
    let dir = temp_test_dir("stdin_cli_args");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &kw_src("@PRINT(args()[0], read_line());"));
    let out = cli_run_with_stdin(&entry, &["--flag"], b"text\n".to_vec());
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        out.status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "--flag text\n");
}

// ===== std/time: time_now_ms() / time_monotonic_ms() (T014, M006) =====
//
// `time_now_ms()` — миллисекунды с 1970-01-01T00:00:00Z по системным часам;
// `time_monotonic_ms()` — миллисекунды от создания VM по монотонным часам.
// Время недетерминировано, поэтому тесты проверяют СВОЙСТВА, а не точные
// значения:
// - `time_now_ms()` берётся в СКОБКУ из двух показаний системных часов, снятых
//   самим тестом до и после запуска (допущение, присущее любому тесту
//   календарного времени: часы не переводят прямо во время теста);
// - `time_monotonic_ms()` сравнивается с `Instant` теста: значение не меньше 0 и
//   не больше времени, реально прошедшего с запуска (строгая верхняя граница, без
//   допуска); нижнюю границу задаёт `thread::sleep` (он не спит меньше заданного);
// - программы с активным ожиданием по часам запускаются так, чтобы остановившиеся
//   часы давали упавший тест, а не повисший набор (поток с таймаутом /
//   `wait_exit_within`).
// Чистая арифметика (эпоха Unix, округление вниз, предел `int`) покрыта
// юнит-тестами в `src/clock.rs`.

/// Миллисекунды с эпохи Unix по «сырым» показаниям системных часов —
/// независимо от реализации `time_now_ms` (для скобки до/после запуска).
fn system_millis() -> i64 {
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("системные часы должны показывать время после 1970-01-01");
    i64::try_from(since_epoch.as_millis()).expect("время помещается в i64")
}

/// Целые миллисекунды, прошедшие с `since` по монотонным часам теста.
fn millis_since(since: std::time::Instant) -> i64 {
    i64::try_from(since.elapsed().as_millis()).expect("время помещается в i64")
}

fn expect_int(value: Value) -> i64 {
    match value {
        Value::Int(n) => n,
        other => panic!("ожидался int, получено {:?}", other),
    }
}

/// Запускает программу и проверяет, что её значение — `int` из скобки двух
/// показаний системных часов, снятых до и после запуска.
fn assert_program_yields_unix_millis_between_readings(source: &str) {
    let before = system_millis();
    let value = expect_int(run(source).unwrap());
    let after = system_millis();
    assert!(
        (before..=after).contains(&value),
        "{before} <= {value} <= {after} не выполнено для программы {source:?}"
    );
}

/// Текст -> токены -> AST -> байткод (без semantic): путь, на котором `Vm`
/// можно держать в руках между созданием и запуском.
fn compile_source(source: &str) -> CompiledProgram {
    let tokens = lexer::Lexer::new(source)
        .tokenize()
        .unwrap_or_else(|e| panic!("лексер: {e}"));
    let program = parser::Parser::new(tokens)
        .parse_program()
        .unwrap_or_else(|e| panic!("парсер: {e}"));
    codegen::compile(&program)
}

/// Запускает программу в отдельном потоке и ждёт её значение-`int` не дольше
/// `limit`. Нужен программам с активным ожиданием по часам: если часы встанут,
/// цикл не завершится никогда, и тест должен упасть с понятным сообщением, а не
/// повесить весь набор. `Value` содержит `Rc` и через границу потока не
/// пересылается — наружу уходит извлечённый `i64`.
fn run_int_with_timeout(source: String, limit: std::time::Duration) -> i64 {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = match sga::run_source(&source) {
            Ok(Value::Int(n)) => Ok(n),
            other => Err(format!("{:?}", other)),
        };
        let _ = sender.send(outcome);
    });
    match receiver.recv_timeout(limit) {
        Ok(Ok(n)) => n,
        Ok(Err(found)) => panic!("программа не вернула int: {found}"),
        Err(_) => {
            panic!("программа не завершилась за {limit:?}: часы стоят (или идут слишком медленно)?")
        }
    }
}

#[test]
fn test_time_now_ms_is_an_int_between_system_clock_readings() {
    // AC-01.
    assert_program_yields_unix_millis_between_readings("time_now_ms();");
}

#[test]
fn test_time_now_ms_in_function_closure_and_expression_is_between_system_clock_readings() {
    // AC-01: вызов внутри функции, замыкания и выражения.
    assert_program_yields_unix_millis_between_readings(&kw_src(
        "@FN stamp() { @RETURN time_now_ms(); } stamp();",
    ));
    assert_program_yields_unix_millis_between_readings(&kw_src(
        "@LET f = @FN() { @RETURN time_now_ms(); }; f();",
    ));
    assert_program_yields_unix_millis_between_readings("time_now_ms() + 0;");
}

#[test]
fn test_time_builtins_return_int() {
    // AC-01/AC-02: тип результата — int (целые миллисекунды), не float.
    for call in ["time_now_ms", "time_monotonic_ms"] {
        assert_eq!(
            run(&format!("type_of({call}());")).unwrap(),
            Value::Str("int".to_string()),
            "{call}()"
        );
    }
}

#[test]
fn test_time_monotonic_ms_starts_near_zero_and_never_exceeds_elapsed_time() {
    // AC-02: 0 <= v <= время, реально прошедшее с запуска. Верхняя граница
    // строгая, без допуска: `Instant` теста снят ДО создания VM, а показание —
    // внутри запуска, раньше, чем `elapsed()` после него. Сразу после старта
    // значение мало (запас на нагруженную машину — минута).
    let started = std::time::Instant::now();
    let value = expect_int(run("time_monotonic_ms();").unwrap());
    let elapsed = millis_since(started);
    assert!(
        (0..=elapsed).contains(&value),
        "0 <= {value} <= {elapsed} не выполнено"
    );
    assert!(
        value < 60_000,
        "первый вызов сразу после старта: {value} мс"
    );
}

#[test]
fn test_time_monotonic_ms_in_function_and_closure_stays_within_elapsed_time() {
    // AC-02: тот же контракт при вызове внутри функции и замыкания.
    for src in [
        kw_src("@FN elapsed() { @RETURN time_monotonic_ms(); } elapsed();"),
        kw_src("@LET g = @FN() { @RETURN time_monotonic_ms(); }; g();"),
    ] {
        let started = std::time::Instant::now();
        let value = expect_int(run(&src).unwrap());
        let elapsed = millis_since(started);
        assert!(
            (0..=elapsed).contains(&value),
            "0 <= {value} <= {elapsed} не выполнено"
        );
    }
}

#[test]
fn test_time_monotonic_ms_is_non_decreasing_over_10000_calls() {
    // AC-03: монотонность — считаем нарушения порядка между соседними
    // показаниями; ожидаем ровно 0.
    let src = kw_src(
        "@VAR previous = time_monotonic_ms(); \
         @VAR violations = 0; \
         @VAR i = 0; \
         @WHILE i < 10000 { \
             @LET current = time_monotonic_ms(); \
             @IF current < previous { violations = violations + 1; } \
             previous = current; \
             i = i + 1; \
         } \
         violations;",
    );
    assert_eq!(run(&src).unwrap(), Value::Int(0));
}

#[test]
fn test_time_monotonic_ms_busy_wait_advances_with_real_time() {
    // AC-03: ждём по часам, пока они не уйдут на 25 мс вперёд. Цикл завершается
    // только если часы реально идут (константа или остановленные часы -> таймаут
    // потока), а разность не может превысить время, реально прошедшее с запуска
    // (часы не идут быстрее реальных; верхняя граница — `floor(t1) <= elapsed`,
    // т.к. t0 >= 0). Заодно подтверждает, что оптимизатор не свернул вызовы в
    // константу и не выкинул их из условия цикла.
    let src = kw_src(
        "@LET t0 = time_monotonic_ms(); \
         @VAR t1 = t0; \
         @WHILE t1 - t0 < 25 { t1 = time_monotonic_ms(); } \
         t1 - t0;",
    );
    let started = std::time::Instant::now();
    let diff = run_int_with_timeout(src, std::time::Duration::from_secs(30));
    let elapsed = millis_since(started);
    assert!(
        (25..=elapsed).contains(&diff),
        "25 <= {diff} <= {elapsed} не выполнено"
    );
}

#[test]
fn test_time_builtins_with_arguments_are_semantic_errors() {
    // AC-04 (статическая часть): арность (0, 0) проверяется до исполнения.
    for (call, quoted_name) in [
        ("time_now_ms(1);", "'time_now_ms'"),
        ("time_monotonic_ms(\"x\");", "'time_monotonic_ms'"),
    ] {
        match run(call) {
            Err(SgaError::Semantic(msg)) => {
                assert!(msg.contains(quoted_name), "получено: {msg}");
                assert!(msg.contains("ожидает 0"), "получено: {msg}");
            }
            other => panic!("ожидалась SemError для {call}, получено {:?}", other),
        }
    }
}

#[test]
fn test_vm_time_calls_with_arguments_via_handbuilt_bytecode_are_runtime_errors() {
    // AC-04 (рантайм-часть, defense-in-depth): VM можно запустить на вручную
    // собранном байткоде в обход semantic — вызов с аргументом обязан дать
    // RuntimeError, а не panic и не молчаливое игнорирование.
    for name in ["time_now_ms", "time_monotonic_ms"] {
        let chunk = Chunk {
            code: vec![
                OpCode::PushConst(0),
                OpCode::Call(name.to_string(), 1),
                OpCode::Return(true),
            ],
            constants: vec![Value::Int(1)],
        };
        let program = CompiledProgram {
            main: chunk,
            functions: HashMap::new(),
        };
        let (mut machine, main) =
            Vm::new(program, None).expect("байткод должен пройти верификатор");
        match machine.run(&main) {
            Err(e) => assert!(
                e.to_string().contains(&format!("{name}()")),
                "получено: {e}"
            ),
            Ok(v) => panic!(
                "ожидалась RuntimeError ({name} с аргументом), получено {:?}",
                v
            ),
        }
    }
}

#[test]
fn test_time_monotonic_ms_counts_from_vm_creation() {
    // AC-06 (а): отсчёт — от `Vm::new`, а не от первого вызова: пауза между
    // созданием VM и запуском обязана быть видна уже в первом показании.
    // `thread::sleep` не спит меньше заданного, значит, значение >= 30.
    let created = std::time::Instant::now();
    let (mut machine, main) =
        Vm::new(compile_source("time_monotonic_ms();"), None).expect("байткод валиден");
    std::thread::sleep(std::time::Duration::from_millis(30));
    let value = expect_int(machine.run(&main).unwrap());
    let elapsed = millis_since(created);
    assert!(
        (30..=elapsed).contains(&value),
        "30 <= {value} <= {elapsed} не выполнено"
    );
}

#[test]
fn test_time_monotonic_ms_is_not_reset_by_a_second_run_on_the_same_vm() {
    // AC-06 (б): повторный `run` на той же VM продолжает отсчёт, а не
    // начинает его заново (иначе второе показание было бы около 0).
    let (mut machine, main) =
        Vm::new(compile_source("time_monotonic_ms();"), None).expect("байткод валиден");
    let first = expect_int(machine.run(&main).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = expect_int(machine.run(&main).unwrap());
    assert!(
        first + 20 <= second,
        "{first} + 20 <= {second}: второй запуск не должен обнулять отсчёт"
    );
}

#[test]
fn test_discarded_time_calls_are_harmless() {
    // Результат, который никуда не записан, не ломает программу и не оставляет
    // мусор на стеке: значение последнего выражения — по-прежнему 7.
    assert_eq!(
        run("time_now_ms(); time_monotonic_ms(); 7;").unwrap(),
        Value::Int(7)
    );
}

#[test]
fn test_time_builtins_work_with_int_annotations_and_arithmetic() {
    // Результат — настоящий int: проходит аннотацию `: int` и участвует в
    // арифметике и сравнениях.
    // (`@AND` — ключевое слово AND; `kw_src` его не знает, подставляем сами.)
    let src = kw_src(
        "@LET t0: int = time_monotonic_ms(); \
         @LET stamp: int = time_now_ms(); \
         @LET t1: int = time_monotonic_ms(); \
         @LET delta: int = t1 - t0; \
         delta >= 0 @AND stamp > 0;",
    )
    .replace("@AND", &kw("AND"));
    assert_eq!(run(&src).unwrap(), Value::Bool(true));
}

#[test]
fn test_variable_named_like_a_time_builtin_is_unaffected() {
    // AC-08: имена заняты ТОЛЬКО как имена вызова `time_now_ms(..)` /
    // `time_monotonic_ms(..)`; переменная с таким именем работает как раньше.
    let src = kw_src("@LET time_now_ms = 5; @VAR time_monotonic_ms = 1; time_monotonic_ms = time_monotonic_ms + time_now_ms; time_monotonic_ms;");
    assert_eq!(run(&src).unwrap(), Value::Int(6));
}

#[test]
fn test_parameter_named_like_a_time_builtin_is_unaffected() {
    // AC-08: то же для параметра функции.
    let src = kw_src(
        "@FN keep(time_now_ms, time_monotonic_ms) { @RETURN time_now_ms + time_monotonic_ms; } \
         keep(40, 2);",
    );
    assert_eq!(run(&src).unwrap(), Value::Int(42));
}

// ----- Сквозные тесты через НАСТОЯЩИЙ бинарник (AC-07) -----

/// Запускает `sga run <entry>` с пустым stdin и возвращает весь вывод.
fn cli_run_plain(entry: &std::path::Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(entry)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

/// Разбирает вывод программы, напечатавшей ровно одно целое число.
fn printed_int(out: &std::process::Output) -> i64 {
    assert!(
        out.status.success(),
        "stderr = {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim_end_matches('\n')
        .parse::<i64>()
        .unwrap_or_else(|e| panic!("stdout — не одно целое число ({e}): {text:?}"))
}

#[test]
fn test_cli_run_prints_time_now_ms_between_system_clock_readings() {
    // AC-07 (а): скобка по системным часам, снятая ВОКРУГ процесса — до запуска
    // и после завершения.
    let dir = temp_test_dir("time_cli_now");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &kw_src("@PRINT(time_now_ms());"));
    let before = system_millis();
    let out = cli_run_plain(&entry);
    let after = system_millis();
    std::fs::remove_dir_all(&dir).ok();
    let value = printed_int(&out);
    assert!(
        (before..=after).contains(&value),
        "{before} <= {value} <= {after} не выполнено"
    );
}

#[test]
fn test_cli_run_time_monotonic_ms_does_not_exceed_the_process_lifetime() {
    // AC-07 (б): 0 <= v <= время жизни процесса (от spawn до выхода, по
    // `Instant` теста).
    let dir = temp_test_dir("time_cli_monotonic");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &kw_src("@PRINT(time_monotonic_ms());"));
    let started = std::time::Instant::now();
    let out = cli_run_plain(&entry);
    let elapsed = millis_since(started);
    std::fs::remove_dir_all(&dir).ok();
    let value = printed_int(&out);
    assert!(
        (0..=elapsed).contains(&value),
        "0 <= {value} <= {elapsed} не выполнено"
    );
}

#[test]
fn test_cli_run_time_monotonic_ms_measures_an_interval_around_a_busy_wait() {
    // AC-07 (в): разность двух замеров вокруг активного ожидания печатается,
    // не меньше 25 и не больше времени жизни процесса. Процесс ждётся через
    // `wait_exit_within`: остановившиеся часы дадут упавший тест, а не зависший.
    use std::io::Read;
    use std::process::{Command, Stdio};
    let dir = temp_test_dir("time_cli_interval");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &kw_src(
            "@LET t0 = time_monotonic_ms(); \
             @VAR t1 = t0; \
             @WHILE t1 - t0 < 25 { t1 = time_monotonic_ms(); } \
             @PRINT(t1 - t0);",
        ),
    );
    let started = std::time::Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(&entry)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_exit_within(&mut child, std::time::Duration::from_secs(30));
    let elapsed = millis_since(started);
    std::fs::remove_dir_all(&dir).ok();
    let status = status.expect("процесс не завершился за 30 с: часы стоят?");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "stderr = {stderr}");
    let diff: i64 = stdout
        .trim_end_matches('\n')
        .parse()
        .unwrap_or_else(|e| panic!("stdout — не одно целое число ({e}): {stdout:?}"));
    assert!(
        (25..=elapsed).contains(&diff),
        "25 <= {diff} <= {elapsed} не выполнено"
    );
}

// ===== T015 (M006): write_file(path, content) — запись файлов с явным разрешением =====
//
// Feature Contract — PROJECT_STATUS.md (T015). `write_file` — единственный
// builtin, меняющий файловую систему, поэтому по умолчанию запись ЗАПРЕЩЕНА:
// её разрешает только `sga run --allow-write` (в библиотеке —
// `RunOptions::with_allow_write`). Путь — относительный, строго внутри
// каталога входного файла (тот же корень, что у IMPORT и read_file).
//
// Сама проверка пути живёт в src/fs_sandbox.rs (там свои юнит-тесты); здесь —
// сквозные тесты: библиотека (`run_source_file_with_options`), вручную
// собранный байткод и НАСТОЯЩИЙ бинарник `sga`. Всё — на реальных каталогах
// диска; каждый тест получает свой временный каталог, который удаляется при
// выходе из теста (`WfDir`), в том числе при падении assert'а.
//
// Блок включает и регрессионные тесты ошибки, найденной при аудите T015:
// при запуске `sga run main.sga` (имя файла без каталога) корень песочницы был
// ПУСТЫМ, и `read_file("../x")`, `IMPORT "../x.sga"` выходили за каталог
// программы. Эти тесты запускают бинарник с `current_dir`, потому что
// относительный путь входного файла нельзя проверить из процесса теста, не
// меняя его общий текущий каталог.

/// Временный каталог теста (через `temp_test_dir`), удаляемый при выходе —
/// в том числе при панике assert'а.
struct WfDir(std::path::PathBuf);

impl WfDir {
    fn new(name: &str) -> Self {
        WfDir(temp_test_dir(name))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn join(&self, relative: &str) -> std::path::PathBuf {
        self.0.join(relative)
    }
}

impl Drop for WfDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Раскладка «проект внутри внешнего каталога»: `outer/proj/` — корень
/// песочницы (там лежит `main.sga`), `outer/` — то, что снаружи корня.
fn wf_outer_and_proj(name: &str) -> (WfDir, std::path::PathBuf) {
    let outer = WfDir::new(name);
    let proj = outer.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    (outer, proj)
}

/// Строковый литерал SGA с текстом `text` (экранируются `\` и `"`).
fn sga_lit(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Исходник `write_file(path, content);` с литералами.
fn wf_write_src(path: &str, content: &str) -> String {
    format!("write_file({}, {});", sga_lit(path), sga_lit(content))
}

/// Исходник `PRINT(read_file(path));`.
fn wf_print_read_src(path: &str) -> String {
    format!("{}(read_file({}));", kw("PRINT"), sga_lit(path))
}

/// Пишет `source` в `main.sga` каталога `dir` и запускает его как входной файл
/// с разрешением на запись `allow_write`.
fn wf_run(dir: &std::path::Path, source: &str, allow_write: bool) -> Result<Value, SgaError> {
    let entry = dir.join("main.sga");
    write_test_file(&entry, source);
    sga::run_source_file_with_options(&entry, RunOptions::new().with_allow_write(allow_write))
}

/// Ожидает RuntimeError и возвращает его текст.
fn wf_expect_runtime_error(result: Result<Value, SgaError>) -> String {
    match result {
        Err(SgaError::Runtime(msg)) => msg,
        other => panic!("ожидалась RuntimeError, получено {:?}", other),
    }
}

/// Имена записей каталога `dir`, отсортированные.
fn wf_listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn wf_read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Запускает настоящий бинарник: `sga run <run_args…>` с пустым stdin;
/// `cwd` — текущий каталог процесса (если задан).
fn wf_cli(cwd: Option<&std::path::Path>, run_args: &[&str]) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_sga"));
    command
        .arg("run")
        .args(run_args)
        .stdin(std::process::Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.output().unwrap()
}

fn wf_stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wf_stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn wf_path_str(path: &std::path::Path) -> &str {
    path.to_str().expect("путь теста должен быть UTF-8")
}

// ----- AC-01: без разрешения запись невозможна -----

#[test]
fn test_write_file_is_forbidden_by_default_and_names_both_ways_to_allow_it() {
    // AC-01: RuntimeError, объясняющая, как выдать разрешение (CLI и
    // библиотека); файл не создаётся.
    let dir = WfDir::new("wf_default_forbidden");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &wf_write_src("out.txt", "x"));
    let msg = wf_expect_runtime_error(sga::run_source_file(&entry));
    assert!(msg.contains("--allow-write"), "получено: {msg}");
    assert!(msg.contains("with_allow_write"), "получено: {msg}");
    assert!(msg.contains("запрещена"), "получено: {msg}");
    assert_eq!(wf_listing(dir.path()), vec!["main.sga"]);
}

#[test]
fn test_write_file_explicit_false_forbids_and_the_last_builder_call_wins() {
    // AC-01: `with_allow_write(false)` — тоже запрет; при нескольких вызовах
    // побеждает последний.
    let dir = WfDir::new("wf_false_wins");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &wf_write_src("out.txt", "x"));
    for options in [
        RunOptions::new().with_allow_write(false),
        RunOptions::new()
            .with_allow_write(true)
            .with_allow_write(false),
    ] {
        let msg = wf_expect_runtime_error(sga::run_source_file_with_options(&entry, options));
        assert!(msg.contains("запрещена"), "получено: {msg}");
    }
    assert_eq!(wf_listing(dir.path()), vec!["main.sga"]);
    sga::run_source_file_with_options(
        &entry,
        RunOptions::new()
            .with_allow_write(false)
            .with_allow_write(true),
    )
    .unwrap();
    assert_eq!(wf_read(&dir.join("out.txt")), "x");
}

#[test]
fn test_write_file_without_permission_leaves_an_existing_file_untouched() {
    // AC-01: запрещена и перезапись, не только создание.
    let dir = WfDir::new("wf_no_overwrite");
    std::fs::write(dir.join("out.txt"), "old content").unwrap();
    let entry = dir.join("main.sga");
    write_test_file(&entry, &wf_write_src("out.txt", "new"));
    assert!(sga::run_source_file(&entry).is_err());
    assert_eq!(wf_read(&dir.join("out.txt")), "old content");
}

#[test]
fn test_write_file_permission_check_comes_before_any_path_check() {
    // AC-01: без разрешения ответ не зависит от пути — по тексту ошибки
    // программа не может выяснить, что лежит на диске (есть ли каталог, выходит
    // ли путь за корень, что за цель).
    let (outer, proj) = wf_outer_and_proj("wf_permission_first");
    std::fs::create_dir(outer.join("existing_dir")).unwrap();
    let absolute = outer.join("abs.txt");
    let paths = [
        "out.txt",
        "../outside.txt",
        "../existing_dir/x.txt",
        "no_such_dir/x.txt",
        wf_path_str(&absolute),
        "",
        "sub/",
    ];
    for path in paths {
        let msg = wf_expect_runtime_error(wf_run(&proj, &wf_write_src(path, "x"), false));
        assert!(
            msg.contains("запрещена") && msg.contains("--allow-write"),
            "{path:?}: {msg}"
        );
        for leak in [
            "выходит за пределы",
            "не существует",
            "абсолютные пути",
            "не является",
            "путь пуст",
            "каталог",
        ] {
            assert!(
                !msg.contains(leak),
                "{path:?}: сообщение без разрешения раскрывает проверку пути ({leak}): {msg}"
            );
        }
    }
    assert_eq!(wf_listing(&proj), vec!["main.sga"]);
    assert_eq!(wf_listing(outer.path()), vec!["existing_dir", "proj"]);
}

// ----- AC-02: с разрешением запись создаёт и перезаписывает файл целиком -----

#[test]
fn test_write_file_creates_a_file_with_the_exact_content_and_returns_nil() {
    // AC-02
    let dir = WfDir::new("wf_create");
    let src = kw_src("@LET result = write_file(\"out.txt\", \"hello\"); result;");
    assert_eq!(wf_run(dir.path(), &src, true).unwrap(), Value::Nil);
    assert_eq!(std::fs::read(dir.join("out.txt")).unwrap(), b"hello");
}

#[test]
fn test_write_file_overwrites_the_whole_file_and_leaves_no_tail() {
    // AC-02: более короткое содержимое не оставляет «хвоста» старого.
    let dir = WfDir::new("wf_overwrite");
    std::fs::write(
        dir.join("out.txt"),
        "a very long previous content that must vanish completely",
    )
    .unwrap();
    wf_run(dir.path(), &wf_write_src("out.txt", "short"), true).unwrap();
    assert_eq!(std::fs::read(dir.join("out.txt")).unwrap(), b"short");
}

#[test]
fn test_write_file_writes_bytes_verbatim_and_adds_no_newline() {
    // AC-02: байты строки UTF-8 — дословно; escape-последовательности SGA
    // (`\n`, `\t`, `\"`, `\\`) превращаются в обычные символы, ничего не
    // дописывается.
    let dir = WfDir::new("wf_verbatim");
    let cases: [(&str, &[u8]); 5] = [
        ("no newline added", b"no newline added"),
        ("Привет, мир! ✓ 😀", "Привет, мир! ✓ 😀".as_bytes()),
        ("line1\\nline2\\n", b"line1\nline2\n"),
        ("tab\\there", b"tab\there"),
        ("quote\\\"inside\\\\", b"quote\"inside\\"),
    ];
    for (index, (literal, expected)) in cases.iter().enumerate() {
        let file = format!("case{index}.txt");
        let src = format!("write_file(\"{file}\", \"{literal}\");");
        wf_run(dir.path(), &src, true).unwrap();
        assert_eq!(
            std::fs::read(dir.join(&file)).unwrap(),
            *expected,
            "{literal}"
        );
    }
}

#[test]
fn test_write_file_empty_content_creates_or_truncates_to_an_empty_file() {
    // AC-02
    let dir = WfDir::new("wf_empty");
    std::fs::write(dir.join("old.txt"), "not empty").unwrap();
    let src = "write_file(\"new.txt\", \"\"); write_file(\"old.txt\", \"\");";
    wf_run(dir.path(), src, true).unwrap();
    assert_eq!(std::fs::metadata(dir.join("new.txt")).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(dir.join("old.txt")).unwrap().len(), 0);
}

#[test]
fn test_write_file_writes_one_mebibyte_exactly() {
    // AC-02: большой объём; строка получается 20 удвоениями.
    let dir = WfDir::new("wf_big");
    let src = kw_src(
        "@VAR text = \"x\"; @VAR i = 0; \
         @WHILE i < 20 { text = text + text; i = i + 1; } \
         write_file(\"big.txt\", text);",
    );
    wf_run(dir.path(), &src, true).unwrap();
    let bytes = std::fs::read(dir.join("big.txt")).unwrap();
    assert_eq!(bytes.len(), 1 << 20);
    assert!(bytes.iter().all(|&b| b == b'x'));
}

#[test]
fn test_write_file_into_a_subdirectory_and_through_an_inner_dotdot() {
    // AC-02: существующий подкаталог; `sub/../x` остаётся внутри корня.
    let dir = WfDir::new("wf_subdir");
    std::fs::create_dir(dir.join("sub")).unwrap();
    let src = "write_file(\"sub/inner.txt\", \"a\"); write_file(\"sub/../outer.txt\", \"b\");";
    wf_run(dir.path(), src, true).unwrap();
    assert_eq!(wf_read(&dir.join("sub/inner.txt")), "a");
    assert_eq!(wf_read(&dir.join("outer.txt")), "b");
    assert!(!dir.join("sub/outer.txt").exists());
}

#[test]
fn test_write_file_then_read_file_round_trips_inside_one_program() {
    // AC-02: записанное сразу видно read_file, повторная запись — тоже.
    let dir = WfDir::new("wf_round_trip");
    let src = kw_src(
        "write_file(\"a.txt\", \"first\"); \
         @LET one = read_file(\"a.txt\"); \
         write_file(\"a.txt\", one + \" and second\"); \
         read_file(\"a.txt\");",
    );
    assert_eq!(
        wf_run(dir.path(), &src, true).unwrap(),
        Value::Str("first and second".into())
    );
}

#[test]
fn test_write_file_works_inside_functions_closures_and_loops() {
    // AC-02: состояние (разрешение, корень) принадлежит VM, а не кадру.
    let dir = WfDir::new("wf_frames");
    let src = kw_src(
        "@FN save(name, text) { write_file(name, text); } \
         @LET saver = @FN(text) { write_file(\"closure.txt\", text); }; \
         save(\"function.txt\", \"from fn\"); \
         saver(\"from closure\"); \
         @VAR i = 0; \
         @WHILE i < 3 { \
             write_file(\"f\" + to_string(i) + \".txt\", to_string(i * 10)); \
             i = i + 1; \
         }",
    );
    wf_run(dir.path(), &src, true).unwrap();
    assert_eq!(wf_read(&dir.join("function.txt")), "from fn");
    assert_eq!(wf_read(&dir.join("closure.txt")), "from closure");
    for i in 0..3 {
        assert_eq!(
            wf_read(&dir.join(&format!("f{i}.txt"))),
            (i * 10).to_string()
        );
    }
}

#[test]
fn test_write_file_copies_awkward_but_valid_utf8_exactly() {
    // AC-02: CR, NUL, DEL, BOM, пустые строки. Лексер SGA не умеет `\r` и `\0`,
    // поэтому такое содержимое приходит из файла через read_file и уходит
    // обратно через write_file.
    let dir = WfDir::new("wf_copy");
    let original = "crlf\r\nlone cr\rnul\0del\u{7f}bom\u{feff}end ✓\n\n";
    std::fs::write(dir.join("src.bin"), original).unwrap();
    let src = "write_file(\"dst.bin\", read_file(\"src.bin\"));";
    wf_run(dir.path(), src, true).unwrap();
    assert_eq!(
        std::fs::read(dir.join("dst.bin")).unwrap(),
        original.as_bytes()
    );
}

#[test]
fn test_write_file_permission_covers_imported_modules_and_writes_into_the_entry_directory() {
    // AC-02: разрешение — на всю программу, включая импортированный код; корень
    // — каталог входного файла.
    let dir = WfDir::new("wf_import");
    write_test_file(
        &dir.join("lib.sga"),
        &kw_src("@FN save() { write_file(\"from_lib.txt\", \"lib\"); }"),
    );
    let main = format!("{} \"lib.sga\"; save();", kw("IMPORT"));
    let msg = wf_expect_runtime_error(wf_run(dir.path(), &main, false));
    assert!(msg.contains("запрещена"), "получено: {msg}");
    assert!(!dir.join("from_lib.txt").exists());
    wf_run(dir.path(), &main, true).unwrap();
    assert_eq!(wf_read(&dir.join("from_lib.txt")), "lib");
}

#[test]
fn test_write_file_from_a_package_dependency_writes_relative_to_the_entry_directory() {
    // AC-02: пакет лежит ВНЕ каталога проекта, но его код пишет в каталог
    // входного файла программы, а не в каталог пакета.
    let root = WfDir::new("wf_pkg");
    let project = root.join("project");
    write_test_file(
        &project.join("sga.toml"),
        "[dependencies]\nmylib = { path = \"../mylib\" }\n",
    );
    write_test_file(
        &root.join("mylib").join("saver.sga"),
        &kw_src("@FN save() { write_file(\"from_pkg.txt\", \"pkg\"); }"),
    );
    let entry = project.join("main.sga");
    write_test_file(
        &entry,
        &format!("{} \"pkg:mylib/saver.sga\"; save();", kw("IMPORT")),
    );
    sga::run_source_file_with_options(&entry, RunOptions::new().with_allow_write(true)).unwrap();
    assert_eq!(wf_read(&project.join("from_pkg.txt")), "pkg");
    assert!(!root.join("mylib").join("from_pkg.txt").exists());
}

#[test]
fn test_write_file_permission_covers_every_file_in_the_directory_including_the_manifest() {
    // Документированное свойство (docs/SECURITY.md): разрешение на запись
    // относится ко всему каталогу входного файла — в том числе к sga.toml и к
    // самой программе. Тест фиксирует это, чтобы любое изменение правила было
    // осознанным.
    let dir = WfDir::new("wf_whole_directory");
    write_test_file(&dir.join("sga.toml"), "[dependencies]\n");
    wf_run(
        dir.path(),
        "write_file(\"sga.toml\", \"# rewritten\\n\");",
        true,
    )
    .unwrap();
    assert_eq!(wf_read(&dir.join("sga.toml")), "# rewritten\n");
}

// ----- AC-03: абсолютные пути -----

#[test]
fn test_write_file_absolute_path_is_rejected_even_when_it_points_into_the_root() {
    // AC-03: безусловно, до обращения к файловой системе.
    let (outer, proj) = wf_outer_and_proj("wf_absolute");
    std::fs::write(outer.join("victim.txt"), "keep").unwrap();
    for target in [
        outer.join("victim.txt"),
        proj.join("inside.txt"),
        outer.join("new.txt"),
    ] {
        let src = wf_write_src(wf_path_str(&target), "pwned");
        let msg = wf_expect_runtime_error(wf_run(&proj, &src, true));
        assert!(msg.contains("абсолютн"), "получено: {msg}");
    }
    assert_eq!(wf_read(&outer.join("victim.txt")), "keep");
    assert!(!proj.join("inside.txt").exists());
    assert!(!outer.join("new.txt").exists());
}

// ----- AC-04: выход за корень через `..` -----

#[test]
fn test_write_file_parent_escape_is_rejected() {
    // AC-04
    let (outer, proj) = wf_outer_and_proj("wf_dotdot");
    std::fs::create_dir(proj.join("sub")).unwrap();
    std::fs::write(outer.join("victim.txt"), "keep").unwrap();
    // Уникальное имя для выхода на два уровня — в общий временный каталог.
    let far = format!("sga_wf_escape_{}.txt", std::process::id());
    let far_path = format!("../../{far}");
    let far_nested = format!("sub/../../../{far}");
    for path in [
        "../victim.txt",
        "../new.txt",
        "./../new.txt",
        "sub/../../new.txt",
        "sub/../../victim.txt",
        far_path.as_str(),
        far_nested.as_str(),
    ] {
        let msg = wf_expect_runtime_error(wf_run(&proj, &wf_write_src(path, "pwned"), true));
        assert!(msg.contains("выходит за пределы"), "{path}: {msg}");
    }
    assert_eq!(wf_read(&outer.join("victim.txt")), "keep");
    assert_eq!(wf_listing(outer.path()), vec!["proj", "victim.txt"]);
    assert_eq!(wf_listing(&proj), vec!["main.sga", "sub"]);
    assert!(!std::env::temp_dir().join(&far).exists());
}

#[cfg(unix)]
#[test]
fn test_write_file_dotdot_after_a_symlink_to_an_outside_directory_is_rejected() {
    // AC-04: `link/../x.txt` лексически — `x.txt` в корне, но ОС разрешает `..`
    // физически: родитель каталога `elsewhere` — это `outer`, то есть вне корня.
    let (outer, proj) = wf_outer_and_proj("wf_link_dotdot");
    std::fs::create_dir(outer.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink("../elsewhere", proj.join("link")).unwrap();
    let msg = wf_expect_runtime_error(wf_run(&proj, &wf_write_src("link/../x.txt", "pwned"), true));
    assert!(msg.contains("выходит за пределы"), "получено: {msg}");
    assert!(!outer.join("x.txt").exists());
    assert!(!proj.join("x.txt").exists());
}

// ----- AC-05: каталоги-symlink -----

#[cfg(unix)]
#[test]
fn test_write_file_through_a_directory_symlink_pointing_outside_is_rejected() {
    // AC-05
    let (outer, proj) = wf_outer_and_proj("wf_dirlink_outside");
    std::fs::create_dir(outer.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink("../elsewhere", proj.join("out_link")).unwrap();
    let msg = wf_expect_runtime_error(wf_run(
        &proj,
        &wf_write_src("out_link/x.txt", "pwned"),
        true,
    ));
    assert!(msg.contains("выходит за пределы"), "получено: {msg}");
    assert!(wf_listing(&outer.join("elsewhere")).is_empty());
}

#[cfg(unix)]
#[test]
fn test_write_file_through_a_directory_symlink_pointing_inside_is_allowed() {
    // AC-05
    let dir = WfDir::new("wf_dirlink_inside");
    std::fs::create_dir(dir.join("real")).unwrap();
    std::os::unix::fs::symlink("real", dir.join("alias")).unwrap();
    wf_run(dir.path(), &wf_write_src("alias/x.txt", "ok"), true).unwrap();
    assert_eq!(wf_read(&dir.join("real/x.txt")), "ok");
}

#[cfg(unix)]
#[test]
fn test_write_file_resolves_dotdot_after_an_inside_symlink_physically() {
    // AC-05: `link` -> `a/b`; `link/../x.txt` — это `a/x.txt` (физический
    // родитель `b`), а не `x.txt` в корне.
    let dir = WfDir::new("wf_dirlink_dotdot_inside");
    std::fs::create_dir_all(dir.join("a/b")).unwrap();
    std::os::unix::fs::symlink("a/b", dir.join("link")).unwrap();
    wf_run(dir.path(), &wf_write_src("link/../x.txt", "phys"), true).unwrap();
    assert_eq!(wf_read(&dir.join("a/x.txt")), "phys");
    assert!(!dir.join("x.txt").exists());
}

// ----- AC-06: цель — symlink -----

#[cfg(unix)]
#[test]
fn test_write_file_to_a_symlink_target_is_rejected_whatever_it_points_to() {
    // AC-06: внутрь корня, наружу и висячая ссылка; цель ссылки не меняется.
    let (outer, proj) = wf_outer_and_proj("wf_filelink");
    std::fs::write(proj.join("inside_target.txt"), "inside").unwrap();
    std::fs::write(outer.join("outside_target.txt"), "outside").unwrap();
    std::os::unix::fs::symlink("inside_target.txt", proj.join("to_inside.txt")).unwrap();
    std::os::unix::fs::symlink("../outside_target.txt", proj.join("to_outside.txt")).unwrap();
    std::os::unix::fs::symlink("../dangling_target.txt", proj.join("dangling.txt")).unwrap();
    for name in ["to_inside.txt", "to_outside.txt", "dangling.txt"] {
        let msg = wf_expect_runtime_error(wf_run(&proj, &wf_write_src(name, "pwned"), true));
        assert!(msg.contains("символическ"), "{name}: {msg}");
        assert!(
            std::fs::symlink_metadata(proj.join(name))
                .unwrap()
                .file_type()
                .is_symlink(),
            "{name} перестала быть ссылкой"
        );
    }
    assert_eq!(wf_read(&proj.join("inside_target.txt")), "inside");
    assert_eq!(wf_read(&outer.join("outside_target.txt")), "outside");
    assert!(!outer.join("dangling_target.txt").exists());
}

// ----- AC-07: типы аргументов -----

#[test]
fn test_write_file_non_string_arguments_are_runtime_errors_with_and_without_permission() {
    // AC-07: проверка типов идёт раньше проверки разрешения и не зависит от него.
    let dir = WfDir::new("wf_types");
    let programs = [
        ("write_file(1, \"x\");", "первым"),
        ("write_file(1.5, \"x\");", "первым"),
        ("write_file(@NIL, \"x\");", "первым"),
        ("write_file([\"a.txt\"], \"x\");", "первым"),
        ("write_file(1, 2);", "первым"),
        ("write_file(\"a.txt\", 5);", "вторым"),
        ("write_file(\"a.txt\", @TRUE);", "вторым"),
        ("write_file(\"a.txt\", [1, 2]);", "вторым"),
        ("write_file(\"a.txt\", @NIL);", "вторым"),
    ];
    for allow in [false, true] {
        for (template, which) in programs {
            let msg = wf_expect_runtime_error(wf_run(dir.path(), &kw_src(template), allow));
            assert!(msg.contains("ожидает string"), "{template}: {msg}");
            assert!(msg.contains(which), "{template}: {msg}");
        }
    }
    assert_eq!(wf_listing(dir.path()), vec!["main.sga"]);
}

// ----- AC-08: арность, байткод, контекст -----

#[test]
fn test_write_file_wrong_arity_is_a_semantic_error() {
    // AC-08: ошибка компиляции — до запуска, с разрешением и без.
    for src in [
        "write_file();",
        "write_file(\"a.txt\");",
        "write_file(\"a.txt\", \"b\", \"c\");",
    ] {
        for options in [RunOptions::new(), RunOptions::new().with_allow_write(true)] {
            match sga::run_source_with_options(src, options) {
                Err(SgaError::Semantic(msg)) => {
                    assert!(msg.contains("write_file"), "{src}: {msg}")
                }
                other => panic!("ожидалась SemanticError для {src}, получено {:?}", other),
            }
        }
    }
}

#[test]
fn test_vm_write_file_with_wrong_arity_via_handbuilt_bytecode_is_a_runtime_error() {
    // AC-08 (defense-in-depth): VM можно запустить на вручную собранном байткоде
    // в обход semantic; даже при разрешении и файловом контексте — RuntimeError.
    let dir = WfDir::new("wf_vm_arity");
    for argc in [0usize, 1, 3] {
        let mut code = vec![OpCode::PushConst(0); argc];
        code.push(OpCode::Call("write_file".to_string(), argc));
        code.push(OpCode::Return(true));
        let program = CompiledProgram {
            main: Chunk {
                code,
                constants: vec![Value::Str("a.txt".to_string())],
            },
            functions: HashMap::new(),
        };
        let (mut machine, main) = Vm::new(program, Some(dir.path().to_path_buf()))
            .expect("байткод должен пройти верификатор");
        machine.set_allow_write(true);
        match machine.run(&main) {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("write_file()") && msg.contains("2 аргумента"),
                    "argc = {argc}: {msg}"
                );
            }
            Ok(v) => panic!("ожидалась RuntimeError при argc = {argc}, получено {:?}", v),
        }
    }
    assert!(wf_listing(dir.path()).is_empty());
}

#[test]
fn test_vm_is_read_only_by_default_and_set_allow_write_toggles_writing_inside_the_root() {
    // AC-01/AC-08: `Vm::new(program, Some(root))` без `set_allow_write` — запись
    // запрещена; `set_allow_write(true)` разрешает, `false` запрещает снова.
    let dir = WfDir::new("wf_vm_toggle");
    let program = compile_source(&wf_write_src("out.txt", "via vm"));
    let (mut machine, main) = Vm::new(program, Some(dir.path().to_path_buf())).unwrap();
    assert!(machine.run(&main).is_err());
    assert!(!dir.join("out.txt").exists());
    machine.set_allow_write(true);
    machine.run(&main).unwrap();
    assert_eq!(wf_read(&dir.join("out.txt")), "via vm");
    std::fs::write(dir.join("out.txt"), "untouched").unwrap();
    machine.set_allow_write(false);
    assert!(machine.run(&main).is_err());
    assert_eq!(wf_read(&dir.join("out.txt")), "untouched");
}

#[test]
fn test_write_file_without_a_file_context_fails_even_with_permission() {
    // AC-08: `run_source` (и `Vm::new(.., None)`) не знает корня песочницы —
    // писать некуда, и разрешение этого не меняет.
    let name = "__sga_t015_no_file_context__.txt";
    assert!(!std::path::Path::new(name).exists());
    let src = wf_write_src(name, "x");
    let msg = wf_expect_runtime_error(sga::run_source_with_options(
        &src,
        RunOptions::new().with_allow_write(true),
    ));
    assert!(msg.contains("без файлового контекста"), "получено: {msg}");
    let (mut machine, main) = Vm::new(compile_source(&src), None).unwrap();
    machine.set_allow_write(true);
    let msg = machine.run(&main).unwrap_err().to_string();
    assert!(msg.contains("без файлового контекста"), "получено: {msg}");
    assert!(!std::path::Path::new(name).exists());
}

#[test]
fn test_variable_and_parameter_named_write_file_are_unaffected_by_the_builtin() {
    // AC-08: имя `write_file` как переменная/параметр работает как раньше.
    let variable = kw_src("@LET write_file = 5; write_file + 1;");
    assert_eq!(run(&variable).unwrap(), Value::Int(6));
    let parameter = kw_src("@FN keep(write_file) { @RETURN write_file * 2; } keep(21);");
    assert_eq!(run(&parameter).unwrap(), Value::Int(42));
}

// ----- AC-09: прочие отказы -----

#[test]
fn test_write_file_missing_parent_directory_is_an_error_and_nothing_is_created() {
    // AC-09: каталоги не создаются.
    let dir = WfDir::new("wf_missing_parent");
    for path in ["nope/x.txt", "a/b/c/x.txt"] {
        let msg = wf_expect_runtime_error(wf_run(dir.path(), &wf_write_src(path, "x"), true));
        assert!(msg.contains("не существует"), "{path}: {msg}");
    }
    assert_eq!(wf_listing(dir.path()), vec!["main.sga"]);
}

#[test]
fn test_write_file_to_a_directory_is_an_error_and_the_directory_stays_intact() {
    // AC-09
    let dir = WfDir::new("wf_target_directory");
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/keep.txt"), "keep").unwrap();
    let msg = wf_expect_runtime_error(wf_run(dir.path(), &wf_write_src("sub", "x"), true));
    assert!(msg.contains("каталог"), "получено: {msg}");
    assert!(dir.join("sub").is_dir());
    assert_eq!(wf_read(&dir.join("sub/keep.txt")), "keep");
}

#[test]
fn test_write_file_paths_that_do_not_name_a_file_are_errors() {
    // AC-09: пустой путь, завершающий разделитель, `.`, `..`, NUL.
    let dir = WfDir::new("wf_bad_paths");
    std::fs::create_dir(dir.join("sub")).unwrap();
    // `newdir/` и подобные — не файл и тогда, когда такого каталога ещё нет:
    // иначе завершающий разделитель молча превратился бы в файл `newdir`.
    for path in [
        "",
        "sub/",
        "sub//",
        ".",
        "..",
        "sub/.",
        "sub/..",
        "./",
        "../",
        "newdir/",
        "newdir//",
        "newdir/.",
        "sub/newdir/",
    ] {
        wf_expect_runtime_error(wf_run(dir.path(), &wf_write_src(path, "x"), true));
    }
    let with_nul = "write_file(\"a\u{0}b.txt\", \"x\");";
    let msg = wf_expect_runtime_error(wf_run(dir.path(), with_nul, true));
    assert!(msg.contains("нулев"), "получено: {msg}");
    assert_eq!(wf_listing(dir.path()), vec!["main.sga", "sub"]);
}

#[test]
fn test_write_file_with_a_300_character_name_is_a_clear_error_not_a_panic() {
    // AC-09: слишком длинное имя — ошибка ОС, превращённая в RuntimeError.
    let dir = WfDir::new("wf_long_name");
    let path = format!("{}.txt", "n".repeat(300));
    let msg = wf_expect_runtime_error(wf_run(dir.path(), &wf_write_src(&path, "x"), true));
    assert!(msg.contains("write_file("), "получено: {msg}");
    assert_eq!(wf_listing(dir.path()), vec!["main.sga"]);
}

// ----- AC-10: сквозные тесты через НАСТОЯЩИЙ бинарник -----

#[test]
fn test_cli_allow_write_before_the_file_creates_the_file_next_to_the_program() {
    // AC-10 (а): абсолютный путь входного файла.
    let dir = WfDir::new("wf_cli_flag");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &wf_write_src("out.txt", "from cli"));
    let out = wf_cli(None, &["--allow-write", wf_path_str(&entry)]);
    assert!(out.status.success(), "stderr = {}", wf_stderr(&out));
    assert_eq!(wf_read(&dir.join("out.txt")), "from cli");
}

#[test]
fn test_cli_without_the_flag_refuses_to_write_and_says_how_to_allow_it() {
    // AC-10 (б)
    let dir = WfDir::new("wf_cli_no_flag");
    let entry = dir.join("main.sga");
    write_test_file(&entry, &wf_write_src("out.txt", "x"));
    let out = wf_cli(None, &[wf_path_str(&entry)]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        wf_stderr(&out).contains("--allow-write"),
        "{}",
        wf_stderr(&out)
    );
    assert!(!dir.join("out.txt").exists());
}

#[test]
fn test_cli_allow_write_after_the_file_is_a_program_argument_and_grants_nothing() {
    // AC-10 (в): флаг ПОСЛЕ пути — аргумент программы (`args()`), а не разрешение.
    let dir = WfDir::new("wf_cli_flag_after");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &kw_src("@PRINT(args()); write_file(\"out.txt\", \"x\");"),
    );
    let out = wf_cli(None, &[wf_path_str(&entry), "--allow-write"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        wf_stdout(&out).contains("--allow-write"),
        "{}",
        wf_stdout(&out)
    );
    assert!(wf_stderr(&out).contains("запрещена"), "{}", wf_stderr(&out));
    assert!(!dir.join("out.txt").exists());
    // Флаг и до, и после: разрешение есть, а второй флаг всё равно дошёл до args().
    let out = wf_cli(
        None,
        &["--allow-write", wf_path_str(&entry), "--allow-write"],
    );
    assert!(out.status.success(), "stderr = {}", wf_stderr(&out));
    assert!(
        wf_stdout(&out).contains("--allow-write"),
        "{}",
        wf_stdout(&out)
    );
    assert_eq!(wf_read(&dir.join("out.txt")), "x");
}

#[test]
fn test_cli_unknown_flag_before_the_file_is_an_error_and_nothing_runs() {
    // AC-10 (г): опечатка во флаге не должна молча превращаться в запуск.
    let dir = WfDir::new("wf_cli_unknown_flag");
    let entry = dir.join("main.sga");
    write_test_file(
        &entry,
        &kw_src("@PRINT(\"ran\"); write_file(\"out.txt\", \"x\");"),
    );
    for flag in ["--bogus", "--allow-writes", "--allow-write=1", "-w"] {
        let out = wf_cli(None, &[flag, wf_path_str(&entry)]);
        assert_eq!(out.status.code(), Some(1), "{flag}");
        assert!(
            wf_stderr(&out).contains(flag),
            "{flag}: {}",
            wf_stderr(&out)
        );
        assert!(
            wf_stdout(&out).is_empty(),
            "{flag}: программа не должна запускаться"
        );
    }
    assert!(!dir.join("out.txt").exists());
}

#[test]
fn test_cli_double_dash_ends_the_flags() {
    // AC-10 (д): `--` завершает флаги — файл запускается БЕЗ разрешения записи;
    // имя файла, похожее на флаг, пишется через `--` или `./`.
    let dir = WfDir::new("wf_cli_double_dash");
    write_test_file(&dir.join("main.sga"), &kw_src("@PRINT(\"ran\");"));
    let out = wf_cli(None, &["--", wf_path_str(&dir.join("main.sga"))]);
    assert!(out.status.success(), "stderr = {}", wf_stderr(&out));
    assert_eq!(wf_stdout(&out), "ran\n");

    write_test_file(&dir.join("-weird.sga"), &kw_src("@PRINT(\"weird ran\");"));
    for args in [["--", "-weird.sga"], ["--allow-write", "./-weird.sga"]] {
        let out = wf_cli(Some(dir.path()), &args);
        assert!(out.status.success(), "{args:?}: {}", wf_stderr(&out));
        assert_eq!(wf_stdout(&out), "weird ran\n", "{args:?}");
    }
    let out = wf_cli(Some(dir.path()), &["-weird.sga"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = wf_stderr(&out);
    assert!(
        stderr.contains("'-weird.sga'") && stderr.contains("./-weird.sga"),
        "{stderr}"
    );
}

#[test]
fn test_cli_run_without_a_file_prints_the_usage() {
    // AC-10 (д): только флаги или ничего — использование, код 1.
    for args in [
        vec![],
        vec!["--allow-write"],
        vec!["--"],
        vec!["--allow-write", "--"],
    ] {
        let out = wf_cli(None, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = wf_stderr(&out);
        assert!(
            stderr.contains("использование: sga run [--allow-write] [--] <файл.sga>"),
            "{args:?}: {stderr}"
        );
        assert!(wf_stdout(&out).is_empty(), "{args:?}");
    }
    // Общая справка тоже рассказывает про флаг.
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_sga"))
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(1));
    let text = wf_stderr(&help);
    assert!(
        text.contains("--allow-write") && text.contains("write_file"),
        "{text}"
    );
}

#[test]
fn test_cli_bare_and_dot_slash_entry_names_confine_writes_to_the_program_directory() {
    // AC-10 (е): `sga run main.sga` и `sga run ./main.sga` из каталога программы.
    let (outer, proj) = wf_outer_and_proj("wf_cli_bare_write");
    std::fs::write(outer.join("victim.txt"), "keep").unwrap();
    std::fs::create_dir(proj.join("sub")).unwrap();
    for entry_arg in ["main.sga", "./main.sga"] {
        write_test_file(&proj.join("main.sga"), &wf_write_src("inside.txt", "ok"));
        let out = wf_cli(Some(&proj), &["--allow-write", entry_arg]);
        assert!(out.status.success(), "{entry_arg}: {}", wf_stderr(&out));
        assert_eq!(wf_read(&proj.join("inside.txt")), "ok");
        std::fs::remove_file(proj.join("inside.txt")).unwrap();

        for path in [
            "../victim.txt",
            "../new.txt",
            "sub/../../new.txt",
            "../proj/../new.txt",
        ] {
            write_test_file(&proj.join("main.sga"), &wf_write_src(path, "pwned"));
            let out = wf_cli(Some(&proj), &["--allow-write", entry_arg]);
            assert_eq!(out.status.code(), Some(1), "{entry_arg} {path}");
            assert!(
                wf_stderr(&out).contains("выходит за пределы"),
                "{entry_arg} {path}: {}",
                wf_stderr(&out)
            );
        }
        assert_eq!(wf_read(&outer.join("victim.txt")), "keep", "{entry_arg}");
        assert!(!outer.join("new.txt").exists(), "{entry_arg}");
    }
}

#[cfg(unix)]
#[test]
fn test_cli_bare_entry_name_rejects_writes_through_a_symlink_to_an_outside_directory() {
    // AC-10 (е): symlink наружу при запуске по голому имени файла.
    let (outer, proj) = wf_outer_and_proj("wf_cli_bare_symlink");
    std::fs::create_dir(outer.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink("../elsewhere", proj.join("link")).unwrap();
    for entry_arg in ["main.sga", "./main.sga"] {
        write_test_file(&proj.join("main.sga"), &wf_write_src("link/x.txt", "pwned"));
        let out = wf_cli(Some(&proj), &["--allow-write", entry_arg]);
        assert_eq!(out.status.code(), Some(1), "{entry_arg}");
        assert!(
            wf_stderr(&out).contains("выходит за пределы"),
            "{entry_arg}: {}",
            wf_stderr(&out)
        );
        assert!(
            wf_listing(&outer.join("elsewhere")).is_empty(),
            "{entry_arg}"
        );
    }
}

#[test]
fn test_cli_write_file_example_runs_with_the_flag_and_is_refused_without_it() {
    // AC-12: examples/write_file.sga выполняется (копия во временном каталоге,
    // чтобы тест не оставлял следов в examples/data/).
    let dir = WfDir::new("wf_example");
    std::fs::create_dir(dir.join("data")).unwrap();
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("write_file.sga");
    let entry = dir.join("write_file.sga");
    std::fs::copy(&example, &entry).unwrap();

    let denied = wf_cli(None, &[wf_path_str(&entry)]);
    assert_eq!(denied.status.code(), Some(1));
    assert!(
        wf_stderr(&denied).contains("--allow-write"),
        "{}",
        wf_stderr(&denied)
    );
    assert!(!dir.join("data/written_by_example.txt").exists());

    let out = wf_cli(None, &["--allow-write", wf_path_str(&entry)]);
    assert!(out.status.success(), "stderr = {}", wf_stderr(&out));
    let stdout = wf_stdout(&out);
    assert!(stdout.contains("первая запись"), "{stdout}");
    assert!(stdout.contains("вторая"), "{stdout}");
    assert_eq!(
        wf_read(&dir.join("data/written_by_example.txt")),
        "вторая\n"
    );
}

// ----- AC-11: корень песочницы при запуске по голому имени файла (регрессия) -----

/// Способы указать один и тот же входной файл `main.sga` каталога `proj`:
/// (аргумент командной строки, текущий каталог процесса). Первые два — голое имя
/// и `./`-имя (корень песочницы раньше был пустым), остальные — абсолютный и
/// «каталог/имя» (работали и раньше; фиксируем, что не сломались).
fn wf_entry_variants(
    outer: &std::path::Path,
    proj: &std::path::Path,
) -> Vec<(String, Option<std::path::PathBuf>)> {
    vec![
        ("main.sga".to_string(), Some(proj.to_path_buf())),
        ("./main.sga".to_string(), Some(proj.to_path_buf())),
        (wf_path_str(&proj.join("main.sga")).to_string(), None),
        ("proj/main.sga".to_string(), Some(outer.to_path_buf())),
    ]
}

#[test]
fn test_cli_read_file_cannot_escape_the_program_directory_for_any_entry_spelling() {
    // AC-11: до исправления `sga run main.sga` читал `../secret.txt`.
    let (outer, proj) = wf_outer_and_proj("wf_root_read");
    std::fs::write(outer.join("secret.txt"), "TOP SECRET").unwrap();
    std::fs::write(proj.join("inside.txt"), "inside ok").unwrap();
    std::fs::create_dir(proj.join("sub")).unwrap();
    for (entry_arg, cwd) in wf_entry_variants(outer.path(), &proj) {
        for path in ["../secret.txt", "sub/../../secret.txt"] {
            write_test_file(&proj.join("main.sga"), &wf_print_read_src(path));
            let out = wf_cli(cwd.as_deref(), &[entry_arg.as_str()]);
            assert_eq!(out.status.code(), Some(1), "{entry_arg} {path}");
            assert!(
                wf_stderr(&out).contains("выходит за пределы"),
                "{entry_arg} {path}: {}",
                wf_stderr(&out)
            );
            assert!(
                !wf_stdout(&out).contains("TOP SECRET"),
                "{entry_arg} {path}"
            );
            assert!(
                !wf_stderr(&out).contains("TOP SECRET"),
                "{entry_arg} {path}"
            );
        }
        for path in ["inside.txt", "sub/../inside.txt"] {
            write_test_file(&proj.join("main.sga"), &wf_print_read_src(path));
            let out = wf_cli(cwd.as_deref(), &[entry_arg.as_str()]);
            assert!(
                out.status.success(),
                "{entry_arg} {path}: {}",
                wf_stderr(&out)
            );
            assert_eq!(wf_stdout(&out), "inside ok\n", "{entry_arg} {path}");
        }
    }
}

#[test]
fn test_cli_import_cannot_escape_the_program_directory_for_any_entry_spelling() {
    // AC-11: то же для IMPORT (раньше `IMPORT "../x.sga"` при голом имени
    // входного файла выполнял чужой код).
    let (outer, proj) = wf_outer_and_proj("wf_root_import");
    write_test_file(
        &outer.join("lib_outside.sga"),
        &kw_src("@PRINT(\"OUTSIDE LIB RAN\");"),
    );
    write_test_file(
        &proj.join("sub").join("lib.sga"),
        &kw_src("@PRINT(\"inside lib ran\");"),
    );
    for (entry_arg, cwd) in wf_entry_variants(outer.path(), &proj) {
        write_test_file(
            &proj.join("main.sga"),
            &format!("{} \"../lib_outside.sga\";", kw("IMPORT")),
        );
        let out = wf_cli(cwd.as_deref(), &[entry_arg.as_str()]);
        assert_eq!(out.status.code(), Some(1), "{entry_arg}");
        assert!(
            wf_stderr(&out).contains("выходит за пределы"),
            "{entry_arg}: {}",
            wf_stderr(&out)
        );
        assert!(!wf_stdout(&out).contains("OUTSIDE LIB RAN"), "{entry_arg}");

        write_test_file(
            &proj.join("main.sga"),
            &format!("{} \"sub/lib.sga\";", kw("IMPORT")),
        );
        let out = wf_cli(cwd.as_deref(), &[entry_arg.as_str()]);
        assert!(out.status.success(), "{entry_arg}: {}", wf_stderr(&out));
        assert_eq!(wf_stdout(&out), "inside lib ran\n", "{entry_arg}");
    }
}

#[cfg(unix)]
#[test]
fn test_cli_read_file_through_a_symlink_to_an_outside_directory_is_rejected_for_any_entry_spelling()
{
    // AC-11: каталог-symlink наружу при чтении (раньше при голом имени файла
    // проходил: пустой корень сравнивался лексически).
    let (outer, proj) = wf_outer_and_proj("wf_root_symlink");
    std::fs::create_dir(outer.join("elsewhere")).unwrap();
    std::fs::write(outer.join("elsewhere/secret.txt"), "TOP SECRET").unwrap();
    std::os::unix::fs::symlink("../elsewhere", proj.join("link")).unwrap();
    for (entry_arg, cwd) in wf_entry_variants(outer.path(), &proj) {
        write_test_file(
            &proj.join("main.sga"),
            &wf_print_read_src("link/secret.txt"),
        );
        let out = wf_cli(cwd.as_deref(), &[entry_arg.as_str()]);
        assert_eq!(out.status.code(), Some(1), "{entry_arg}");
        assert!(
            wf_stderr(&out).contains("выходит за пределы"),
            "{entry_arg}: {}",
            wf_stderr(&out)
        );
        assert!(!wf_stdout(&out).contains("TOP SECRET"), "{entry_arg}");
    }
}
