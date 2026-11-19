//! Позиции ошибок semantic и typechecker (T017, M007): каждая ошибка указывает
//! на узел AST, из-за которого возникла, а `sga run` печатает `[строка:колонка]`
//! этого узла перед сообщением — как у ошибок лексера и парсера.
//!
//! Ожидаемый узел в шаблоне отмечается парой меток `«…»`: тест убирает метки,
//! запоминает границы помеченного текста и сравнивает их с границами, в которые
//! переводится `span` ошибки. Так диапазон проверяется ПОЛНОСТЬЮ (строка и
//! колонка начала и конца), а не только по тексту под ним: два одинаковых `v` в
//! разных местах программы различаются положением метки. Ключевые слова пишутся
//! `@LET`, `@IF`, … — подставляются словами из SGA-букв.
//!
//! Таблицы ниже покрывают КАЖДОЕ место порождения ошибки: 21 — в
//! `semantic::Analyzer` и 14 — в `typechecker::Typechecker`.

use sga::ast::Program;
use sga::lexer::token::TokenSpan;
use sga::lexer::Lexer;
use sga::parser::Parser;
use sga::semantic::Analyzer;
use sga::sga_alphabet::encode_word;
use sga::typechecker::Typechecker;
use sga::{run_source, run_source_file, SgaError};
use std::path::PathBuf;

// ── помощники ──────────────────────────────────────────────────────────────

/// Подставляет SGA-слова: `@LET` -> слово `LET` из SGA-букв.
fn sga(template: &str) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '@' {
            out.push(c);
            continue;
        }
        let mut word = String::new();
        while let Some(&next) = chars.peek() {
            if !next.is_ascii_uppercase() {
                break;
            }
            word.push(next);
            chars.next();
        }
        assert!(!word.is_empty(), "`@` без слова в шаблоне {template:?}");
        out.push_str(&encode_word(&word));
    }
    out
}

/// Шаблон с одной парой меток `«…»`: исходный текст без меток и границы
/// помеченного текста (строки и колонки с единицы, колонки — в символах, конец
/// исключающий; меры те же, что у `Lexer::tokenize_with_spans`).
fn marked(template: &str) -> (String, TokenSpan) {
    let expanded = sga(template);
    let (mut line, mut col) = (1usize, 1usize);
    let mut clean = String::new();
    let (mut start, mut end) = (None, None);
    for c in expanded.chars() {
        match c {
            '«' => {
                assert!(start.is_none(), "две метки «: {template:?}");
                start = Some((line, col));
            }
            '»' => {
                assert!(end.is_none(), "две метки »: {template:?}");
                end = Some((line, col));
            }
            '\n' => {
                clean.push(c);
                line += 1;
                col = 1;
            }
            _ => {
                clean.push(c);
                col += 1;
            }
        }
    }
    let (start_line, start_col) = start.unwrap_or_else(|| panic!("нет метки «: {template:?}"));
    let (end_line, end_col) = end.unwrap_or_else(|| panic!("нет метки »: {template:?}"));
    (
        clean,
        TokenSpan {
            line: start_line,
            col: start_col,
            end_line,
            end_col,
        },
    )
}

fn parse(src: &str) -> (Program, Vec<TokenSpan>) {
    let (tokens, table) = Lexer::new(src)
        .tokenize_with_spans()
        .unwrap_or_else(|e| panic!("лексинг {src:?}: {e}"));
    let program = Parser::new(tokens)
        .parse_program()
        .unwrap_or_else(|e| panic!("парсинг {src:?}: {e}"));
    (program, table)
}

/// Таблица случаев: название, шаблон с меткой на ожидаемом узле, подстрока
/// сообщения.
type Cases = &'static [(&'static str, &'static str, &'static str)];

fn check_semantic(cases: Cases) {
    for &(name, template, message) in cases {
        let (src, expected) = marked(template);
        let (program, table) = parse(&src);
        let err = Analyzer::new()
            .analyze(&program)
            .expect_err(&format!("{name}: ожидалась ошибка семантики"));
        assert!(
            err.message.contains(message),
            "{name}: сообщение {:?} не содержит {message:?}",
            err.message
        );
        let actual = err
            .span
            .resolve(&table)
            .unwrap_or_else(|| panic!("{name}: диапазон {:?} не переводится в позиции", err.span));
        assert_eq!(actual, expected, "{name}: ошибка указывает не на тот узел");
        // `Display` не знает о позиции: префикс добавляет вызывающий
        assert_eq!(
            err.to_string(),
            format!("ошибка семантики: {}", err.message)
        );
    }
}

