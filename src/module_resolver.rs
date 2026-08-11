//! Резолвер `IMPORT` — статическое разрешение модулей SGA v0.1.
//!
//! ЧЕСТНОЕ ОПИСАНИЕ ОГРАНИЧЕНИЙ (намеренно, не скрыто): это НЕ
//! полноценная модульная система с пространствами имён, приватностью
//! экспортов или раздельной компиляцией. Это простейший рабочий
//! механизм первого уровня — **textual inlining** (концептуально близко
//! к C `#include`, но с защитой от циклов и повторных импортов одного
//! файла, чего у `#include` из коробки нет):
//!
//!   - `IMPORT "путь.sga";` читает указанный файл, токенизирует и
//!     парсит его как самостоятельную программу, затем **рекурсивно
//!     резолвит** его собственные `IMPORT`-ы, и наконец инлайнит его
//!     top-level `Stmt`-ы (кроме самого `Import`) в начало текущей
//!     программы — в порядке появления `IMPORT`-ов.
//!   - Все имена функций/переменных верхнего уровня всех
//!     импортированных файлов попадают в ОДНО общее пространство имён с
//!     текущим файлом. Конфликт имён (две функции с одинаковым именем
//!     в разных файлах) обнаруживается обычным семантическим анализом
//!     (`Analyzer::analyze`) ПОСЛЕ резолвинга — `module_resolver` сам
//!     не проверяет конфликты имён между модулями, только корректность
//!     графа импортов.
//!   - Один и тот же файл, импортированный дважды (прямо или
//!     транзитивно через разные пути), инлайнится **один раз**
//!     (`#pragma once`-семантика, не C-style многократное копирование).
//!   - Циклический импорт (`a.sga` импортирует `b.sga`, который
//!     импортирует `a.sga`) — явная `ImportError`, а не stack overflow
//!     от бесконечной рекурсии резолвера.
//!   - Путь в `IMPORT "..."` всегда разрешается относительно директории
//!     файла, в котором записан этот `IMPORT` (не относительно cwd
//!     процесса) — иначе поведение зависело бы от того, откуда запущен
//!     `sga run`, что является источником трудноуловимых багов в
//!     реальных компиляторах с относительными путями.
//!
//! НЕ реализовано (см. docs/ROADMAP.md, раздел Modules):
//!   - именованные модули/`USE module::item` синтаксис;
//!   - приватность (`pub`/неэкспортируемые элементы);
//!   - переименование при импорте (`IMPORT "x.sga" AS y`);
//!   - разрешение пакетов по имени через package manager (которого нет).

use crate::ast::{Program, Stmt};
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ImportError(pub String);

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ошибка импорта: {}", self.0)
    }
}

type IResult<T> = Result<T, ImportError>;

/// Абстракция источника файлов модулей. В production-пути (`run_source_file`
/// в `lib.rs`/`main.rs`) реализована поверх `std::fs`. В тестах
/// подставляется in-memory реализация (`tests/integration_test.rs`),
/// чтобы тестировать граф импортов/защиту от циклов без создания
/// реальных файлов на диске — что особенно важно в этом окружении
/// аудита, где `cargo test` не может быть запущен вживую и каждая
/// строка теста должна быть как можно более самодостаточной и
/// детерминированной.
pub trait ModuleLoader {
    /// Читает содержимое файла по канонизированному пути. Реализация
    /// сама решает, что считать "канонизацией" (для `fs`-реализации —
    /// `std::fs::canonicalize`; для in-memory — обычно тождественная
    /// нормализация строки пути).
    fn read(&self, path: &Path) -> IResult<String>;
}

/// `ModuleLoader` поверх реальной файловой системы.
pub struct FsLoader;

impl ModuleLoader for FsLoader {
    fn read(&self, path: &Path) -> IResult<String> {
        std::fs::read_to_string(path)
            .map_err(|e| ImportError(format!("не удалось прочитать '{}': {}", path.display(), e)))
    }
}

/// Резолвит все `IMPORT` в `program`, который был распарсен из файла
/// `entry_path`. Возвращает плоский `Program` без единого `Stmt::Import`
/// — готовый к передаче в `semantic::Analyzer::analyze` как обычно.
///
/// `parse_fn` — функция полного цикла "текст -> Program" (лексер +
/// парсер), передаётся снаружи, чтобы `module_resolver` не имел
/// прямой зависимости от конкретных типов ошибок `lexer`/`parser`
/// (избегаем циклической связи модулей и держим `module_resolver`
/// маленьким и независимым).
pub fn resolve_imports<L: ModuleLoader>(
    program: Program,
    entry_path: &Path,
    loader: &L,
    parse_fn: &dyn Fn(&str) -> Result<Program, String>,
) -> IResult<Program> {
    let mut visited = HashSet::new();
    let mut in_progress = Vec::new(); // стек текущей цепочки импортов — для сообщения о цикле
    let entry_dir = entry_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // SECURITY (см. docs/SECURITY.md, "IMPORT confinement"): `entry_dir`
    // — это и есть граница песочницы для ВСЕГО графа импортов, включая
    // транзитивные. Она вычисляется один раз здесь и передаётся вниз по
    // рекурсии неизменной (`root`), в отличие от `current_dir`, который
    // меняется на каждом уровне вложенности для резолвинга
    // относительных путей. Если бы каждый уровень вычислял свою
    // собственную границу из своего `current_dir`, `..` мог бы "сбежать"
    // выше предыдущей границы на каждом новом уровне импорта.
    let root = lexically_normalize(&entry_dir);
    visited.insert(normalize_key(entry_path));
    in_progress.push(normalize_key(entry_path));
    let resolved = resolve_program(
        program,
        &entry_dir,
        &root,
        loader,
        parse_fn,
        &mut visited,
        &mut in_progress,
    )?;
    in_progress.pop();
    Ok(resolved)
}

