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
mod clock;
pub mod codegen;
mod fs_sandbox;
pub mod json;
pub mod lexer;
pub mod manifest;
pub mod module_resolver;
pub mod optimizer;
pub mod parser;
pub mod runtime;
pub mod semantic;
pub mod sga_alphabet;
mod stdin;
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
    let tokens = lexer::Lexer::new(source)
        .tokenize()
        .map_err(|e| ParseFullError::Lex(e.to_string()))?;
    parser::Parser::new(tokens)
        .parse_program()
        .map_err(|e| ParseFullError::Parse(e.to_string()))
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

/// Откуда программа получает стандартный ввод (`read_line()` /
/// `read_stdin()`, T013, M006). Тип приватный: наружу видны только
/// builder-методы `RunOptions::with_stdin_bytes` / `with_process_stdin`.
#[derive(Debug, Clone, Default)]
enum StdinSource {
    /// Пустой ввод: `read_line()` сразу даёт `nil`, `read_stdin()` — `""`.
    /// Значение по умолчанию — библиотечные точки входа не читают stdin
    /// процесса, пока их об этом явно не попросили.
    #[default]
    Empty,
    /// Заданные байты (детерминированные тесты, встраивание без процесса).
    Bytes(Vec<u8>),
    /// stdin текущего процесса — так программу запускает CLI.
    Process,
}

/// Параметры запуска программы, не относящиеся к её исходному тексту
/// (M006): всё, что окружение передаёт программе «снаружи». Для запуска
/// с параметрами — `run_source_with_options` / `run_source_file_with_options`;
/// `run_source` / `run_source_file` запускают с параметрами по умолчанию
/// (без аргументов, с пустым вводом, без разрешения записи).
///
/// Строится цепочкой builder-методов (`RunOptions::new().with_args(..)`).
/// Поля приватны намеренно: новые параметры (stdin — T013, разрешение
/// записи — T015) добавляются без поломки существующих вызывающих.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    args: Vec<String>,
    stdin: StdinSource,
    allow_write: bool,
}

impl RunOptions {
    /// Параметры по умолчанию: программа запускается без аргументов, с
    /// пустым стандартным вводом и БЕЗ разрешения записи файлов.
    pub fn new() -> Self {
        Self::default()
    }

    /// Аргументы командной строки программы (T012, M006) — то, что вернёт
    /// builtin `args()`, в том же порядке. Путь к самой программе сюда НЕ
    /// входит. Повторный вызов заменяет ранее заданные аргументы.
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Задаёт стандартный ввод программы (T013, M006) — байты, которые
    /// увидят `read_line()` и `read_stdin()`. Не обязаны быть валидным
    /// UTF-8: невалидный ввод даёт `RuntimeError` при чтении, а не при
    /// запуске. Каждый запуск читает заданные байты С НАЧАЛА, поэтому одни и
    /// те же `RunOptions` (или их клон) можно переиспользовать для
    /// нескольких запусков. Повторный вызов этого метода или
    /// `with_process_stdin` заменяет ранее заданный источник ввода.
    pub fn with_stdin_bytes(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = StdinSource::Bytes(bytes.into());
        self
    }

    /// Подключает программе стандартный ввод ТЕКУЩЕГО ПРОЦЕССА (T013,
    /// M006) — так запускает программы CLI (`sga run`). Чтение ленивое:
    /// stdin не трогается, пока программа сама не вызовет `read_line()` или
    /// `read_stdin()`. На время запуска блокируется общий буфер stdin
    /// процесса (`io::stdin().lock()`), поэтому два запуска с этим режимом в
    /// разных потоках одного процесса выполняются по очереди. Повторный
    /// вызов этого метода или `with_stdin_bytes` заменяет ранее заданный
    /// источник ввода.
    pub fn with_process_stdin(mut self) -> Self {
        self.stdin = StdinSource::Process;
        self
    }

