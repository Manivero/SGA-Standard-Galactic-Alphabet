//! Минимальный ручной парсер `sga.toml` — ТОЛЬКО секция `[dependencies]`
//! (T011, M005). НЕ полный TOML-парсер: остальные секции (`[package]`,
//! `[targets]`, `[compiler]`) пропускаются, их содержимое не проверяется
//! и не используется — они существуют для человека-читателя манифеста,
//! не для кода (см. `src/main.rs::cmd_init`). Без внешней зависимости —
//! тот же принцип, что и у `src/json.rs`: единственный формат записи,
//! который реально нужен (`name = { path = "..." }`), проще разобрать
//! вручную, чем подключать `toml`-крейт ради него одного.
//!
//! ПОИСК МАНИФЕСТА (дополнено при сквозной проверке T011): `sga.toml`
//! ищется не только в каталоге входного файла, но и ВЫШЕ по дереву —
//! ближайший (nearest wins) — как Cargo ищет `Cargo.toml`. Иначе
//! структура, которую сам создаёт `sga init` (`sga.toml` в корне проекта,
//! `src/main.sga` — уровнем ниже), не работала бы с path-зависимостями:
//! манифест лежал бы НЕ рядом с запускаемым файлом. Пути зависимостей
//! (`path = "..."`) считаются от каталога НАЙДЕННОГО манифеста, а не от
//! каталога входного файла.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Найденный и разобранный `sga.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Каталог, в котором лежит найденный `sga.toml` (абсолютный,
    /// канонизированный). Относительно него разрешаются `path = "..."`
    /// всех зависимостей (Cargo-style).
    pub dir: PathBuf,
    /// Содержимое секции `[dependencies]`: имя пакета -> путь (как записан).
    pub dependencies: HashMap<String, PathBuf>,
}

/// Находит каталог проекта — БЛИЖАЙШИЙ каталог, содержащий `sga.toml`:
/// сам `start_dir` либо один из его предков. `None`, если манифеста нет
/// ни в одном из них (или `start_dir` не существует на диске — например,
/// in-memory `ModuleLoader` в тестах резолвера).
///
/// Подъём делается по КАНОНИЗИРОВАННОМУ (абсолютному) пути — иначе для
/// относительных входов (`sga run main.sga` из `proj/src`, где каталог
/// входного файла — пустой путь) подъём упирался бы в начало
/// относительного пути, не доходя до `proj/`.
pub fn find_manifest_dir(start_dir: &Path) -> Option<PathBuf> {
    let start = if start_dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        start_dir
    };
    let abs = std::fs::canonicalize(start).ok()?;
    abs.ancestors()
        .find(|dir| dir.join("sga.toml").is_file())
        .map(Path::to_path_buf)
}

/// Находит ближайший `sga.toml` (см. `find_manifest_dir`) и разбирает его
/// `[dependencies]`. Манифеста нигде нет — `Ok(None)`, НЕ ошибка
/// (обратная совместимость, Feature Contract AC-07). Ошибка — только если
/// манифест ЕСТЬ, но его не удалось прочитать или `[dependencies]`
/// синтаксически некорректна (сообщение содержит путь к манифесту: он
/// может лежать выше каталога запускаемого файла).
pub fn load_nearest(start_dir: &Path) -> Result<Option<Manifest>, String> {
    let Some(dir) = find_manifest_dir(start_dir) else {
        return Ok(None);
    };
    let dependencies = read_and_parse(&dir.join("sga.toml"))?;
    Ok(Some(Manifest { dir, dependencies }))
}

/// Загружает `[dependencies]` из `dir/sga.toml` — ТОЛЬКО из указанного
/// каталога, без подъёма вверх (для поиска вверх см. `load_nearest`). Если файла нет —
/// возвращает ПУСТУЮ карту, НЕ ошибку (обратная совместимость: программы
/// без `pkg:`-импортов не обязаны иметь `sga.toml` вовсе — см. Feature
/// Contract, AC-07). Ошибка возвращается только если файл ЕСТЬ, но
/// `[dependencies]` в нём синтаксически некорректна.
pub fn load_dependencies(dir: &Path) -> Result<HashMap<String, PathBuf>, String> {
    let manifest_path = dir.join("sga.toml");
    if !manifest_path.is_file() {
        return Ok(HashMap::new());
    }
    read_and_parse(&manifest_path)
}

/// Читает файл манифеста и разбирает `[dependencies]`; любая ошибка
/// (чтения или синтаксиса) снабжается путём к файлу.
fn read_and_parse(manifest_path: &Path) -> Result<HashMap<String, PathBuf>, String> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("не удалось прочитать '{}': {}", manifest_path.display(), e))?;
    parse_dependencies(&text).map_err(|e| format!("{} (манифест '{}')", e, manifest_path.display()))
}

