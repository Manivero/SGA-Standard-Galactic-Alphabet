//! Песочница записи файлов для `write_file` — T015, M006 (`std/io`).
//! Полный Feature Contract — см. PROJECT_STATUS.md.
//!
//! Здесь только проверка пути и сама запись: разрешение (`--allow-write`) и
//! корень песочницы приходят от `Vm`. Проверка идёт по РЕАЛЬНОЙ файловой
//! системе, без «лексических» упрощений: для записи они не годятся — новый
//! файл ещё не существует, и `module_resolver::is_within_root` для такого
//! пути откатывается на лексическую проверку, которая не раскрывает symlink'и
//! в каталогах пути (файл в каталоге-symlink'е, ведущем наружу, прошёл бы).
//!
//! Принцип: безопасность определяет РОДИТЕЛЬСКИЙ каталог цели, приведённый к
//! физическому пути (`canonicalize` раскрывает symlink'и и `..`) — он обязан
//! лежать внутри физического корня; сам файл — только обычный (не symlink, не
//! каталог, не сокет/fifo/устройство).
//!
//! Известный предел (TD-016): между проверкой и записью другой процесс может
//! подменить каталог или файл (TOCTOU). Полностью закрыть это в переносимом
//! `std` нельзя (нужны `openat`/`O_NOFOLLOW`, то есть платформенный код); для
//! нового файла окно сужено — он создаётся через `create_new`, который не
//! следует по symlink'у, появившемуся в этом месте.

use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// Проверенная цель записи (результат [`resolve_write_target`]).
#[derive(Debug)]
pub(crate) struct WriteTarget {
    /// Физический путь файла: канонизированный родительский каталог + имя.
    path: PathBuf,
    /// Файл уже существовал при проверке (обычный файл — будет перезаписан
    /// целиком); иначе файл создаётся.
    existed: bool,
}

/// Сообщение об отказе для пути вне песочницы — одно на все случаи (существующий
/// и несуществующий каталог вне корня, `..`, symlink), чтобы по тексту ошибки
/// нельзя было выяснить, что лежит за пределами корня.
fn outside_root_message(root_dir: &Path) -> String {
    format!(
        "путь выходит за пределы каталога входного файла программы ('{}'); \
         обход через '..' и symlink запрещён",
        root_dir.display()
    )
}

