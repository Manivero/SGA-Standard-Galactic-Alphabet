//! Тесты `sga::module_resolver` — резолвер `IMPORT`.
//!
//! ВАЖНО: SGA распознаёт ключевые слова ТОЛЬКО как Unicode-кодпоинты
//! Private Use Area (зарегистрированный UCSUR-блок U+EB40..U+EB59) — см.
//! `src/sga_alphabet.rs` и `src/lexer/mod.rs::is_sga_letter`.
//! ASCII-текст `import`/`fn`/`let` лексер интерпретирует как обычный
//! идентификатор (`TokenKind::Ident`), а НЕ как ключевое слово. Поэтому,
//! как и во всех остальных тестах проекта (`tests/integration_test.rs`),
//! здесь используется `kw("IMPORT")` = `encode_word("IMPORT")`, дающее
//! строку из реальных SGA-кодпоинтов, которую лексер распознает как
//! `TokenKind::Import`.
//!
//! Используется in-memory `ModuleLoader` (а не реальные файлы на диске),
//! чтобы тесты были самодостаточными, детерминированными и не зависели
//! от файловой системы CI/окружения. См. `module_resolver::ModuleLoader`
//! — реализация ниже (`InMemoryLoader`) подставляется вместо
//! `module_resolver::FsLoader`, используемого в production-пути
//! (`sga::run_source_file`).
//!
//! ОГОВОРКА (см. также комментарии в `src/module_resolver.rs`): эта
//! in-memory реализация не выполняет канонизацию путей (`..`, симлинки)
//! — `normalize_key` в `module_resolver` откатывается на сравнение пути
//! "как написан", если `std::fs::canonicalize` не может найти файл на
//! реальном диске (что верно для всех путей в этих тестах). Поэтому
//! тесты ниже используют только простые относительные пути без `..`,
//! чтобы не зависеть от этого ограничения in-memory тестового пути.
//!
//! Пути импорта (например, `"lib.sga"`) — это СТРОКОВЫЕ ЛИТЕРАЛЫ языка
//! SGA, а не идентификаторы, поэтому они остаются обычным ASCII-текстом
//! в кавычках и не требуют кодирования через `kw()`. Аналогично, имена
//! пользовательских функций (`helper`, `mid_fn`, ...) — это ASCII
//! идентификаторы, тоже не требующие кодирования.

use sga::ast::{Expr, Program, Stmt};
use sga::lexer::Lexer;
use sga::module_resolver::{resolve_imports, ImportError, ModuleLoader};
use sga::parser::Parser;
use sga::sga_alphabet::encode_word;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn kw(word: &str) -> String {
    encode_word(word)
}

/// `ModuleLoader`, читающий из заранее заполненной in-memory карты
/// `путь -> исходный текст`, вместо обращения к реальной файловой
/// системе. Используется только в тестах.
struct InMemoryLoader {
    files: Mutex<HashMap<PathBuf, String>>,
}

impl InMemoryLoader {
    fn new(files: &[(&str, &str)]) -> Self {
        let map = files
            .iter()
            .map(|(p, s)| (PathBuf::from(p), s.to_string()))
            .collect();
        InMemoryLoader {
            files: Mutex::new(map),
        }
    }
}

impl ModuleLoader for InMemoryLoader {
    fn read(&self, path: &Path) -> Result<String, ImportError> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| {
                ImportError(format!(
                    "файл не найден в in-memory loader'е: {}",
                    path.display()
                ))
            })
    }
}

fn parse(source: &str) -> Result<Program, String> {
    let tokens = Lexer::new(source).tokenize().map_err(|e| e.to_string())?;
    Parser::new(tokens)
        .parse_program()
        .map_err(|e| e.to_string())
}

/// Считает количество top-level `Stmt::FnDecl` с данным именем в
/// программе — удобно для проверки, что инлайнинг произошёл ровно
/// один раз (а не 0 и не 2+ раза).
fn count_fn_decls(program: &Program, name: &str) -> usize {
    program
        .iter()
        .filter(|s| matches!(s, Stmt::FnDecl { name: n, .. } if n == name))
        .count()
}

#[test]
fn test_simple_import_inlines_function_from_other_file() {
    let entry_src = format!(
        "{import} \"lib.sga\";\n{pr}(helper());\n",
        import = kw("IMPORT"),
        pr = kw("PRINT")
    );
    let lib_src = format!("{fn} helper() {{ {ret} 42; }}\n", fn = kw("FN"), ret = kw("RETURN"));
    let loader = InMemoryLoader::new(&[("entry.sga", &entry_src), ("lib.sga", &lib_src)]);

    let program = parse(&entry_src).unwrap();
    let resolved = resolve_imports(program, Path::new("entry.sga"), &loader, &parse).unwrap();

    // Stmt::Import должен полностью исчезнуть из резолвленного AST
    assert!(!resolved.iter().any(|s| matches!(s, Stmt::Import(_))));
    // helper() должен быть инлайнен ровно один раз
    assert_eq!(count_fn_decls(&resolved, "helper"), 1);
}