fn normalize_key(path: &Path) -> String {
    // Лучшая по усилиям канонизация: пытаемся через std::fs::canonicalize
    // (резолвит симлинки и `..`), иначе используем путь как есть.
    // Для in-memory loader'а (тесты) canonicalize обычно проваливается
    // (файла физически нет), и мы откатываемся на строковое сравнение
    // пути "как написан" — это осознанное упрощение: in-memory-тесты
    // обязаны использовать согласованные ключи путей, а не полагаться
    // на canonicalize.
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// Чисто лексическая нормализация пути: резолвит `.` и `..` через
/// компоненты пути, БЕЗ обращения к файловой системе (в отличие от
/// `std::fs::canonicalize`, которому нужно, чтобы путь физически
/// существовал, и который не работает для in-memory `ModuleLoader` в
/// тестах). Если `..` пытается "подняться" выше уже нормализованной
/// части пути (нет предыдущего `Normal`-компонента, который можно
/// снять), он сохраняется буквально, а не отбрасывается молча — это
/// важно для проверки confinement ниже: путь типа `../../secret.sga`,
/// у которого после нормализации остаются нескомпенсированные `..`,
/// никогда не будет иметь префиксом нормализованный `root` (у которого
/// таких `..` нет), и `starts_with` корректно провалит проверку, а не
/// тихо "проглотит" попытку выхода.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(".."),
            },
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Проверяет, что `candidate` (уже объединённый с `current_dir`, но
/// ещё не прочитанный с диска) остаётся внутри `root` — каталога
/// входного файла программы. Это единственная граница доверия для
/// `IMPORT`: всё, что внутри неё, резолвится свободно (в том числе
/// через `..`, если он не выводит за пределы `root` — например,
/// `project/a/b.sga`, импортирующий `../c.sga`, остаётся внутри
/// `project/`), а всё, что снаружи — отклоняется до попытки чтения
/// файла.
///
/// На реальной ФС (`FsLoader`) предпочитается `std::fs::canonicalize`
/// (резолвит symlink'и — лексическая проверка сама по себе не защищает
/// от symlink, указывающего наружу `root`); если канонизация
/// недоступна (путь ещё не существует на диске — в частности, ВСЕГДА
/// верно для in-memory `ModuleLoader` в тестах), используется
/// лексический fallback.
fn is_within_root(candidate: &Path, root: &Path) -> bool {
    match (
        std::fs::canonicalize(candidate),
        std::fs::canonicalize(root),
    ) {
        (Ok(c), Ok(r)) => c.starts_with(r),
        _ => lexically_normalize(candidate).starts_with(lexically_normalize(root)),
    }
}

fn resolve_program<L: ModuleLoader>(
    program: Program,
    current_dir: &Path,
    root: &Path,
    loader: &L,
    parse_fn: &dyn Fn(&str) -> Result<Program, String>,
    visited: &mut HashSet<String>,
    in_progress: &mut Vec<String>,
) -> IResult<Program> {
    let mut out = Vec::new();
    for stmt in program {
        match stmt {
            Stmt::Import(rel_path) => {
                // SECURITY: абсолютные пути в IMPORT запрещены безусловно.
                // `Path::join` в Rust, если аргумент абсолютный, ПОЛНОСТЬЮ
                // заменяет базовый путь (это документированное поведение
                // std, не баг) — без этой проверки `IMPORT "/etc/passwd";`
                // игнорировал бы `current_dir` целиком и читал бы
                // произвольный файл с прав процесса. Проверяется ДО
                // `.join`, чтобы не зависеть от этого нюанса `Path::join`.
                if Path::new(&rel_path).is_absolute() {
                    return Err(ImportError(format!(
                        "IMPORT с абсолютным путём запрещён: '{}'. Пути импорта обязаны быть относительными и не выходить за пределы каталога входного файла программы",
                        rel_path
                    )));
                }

                let full_path = current_dir.join(&rel_path);

                // SECURITY: то же самое для `..`-побега за пределы
                // каталога входного файла (path traversal) — см.
                // `is_within_root` и docs/SECURITY.md.
                if !is_within_root(&full_path, root) {
                    return Err(ImportError(format!(
                        "IMPORT \"{}\" выходит за пределы каталога входного файла программы ('{}'); обход через '..' запрещён",
                        rel_path,
                        root.display()
                    )));
                }

                let key = normalize_key(&full_path);

                if in_progress.contains(&key) {
                    let mut chain = in_progress.clone();
                    chain.push(key.clone());
                    return Err(ImportError(format!(
                        "циклический IMPORT обнаружен: {}",
                        chain.join(" -> ")
                    )));
                }
                if visited.contains(&key) {
                    // Уже импортирован где-то в графе раньше — пропускаем
                    // молча (#pragma once semantics), это НЕ ошибка.
                    continue;
                }
                visited.insert(key.clone());
                in_progress.push(key.clone());

                let source = loader.read(&full_path)?;
                let imported_program = parse_fn(&source).map_err(|e| {
                    ImportError(format!(
                        "ошибка разбора импортируемого файла '{}': {}",
                        full_path.display(),
                        e
                    ))
                })?;
                let imported_dir = full_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                let resolved_imported = resolve_program(
                    imported_program,
                    &imported_dir,
                    root,
                    loader,
                    parse_fn,
                    visited,
                    in_progress,
                )?;

                in_progress.pop();
                out.extend(resolved_imported);
            }
            other => out.push(other),
        }
    }
    Ok(out)
}