/// Проверяет путь записи `user_path` (как его написала программа) относительно
/// корня песочницы `root` и возвращает физическую цель записи.
///
/// Правила (все — отказ с понятной причиной, без panic):
/// 1. пустой путь и NUL в пути;
/// 2. абсолютный путь (любой путь с корнем или префиксом) — ДО обращения к ФС:
///    `Path::join` с абсолютным аргументом заменил бы базу целиком;
/// 3. путь должен указывать на файл: не оканчиваться разделителем, `.`, `..`;
/// 4. родитель цели берётся как написано (`root.join(путь)` БЕЗ лексической
///    нормализации — иначе `link/..` была бы свёрнута лексически, тогда как ОС
///    разрешает её физически) и обязан существовать: каталоги не создаются;
///    после `canonicalize` он обязан лежать внутри `canonicalize(root)`;
/// 5. цель, если существует: symlink (в т.ч. висячий), каталог или не обычный
///    файл — отказ; обычный файл будет перезаписан.
///
/// Путь вне корня отклоняется ОДНИМ сообщением независимо от того, существует
/// ли он (правило 4): по ошибкам нельзя разведать содержимое за пределами
/// песочницы.
pub(crate) fn resolve_write_target(root: &Path, user_path: &str) -> Result<WriteTarget, String> {
    if user_path.is_empty() {
        return Err("путь пуст".to_string());
    }
    if user_path.contains('\0') {
        return Err("путь содержит нулевой символ".to_string());
    }
    let relative = Path::new(user_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return Err(
            "абсолютные пути запрещены — путь обязан быть относительным \
                    и не выходить за пределы каталога входного файла программы"
                .to_string(),
        );
    }
    // `Path::components` молча отбрасывает хвостовые разделители и `.`, поэтому
    // «это не файл» проверяется по самой строке.
    let last_segment = user_path
        .rsplit(std::path::is_separator)
        .next()
        .unwrap_or("");
    if last_segment.is_empty() || last_segment == "." || last_segment == ".." {
        return Err("путь должен указывать на файл: он не может оканчиваться \
                    разделителем, '.' или '..'"
            .to_string());
    }
    let file_name = relative
        .file_name()
        .ok_or_else(|| "путь не содержит имени файла".to_string())?;

    let root_dir: &Path = if root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        root
    };
    let full = root_dir.join(relative);
    let parent: PathBuf = match full.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let canonical_parent = match fs::canonicalize(&parent) {
        Ok(p) => p,
        Err(e) => {
            // Программа не должна узнавать, существуют ли каталоги ВНЕ
            // песочницы: родитель, лежащий вне корня, получает тот же отказ,
            // что и существующий, даже если его нет (для несуществующего путь
            // сравнивается лексически — `is_within_root` откатывается на это
            // сам; symlink'и в несуществующем хвосте так не раскрыть — TD-015).
            if !crate::module_resolver::is_within_root(&parent, root_dir) {
                return Err(outside_root_message(root_dir));
            }
            return Err(if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "каталог '{}' не существует (write_file каталоги не создаёт)",
                    parent.display()
                )
            } else {
                format!("каталог '{}' недоступен: {}", parent.display(), e)
            });
        }
    };
    let canonical_root = fs::canonicalize(root_dir).map_err(|e| {
        format!(
            "каталог входного файла программы ('{}') недоступен: {}",
            root_dir.display(),
            e
        )
    })?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(outside_root_message(root_dir));
    }
    // `canonicalize` успешно разрешает и обычный файл; «каталог» здесь — это
    // именно каталог (иначе `file.txt/out.txt` дал бы невнятный ENOTDIR ниже).
    if !canonical_parent.is_dir() {
        return Err(format!("'{}' не является каталогом", parent.display()));
    }

    let path = canonical_parent.join(file_name);
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                Err("цель — символическая ссылка: запись через symlink запрещена".to_string())
            } else if file_type.is_dir() {
                Err("цель — каталог, а не файл".to_string())
            } else if !file_type.is_file() {
                Err("цель — не обычный файл (сокет, fifo или устройство)".to_string())
            } else {
                Ok(WriteTarget {
                    path,
                    existed: true,
                })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(WriteTarget {
            path,
            existed: false,
        }),
        Err(e) => Err(format!("не удалось проверить цель: {}", e)),
    }
}