/// Разбирает ТОЛЬКО секцию `[dependencies]` из текста манифеста. Каждая
/// строка внутри секции обязана быть вида
/// `имя = { path = "относительный/путь" }` (двойные кавычки, ровно один
/// ключ `path` во внутренней таблице — единственная поддерживаемая форма
/// в v0.1, см. Feature Contract: нет registry, версии без `path`
/// бессмысленны). Пустые строки и строки, начинающиеся с `#`, игнорируются.
fn parse_dependencies(text: &str) -> Result<HashMap<String, PathBuf>, String> {
    let mut deps = HashMap::new();
    let mut in_dependencies = false;
    for (line_no, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if !in_dependencies {
            continue; // строки вне [dependencies] эта функция не разбирает
        }
        let (name, value) = line.split_once('=').ok_or_else(|| {
            format!(
                "sga.toml:{}: ожидалось 'имя = значение' в [dependencies], получено: {}",
                line_no + 1,
                line
            )
        })?;
        let name = name.trim().to_string();
        let value = value.trim();
        if name.is_empty() {
            return Err(format!(
                "sga.toml:{}: пустое имя зависимости слева от '='",
                line_no + 1
            ));
        }
        if deps.contains_key(&name) {
            return Err(format!(
                "sga.toml:{}: зависимость '{}' объявлена повторно",
                line_no + 1,
                name
            ));
        }
        let path = parse_path_dependency_value(value).ok_or_else(|| {
            format!(
                "sga.toml:{}: зависимость '{}' — не path-зависимость ('{}'). \
                 В v0.1 поддерживаются ТОЛЬКО path-зависимости: {} = {{ path = \"...\" }} \
                 (нет package registry — версии без явного пути резолвить неоткуда)",
                line_no + 1,
                name,
                value,
                name
            )
        })?;
        deps.insert(name, path);
    }
    Ok(deps)
}