fn check_types(cases: Cases) {
    for &(name, template, message) in cases {
        let (src, expected) = marked(template);
        let (program, table) = parse(&src);
        let err = Typechecker::new()
            .analyze(&program)
            .expect_err(&format!("{name}: ожидалась ошибка типов"));
        assert!(
            err.message.contains(message),
            "{name}: сообщение {:?} не содержит {message:?}",
            err.message
        );
        let actual = err
            .span
            .resolve(&table)
            .unwrap_or_else(|| panic!("{name}: диапазон {:?} не переводится в позиции", err.span));
        assert_eq!(actual, expected, "{name}: ошибка указывает не на тот узел");
        assert_eq!(err.to_string(), format!("ошибка типов: {}", err.message));
    }
}

// ── semantic: 21 место ошибки ─────────────────────────────────────────────

#[test]
fn duplicate_declarations_point_at_the_declaring_node() {
    check_semantic(&[
        (
            "функция объявлена дважды — весь второй оператор",
            "@FN f() { 1; } «@FN f() { 2; }»",
            "функция 'f' уже объявлена",
        ),
        (
            "тип объявлен дважды — весь второй оператор",
            "@STRUCT P { x } «@STRUCT P { y }»",
            "тип 'P' уже объявлен",
        ),
        (
            "переменная объявлена дважды — оператор LET",
            "@LET a = 1; «@LET a = 2;»",
            "переменная 'a' уже объявлена",
        ),
        (
            "повтор VAR после LET — оператор VAR",
            "@LET a = 1;\n«@VAR a = 2;»",
            "переменная 'a' уже объявлена",
        ),
        (
            "параметр функции объявлен дважды — второй параметр",
            "@FN f(a, «@MUT a») { 1; }",
            "переменная 'a' уже объявлена",
        ),
        (
            "параметр замыкания объявлен дважды — всё замыкание",
            "@LET f = «@FN(x, x) { 1; }»;",
            "переменная 'x' уже объявлена",
        ),
    ]);
}

#[test]
fn statements_outside_their_place_point_at_the_statement() {
    check_semantic(&[
        ("RETURN вне функции", "«@RETURN 1;»", "RETURN вне функции"),
        ("BREAK вне цикла", "«@BREAK;»", "BREAK вне цикла"),
        ("CONTINUE вне цикла", "«@CONTINUE;»", "CONTINUE вне цикла"),
        (
            "BREAK в замыкании внутри цикла",
            "@WHILE @TRUE { @LET f = @FN() { «@BREAK;» }; }",
            "BREAK вне цикла",
        ),
        (
            "IMPORT не резолвлен",
            "«@IMPORT \"m.sga\";»",
            "не был резолвлен до семантического анализа",
        ),
    ]);
}

#[test]
fn undefined_names_and_calls_point_at_the_name_or_the_call() {
    check_semantic(&[
        (
            "неопределённая переменная — сам идентификатор",
            "@PRINT(«b»);",
            "неопределённая переменная 'b'",
        ),
        (
            "неопределённая переменная в присваивании — всё присваивание",
            "«b = 1»;",
            "неопределённая переменная 'b'",
        ),
        (
            "неопределённая переменная внутри выражения",
            "@LET a = 1;\n@LET c = a + «zz» * 2;",
            "неопределённая переменная 'zz'",
        ),
        (
            "неопределённая функция — весь вызов",
            "«g(1, 2)»;",
            "неопределённая функция 'g'",
        ),
        (
            "арность пользовательской функции — весь вызов",
            "@FN f(a) { 1; } @LET r = «f(1, 2)»;",
            "функция 'f' ожидает 1 аргумент(ов), передано 2",
        ),
        (
            "арность builtin — весь вызов",
            "@PRINT(«len()»);",
            "функция 'len' ожидает 1 аргумент(ов), передано 0",
        ),
        (
            "диапазон арности builtin — весь вызов",
            "«push(1, 2, 3)»;",
            "функция 'push' ожидает от 1 до 2",
        ),
    ]);
}