/// Записывает `content` (байты UTF-8 строки, дословно) в проверенную цель.
///
/// Существующий файл открывается на запись с усечением; новый — через
/// `create_new`: она отказывает, если по этому пути что-то появилось после
/// проверки (в т.ч. symlink — `O_EXCL` по ссылке не идёт). Запись не атомарна
/// и не синхронизируется с диском: сбой посреди записи оставляет усечённый
/// файл. Ошибки ОС возвращаются текстом.
pub(crate) fn write_target(target: &WriteTarget, content: &str) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if target.existed {
        options.truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(&target.path).map_err(|e| e.to_string())?;
    file.write_all(content.as_bytes())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Временный каталог с уникальным именем (PID + счётчик); удаляется при
    /// выходе из теста. Без внешних крейтов — проект остаётся без зависимостей.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("sga_fsb_{}_{}_{}", std::process::id(), name, n));
            fs::create_dir_all(&dir).expect("не удалось создать временный каталог");
            TestDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// Физический (канонизированный) путь каталога — как его видит
        /// `resolve_write_target`.
        fn real(&self) -> PathBuf {
            fs::canonicalize(&self.0).expect("каталог теста должен существовать")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn err_of(result: Result<WriteTarget, String>) -> String {
        match result {
            Err(e) => e,
            Ok(t) => panic!("ожидался отказ, получена цель {:?}", t),
        }
    }

    #[test]
    fn a_new_file_in_the_root_resolves_to_a_physical_path_and_is_marked_as_new() {
        let dir = TestDir::new("new_file");
        let target = resolve_write_target(dir.path(), "out.txt").unwrap();
        assert_eq!(target.path, dir.real().join("out.txt"));
        assert!(!target.existed);
        assert!(
            !dir.path().join("out.txt").exists(),
            "проверка не создаёт файл"
        );
    }

    #[test]
    fn an_existing_regular_file_resolves_as_existing() {
        let dir = TestDir::new("existing");
        fs::write(dir.path().join("out.txt"), "old").unwrap();
        let target = resolve_write_target(dir.path(), "out.txt").unwrap();
        assert!(target.existed);
        assert_eq!(
            fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "old"
        );
    }

    #[test]
    fn a_file_in_an_existing_subdirectory_resolves_inside_the_root() {
        let dir = TestDir::new("subdir");
        fs::create_dir(dir.path().join("sub")).unwrap();
        let target = resolve_write_target(dir.path(), "sub/out.txt").unwrap();
        assert_eq!(target.path, dir.real().join("sub").join("out.txt"));
    }

    #[test]
    fn dotdot_that_stays_inside_the_root_is_allowed() {
        let dir = TestDir::new("dotdot_inside");
        fs::create_dir(dir.path().join("sub")).unwrap();
        let target = resolve_write_target(dir.path(), "sub/../out.txt").unwrap();
        assert_eq!(target.path, dir.real().join("out.txt"));
    }

    #[test]
    fn dotdot_that_leaves_the_root_is_rejected() {
        let dir = TestDir::new("dotdot_escape");
        fs::create_dir(dir.path().join("sub")).unwrap();
        for path in ["../x.txt", "sub/../../x.txt", "../../x.txt"] {
            let err = err_of(resolve_write_target(dir.path(), path));
            assert!(err.contains("выходит за пределы"), "{path}: {err}");
        }
    }

    #[test]
    fn a_directory_outside_the_root_is_refused_with_one_message_whether_or_not_it_exists() {
        // Корень — подкаталог `outer/root`; рядом лежит существующий `outer/sibling`
        // и обычный файл `outer/file.txt`. Отказ обязан быть одним и тем же для
        // существующего, несуществующего и «файлового» родителя вне корня —
        // иначе программа разведывала бы содержимое за пределами песочницы.
        let outer = TestDir::new("outside_oracle");
        let root = outer.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::create_dir(outer.path().join("sibling")).unwrap();
        fs::write(outer.path().join("file.txt"), "x").unwrap();

        let existing = err_of(resolve_write_target(&root, "../sibling/x.txt"));
        let missing = err_of(resolve_write_target(&root, "../no_such_dir/x.txt"));
        let missing_deep = err_of(resolve_write_target(&root, "../no_such_dir/a/b/x.txt"));
        let file_parent = err_of(resolve_write_target(&root, "../file.txt/x.txt"));
        assert!(existing.contains("выходит за пределы"), "{existing}");
        assert_eq!(existing, missing);
        assert_eq!(existing, missing_deep);
        assert_eq!(existing, file_parent);
        assert!(!outer.path().join("no_such_dir").exists());
    }

    #[test]
    fn an_absolute_path_is_rejected_before_touching_the_filesystem() {
        let dir = TestDir::new("absolute");
        let absolute = if cfg!(windows) {
            "C:\\sga_t015_must_not_be_created.txt"
        } else {
            "/sga_t015_must_not_be_created.txt"
        };
        let err = err_of(resolve_write_target(dir.path(), absolute));
        assert!(err.contains("абсолютные пути запрещены"), "{err}");
        // Даже если каталог цели существует, а корень — несуществующий путь:
        // до ФС дело не доходит.
        let err = err_of(resolve_write_target(
            Path::new("/sga_t015_no_such_root"),
            absolute,
        ));
        assert!(err.contains("абсолютные пути запрещены"), "{err}");
    }

    #[test]
    fn empty_nul_and_directory_like_paths_are_rejected() {
        let dir = TestDir::new("malformed");
        fs::create_dir(dir.path().join("sub")).unwrap();
        let err = err_of(resolve_write_target(dir.path(), ""));
        assert!(err.contains("пуст"), "{err}");
        let err = err_of(resolve_write_target(dir.path(), "a\0b.txt"));
        assert!(err.contains("нулев"), "{err}");
        for path in ["sub/", "sub//", ".", "..", "sub/.", "sub/..", "./", "../"] {
            let err = err_of(resolve_write_target(dir.path(), path));
            assert!(err.contains("должен указывать на файл"), "{path:?}: {err}");
        }
        // Имя с завершающим разделителем или точкой — не файл и тогда, когда
        // такого каталога ещё нет: иначе `newdir/` молча стал бы файлом `newdir`.
        for path in ["newdir/", "newdir//", "newdir/.", "sub/newdir/"] {
            let err = err_of(resolve_write_target(dir.path(), path));
            assert!(err.contains("должен указывать на файл"), "{path:?}: {err}");
        }
        assert!(!dir.path().join("newdir").exists());
    }

    #[test]
    fn a_missing_parent_directory_is_rejected_and_not_created() {
        let dir = TestDir::new("missing_parent");
        let err = err_of(resolve_write_target(dir.path(), "nope/out.txt"));
        assert!(err.contains("не существует"), "{err}");
        assert!(
            !dir.path().join("nope").exists(),
            "каталог не должен создаваться"
        );
        let err = err_of(resolve_write_target(dir.path(), "a/b/c/out.txt"));
        assert!(err.contains("не существует"), "{err}");
        assert!(!dir.path().join("a").exists());
    }

    #[test]
    fn a_parent_that_is_a_regular_file_is_rejected() {
        let dir = TestDir::new("parent_is_file");
        fs::write(dir.path().join("file.txt"), "x").unwrap();
        let err = err_of(resolve_write_target(dir.path(), "file.txt/out.txt"));
        assert!(err.contains("не является каталогом"), "{err}");
        // Глубже файла каталога тоже нет — отказ остаётся отказом (без panic).
        let err = err_of(resolve_write_target(dir.path(), "file.txt/sub/out.txt"));
        assert!(err.contains("каталог"), "{err}");
        assert_eq!(
            fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "x"
        );
    }

    #[test]
    fn a_directory_as_the_target_is_rejected() {
        let dir = TestDir::new("target_is_dir");
        fs::create_dir(dir.path().join("sub")).unwrap();
        let err = err_of(resolve_write_target(dir.path(), "sub"));
        assert!(err.contains("каталог"), "{err}");
        assert!(dir.path().join("sub").is_dir());
    }

    #[test]
    fn an_empty_root_is_the_current_directory() {
        // Пустой корень (вручную собранная VM) трактуется как `.`, а не
        // принимает всё подряд: цель — файл в cwd, `..` — отказ. Ничего не
        // создаётся: проверяется только разбор пути.
        let cwd = fs::canonicalize(".").unwrap();
        let target = resolve_write_target(Path::new(""), "__sga_t015_no_such_file__.txt").unwrap();
        assert_eq!(target.path, cwd.join("__sga_t015_no_such_file__.txt"));
        let err = err_of(resolve_write_target(
            Path::new(""),
            "../__sga_t015_no_such_file__.txt",
        ));
        assert!(err.contains("выходит за пределы"), "{err}");
    }

    #[test]
    fn writing_creates_and_overwrites_with_exact_bytes() {
        let dir = TestDir::new("write_bytes");
        let file = dir.path().join("out.txt");

        let target = resolve_write_target(dir.path(), "out.txt").unwrap();
        write_target(&target, "привет, мир ✓\n").unwrap();
        assert_eq!(fs::read(&file).unwrap(), "привет, мир ✓\n".as_bytes());

        // Перезапись более КОРОТКИМ содержимым не оставляет хвоста.
        let target = resolve_write_target(dir.path(), "out.txt").unwrap();
        assert!(target.existed);
        write_target(&target, "ab").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"ab");

        // Пустая строка — пустой файл; CRLF и NUL сохраняются дословно.
        let target = resolve_write_target(dir.path(), "out.txt").unwrap();
        write_target(&target, "").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"");
        let target = resolve_write_target(dir.path(), "out.txt").unwrap();
        write_target(&target, "a\r\nb\0c\n").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"a\r\nb\0c\n");
    }

    #[test]
    fn a_file_that_appeared_after_the_check_is_not_overwritten() {
        // Цель проверена как НОВАЯ, но к моменту записи файл уже есть:
        // `create_new` отказывает, содержимое не теряется.
        let dir = TestDir::new("appeared");
        let target = resolve_write_target(dir.path(), "out.txt").unwrap();
        assert!(!target.existed);
        fs::write(dir.path().join("out.txt"), "кто-то успел раньше").unwrap();
        let err = write_target(&target, "новое").unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(
            fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "кто-то успел раньше"
        );
    }

    #[cfg(unix)]
    mod unix_symlinks {
        use super::*;
        use std::os::unix::fs::symlink;

        #[test]
        fn a_symlink_target_is_rejected_whatever_it_points_to() {
            let dir = TestDir::new("link_target");
            let outside = TestDir::new("link_target_outside");
            fs::write(dir.path().join("inside.txt"), "inside").unwrap();
            fs::write(outside.path().join("outside.txt"), "outside").unwrap();
            symlink("inside.txt", dir.path().join("to_inside")).unwrap();
            symlink(
                outside.path().join("outside.txt"),
                dir.path().join("to_outside"),
            )
            .unwrap();
            symlink(
                outside.path().join("missing.txt"),
                dir.path().join("dangling"),
            )
            .unwrap();
            for name in ["to_inside", "to_outside", "dangling"] {
                let err = err_of(resolve_write_target(dir.path(), name));
                assert!(err.contains("символическая ссылка"), "{name}: {err}");
            }
            assert_eq!(
                fs::read_to_string(dir.path().join("inside.txt")).unwrap(),
                "inside"
            );
            assert_eq!(
                fs::read_to_string(outside.path().join("outside.txt")).unwrap(),
                "outside"
            );
            assert!(!outside.path().join("missing.txt").exists());
        }

        #[test]
        fn a_directory_symlink_pointing_outside_is_rejected() {
            let dir = TestDir::new("link_dir_out");
            let outside = TestDir::new("link_dir_out_target");
            symlink(outside.path(), dir.path().join("link")).unwrap();
            let err = err_of(resolve_write_target(dir.path(), "link/x.txt"));
            assert!(err.contains("выходит за пределы"), "{err}");
            // Лексически `link/../x.txt` — это `x.txt` внутри корня, но ОС
            // разрешит `link/..` физически — в родителя внешнего каталога.
            let err = err_of(resolve_write_target(dir.path(), "link/../x.txt"));
            assert!(err.contains("выходит за пределы"), "{err}");
            assert!(!outside.path().join("x.txt").exists());
        }

        #[test]
        fn a_directory_symlink_pointing_inside_is_allowed() {
            let dir = TestDir::new("link_dir_in");
            fs::create_dir(dir.path().join("real")).unwrap();
            symlink("real", dir.path().join("link")).unwrap();
            let target = resolve_write_target(dir.path(), "link/x.txt").unwrap();
            assert_eq!(target.path, dir.real().join("real").join("x.txt"));
        }

        #[test]
        fn dotdot_after_a_symlink_is_resolved_physically() {
            let dir = TestDir::new("link_dotdot");
            fs::create_dir_all(dir.path().join("a").join("b")).unwrap();
            symlink("a/b", dir.path().join("link")).unwrap();
            // `link` -> `a/b`, значит `link/..` — это `a`, а не корень.
            let target = resolve_write_target(dir.path(), "link/../x.txt").unwrap();
            assert_eq!(target.path, dir.real().join("a").join("x.txt"));
        }

        #[test]
        fn a_symlink_that_appeared_after_the_check_is_not_followed() {
            // Цель проверена как НОВАЯ, потом на её месте появился symlink на
            // несуществующий внешний файл: `create_new` (O_EXCL) по ссылке не
            // идёт — запись отклоняется, внешний файл не создан.
            let dir = TestDir::new("link_race");
            let outside = TestDir::new("link_race_outside");
            let target = resolve_write_target(dir.path(), "out.txt").unwrap();
            assert!(!target.existed);
            let victim = outside.path().join("victim.txt");
            symlink(&victim, dir.path().join("out.txt")).unwrap();
            assert!(write_target(&target, "payload").is_err());
            assert!(!victim.exists(), "запись прошла по symlink наружу");
        }

        #[test]
        fn a_socket_as_the_target_is_rejected_as_not_a_regular_file() {
            let dir = TestDir::new("special");
            let _listener = std::os::unix::net::UnixListener::bind(dir.path().join("sock"))
                .expect("не удалось создать сокет-файл для теста");
            let err = err_of(resolve_write_target(dir.path(), "sock"));
            assert!(err.contains("не обычный файл"), "{err}");
        }
    }
}