/// Разбирает правую часть `имя = <значение>` — распознаёт ТОЛЬКО
/// `{ path = "..." }` (в любых пробелах). Всё остальное (голая строка
/// `"1.0.0"`, таблица без `path`, что угодно ещё) — `None`, вызывающий
/// код превращает это в понятную ошибку (см. `parse_dependencies` выше).
fn parse_path_dependency_value(value: &str) -> Option<PathBuf> {
    let inner = value.strip_prefix('{')?.strip_suffix('}')?.trim();
    let (key, val) = inner.split_once('=')?;
    if key.trim() != "path" {
        return None;
    }
    let val = val.trim();
    let path_str = val.strip_prefix('"')?.strip_suffix('"')?;
    if path_str.is_empty() {
        return None;
    }
    Some(PathBuf::from(path_str))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_path_dependency() {
        let toml = "[package]\nname = \"x\"\n\n[dependencies]\nmylib = { path = \"../mylib\" }\n";
        let deps = parse_dependencies(toml).unwrap();
        assert_eq!(deps.get("mylib"), Some(&PathBuf::from("../mylib")));
    }

    #[test]
    fn test_parse_multiple_dependencies_and_ignores_other_sections() {
        let toml = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\
                     [dependencies]\na = { path = \"./a\" }\nb = { path = \"../b\" }\n\
                     [targets]\nplatform = \"linux\"\n";
        let deps = parse_dependencies(toml).unwrap();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps.get("a"), Some(&PathBuf::from("./a")));
        assert_eq!(deps.get("b"), Some(&PathBuf::from("../b")));
    }

    #[test]
    fn test_empty_dependencies_section_is_ok() {
        let toml = "[package]\nname = \"x\"\n\n[dependencies]\n\n[targets]\n";
        let deps = parse_dependencies(toml).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_no_dependencies_section_at_all_is_ok() {
        let toml = "[package]\nname = \"x\"\n";
        let deps = parse_dependencies(toml).unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_bare_version_string_is_rejected_with_clear_error() {
        // AC-04: name = "1.0.0" (без path) — явная ошибка, не паника, не
        // тихая заглушка.
        let toml = "[dependencies]\nmylib = \"1.0.0\"\n";
        let err = parse_dependencies(toml).unwrap_err();
        assert!(err.contains("path-зависимости"), "получено: {err}");
        assert!(err.contains("mylib"), "получено: {err}");
    }

    #[test]
    fn test_duplicate_dependency_name_is_rejected() {
        let toml = "[dependencies]\na = { path = \"./a\" }\na = { path = \"./a2\" }\n";
        let err = parse_dependencies(toml).unwrap_err();
        assert!(err.contains("повторно"), "получено: {err}");
    }

    #[test]
    fn test_malformed_line_is_rejected_not_panicking() {
        let toml = "[dependencies]\nthis is not valid toml at all\n";
        assert!(parse_dependencies(toml).is_err());
    }

    #[test]
    fn test_missing_manifest_file_returns_empty_map_not_error() {
        // AC-07
        let deps = load_dependencies(Path::new(
            "/tmp/sga_manifest_test_definitely_does_not_exist_xyz",
        ));
        assert_eq!(deps.unwrap(), HashMap::new());
    }

    #[test]
    fn test_comments_and_blank_lines_are_ignored() {
        let toml =
            "[dependencies]\n# это комментарий\n\nmylib = { path = \"../mylib\" }  # тоже можно?\n";
        // Примечание: инлайн-комментарий после значения НЕ поддерживается
        // этим минимальным парсером (см. doc-комментарий модуля) — этот
        // тест фиксирует ТЕКУЩЕЕ поведение (ошибка на такой строке), а
        // не удобство. Полноценные comment-парсинг был бы избыточен для
        // единственной поддерживаемой формы записи.
        let result = parse_dependencies(toml);
        assert!(
            result.is_err(),
            "инлайн-комментарий после значения пока не поддерживается — ожидалась ошибка"
        );
    }

    // ---- поиск манифеста вверх по дереву (дополнение T011) ----

    /// Временный каталог с уникальным именем (PID + счётчик) — без внешнего
    /// крейта `tempfile`, как и в интеграционных тестах.
    fn unit_temp_dir(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "sga_manifest_unit_{}_{}_{}",
            std::process::id(),
            name,
            n
        ));
        std::fs::create_dir_all(&dir).expect("не удалось создать временный каталог");
        dir
    }

    #[test]
    fn test_find_manifest_dir_returns_start_dir_when_it_holds_the_manifest() {
        let root = unit_temp_dir("find_self");
        std::fs::write(root.join("sga.toml"), "[dependencies]\n").unwrap();
        let found = find_manifest_dir(&root).unwrap();
        let expected = std::fs::canonicalize(&root).unwrap();
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(found, expected);
    }

    #[test]
    fn test_find_manifest_dir_walks_up_to_ancestor() {
        // Раскладка, которую создаёт `sga init`: sga.toml в корне проекта,
        // исходник — уровнем (или несколькими) ниже.
        let root = unit_temp_dir("find_up");
        std::fs::write(root.join("sga.toml"), "[dependencies]\n").unwrap();
        let deep = root.join("src").join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        let found = find_manifest_dir(&deep).unwrap();
        let expected = std::fs::canonicalize(&root).unwrap();
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(found, expected);
    }

    #[test]
    fn test_find_manifest_dir_nearest_manifest_wins() {
        let root = unit_temp_dir("find_nearest");
        std::fs::write(root.join("sga.toml"), "[dependencies]\n").unwrap();
        let inner = root.join("inner");
        std::fs::create_dir_all(inner.join("src")).unwrap();
        std::fs::write(inner.join("sga.toml"), "[dependencies]\n").unwrap();
        let found = find_manifest_dir(&inner.join("src")).unwrap();
        let expected = std::fs::canonicalize(&inner).unwrap();
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(found, expected);
    }

    #[test]
    fn test_find_manifest_dir_never_returns_dir_inside_a_manifest_free_tree() {
        // Герметично: поддерево `root` манифестов не содержит, поэтому
        // найденное (если что-то найдено) может быть ТОЛЬКО предком `root`
        // — например, случайным /tmp/sga.toml в окружении, — но никак не
        // каталогом внутри `root`.
        let root = unit_temp_dir("find_none");
        let leaf = root.join("a").join("b");
        std::fs::create_dir_all(&leaf).unwrap();
        let canon_root = std::fs::canonicalize(&root).unwrap();
        let found = find_manifest_dir(&leaf);
        std::fs::remove_dir_all(&root).ok();
        if let Some(dir) = found {
            assert!(
                !dir.starts_with(&canon_root),
                "манифест не должен находиться внутри дерева без манифестов: {}",
                dir.display()
            );
        }
    }

    #[test]
    fn test_find_manifest_dir_nonexistent_start_dir_is_none() {
        // Нет такого каталога на диске (как у in-memory ModuleLoader в
        // тестах резолвера) — не паника и не ошибка, просто "не найден".
        let missing = Path::new("/tmp/sga_manifest_unit_definitely_missing_xyz/a/b");
        assert_eq!(find_manifest_dir(missing), None);
    }

    #[test]
    fn test_find_manifest_dir_empty_path_means_current_directory() {
        // `sga run main.sga`: родитель относительного пути — пустой путь,
        // он обязан трактоваться как ".", а не проваливаться в None.
        assert_eq!(
            find_manifest_dir(Path::new("")),
            find_manifest_dir(Path::new("."))
        );
    }

    #[test]
    fn test_load_nearest_returns_manifest_dir_and_dependencies() {
        let root = unit_temp_dir("load_nearest");
        std::fs::write(
            root.join("sga.toml"),
            "[dependencies]\nmylib = { path = \"../mylib\" }\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let manifest = load_nearest(&root.join("src")).unwrap().unwrap();
        let expected_dir = std::fs::canonicalize(&root).unwrap();
        std::fs::remove_dir_all(&root).ok();
        assert_eq!(manifest.dir, expected_dir);
        assert_eq!(
            manifest.dependencies.get("mylib"),
            Some(&PathBuf::from("../mylib"))
        );
    }

    #[test]
    fn test_load_nearest_malformed_manifest_error_names_the_manifest_file() {
        let root = unit_temp_dir("load_bad");
        std::fs::write(root.join("sga.toml"), "[dependencies]\nmylib = \"1.0.0\"\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let err = load_nearest(&root.join("src")).unwrap_err();
        std::fs::remove_dir_all(&root).ok();
        assert!(err.contains("path-зависимости"), "получено: {err}");
        assert!(err.contains("sga.toml"), "получено: {err}");
    }
}