#[test]
fn ownership_violations_point_at_the_offending_operand() {
    check_semantic(&[
        (
            "immutable-переменная в MUT-параметр — сам аргумент",
            "@FN f(@MUT a) { 1; } @LET v = [1]; f(«v»);",
            "нельзя передать immutable-переменную 'v' в параметр 1 функции 'f'",
        ),
        (
            "второй аргумент в MUT-параметр — именно он",
            "@FN f(a, @MUT b) { 1; } @LET w = [1]; @VAR v = [2];\nf(v, «w»);",
            "в параметр 2 функции 'f'",
        ),
        (
            "push в immutable — первый аргумент",
            "@LET a = [1]; push(«a», 2);",
            "через push()",
        ),
        (
            "индексное присваивание в immutable — цель присваивания",
            "@LET a = [1]; «a»[0] = 5;",
            "через индексное присваивание",
        ),
        (
            "вложенный индекс: корень — цель целиком",
            "@LET a = [[1]]; «a[0]»[0] = 5;",
            "через индексное присваивание",
        ),
        (
            "присваивание полю immutable struct — объект присваивания",
            "@STRUCT P { x } @LET p = P { x: 1 }; «p».x = 2;",
            "через присваивание полю struct",
        ),
        (
            "присваивание immutable-переменной — всё присваивание",
            "@LET a = 1;\n«a = 2»;",
            "невозможно изменить immutable-переменную 'a'",
        ),
        (
            "присваивание immutable-параметру — всё присваивание",
            "@FN f(p) { «p = 1»; }",
            "невозможно изменить immutable-переменную 'p'",
        ),
    ]);
}

#[test]
fn struct_literal_errors_point_at_the_whole_literal() {
    check_semantic(&[
        (
            "неизвестный тип",
            "@LET p = «Q { x: 1 }»;",
            "неизвестный тип 'Q'",
        ),
        (
            "неизвестное поле",
            "@STRUCT P { x }\n@LET p = «P { y: 1 }»;",
            "тип 'P' не имеет поля 'y'",
        ),
        (
            "поле указано дважды",
            "@STRUCT P { x } @LET p = «P { x: 1, x: 2 }»;",
            "поле 'x' указано дважды",
        ),
        (
            "многострочный литерал — от имени типа до закрывающей скобки",
            "@STRUCT P { x }\n@LET p = «P {\n    y: 1\n}»;",
            "тип 'P' не имеет поля 'y'",
        ),
    ]);
}

#[test]
fn match_errors_point_at_the_match_or_the_offending_arm() {
    check_semantic(&[
        (
            "MATCH без arm'ов — всё выражение",
            "@LET r = «@MATCH 1 { }»;",
            "MATCH без единого arm",
        ),
        (
            "catch-all не последний — этот пункт",
            "@LET r = @MATCH 1 { «_ -> 0», 1 -> 2 };",
            "недостижимый паттерн в MATCH",
        ),
        (
            "bind-имя не последним — этот пункт",
            "@LET r = @MATCH 1 { 5 -> 0, «n -> n», 1 -> 2 };",
            "catch-all ('_' или имя-привязка) на позиции 2",
        ),
        (
            "последний пункт не catch-all — именно он",
            "@LET r = @MATCH 1 { 1 -> 2, «2 -> 3» };",
            "MATCH обязан заканчиваться catch-all",
        ),
        (
            "ошибка в теле пункта — сам узел-причина, а не пункт",
            "@LET r = @MATCH 1 { 1 -> «zz», _ -> 0 };",
            "неопределённая переменная 'zz'",
        ),
    ]);
}

#[test]
fn errors_inside_nested_constructs_point_inside_them() {
    check_semantic(&[
        (
            "в теле функции",
            "@FN f() {\n    @IF @TRUE {\n        @PRINT(«nope»);\n    }\n}",
            "неопределённая переменная 'nope'",
        ),
        (
            "в теле замыкания",
            "@LET g = @FN(x) { @RETURN x + «y»; };",
            "неопределённая переменная 'y'",
        ),
        (
            "в границе цикла FOR",
            "@FOR i @IN 0..«n» { 1; }",
            "неопределённая переменная 'n'",
        ),
        (
            "в аргументе после скобок",
            "@PRINT((1 + 2) * «q»);",
            "неопределённая переменная 'q'",
        ),
    ]);
}

