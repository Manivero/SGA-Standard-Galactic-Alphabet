//! sga-translit — препроцессор для удобного написания SGA-кода.
//!
//! Разработчик пишет `.sga.src` файл с ASCII-мнемониками ключевых слов
//! (let, fn, if, while, ...), а этот инструмент заменяет ТОЛЬКО эти слова
//! на настоящие SGA Unicode-кодпоинты из зарегистрированного блока UCSUR
//! "Standard Galactic" (U+EB40..U+EB59, см. src/sga_alphabet.rs основного
//! компилятора и docs/SGA_ALPHABET.md). Идентификаторы, строки и числа не
//! трогаются.
//!
//! Без этого шага писать .sga-код можно только напрямую через ввод
//! Unicode-символов (например, копированием из готового примера), что
//! неудобно без поддержки во входном методе/VS Code расширении.
//!
//! Использование: sga-translit <input.sga.src> <output.sga>
//!
//! FUSION-ПРИМЕЧАНИЕ: кодирование (`encode_word`) и распознавание
//! ключевых слов (`is_keyword`) ПЕРЕИСПОЛЬЗУЮТ основной крейт `sga`
//! (path-зависимость, см. Cargo.toml) вместо собственных копий
//! константы базового кодпоинта и списка ключевых слов. Раньше (в одной
//! из родительских веток) эти копии существовали независимо и
//! рассинхронизировались — переименование `SGA_BASE` в этом файле
//! случайно заменило 0xEB40 (верно) на 0xF100 (несуществующий диапазон
//! в `src/sga_alphabet.rs`), и инструмент начал генерировать `.sga`-файлы,
//! которые собственный лексер компилятора не распознаёт как
//! SGA-буквы вообще — баг был обнаружен и исправлен при слиянии (см.
//! MIGRATION_REPORT.md). Переиспользование делает класс багов
//! "транслитератор и компилятор расходятся в кодировании" структурно
//! невозможным на будущее.
//!
//! ИЗВЕСТНОЕ ОГРАНИЧЕНИЕ: транслитератор работает на уровне лексем без
//! учёта синтаксической роли слова. Если в `.sga.src` используется
//! ASCII-идентификатор (имя переменной/функции/параметра), который
//! текстуально совпадает с одним из ключевых слов языка (`in`, `for`,
//! `if`, `not`, `nil`, ...) без учёта регистра, он будет ОШИБОЧНО
//! заменён на SGA-кодпоинт ключевого слова, что сломает программу или
//! приведёт к трудноотлаживаемой ошибке лексера/парсера на выходном
//! `.sga`-файле. Сам язык SGA не резервирует эти слова как identifier
//! на уровне `src/lexer/mod.rs` (ASCII-идентификаторы и SGA-ключевые
//! слова занимают разные кодовые пространства и не могут физически
//! столкнуться в скомпилированном `.sga`-файле) — конфликт существует
//! только на этапе `.sga.src` → `.sga` транслитерации. Избегайте имён
//! переменных/функций, совпадающих (без учёта регистра) с: LET VAR
//! CONST FN RETURN IF ELSE WHILE FOR IN TRUE FALSE STRUCT PRINT BREAK
//! CONTINUE AND OR NOT NIL MUT IMPORT.

use sga::lexer::token::keyword_from_mnemonic;
use sga::sga_alphabet::encode_word;
use std::env;
use std::fs;

fn is_keyword(word: &str) -> bool {
    keyword_from_mnemonic(&word.to_ascii_uppercase()).is_some()
}

fn translit(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            // строковый литерал — копируем как есть, не трогаем содержимое
            out.push(c);
            i += 1;
            while i < chars.len() {
                out.push(chars[i]);
                let is_quote = chars[i] == '"';
                let is_escape = chars[i] == '\\' && i + 1 < chars.len();
                i += 1;
                if is_escape {
                    out.push(chars[i]);
                    i += 1;
                    continue;
                }
                if is_quote {
                    break;
                }
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if is_keyword(&word) {
                out.push_str(&encode_word(&word.to_ascii_uppercase()));
            } else {
                out.push_str(&word);
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("использование: sga-translit <input.sga.src> <output.sga>");
        std::process::exit(1);
    }
    let source = fs::read_to_string(&args[1]).unwrap_or_else(|e| {
        eprintln!("не удалось прочитать '{}': {}", args[1], e);
        std::process::exit(1);
    });
    let result = translit(&source);
    fs::write(&args[2], result).unwrap_or_else(|e| {
        eprintln!("не удалось записать '{}': {}", args[2], e);
        std::process::exit(1);
    });
    println!("записано: {}", args[2]);
}