#[test]
fn test_diamond_import_is_deduplicated_pragma_once_style() {
    // entry.sga импортирует и a.sga, и b.sga; оба, в свою очередь,
    // импортируют один и тот же common.sga ("ромбовидный" граф импортов).
    // common.sga должен быть инлайнен РОВНО ОДИН раз, а не дважды.
    let entry_src = format!("{i} \"a.sga\";\n{i} \"b.sga\";\n", i = kw("IMPORT"));
    let a_src = format!("{i} \"common.sga\";\n", i = kw("IMPORT"));
    let b_src = format!("{i} \"common.sga\";\n", i = kw("IMPORT"));
    let common_src = format!("{fn} shared() {{ {ret} 1; }}\n", fn = kw("FN"), ret = kw("RETURN"));
    let loader = InMemoryLoader::new(&[
        ("entry.sga", &entry_src),
        ("a.sga", &a_src),
        ("b.sga", &b_src),
        ("common.sga", &common_src),
    ]);

    let program = parse(&entry_src).unwrap();
    let resolved = resolve_imports(program, Path::new("entry.sga"), &loader, &parse).unwrap();

    assert_eq!(
        count_fn_decls(&resolved, "shared"),
        1,
        "common.sga импортирован дважды транзитивно (через a.sga и b.sga), но должен быть инлайнен только один раз"
    );
}

#[test]
fn test_cyclic_import_is_rejected_with_explicit_error() {
    // a.sga импортирует b.sga, который импортирует a.sga обратно.
    let a_src = format!("{i} \"b.sga\";\n", i = kw("IMPORT"));
    let b_src = format!("{i} \"a.sga\";\n", i = kw("IMPORT"));
    let loader = InMemoryLoader::new(&[("a.sga", &a_src), ("b.sga", &b_src)]);

    let program = parse(&a_src).unwrap();
    let result = resolve_imports(program, Path::new("a.sga"), &loader, &parse);

    match result {
        Err(_) => {}
        Ok(_) => {
            panic!("ожидалась ошибка циклического импорта (a.sga -> b.sga -> a.sga), получен Ok")
        }
    }
}

#[test]
fn test_self_import_is_rejected() {
    // a.sga импортирует сам себя напрямую.
    let a_src = format!("{i} \"a.sga\";\n", i = kw("IMPORT"));
    let loader = InMemoryLoader::new(&[("a.sga", &a_src)]);

    let program = parse(&a_src).unwrap();
    let result = resolve_imports(program, Path::new("a.sga"), &loader, &parse);

    match result {
        Err(_) => {}
        Ok(_) => panic!("ожидалась ошибка самоимпорта (a.sga импортирует a.sga), получен Ok"),
    }
}

#[test]
fn test_missing_import_file_gives_explicit_error_not_panic() {
    let entry_src = format!("{i} \"does_not_exist.sga\";\n", i = kw("IMPORT"));
    let loader = InMemoryLoader::new(&[("entry.sga", &entry_src)]);

    let program = parse(&entry_src).unwrap();
    let result = resolve_imports(program, Path::new("entry.sga"), &loader, &parse);

    match result {
        Err(ImportError(msg)) => {
            assert!(msg.contains("does_not_exist.sga") || msg.contains("не найден"))
        }
        Ok(_) => panic!("ожидалась ошибка отсутствующего файла импорта, получен Ok"),
    }
}

// --- SECURITY: confinement тесты (см. docs/SECURITY.md, "IMPORT
// confinement") -----------------------------------------------------
//
// До фикса `IMPORT "/etc/passwd";` или `IMPORT "../../secret.sga";`
// читали произвольный файл с прав процесса вместо отказа — эмпирически
// подтверждено PoC при аудите (абсолютный путь и `../../` оба успешно
// инлайнили файл за пределами каталога входной программы). Тесты ниже
// закрывают оба вектора регрессионным тестом.