// ── typechecker: 14 мест ошибки ───────────────────────────────────────────

#[test]
fn declaration_and_assignment_type_errors_point_at_the_value() {
    check_types(&[
        (
            "несовместимое значение в LET — инициализатор",
            "@LET x: int = «\"s\"»;",
            "переменная 'x' объявлена с типом int, но присвоено значение типа string",
        ),
        (
            "границы FOR..IN — конец диапазона",
            "@FOR i @IN 1..«\"a\"» { 1; }",
            "границы диапазона FOR..IN имеют несовместимые типы: int и string",
        ),
        (
            "присваивание несовместимого типа — присваиваемое значение",
            "@VAR x: int = 1;\nx = «\"s\"»;",
            "невозможно присвоить значение типа string переменной 'x'",
        ),
    ]);
}

#[test]
fn return_type_errors_point_at_the_returned_value_or_the_statement() {
    check_types(&[
        (
            "RETURN со значением — возвращаемое выражение",
            "@FN f() -> int { @RETURN «\"s\"»; }",
            "RETURN возвращает string",
        ),
        (
            "RETURN без значения — весь оператор",
            "@FN f() -> int { «@RETURN;» }",
            "RETURN возвращает nil",
        ),
    ]);
}

#[test]
fn indexing_type_errors_point_at_the_target_or_the_index() {
    check_types(&[
        (
            "индексация не определена — цель",
            "@LET n: int = 5; @LET v = «n»[0];",
            "индексация '[...]' не определена для типа int",
        ),
        (
            "индекс не int — индекс",
            "@LET a = [1]; @LET v = a[«\"x\"»];",
            "индекс массива должен быть int, получено string",
        ),
        (
            "индексное присваивание не определено — цель",
            "@VAR n: int = 1; «n»[0] = 1;",
            "индексное присваивание не определено для типа int",
        ),
        (
            "индекс присваивания не int — индекс",
            "@VAR a = [1]; a[«\"x\"»] = 1;",
            "индекс массива должен быть int, получено string",
        ),
    ]);
}

#[test]
fn operator_type_errors_point_at_the_whole_expression() {
    check_types(&[
        (
            "унарный минус — всё выражение",
            "@LET s: string = \"a\";\n@LET v = «-s»;",
            "унарный '-' не определён для string",
        ),
        (
            "сложение несовместимых типов — всё выражение",
            "@LET a: int = 1; @LET b: string = \"x\"; @LET v = «a + b»;",
            "оператор '+' не определён для int и string",
        ),
        (
            "арифметика несовместимых типов — всё выражение",
            "@LET a: int = 1; @LET b: string = \"x\"; @LET v = «a - b»;",
            "арифметический оператор не определён для int и string",
        ),
        (
            "бинарное выражение со скобками — вместе со скобками",
            "@LET a: int = 1; @LET b: string = \"x\"; @LET v = «(a) * (b)»;",
            "арифметический оператор не определён",
        ),
    ]);
}

#[test]
fn call_type_errors_point_at_the_argument_or_the_callee() {
    check_types(&[
        (
            "несовместимый аргумент — сам аргумент",
            "@FN f(a: int) { 1; } @LET v = f(«\"s\"»);",
            "функция 'f': аргумент 1 должен быть int, передан string",
        ),
        (
            "второй аргумент — именно он",
            "@FN f(a: int, b: string) { 1; }\n@LET v = f(1, «2»);",
            "аргумент 2 должен быть string, передан int",
        ),
        (
            "вызов значения не-замыкания — вызываемое выражение",
            "@LET n: int = 5; @LET v = («n»)(1);",
            "вызов значения типа int как функции невозможен",
        ),
    ]);
}

#[test]
fn an_unresolved_import_points_at_the_statement() {
    check_types(&[(
        "IMPORT не резолвлен",
        "«@IMPORT \"m.sga\";»",
        "не был резолвлен до проверки типов",
    )]);
}

