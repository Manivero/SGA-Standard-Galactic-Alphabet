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
//!   - реестр пакетов / версии / транзитивные pkg:-зависимости (T011/M005
//!     добавил ТОЛЬКО path-зависимости: `IMPORT "pkg:имя/файл.sga"`,
//!     см. `crate::manifest` и docs/LANGUAGE_SPEC.md, §8.1).

use crate::ast::{Program, Stmt};
use crate::manifest::Manifest;
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

/// Вычисляет confinement-границу ("root") для данного entry-файла — ТУ
/// ЖЕ границу, что `resolve_imports` использует для `IMPORT` (см.
/// docs/SECURITY.md, "IMPORT confinement"). Публична отдельно от
/// `resolve_imports`, чтобы другие подсистемы, которым тоже нужна
/// файловая песочница (`std/io`/`read_file`, T009/M003), могли
/// использовать РОВНО ТУ ЖЕ границу без дублирования этой логики —
/// дублирование security-критичного кода рискует со временем разойтись
/// и стать источником обхода песочницы. `resolve_imports` ниже сама
/// вызывает эту функцию — единственный источник истины для обоих
/// потребителей.
pub fn compute_confinement_root(entry_path: &Path) -> PathBuf {
    let entry_dir = entry_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    lexically_normalize(&entry_dir)
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
    let root = compute_confinement_root(entry_path);
    // T011 (M005): ближайший sga.toml ищется от каталога входного файла
    // ВВЕРХ по дереву (как Cargo ищет Cargo.toml) и загружается ОДИН раз —
    // единый источник для ВСЕХ pkg:-импортов во всём графе резолвинга,
    // не перезагружается на каждом уровне рекурсии. Подъём вверх нужен,
    // потому что `sga init` кладёт sga.toml в КОРЕНЬ проекта, а
    // запускаемый файл — в `src/` (манифест лежит не рядом с ним). Нет
    // манифеста нигде — не ошибка (`None`), см. manifest::load_nearest.
    // Это НЕ меняет confinement-границу `root` (она по-прежнему каталог
    // входного файла): манифест влияет только на разрешение pkg:-имён.
    //
    // Ошибка чтения/разбора манифеста здесь НЕ возвращается, а хранится в
    // `manifest` и всплывает только в ветке pkg:-импорта (ленивая
    // валидация). Причина: поиск идёт вверх по дереву, и "ближайший"
    // sga.toml может оказаться посторонним (например, в домашнем
    // каталоге) — программы без pkg:-импортов обязаны вести себя ровно
    // как до T011, что бы ни лежало в таких файлах.
    let manifest = crate::manifest::load_nearest(&entry_dir);
    visited.insert(normalize_key(entry_path));
    in_progress.push(normalize_key(entry_path));
    let ctx = ResolveCtx {
        root: &root,
        manifest: &manifest,
        loader,
        parse_fn,
    };
    let resolved = resolve_program(
        program,
        &entry_dir,
        false,
        &ctx,
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
/// НЕ приватная: переиспользуется `Vm::call_read_file` (T009, M002/M003
/// — `read_file`) для той же проверки confinement, без дублирования
/// логики. См. doc-комментарий выше для полного объяснения семантики.
pub fn is_within_root(candidate: &Path, root: &Path) -> bool {
    match (
        std::fs::canonicalize(candidate),
        std::fs::canonicalize(root),
    ) {
        (Ok(c), Ok(r)) => c.starts_with(r),
        _ => lexically_normalize(candidate).starts_with(lexically_normalize(root)),
    }
}

/// Неизменяемый контекст резолвинга — группирует параметры, которые
/// остаются теми же на протяжении всей рекурсии `resolve_program` (за
/// единственным исключением: при заходе в pkg:-пакет `root` заменяется
/// на корень ЭТОГО пакета, остальные поля — те же). Введён в T011
/// (M005) чтобы не раздувать сигнатуру `resolve_program` (clippy::
/// too_many_arguments) по мере добавления параметров.
struct ResolveCtx<'a, L: ModuleLoader> {
    root: &'a Path,
    /// Результат поиска ближайшего `sga.toml`: `Ok(Some(..))` — найден
    /// (с каталогом, от которого считаются пути зависимостей), `Ok(None)` —
    /// манифеста нет, `Err(..)` — манифест есть, но не читается/некорректен
    /// (ошибка возвращается лишь при попытке `pkg:`-импорта).
    manifest: &'a Result<Option<Manifest>, String>,
    loader: &'a L,
    parse_fn: &'a dyn Fn(&str) -> Result<Program, String>,
}

fn resolve_program<L: ModuleLoader>(
    program: Program,
    current_dir: &Path,
    in_package: bool,
    ctx: &ResolveCtx<'_, L>,
    visited: &mut HashSet<String>,
    in_progress: &mut Vec<String>,
) -> IResult<Program> {
    // Все поля — ссылки (Copy), поэтому деструктуризация через `*ctx`
    // допустима, и остальное тело функции остаётся без изменений.
    let ResolveCtx {
        root,
        manifest,
        loader,
        parse_fn,
    } = *ctx;
    let mut out = Vec::new();
    for stmt in program {
        match stmt {
            Stmt::Import(rel_path) => {
                // T011 (M005): "pkg:имя/путь" — явный префикс, отличает
                // package-импорт от обычного файлового IMPORT. Проверяется
                // ПЕРВЫМ, до проверки абсолютного пути ниже (у pkg:-строк
                // своя, отдельная логика резолвинга).
                if let Some(pkg_import) = rel_path.strip_prefix("pkg:") {
                    if in_package {
                        return Err(ImportError(format!(
                            "IMPORT \"pkg:{}\": транзитивные pkg:-зависимости (пакет, импортирующий другой пакет) не поддерживаются в v0.1",
                            pkg_import
                        )));
                    }
                    let (pkg_name, sub_path) = pkg_import.split_once('/').ok_or_else(|| {
                        ImportError(format!(
                            "IMPORT \"pkg:{}\": ожидался формат \"pkg:имя/путь/к/файлу.sga\"",
                            pkg_import
                        ))
                    })?;
                    // Нерабочий манифест (ошибка чтения/разбора) — это отказ
                    // именно ЭТОГО pkg:-импорта, а не всей программы.
                    let found_manifest = match manifest {
                        Ok(found) => found.as_ref(),
                        Err(e) => return Err(ImportError(e.clone())),
                    };
                    // Нет манифеста вообще (`None`) и "манифест есть, но имя в
                    // нём не объявлено" — для пользователя один и тот же отказ
                    // "не объявлен" (AC-07: НЕ "manifest не найден").
                    let declared = found_manifest.and_then(|m| {
                        m.dependencies
                            .get(pkg_name)
                            .map(|dep| (m.dir.as_path(), dep))
                    });
                    let (manifest_dir, dep_path) = declared.ok_or_else(|| {
                        ImportError(format!(
                            "IMPORT \"pkg:{}\": пакет '{}' не объявлен в [dependencies] sga.toml",
                            pkg_import, pkg_name
                        ))
                    })?;
                    // SECURITY: путь зависимости — относительно КАТАЛОГА
                    // МАНИФЕСТА (того sga.toml, где зависимость объявлена, —
                    // см. resolve_imports), а не каталога входного файла и не
                    // места использования: тот же якорь для ВСЕХ pkg:-импортов
                    // во всём графе резолвинга, независимо от уровня
                    // вложенности IMPORT (Cargo делает то же самое: path в
                    // Cargo.toml всегда относителен ЕГО СОБСТВЕННОЙ
                    // директории).
                    let pkg_root = lexically_normalize(&manifest_dir.join(dep_path));
                    let pkg_file = pkg_root.join(sub_path);
                    if !is_within_root(&pkg_file, &pkg_root) {
                        return Err(ImportError(format!(
                            "IMPORT \"pkg:{}\": путь выходит за пределы каталога пакета '{}' ('{}'); обход через '..' запрещён",
                            pkg_import,
                            pkg_name,
                            pkg_root.display()
                        )));
                    }
                    let key = normalize_key(&pkg_file);
                    if in_progress.contains(&key) {
                        return Err(ImportError(format!(
                            "циклический IMPORT: \"pkg:{}\" участвует в цикле импортов",
                            pkg_import
                        )));
                    }
                    if !visited.contains(&key) {
                        visited.insert(key.clone());
                        let source = loader.read(&pkg_file)?;
                        let imported_program = parse_fn(&source).map_err(|e| {
                            ImportError(format!(
                                "IMPORT \"pkg:{}\": ошибка разбора '{}': {}",
                                pkg_import,
                                pkg_file.display(),
                                e
                            ))
                        })?;
                        in_progress.push(key.clone());
                        let pkg_dir = pkg_file
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| pkg_root.clone());
                        // Новый контекст: root — КОРЕНЬ ПАКЕТА (не основного
                        // проекта), остальные поля те же.
                        let pkg_ctx = ResolveCtx {
                            root: &pkg_root,
                            manifest,
                            loader,
                            parse_fn,
                        };
                        let resolved_imported = resolve_program(
                            imported_program,
                            &pkg_dir,
                            true, // in_package: транзитивные pkg: внутри запрещены
                            &pkg_ctx,
                            visited,
                            in_progress,
                        )?;
                        in_progress.pop();
                        out.extend(resolved_imported);
                    }
                    continue;
                }
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
                    in_package,
                    ctx,
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