#[test]
fn test_absolute_path_import_is_rejected() {
    // Абсолютный путь отклоняется ДО обращения к loader'у (а значит, и
    // в in-memory тесте, без реальной ФС) — см. комментарий в
    // `module_resolver::resolve_program`: `Path::join` с абсолютным
    // аргументом полностью заменяет базовый путь, поэтому проверка
    // обязана быть безусловной, а не зависеть от `current_dir`.
    let absolute = if cfg!(windows) {
        "C:\\secret.sga"
    } else {
        "/etc/secret.sga"
    };
    let entry_src = format!("{i} \"{p}\";\n", i = kw("IMPORT"), p = absolute);
    let loader = InMemoryLoader::new(&[("entry.sga", &entry_src)]);

    let program = parse(&entry_src).unwrap();
    let result = resolve_imports(program, Path::new("entry.sga"), &loader, &parse);

    match result {
        Err(ImportError(msg)) => assert!(
            msg.contains("абсолютным") || msg.to_lowercase().contains("absolute"),
            "сообщение об ошибке должно объяснять, что путь абсолютный: {}",
            msg
        ),
        Ok(_) => panic!("ожидался отказ для абсолютного пути в IMPORT, получен Ok"),
    }
}

/// Создаёт одноразовую директорию во `std::env::temp_dir()` с уникальным
/// именем (PID + имя теста), без новых зависимостей (`tempfile` и
/// аналоги) — соответствует выбору проекта не тащить лишние крейты там,
/// где хватает `std`. Вызывающий тест отвечает за удаление в конце
/// (через `let _guard = ...` с `Drop`, см. ниже).
struct TempDir(PathBuf);