// ── позиция в сообщении пайплайна (`run_source`, `run_source_file`) ───────

fn semantic_message(src: &str) -> String {
    match run_source(src) {
        Err(SgaError::Semantic(message)) => message,
        other => panic!("ожидалась SgaError::Semantic, получено {other:?}"),
    }
}

fn type_message(src: &str) -> String {
    match run_source(src) {
        Err(SgaError::Type(message)) => message,
        other => panic!("ожидалась SgaError::Type, получено {other:?}"),
    }
}

#[test]
fn run_source_prefixes_semantic_errors_with_line_and_column() {
    // `LET a = 1; PRINT(b);` — `b` на 18-й колонке
    assert_eq!(
        semantic_message(&sga("@LET a = 1; @PRINT(b);")),
        "[1:18] ошибка семантики: неопределённая переменная 'b'"
    );
    // переменная на третьей строке после комментария и пустой строки
    assert_eq!(
        semantic_message(&sga("// комментарий\n\n    @PRINT(c);")),
        "[3:11] ошибка семантики: неопределённая переменная 'c'"
    );
}

#[test]
fn run_source_prefixes_type_errors_with_line_and_column() {
    assert_eq!(
        type_message(&sga("@LET x: int = \"s\";")),
        "[1:14] ошибка типов: переменная 'x' объявлена с типом int, но присвоено значение типа string"
    );
    assert_eq!(
        type_message(&sga(
            "@LET a: int = 1;\n@LET b: string = \"x\";\n@PRINT(a + b);"
        )),
        "[3:7] ошибка типов: оператор '+' не определён для int и string"
    );
}

#[test]
fn the_position_is_the_one_of_the_marked_node_for_every_table_case() {
    // для нескольких случаев из таблиц выше — полная сверка префикса сквозным путём
    for (template, kind) in [
        ("@LET a = 1;\n@PRINT(«zz»);", "semantic"),
        ("@FN f(a) { 1; }\n@LET r = «f(1, 2)»;", "semantic"),
        ("@LET a = [1];\npush(«a», 2);", "semantic"),
        ("@VAR x: int = 1;\nx = «\"s\"»;", "type"),
        ("@FN f() -> int {\n    «@RETURN;»\n}", "type"),
    ] {
        let (src, expected) = marked(template);
        let message = match kind {
            "semantic" => semantic_message(&src),
            _ => type_message(&src),
        };
        let prefix = format!("[{}:{}] ", expected.line, expected.col);
        assert!(
            message.starts_with(&prefix),
            "{template:?}: сообщение {message:?} не начинается с {prefix:?}"
        );
    }
}

#[test]
fn the_message_text_after_the_prefix_is_the_same_as_before() {
    // префикс — единственное отличие от прежнего сообщения
    let message = semantic_message(&sga("@LET a = 1; @PRINT(b);"));
    assert_eq!(
        message.strip_prefix("[1:18] ").unwrap(),
        "ошибка семантики: неопределённая переменная 'b'"
    );
}

#[test]
fn lexer_and_parser_errors_keep_their_own_format() {
    match run_source(&sga("@LET = 1;")) {
        Err(SgaError::Parse(message)) => {
            assert!(message.starts_with("[1:5] ошибка парсера: "), "{message}")
        }
        other => panic!("ожидалась SgaError::Parse, получено {other:?}"),
    }
    match run_source("`") {
        Err(SgaError::Lex(message)) => assert!(message.starts_with("[1:1] "), "{message}"),
        other => panic!("ожидалась SgaError::Lex, получено {other:?}"),
    }
}

// ── файлы с IMPORT: позиции только у узлов главного файла ─────────────────

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("sga_spans_{}_{}_{}", std::process::id(), name, n));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_error_in_the_main_file_keeps_its_position_when_modules_are_imported() {
    let dir = TempDir::new("main_error");
    dir.write(
        "lib.sga",
        &sga("@FN helper() { @RETURN 1; }\n@LET lib_a = 1;\n"),
    );
    // главный файл: IMPORT на первой строке, ошибка — на третьей; позиция
    // считается по ГЛАВНОМУ файлу, хотя в программе перед ним стоят операторы
    // модуля
    let (main_src, expected) = marked("@IMPORT \"lib.sga\";\n\n@PRINT(«missing»);\n");
    let main = dir.write("main.sga", &main_src);
    match run_source_file(&main) {
        Err(SgaError::Semantic(message)) => assert_eq!(
            message,
            format!(
                "[{}:{}] ошибка семантики: неопределённая переменная 'missing'",
                expected.line, expected.col
            )
        ),
        other => panic!("ожидалась SgaError::Semantic, получено {other:?}"),
    }
}

