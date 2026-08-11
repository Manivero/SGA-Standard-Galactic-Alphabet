//! CLI компилятора/интерпретатора SGA.

use sga::{lexer, parser, sga_alphabet};

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

const VERSION: &str = "0.1.0";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        return ExitCode::FAILURE;
    }

    match args[1].as_str() {
        "run" => {
            if args.len() < 3 {
                eprintln!("использование: sga run <файл.sga>");
                return ExitCode::FAILURE;
            }
            cmd_run(&args[2])
        }
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
    eprintln!("  run <файл.sga>    скомпилировать и выполнить файл на встроенной VM");
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

fn cmd_run(path: &str) -> ExitCode {
    match sga::run_source_file(Path::new(path)) {
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
    let toml = "[package]\nname = \"my-sga-app\"\nversion = \"0.1.0\"\n\n[dependencies]\n\n[targets]\nplatforms = [\"windows\", \"linux\", \"macos\"]\narch = [\"x86_64\", \"arm64\"]\n\n[compiler]\nbackend = \"vm\"\n";
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