impl TempDir {
    fn new(test_name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sga_test_{}_{}", test_name, std::process::id()));
        std::fs::create_dir_all(&dir).expect("не удалось создать временную директорию для теста");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn test_path_traversal_escaping_entry_root_is_rejected_on_real_fs() {
    // Реальная ФС обязательна для этого теста: лексическая проверка
    // (fallback для in-memory loader'а) тоже сработала бы, но именно
    // здесь нужно подтвердить основной путь — `std::fs::canonicalize`,
    // который резолвит фактические symlink/`..` на диске (тот же путь,
    // которым реально пользуется production `FsLoader`).
    use sga::module_resolver::FsLoader;

    let tmp = TempDir::new("traversal_escape");
    let project_dir = tmp.0.join("project");
    let outside_dir = tmp.0.join("outside");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::create_dir_all(&outside_dir).unwrap();

    let secret_path = outside_dir.join("secret.sga");
    let secret_src = format!(
        "{let_} TOKEN = \"sk_live_should_not_leak\";\n",
        let_ = kw("LET")
    );
    std::fs::write(secret_path, secret_src).unwrap();

    let entry_path = project_dir.join("main.sga");
    let entry_src = format!("{i} \"../outside/secret.sga\";\n", i = kw("IMPORT"));
    std::fs::write(&entry_path, &entry_src).unwrap();

    let program = parse(&entry_src).unwrap();
    let result = resolve_imports(program, &entry_path, &FsLoader, &parse);

    match result {
        Err(ImportError(msg)) => assert!(
            msg.contains("пределы") || msg.to_lowercase().contains("traversal") || msg.contains(".."),
            "сообщение должно объяснять, что путь выходит за пределы каталога входного файла: {}",
            msg
        ),
        Ok(_) => panic!("ожидался отказ для IMPORT, выходящего за пределы каталога входного файла (path traversal), получен Ok"),
    }
}

#[test]
fn test_path_traversal_within_entry_root_still_works_on_real_fs() {
    // Негативный тест на false positive: точка входа — `project/main.sga`,
    // он импортирует `sub/inner.sga`, а ТОТ, в свою очередь (уже на
    // следующем уровне рекурсии, относительно СВОЕЙ директории
    // `project/sub`), импортирует `../shared.sga`. Итоговый путь —
    // `project/shared.sga` — остаётся внутри корня (`project/`,
    // директория исходного входного файла `main.sga`), поэтому должен
    // продолжать резолвиться как раньше: фикс не должен ломать
    // легитимные вложенные модули, только реальный побег за пределы
    // каталога ИСХОДНОГО входного файла программы.
    use sga::module_resolver::FsLoader;

    let tmp = TempDir::new("traversal_legit");
    let project_dir = tmp.0.join("project");
    let sub_dir = project_dir.join("sub");
    std::fs::create_dir_all(&sub_dir).unwrap();

    let shared_path = project_dir.join("shared.sga");
    let shared_src = format!(
        "{fn} shared_helper() {{ {ret} 100; }}\n",
        fn = kw("FN"),
        ret = kw("RETURN")
    );
    std::fs::write(shared_path, shared_src).unwrap();

    let inner_path = sub_dir.join("inner.sga");
    let inner_src = format!(
        "{i} \"../shared.sga\";\n{fn} get_value() {{ {ret} shared_helper() + 1; }}\n",
        i = kw("IMPORT"),
        fn = kw("FN"),
        ret = kw("RETURN"),
    );
    std::fs::write(inner_path, inner_src).unwrap();

    let entry_path = project_dir.join("main.sga");
    let entry_src = format!(
        "{i} \"sub/inner.sga\";\n{pr}(get_value());\n",
        i = kw("IMPORT"),
        pr = kw("PRINT"),
    );
    std::fs::write(&entry_path, &entry_src).unwrap();

    let program = parse(&entry_src).unwrap();
    let result = resolve_imports(program, &entry_path, &FsLoader, &parse);

    assert!(
        result.is_ok(),
        "легитимный '..' внутри каталога исходного входного файла программы не должен отклоняться: {:?}",
        result.err()
    );
}

#[test]
fn test_transitive_import_chain_is_resolved() {
    // entry -> mid -> leaf, без циклов и без дублей. Все три уровня
    // должны быть полностью инлайнены в итоговую плоскую программу.
    let entry_src = format!("{i} \"mid.sga\";\n", i = kw("IMPORT"));
    let mid_src = format!(
        "{i} \"leaf.sga\";\n{fn} mid_fn() {{ {ret} 2; }}\n",
        i = kw("IMPORT"),
        fn = kw("FN"),
        ret = kw("RETURN"),
    );
    let leaf_src = format!("{fn} leaf_fn() {{ {ret} 1; }}\n", fn = kw("FN"), ret = kw("RETURN"));
    let loader = InMemoryLoader::new(&[
        ("entry.sga", &entry_src),
        ("mid.sga", &mid_src),
        ("leaf.sga", &leaf_src),
    ]);

    let program = parse(&entry_src).unwrap();
    let resolved = resolve_imports(program, Path::new("entry.sga"), &loader, &parse).unwrap();

    assert_eq!(count_fn_decls(&resolved, "mid_fn"), 1);
    assert_eq!(count_fn_decls(&resolved, "leaf_fn"), 1);
    assert!(!resolved.iter().any(|s| matches!(s, Stmt::Import(_))));
}

#[test]
fn test_resolved_program_preserves_non_import_statements_in_order() {
    // Проверяем, что обычные (не-IMPORT) statement'ы текущего файла
    // сохраняют относительный порядок, а импортированные вставляются
    // на месте своего IMPORT, а не все скопом в начало/конец.
    let entry_src = format!(
        "{let_} x = 1;\n{i} \"lib.sga\";\n{let_} y = 2;\n",
        let_ = kw("LET"),
        i = kw("IMPORT"),
    );
    let lib_src = format!("{fn} from_lib() {{ {ret} 0; }}\n", fn = kw("FN"), ret = kw("RETURN"));
    let loader = InMemoryLoader::new(&[("entry.sga", &entry_src), ("lib.sga", &lib_src)]);

    let program = parse(&entry_src).unwrap();
    let resolved = resolve_imports(program, Path::new("entry.sga"), &loader, &parse).unwrap();

    // Ожидаемый порядок: VarDecl(x), FnDecl(from_lib), VarDecl(y)
    assert_eq!(resolved.len(), 3);
    assert!(matches!(&resolved[0], Stmt::VarDecl { name, .. } if name == "x"));
    assert!(matches!(&resolved[1], Stmt::FnDecl { name, .. } if name == "from_lib"));
    assert!(matches!(&resolved[2], Stmt::VarDecl { name, .. } if name == "y"));
}

#[test]
fn test_nested_import_inside_block_is_rejected_by_parser() {
    // IMPORT, как и FN, синтаксически допустим только на верхнем уровне.
    let src = format!(
        "{fn} f() {{ {i} \"x.sga\"; }}",
        fn = kw("FN"),
        i = kw("IMPORT"),
    );
    assert!(
        parse(&src).is_err(),
        "ожидалась ошибка парсера: IMPORT внутри функции недопустим"
    );
}

#[test]
fn test_imported_top_level_expr_stmt_is_preserved() {
    let entry_src = format!("{i} \"lib.sga\";\n", i = kw("IMPORT"));
    let lib_src = format!("{pr}(1);\n", pr = kw("PRINT"));
    let loader = InMemoryLoader::new(&[("entry.sga", &entry_src), ("lib.sga", &lib_src)]);

    let program = parse(&entry_src).unwrap();
    let resolved = resolve_imports(program, Path::new("entry.sga"), &loader, &parse).unwrap();

    assert_eq!(resolved.len(), 1);
    match &resolved[0] {
        Stmt::Print(args) => assert!(matches!(args.first(), Some(Expr::Int(1)))),
        other => panic!(
            "ожидался Stmt::Print из импортированного файла, получено {:?}",
            other
        ),
    }
}
