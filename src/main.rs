//! CLI компилятора/интерпретатора SGA.

use sga::{lexer, parser, sga_alphabet, RunOptions};

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

const VERSION: &str = "0.1.0";

/// Аргументы командной строки как `String` (включая имя программы, как
/// `env::args()`). В отличие от `env::args()`, не паникует на не-UTF-8
/// аргументе, а возвращает понятное сообщение: строки SGA — это UTF-8,
/// поэтому такой аргумент нельзя передать программе без потери данных
/// (T012, M006: до этого `sga run файл <не-UTF-8>` завершался паникой).
fn collect_args() -> Result<Vec<String>, String> {
    env::args_os()
        .enumerate()
        .map(|(i, arg)| {
            arg.into_string().map_err(|bad| {
                format!(
                    "аргумент командной строки №{} ({:?}) не является валидным UTF-8: \
                     строки SGA — это UTF-8, передать такой аргумент программе нельзя",
                    i, bad
                )
            })
        })
        .collect()
}

fn main() -> ExitCode {
    let args = match collect_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{}", msg);
            return ExitCode::FAILURE;
        }
    };
    if args.len() < 2 {
        print_usage();
        return ExitCode::FAILURE;
    }

    match args[1].as_str() {
        "run" => match parse_run_args(&args[2..]) {
            Ok(invocation) => cmd_run(&invocation),
            Err(msg) => {
                eprintln!("{}", msg);
                ExitCode::FAILURE
            }
        },
        "build" => {
            eprintln!("'sga build' (нативная компиляция через LLVM) пока не реализована.");
            eprintln!(
                "Текущий бэкенд v0.1 — собственная байткод-VM. Используйте 'sga run <файл.sga>'."
            );
            eprintln!("См. docs/ROADMAP.md, раздел 'Бэкенды'.");
            ExitCode::FAILURE
        }
        "init" => cmd_init(args.get(2).map(|s| s.as_str()).unwrap_or(".")),
        "test" => {
            eprintln!("'sga test' (тест-раннер с аннотацией #[test]) пока не реализован. См. docs/ROADMAP.md.");
            ExitCode::FAILURE
        }
        "fmt" => {
            if args.len() < 3 {
                eprintln!("использование: sga fmt <файл.sga>");
                return ExitCode::FAILURE;
            }
            cmd_fmt(&args[2])
        }
        "lint" | "install" | "uninstall" | "doctor" | "update" | "package" => {
            eprintln!(
                "команда '{}' входит в roadmap CLI, но не реализована в v0.1. См. docs/ROADMAP.md.",
                args[1]
            );
            ExitCode::FAILURE
        }
        "version" | "--version" | "-v" => {
            println!("sga {}", VERSION);
            ExitCode::SUCCESS
        }
        _ => {
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!("SGA Programming Language v{}", VERSION);
    eprintln!("использование: sga <команда> [аргументы]");
    eprintln!();
    eprintln!("команды:");
    eprintln!("  run [--allow-write] [--] <файл.sga> [аргументы...]");
    eprintln!("                    скомпилировать и выполнить файл на встроенной VM;");
    eprintln!("                    всё после файла передаётся программе (builtin args()),");
    eprintln!(
        "                    стандартный ввод процесса — builtin'ам read_line()/read_stdin();"
    );
    eprintln!("                    флаги — только ДО файла: --allow-write разрешает программе");
    eprintln!("                    запись файлов (write_file) внутри каталога входного файла,");
    eprintln!("                    без него запись запрещена; -- завершает список флагов");
    eprintln!(
        "                    (поддерживает IMPORT \"путь.sga\"; — см. docs/COMPILER_SPEC.md)"
    );
    eprintln!("  init [путь]       создать новый проект (sga.toml + src/main.sga)");
    eprintln!("  fmt <файл.sga>    вывести AST-нормализованный псевдокод файла (диагностика;");
    eprintln!("                    НЕ резолвит IMPORT — показывает AST файла как есть)");
    eprintln!("  build/test/lint/install/uninstall/doctor/update/package");
    eprintln!("                    запланированы, статус — docs/ROADMAP.md");
    eprintln!("  version           показать версию компилятора");
}

/// Использование `sga run` — единственный источник текста для всех сообщений
/// об ошибке разбора.
const RUN_USAGE: &str = "использование: sga run [--allow-write] [--] <файл.sga> [аргументы...]";

/// Разобранный вызов `sga run` (T015, M006).
#[derive(Debug, PartialEq, Eq)]
struct RunInvocation {
    /// Был флаг `--allow-write` (только до пути файла): программе разрешена
    /// запись файлов (`write_file`). Больше НИЧТО это разрешение не выдаёт.
    allow_write: bool,
    /// Путь входного файла.
    path: String,
    /// Всё после пути файла — аргументы ПРОГРАММЫ (builtin `args()`),
    /// дословно, в т.ч. то, что выглядит как флаг (`--help`, `-v`,
    /// `--allow-write`).
    program_args: Vec<String>,
}

/// Разбирает аргументы после `sga run` (`rest` = всё после слова `run`).
///
/// Флаги самого `sga` допустимы ТОЛЬКО до пути файла: `--allow-write`, и `--`,
/// завершающий список флагов. Первый аргумент, не являющийся флагом, — путь
/// файла, всё после него — аргументы программы без разбора. Поэтому
/// `sga run prog.sga --allow-write` НЕ даёт программе права записи: флаг
/// получает сама программа через `args()`. Любой другой аргумент, начинающийся
/// с `-` (кроме одиночного `-`, который считается путём), до пути файла —
/// ошибка, а не молчаливо принятый путь: опечатка во флаге не должна
/// превращаться в запуск без разрешения, которого пользователь ждал.
fn parse_run_args(rest: &[String]) -> Result<RunInvocation, String> {
    let mut allow_write = false;
    let mut index = 0;
    while let Some(arg) = rest.get(index) {
        match arg.as_str() {
            "--" => {
                index += 1;
                break;
            }
            "--allow-write" => {
                allow_write = true;
                index += 1;
            }
            flag if flag.starts_with('-') && flag != "-" => {
                return Err(format!(
                    "неизвестный флаг '{}' команды run: до пути файла допустимы только \
                     --allow-write и --; если это путь файла, начинающийся с '-', \
                     напишите './{}' или поставьте перед ним '--'\n{}",
                    flag, flag, RUN_USAGE
                ));
            }
            _ => break,
        }
    }
    match rest.get(index) {
        Some(path) => Ok(RunInvocation {
            allow_write,
            path: path.clone(),
            program_args: rest[index + 1..].to_vec(),
        }),
        None => Err(RUN_USAGE.to_string()),
    }
}

fn cmd_run(invocation: &RunInvocation) -> ExitCode {
    // Стандартный ввод процесса отдаётся программе целиком, но читается
    // лениво — только когда она вызывает read_line()/read_stdin() (T013).
    // Запись файлов разрешена, только если пользователь явно передал
    // `--allow-write` (T015); по умолчанию — запрещена.
    let options = RunOptions::new()
        .with_args(invocation.program_args.iter().cloned())
        .with_allow_write(invocation.allow_write)
        .with_process_stdin();
    match sga::run_source_file_with_options(Path::new(&invocation.path), options) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e);
            ExitCode::FAILURE
        }
    }
}