    /// Разрешает программе запись файлов — builtin `write_file` (T015,
    /// M006). По умолчанию запись ЗАПРЕЩЕНА: без этого разрешения
    /// `write_file` завершается `RuntimeError` с подсказкой. CLI выдаёт его
    /// флагом `sga run --allow-write <файл>`; других источников разрешения
    /// нет — ни манифест `sga.toml`, ни сама программа, ни импортированные
    /// модули выдать или расширить его не могут (импорты и пакеты входят в
    /// программу целиком, так что разрешение распространяется на них).
    ///
    /// Разрешение даёт запись ТОЛЬКО внутри каталога входного файла
    /// (`run_source_file*`; тот же корень, что у `IMPORT` и `read_file`);
    /// у `run_source*` без файла корня нет, и запись невозможна даже с
    /// разрешением. Права распространяются на ЛЮБОЙ файл каталога программы,
    /// включая её собственный исходник и `sga.toml`. Повторный вызов заменяет
    /// ранее заданное значение.
    pub fn with_allow_write(mut self, allow: bool) -> Self {
        self.allow_write = allow;
        self
    }
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
///
/// Запускает программу с параметрами по умолчанию (`RunOptions::new()`:
/// без аргументов — `args()` вернёт пустой массив —, с пустым вводом —
/// `read_line()` сразу даст `nil` — и без разрешения записи); для передачи
/// параметров — `run_source_with_options`.
pub fn run_source(source: &str) -> Result<Value, SgaError> {
    run_source_with_options(source, RunOptions::default())
}

/// То же, что `run_source`, но с явными параметрами запуска (T012, M006).
pub fn run_source_with_options(source: &str, options: RunOptions) -> Result<Value, SgaError> {
    let program = parse_full(source)?;
    reject_imports(&program)?;
    // T009 (M003): нет файлового пути — нет confinement-границы для
    // read_file. `None` даёт понятную RuntimeError при попытке вызова
    // (см. vm::Vm::call_read_file), а не тихо угадывает cwd процесса.
    run_resolved_program(program, None, options)
}

/// Полный конвейер для файла на диске, с поддержкой `IMPORT`. Импорты
/// резолвятся рекурсивно относительно директории каждого файла (см.
/// `module_resolver` для полного описания семантики и ограничений).
///
/// Запускает программу с параметрами по умолчанию (без аргументов, с пустым
/// вводом, без разрешения записи); для передачи параметров —
/// `run_source_file_with_options`.
pub fn run_source_file(path: &Path) -> Result<Value, SgaError> {
    run_source_file_with_options(path, RunOptions::default())
}

/// То же, что `run_source_file`, но с явными параметрами запуска (T012,
/// M006).
pub fn run_source_file_with_options(path: &Path, options: RunOptions) -> Result<Value, SgaError> {
    let source = std::fs::read_to_string(path).map_err(|e| {
        SgaError::Import(format!("не удалось прочитать '{}': {}", path.display(), e))
    })?;
    let program = parse_full(&source)?;
    let resolved = module_resolver::resolve_imports(
        program,
        path,
        &module_resolver::FsLoader,
        &parse_full_flat,
    )
    .map_err(|e| SgaError::Import(e.0))?;
    // T009 (M003): та же confinement-граница, что уже вычислена внутри
    // resolve_imports для IMPORT — единый источник истины, см.
    // module_resolver::compute_confinement_root.
    let sandbox_root = Some(module_resolver::compute_confinement_root(path));
    run_resolved_program(resolved, sandbox_root, options)
}

/// Общий хвост пайплайна после того, как `Program` гарантированно не
/// содержит `Stmt::Import` (либо отклонён в `run_source`, либо
/// полностью резолвлен в `run_source_file`): семантический анализ
/// (включая Ownership/Borrowing) -> проверка типов (градуальная) ->
/// компиляция в байткод -> верификация байткода (`vm::Vm::new`) ->
/// исполнение.
///
/// `sandbox_root` (T009, M003) — confinement-граница для `read_file`;
/// `None` от `run_source`, `Some(..)` от `run_source_file`.
/// `options` (M006) — параметры запуска, которые передаются в VM между
/// `Vm::new` и `run`.
fn run_resolved_program(
    program: Program,
    sandbox_root: Option<std::path::PathBuf>,
    options: RunOptions,
) -> Result<Value, SgaError> {
    semantic::Analyzer::new()
        .analyze(&program)
        .map_err(|e| SgaError::Semantic(e.to_string()))?;
    typechecker::Typechecker::new()
        .analyze(&program)
        .map_err(|e| SgaError::Type(e.to_string()))?;
    // T010 (M004): AST->AST проход между typechecker и codegen —
    // семантически прозрачен (см. src/optimizer.rs), работает только
    // над уже ВАЛИДИРОВАННЫМ AST (недостижимый код по-прежнему был
    // проверен типами/семантикой ДО этой строки, до своего удаления).
    let program = optimizer::optimize(program);
    let compiled = codegen::compile(&program);
    let (mut machine, main_chunk) =
        vm::Vm::new(compiled, sandbox_root).map_err(|e| SgaError::Runtime(e.to_string()))?;
    machine.set_args(options.args);
    machine.set_allow_write(options.allow_write);
    match options.stdin {
        StdinSource::Empty => machine.set_stdin(std::io::empty()),
        StdinSource::Bytes(bytes) => machine.set_stdin(std::io::Cursor::new(bytes)),
        StdinSource::Process => machine.set_stdin(std::io::stdin().lock()),
    }
    machine
        .run(&main_chunk)
        .map_err(|e| SgaError::Runtime(e.to_string()))
}