#[test]
fn a_semantic_error_in_an_imported_module_has_no_position_prefix() {
    let dir = TempDir::new("module_semantic");
    // `zzz` не определена — ошибка в узле из ИМПОРТИРУЕМОГО файла: его позиция
    // относилась бы к другому тексту, поэтому префикса нет
    dir.write("lib.sga", &sga("\n\n\n@PRINT(zzz);\n"));
    let main = dir.write("main.sga", &sga("@IMPORT \"lib.sga\";\n@PRINT(1);\n"));
    match run_source_file(&main) {
        Err(SgaError::Semantic(message)) => {
            assert_eq!(message, "ошибка семантики: неопределённая переменная 'zzz'")
        }
        other => panic!("ожидалась SgaError::Semantic, получено {other:?}"),
    }
}

#[test]
fn a_type_error_in_an_imported_module_has_no_position_prefix() {
    let dir = TempDir::new("module_type");
    dir.write("lib.sga", &sga("\n@LET x: int = \"s\";\n"));
    let main = dir.write("main.sga", &sga("@IMPORT \"lib.sga\";\n"));
    match run_source_file(&main) {
        Err(SgaError::Type(message)) => {
            assert!(message.starts_with("ошибка типов: "), "{message}")
        }
        other => panic!("ожидалась SgaError::Type, получено {other:?}"),
    }
}

#[test]
fn a_plain_file_without_imports_gets_the_prefix_too() {
    let dir = TempDir::new("plain_file");
    let main = dir.write("main.sga", &sga("@LET a = 1;\n@PRINT(b);\n"));
    match run_source_file(&main) {
        Err(SgaError::Semantic(message)) => assert_eq!(
            message,
            "[2:7] ошибка семантики: неопределённая переменная 'b'"
        ),
        other => panic!("ожидалась SgaError::Semantic, получено {other:?}"),
    }
}

// ── настоящий бинарник ────────────────────────────────────────────────────

fn run_binary(path: &std::path::Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_sga"))
        .arg("run")
        .arg(path)
        .output()
        .expect("не удалось запустить sga")
}

#[test]
fn sga_run_prints_the_position_of_a_semantic_error_on_stderr() {
    let dir = TempDir::new("cli_semantic");
    let main = dir.write("main.sga", &sga("@LET a = 1;\n@PRINT(a);\n@PRINT(b);\n"));
    let out = run_binary(&main);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "программа не должна исполняться");
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "[3:7] ошибка семантики: неопределённая переменная 'b'\n"
    );
}

#[test]
fn sga_run_prints_the_position_of_a_type_error_on_stderr() {
    let dir = TempDir::new("cli_type");
    let main = dir.write("main.sga", &sga("@VAR x: int = 1;\nx = \"s\";\n"));
    let out = run_binary(&main);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "[2:5] ошибка типов: невозможно присвоить значение типа string переменной 'x' (объявлена как int)\n"
    );
}

#[test]
fn sga_run_prints_no_position_for_an_error_in_an_imported_file() {
    let dir = TempDir::new("cli_import");
    dir.write("lib.sga", &sga("@PRINT(zzz);\n"));
    let main = dir.write("main.sga", &sga("@IMPORT \"lib.sga\";\n@PRINT(1);\n"));
    let out = run_binary(&main);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "ошибка семантики: неопределённая переменная 'zzz'\n"
    );
}

#[test]
fn sga_run_still_runs_a_correct_program_and_prints_its_output() {
    let dir = TempDir::new("cli_ok");
    let main = dir.write("main.sga", &sga("@LET a = 20;\n@PRINT(a + 22);\n"));
    let out = run_binary(&main);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "42\n");
    assert!(out.stderr.is_empty());
}