fn cmd_fmt(path: &str) -> ExitCode {
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("не удалось прочитать '{}': {}", path, e);
            return ExitCode::FAILURE;
        }
    };
    let tokens = match lexer::Lexer::new(&source).tokenize() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::FAILURE;
        }
    };
    match parser::Parser::new(tokens).parse_program() {
        Ok(program) => {
            for stmt in &program {
                println!("{:#?}", stmt);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", e);
            ExitCode::FAILURE
        }
    }
}

fn cmd_init(dir: &str) -> ExitCode {
    let src_dir = format!("{}/src", dir);
    if let Err(e) = fs::create_dir_all(&src_dir) {
        eprintln!("ошибка создания директории '{}': {}", src_dir, e);
        return ExitCode::FAILURE;
    }
    let toml_path = format!("{}/sga.toml", dir);
    let toml = "[package]\nname = \"my-sga-app\"\nversion = \"0.1.0\"\n\n[dependencies]\n# Path-зависимости: путь считается от каталога ЭТОГО sga.toml.\n# В коде: IMPORT \"pkg:mylib/файл.sga\"; (файл ищется внутри каталога пакета)\n# mylib = { path = \"../mylib\" }\n\n[targets]\nplatforms = [\"windows\", \"linux\", \"macos\"]\narch = [\"x86_64\", \"arm64\"]\n\n[compiler]\nbackend = \"vm\"\n";
    if let Err(e) = fs::write(&toml_path, toml) {
        eprintln!("ошибка записи '{}': {}", toml_path, e);
        return ExitCode::FAILURE;
    }
    let main_sga_path = format!("{}/main.sga", src_dir);
    let main_sga = encode_hello_world();
    if let Err(e) = fs::write(&main_sga_path, main_sga) {
        eprintln!("ошибка записи '{}': {}", main_sga_path, e);
        return ExitCode::FAILURE;
    }
    println!("создан новый проект SGA в '{}'", dir);
    println!("запуск: sga run {}", main_sga_path);
    ExitCode::SUCCESS
}

