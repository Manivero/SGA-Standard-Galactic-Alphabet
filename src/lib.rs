//! Библиотека компилятора SGA: лексер, парсер, AST, семантический
//! анализатор, проверка типов, кодогенератор и VM, доступные как
//! переиспользуемые модули (используется CLI в `main.rs` и
//! интеграционными тестами в `tests/`).
//!
//! FUSION-ПРИМЕЧАНИЕ: полный конвейер объединяет typechecker (из одной
//! родительской ветки) и модульную систему `IMPORT`/`module_resolver`
//! (из другой) — оба шага присутствуют в `run_resolved_program`. См.
//! MIGRATION_REPORT.md.

pub mod ast;
pub mod codegen;
pub mod lexer;
pub mod module_resolver;
pub mod parser;
pub mod runtime;
pub mod sga_alphabet;
pub mod semantic;
pub mod typechecker;
pub mod vm;

use ast::{Program, Stmt};
use runtime::Value;
use std::path::Path;

#[derive(Debug)]
pub enum SgaError {
    Lex(String),
    Parse(String),
    Import(String),
    Semantic(String),
    Type(String),
    Runtime(String),
}

impl std::fmt::Display for SgaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SgaError::Lex(s)
            | SgaError::Parse(s)
            | SgaError::Import(s)
            | SgaError::Semantic(s)
            | SgaError::Type(s)
            | SgaError::Runtime(s) => write!(f, "{}", s),
        }
    }
}

/// Промежуточная ошибка полного цикла "текст -> AST", сохраняющая
/// различение между ошибкой лексера и ошибкой парсера (в отличие от
/// плоской `String`, которая стирала бы эту информацию для вызывающего
/// кода — `SgaError::Lex` и `SgaError::Parse` остаются разными
/// вариантами публичного enum).
enum ParseFullError {
    Lex(String),
    Parse(String),
}

impl From<ParseFullError> for SgaError {
    fn from(e: ParseFullError) -> Self {
        match e {
            ParseFullError::Lex(s) => SgaError::Lex(s),
            ParseFullError::Parse(s) => SgaError::Parse(s),
        }
    }
}

/// Используется и напрямую (даёт `SgaError` через `?`/`From`), и как
/// основа для `parse_full_flat` — версии с плоским `String`-представлением
/// ошибки, нужной для `parse_fn: &dyn Fn(&str) -> Result<Program, String>`
/// в `module_resolver::resolve_imports`, которому не нужно (и не должно)
/// знать о публичном `SgaError`.
fn parse_full(source: &str) -> Result<Program, ParseFullError> {
    let tokens = lexer::Lexer::new(source).tokenize().map_err(|e| ParseFullError::Lex(e.to_string()))?;
    parser::Parser::new(tokens).parse_program().map_err(|e| ParseFullError::Parse(e.to_string()))
}

fn parse_full_flat(source: &str) -> Result<Program, String> {
    parse_full(source).map_err(|e| match e {
        ParseFullError::Lex(s) | ParseFullError::Parse(s) => s,
    })
}

/// Возвращает `Err` при первом `Stmt::Import`, найденном на верхнем
/// уровне программы (рекурсивно внутрь блоков спускаться не нужно —
/// парсер гарантирует, что `IMPORT`, как и `FN`, синтаксически возможен
/// только на верхнем уровне). Используется `run_source`, у которой нет
/// файлового контекста (нет директории, относительно которой резолвить
/// путь импорта) — поэтому `IMPORT` в чистом `run_source(text)` это
/// явная, понятная ошибка, а не нерассмотренный вариант где-то ниже по
/// пайплайну.
fn reject_imports(program: &Program) -> Result<(), SgaError> {
    for stmt in program {
        if let Stmt::Import(path) = stmt {
            return Err(SgaError::Import(format!(
                "IMPORT \"{}\" использован в run_source(), у которого нет файлового контекста для резолвинга путей; используйте run_source_file()",
                path
            )));
        }
    }
    Ok(())
}

/// Полный конвейер для исходного текста БЕЗ файлового контекста:
/// исходный текст -> токены -> AST -> (явный отказ при наличии IMPORT,
/// см. `reject_imports`) -> семантическая проверка -> проверка типов
/// (градуальная, см. `typechecker`) -> байткод -> исполнение на VM.
/// Возвращает значение, которым завершилось выполнение главного блока
/// (как правило `Value::Nil`).
///
/// Для программ с `IMPORT` используйте `run_source_file`, у которой
/// есть путь к файлу на диске и, соответственно, базовая директория,
/// относительно которой резолвятся относительные пути импортов.
pub fn run_source(source: &str) -> Result<Value, SgaError> {
    let program = parse_full(source)?;
    reject_imports(&program)?;
    run_resolved_program(program)
}

/// Полный конвейер для файла на диске, с поддержкой `IMPORT`. Импорты
/// резолвятся рекурсивно относительно директории каждого файла (см.
/// `module_resolver` для полного описания семантики и ограничений).
pub fn run_source_file(path: &Path) -> Result<Value, SgaError> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| SgaError::Import(format!("не удалось прочитать '{}': {}", path.display(), e)))?;
    let program = parse_full(&source)?;
    let resolved = module_resolver::resolve_imports(program, path, &module_resolver::FsLoader, &parse_full_flat)
        .map_err(|e| SgaError::Import(e.0))?;
    run_resolved_program(resolved)
}

/// Общий хвост пайплайна после того, как `Program` гарантированно не
/// содержит `Stmt::Import` (либо отклонён в `run_source`, либо
/// полностью резолвлен в `run_source_file`): семантический анализ
/// (включая Ownership/Borrowing) -> проверка типов (градуальная) ->
/// компиляция в байткод -> верификация байткода (`vm::Vm::new`) ->
/// исполнение.
fn run_resolved_program(program: Program) -> Result<Value, SgaError> {
    semantic::Analyzer::new().analyze(&program).map_err(|e| SgaError::Semantic(e.to_string()))?;
    typechecker::Typechecker::new().analyze(&program).map_err(|e| SgaError::Type(e.to_string()))?;
    let compiled = codegen::compile(&program);
    let (mut machine, main_chunk) = vm::Vm::new(compiled).map_err(|e| SgaError::Runtime(e.to_string()))?;
    machine.run(&main_chunk).map_err(|e| SgaError::Runtime(e.to_string()))
}