fn encode_hello_world() -> String {
    let let_kw = sga_alphabet::encode_word("LET");
    let print_kw = sga_alphabet::encode_word("PRINT");
    format!(
        "{} message = \"Hello, SGA!\";\n{}(message);\n",
        let_kw, print_kw
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn invocation(allow_write: bool, path: &str, program_args: &[&str]) -> RunInvocation {
        RunInvocation {
            allow_write,
            path: path.to_string(),
            program_args: v(program_args),
        }
    }

    #[test]
    fn a_path_alone_runs_without_write_permission_and_without_program_args() {
        assert_eq!(
            parse_run_args(&v(&["main.sga"])),
            Ok(invocation(false, "main.sga", &[]))
        );
    }

    #[test]
    fn allow_write_before_the_path_grants_the_permission() {
        assert_eq!(
            parse_run_args(&v(&["--allow-write", "main.sga"])),
            Ok(invocation(true, "main.sga", &[]))
        );
        assert_eq!(
            parse_run_args(&v(&["--allow-write", "--allow-write", "main.sga", "x"])),
            Ok(invocation(true, "main.sga", &["x"]))
        );
    }

    #[test]
    fn allow_write_after_the_path_belongs_to_the_program_and_grants_nothing() {
        assert_eq!(
            parse_run_args(&v(&["main.sga", "--allow-write"])),
            Ok(invocation(false, "main.sga", &["--allow-write"]))
        );
        assert_eq!(
            parse_run_args(&v(&["--allow-write", "main.sga", "--allow-write"])),
            Ok(invocation(true, "main.sga", &["--allow-write"]))
        );
        assert_eq!(
            parse_run_args(&v(&["main.sga", "a", "--allow-write", "b"])),
            Ok(invocation(false, "main.sga", &["a", "--allow-write", "b"]))
        );
    }

    #[test]
    fn program_arguments_are_passed_verbatim() {
        assert_eq!(
            parse_run_args(&v(&["main.sga", "--help", "-v", "--", "", "-", "--x=1"])),
            Ok(invocation(
                false,
                "main.sga",
                &["--help", "-v", "--", "", "-", "--x=1"]
            ))
        );
    }

    #[test]
    fn double_dash_ends_the_flags_and_the_next_argument_is_the_path() {
        assert_eq!(
            parse_run_args(&v(&["--", "-weird.sga", "a"])),
            Ok(invocation(false, "-weird.sga", &["a"]))
        );
        assert_eq!(
            parse_run_args(&v(&["--allow-write", "--", "-weird.sga"])),
            Ok(invocation(true, "-weird.sga", &[]))
        );
        // После `--` флагов больше нет: `--allow-write` здесь — путь файла.
        assert_eq!(
            parse_run_args(&v(&["--", "--allow-write", "main.sga"])),
            Ok(invocation(false, "--allow-write", &["main.sga"]))
        );
    }

    #[test]
    fn a_lone_dash_and_an_explicit_relative_path_are_paths() {
        assert_eq!(parse_run_args(&v(&["-"])), Ok(invocation(false, "-", &[])));
        assert_eq!(
            parse_run_args(&v(&["./-weird.sga"])),
            Ok(invocation(false, "./-weird.sga", &[]))
        );
    }

    #[test]
    fn an_unknown_flag_before_the_path_is_an_error_with_a_hint() {
        for bad in [
            "--allow-writes",
            "--allow-write=1",
            "-w",
            "--help",
            "-weird.sga",
        ] {
            let err = parse_run_args(&v(&[bad, "main.sga"])).unwrap_err();
            assert!(err.contains(&format!("'{}'", bad)), "{bad}: {err}");
            assert!(err.contains("./"), "нет подсказки про './': {err}");
            assert!(err.contains("--"), "{err}");
            assert!(err.contains(RUN_USAGE), "нет строки использования: {err}");
        }
        // Неизвестный флаг после допустимого — тоже ошибка.
        assert!(parse_run_args(&v(&["--allow-write", "--bogus", "main.sga"])).is_err());
    }

    #[test]
    fn flags_without_a_path_print_the_usage() {
        for rest in [
            v(&[]),
            v(&["--allow-write"]),
            v(&["--"]),
            v(&["--allow-write", "--"]),
        ] {
            assert_eq!(
                parse_run_args(&rest),
                Err(RUN_USAGE.to_string()),
                "аргументы: {:?}",
                rest
            );
        }
    }
}
