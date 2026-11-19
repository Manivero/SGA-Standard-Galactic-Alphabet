//! Диагностики исходного текста БЕЗ исполнения (T016–T017, M007).
//!
//! `analyze_source` прогоняет текст через те же стадии, которые `sga run`
//! проходит до исполнения, — лексер, парсер, `semantic` и `typechecker`, — и
//! возвращает найденную ошибку вместе с диапазоном в исходном тексте. Функция
//! ничего не исполняет, не читает файлы и не пишет в stdout/stderr — это
//! основа языкового сервера (`lsp`), который вызывает её на каждое изменение
//! документа в редакторе.
//!
//! Каждая стадия останавливается на ПЕРВОЙ ошибке (восстановления нет), а
//! следующая стадия после ошибки не запускается, поэтому результат содержит
//! НЕ БОЛЕЕ ОДНОЙ диагностики. Порядок приоритета: ошибка лексера, ошибка
//! парсера, замечание об `IMPORT`, предупреждение о глубине дерева, ошибка
//! `semantic`, ошибка типов. Позиция ошибки `semantic` и типов — узел AST,
//! из-за которого она возникла (`SemError::span`, `TypeError::span`; правила
//! диапазонов узлов — в `ast::Span`).
//!
//! Анализ имён и типов сознательно НЕ выполняется в двух случаях:
//!   - в тексте есть `IMPORT`: сервер не разрешает импорты (не читает файлы),
//!     и имена из подключаемых модулей были бы ложно сочтены неопределёнными.
//!     Вместо ошибок — одно замечание (`Severity::Information`, `Code::Limit`)
//!     на первом `IMPORT`;
//!   - дерево глубже `MAX_ANALYSIS_DEPTH`: `semantic` и `typechecker`
//!     рекурсивны, их стек растёт пропорционально глубине дерева, а парсер не
//!     ограничивает длину цепочек (`1+1+…+1`, `a[0][0]…`). Вместо анализа —
//!     одно предупреждение (`Severity::Warning`, `Code::Limit`).
//!
//! Позиции — как у лексера: строки и колонки с единицы, колонка в СИМВОЛАХ
//! Unicode, строки по `\n`; конец диапазона — исключающий. Перевод в позиции
//! LSP (с нуля, единицы UTF-16) — в `lsp::position`.

use crate::ast::{exceeds_depth, Program, Span, StmtKind};
use crate::lexer::token::TokenSpan;
use crate::lexer::{LexError, Lexer};
use crate::parser::{ParseError, Parser};
use crate::semantic::Analyzer;
use crate::typechecker::Typechecker;

/// Максимальная длина `message` диагностики в символах. Сообщения парсера
/// содержат `Debug`-представление токена (`получено StringLit("...")`), а
/// сообщение лексера — декодированное слово целиком: без предела строковая
/// константа в мегабайт давала бы диагностику в мегабайт, которую редактор
/// получал бы на каждое нажатие клавиши. Более длинное сообщение обрезается
/// до этого числа символов, последний из которых — `…`.
pub const MAX_MESSAGE_CHARS: usize = 1000;

/// Наибольшая глубина AST (`ast::exceeds_depth`: корень — 1), которую
/// `analyze_source` отдаёт `semantic` и `typechecker`. Дерево глубже
/// пропускается с предупреждением `Code::Limit`.
///
/// Предел подобран замером (T017, отладочная сборка, худший из семи видов
/// цепочек: сложение, индексы, вызовы методов, `f()()…`, поля, цепочка под 70
/// вложенными `IF`, под 35 вложенными лямбдами): `semantic` расходует ≈ 5,9 КиБ
/// стека на уровень глубины, `typechecker` ≈ 6,1 КиБ — на пределе ≈ 12,6 МБ.
/// Запасной стек сервера — 64 МиБ (основной — 256 МиБ), запас на нём ≈ 5×;
/// в release-сборке кадры на порядок меньше. Предел одинаков для обеих
/// сборок: результат диагностики не должен зависеть от вида сборки.
pub const MAX_ANALYSIS_DEPTH: usize = 2000;

/// Важность диагностики (соответствует `DiagnosticSeverity` LSP: 1, 2 и 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    /// Замечание: анализ не нашёл ошибки, но сообщает, что часть проверок
    /// не выполнялась (файл с `IMPORT`).
    Information,
}

/// Источник диагностики (поле `code` в LSP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Ошибка лексера.
    Lexer,
    /// Ошибка парсера.
    Parser,
    /// Ошибка семантического анализа (имена, области видимости, владение).
    Semantic,
    /// Ошибка проверки типов.
    Type,
    /// Анализ пропущен из-за предела (слишком большой документ, слишком
    /// глубокое дерево) или невозможен без разрешения импортов (`IMPORT`).
    Limit,
    /// Внутренняя ошибка анализатора (паника) — заводится сервером.
    Internal,
}

impl Code {
    /// Строка, с которой код попадает в протокол.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Lexer => "lexer",
            Code::Parser => "parser",
            Code::Semantic => "semantic",
            Code::Type => "type",
            Code::Limit => "limit",
            Code::Internal => "internal",
        }
    }
}

/// Одна диагностика с диапазоном `[start, end)` в исходном тексте.
///
/// Строки и колонки — с единицы, колонка считается в СИМВОЛАХ. Для
/// диагностики в конце текста (`Eof`) диапазон пуст: `start == end`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: Code,
    /// Сообщение об ошибке БЕЗ префикса `[строка:колонка] ошибка …:` —
    /// позицию редактор показывает сам. Не длиннее `MAX_MESSAGE_CHARS`.
    pub message: String,
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
}

/// Анализирует исходный текст: лексер -> парсер -> [`IMPORT`?] -> [глубина?]
/// -> `semantic` -> `typechecker`. Возвращает пустой вектор, если текст
/// корректен, иначе — одну диагностику (порядок приоритета — в документации
/// модуля). Если стадия нашла ошибку, следующие не запускаются. `IMPORT` не
/// резолвится (файловой системы функция не касается).
///
/// Глубина рекурсии парсера ограничена (`MAX_PARSE_DEPTH`), глубина рекурсии
/// `semantic` и `typechecker` — `MAX_ANALYSIS_DEPTH`. Но ASTом из очень
/// длинной цепочки операторов (`1+1+...+1`) приходится рекурсивно
/// освобождаться даже при пропущенном анализе, поэтому для входов в сотни
/// тысяч операндов вызывающему нужен стек больше стандартного (см. TD-018;
/// сервер `lsp` работает в потоке с большим стеком и ограничивает размер
/// документа).
pub fn analyze_source(source: &str) -> Vec<Diagnostic> {
    let (tokens, spans) = match Lexer::new(source).tokenize_with_spans() {
        Ok(lexed) => lexed,
        Err(e) => return vec![lex_diagnostic(source, &e)],
    };
    let program = match Parser::new(tokens).parse_program() {
        Ok(program) => program,
        Err(e) => return vec![parse_diagnostic(&spans, &e)],
    };
    analyze_program(&program, &spans).into_iter().collect()
}

/// Стадии после парсера. Возвращает первую найденную диагностику.
fn analyze_program(program: &Program, spans: &[TokenSpan]) -> Option<Diagnostic> {
    // `IMPORT` допустим только на верхнем уровне (проверяет парсер)
    if let Some(stmt) = program
        .iter()
        .find(|stmt| matches!(stmt.kind, StmtKind::Import(_)))
    {
        return Some(node_diagnostic(
            spans,
            stmt.span,
            Severity::Information,
            Code::Limit,
            IMPORT_NOTICE,
        ));
    }
    if let Some(stmt) = program
        .iter()
        .find(|stmt| exceeds_depth(stmt, MAX_ANALYSIS_DEPTH))
    {
        return Some(node_diagnostic(
            spans,
            stmt.span,
            Severity::Warning,
            Code::Limit,
            &depth_message(),
        ));
    }
    if let Err(e) = Analyzer::new().analyze(program) {
        return Some(node_diagnostic(
            spans,
            e.span,
            Severity::Error,
            Code::Semantic,
            &e.message,
        ));
    }
    if let Err(e) = Typechecker::new().analyze(program) {
        return Some(node_diagnostic(
            spans,
            e.span,
            Severity::Error,
            Code::Type,
            &e.message,
        ));
    }
    None
}

/// Замечание на первом `IMPORT`: семантический анализ и проверка типов
/// пропущены.
const IMPORT_NOTICE: &str = "семантический анализ и проверка типов не выполняются для файлов с IMPORT: языковой сервер не разрешает импорты, и имена из подключаемых модулей были бы ошибочно сочтены неопределёнными (синтаксические ошибки по-прежнему показываются)";

/// Предупреждение о слишком глубоком дереве.
fn depth_message() -> String {
    format!(
        "дерево программы глубже {MAX_ANALYSIS_DEPTH} уровней: семантический анализ и проверка типов пропущены, чтобы не исчерпать стек языкового сервера (разбейте длинное выражение на части)"
    )
}

/// Диагностика по узлу AST. Индексы токенов `span` переводятся в позиции по
/// таблице границ токенов `spans`. Узел без позиции (`Span::NONE`) или
/// индексы вне таблицы — защитная ветка, при разборе текста недостижимая:
/// пустой диапазон в начале документа.
fn node_diagnostic(
    spans: &[TokenSpan],
    span: Span,
    severity: Severity,
    code: Code,
    message: &str,
) -> Diagnostic {
    let range = span.resolve(spans).unwrap_or(TokenSpan {
        line: 1,
        col: 1,
        end_line: 1,
        end_col: 1,
    });
    Diagnostic {
        severity,
        code,
        message: bounded_message(message),
        start_line: range.line,
        start_col: range.col,
        end_line: range.end_line,
        end_col: range.end_col,
    }
}

/// Диагностика по ошибке лексера. Диапазон — от позиции ошибки до позиции
/// лексера в момент ошибки (`LexError::end_*`): слово из SGA-букв и
/// числовой литерал — целиком, одиночный символ — один символ. Но не дальше
/// конца строки, на которой ошибка началась: незакрытая строковая константа
/// «съедает» весь остаток файла, а подчёркивать его целиком бессмысленно.
/// Диапазон не короче одного символа.
fn lex_diagnostic(source: &str, e: &LexError) -> Diagnostic {
    let line_end_col = line_char_count(source, e.line) + 1;
    let (mut end_line, mut end_col) = (e.end_line, e.end_col);
    if end_line != e.line || end_col > line_end_col {
        end_line = e.line;
        end_col = line_end_col;
    }
    if end_col <= e.col {
        end_col = e.col + 1;
    }
    Diagnostic {
        severity: Severity::Error,
        code: Code::Lexer,
        message: bounded_message(&e.message),
        start_line: e.line,
        start_col: e.col,
        end_line,
        end_col,
    }
}

/// Диагностика по ошибке парсера: диапазон — токен, на котором остановился
/// разбор (для `Eof` — пустой диапазон в конце текста). Парсер сообщает
/// только позицию НАЧАЛА токена (`ParseError` не несёт конца: рост этой
/// структуры съедает запас стека парсера, см. TD-018), поэтому конец берётся
/// из таблицы границ токенов, которую построил лексер.
fn parse_diagnostic(spans: &[TokenSpan], e: &ParseError) -> Diagnostic {
    let (end_line, end_col) = token_end(spans, e.line, e.col);
    Diagnostic {
        severity: Severity::Error,
        code: Code::Parser,
        message: bounded_message(&e.message),
        start_line: e.line,
        start_col: e.col,
        end_line,
        end_col,
    }
}

/// Конец токена, начинающегося в `(line, col)`. Начала токенов строго
/// возрастают (токен непуст, пуст только `Eof`, он последний), поэтому
/// поиск двоичный. Парсер сообщает позиции только существующих токенов; если
/// токена с таким началом всё же нет — диапазон в один символ.
fn token_end(spans: &[TokenSpan], line: usize, col: usize) -> (usize, usize) {
    match spans.binary_search_by(|span| (span.line, span.col).cmp(&(line, col))) {
        Ok(index) => (spans[index].end_line, spans[index].end_col),
        Err(_) => (line, col + 1),
    }
}

/// Число символов в строке `line` (1-based; строки — по `\n`, как в
/// лексере; `\r` перед `\n` остаётся в строке). Нет такой строки — 0.
fn line_char_count(source: &str, line: usize) -> usize {
    source
        .split('\n')
        .nth(line.saturating_sub(1))
        .map_or(0, |l| l.chars().count())
}

/// Обрезает сообщение до `MAX_MESSAGE_CHARS` символов (см. константу).
fn bounded_message(message: &str) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        return message.to_string();
    }
    let cut = message
        .char_indices()
        .nth(MAX_MESSAGE_CHARS - 1)
        .map_or(message.len(), |(index, _)| index);
    format!("{}…", &message[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sga_alphabet::{encode_word, is_sga_letter};

    fn kw(word: &str) -> String {
        encode_word(word)
    }

    /// Единственная диагностика или провал теста.
    fn only(source: &str) -> Diagnostic {
        let mut all = analyze_source(source);
        assert_eq!(
            all.len(),
            1,
            "ожидалась ровно одна диагностика для {source:?}, получено: {all:?}"
        );
        all.remove(0)
    }

    fn range(d: &Diagnostic) -> ((usize, usize), (usize, usize)) {
        ((d.start_line, d.start_col), (d.end_line, d.end_col))
    }

    // ── корректные программы ─────────────────────────────────────────

    #[test]
    fn valid_sources_produce_no_diagnostics() {
        let let_kw = kw("LET");
        let fn_kw = kw("FN");
        let print_kw = kw("PRINT");
        let return_kw = kw("RETURN");
        let sources = [
            String::new(),
            "   \n\t\n".to_string(),
            "// только комментарий".to_string(),
            format!("{let_kw} x = 1;"),
            format!("{let_kw} x = 1 + 2 * 3; {print_kw}(x);"),
            format!("{fn_kw} add(a, b) {{ {return_kw} a + b; }}"),
            format!("{let_kw} s = \"юникод ✓ 😀\"; // комментарий 😀"),
            // без завершающего перевода строки и с CRLF
            format!("{let_kw} a = 1;\r\n{let_kw} b = 2;"),
        ];
        for source in &sources {
            assert_eq!(
                analyze_source(source),
                Vec::<Diagnostic>::new(),
                "источник {source:?}"
            );
        }
    }

    #[test]
    fn every_example_program_is_clean() {
        // Примеры, которые `sga run` исполняет, проходят ВСЕ стадии (лексер,
        // парсер, semantic, типы) без диагностик. Единственное исключение —
        // `modules.sga`: он подключает модуль через IMPORT, для такого файла
        // анализ имён и типов не выполняется и остаётся одно замечание.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
        let mut checked = 0;
        let mut with_import = Vec::new();
        for entry in std::fs::read_dir(dir).expect("каталог examples/ обязан существовать")
        {
            let path = entry.expect("запись каталога").path();
            if path.extension().and_then(|e| e.to_str()) != Some("sga") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("чтение примера");
            let diags = analyze_source(&source);
            if !diags.is_empty() {
                assert_eq!(diags.len(), 1, "пример {}: {diags:?}", path.display());
                assert_eq!(
                    (diags[0].severity, diags[0].code),
                    (Severity::Information, Code::Limit),
                    "пример {} должен проходить анализ без диагностик: {diags:?}",
                    path.display()
                );
                with_import.push(path.file_name().unwrap().to_string_lossy().into_owned());
            }
            checked += 1;
        }
        assert!(
            checked >= 17,
            "ожидалось не меньше 17 примеров, проверено {checked}"
        );
        assert_eq!(with_import, ["modules.sga"]);
    }

    // ── ошибки лексера ───────────────────────────────────────────────

    #[test]
    fn unknown_sga_word_is_underlined_as_a_whole() {
        let d = only(&format!("  {}", kw("XYZ")));
        assert_eq!(d.code, Code::Lexer);
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(range(&d), ((1, 3), (1, 6)));
        assert!(d.message.contains("XYZ"), "{}", d.message);
    }

    #[test]
    fn lone_bang_and_unexpected_chars_are_one_character_wide() {
        let d = only("!");
        assert_eq!((d.code, range(&d)), (Code::Lexer, ((1, 1), (1, 2))));
        let d = only("1 + @");
        assert_eq!((d.code, range(&d)), (Code::Lexer, ((1, 5), (1, 6))));
        // символ вне BMP — один СИМВОЛ (в колонках лексера), хотя две
        // единицы UTF-16 (перевод — в lsp::position)
        let d = only("x 😀");
        assert_eq!((d.code, range(&d)), (Code::Lexer, ((1, 3), (1, 4))));
        assert!(d.message.contains("1F600"), "{}", d.message);
    }

    #[test]
    fn oversized_int_literal_is_underlined_as_a_whole() {
        let d = only("LET_x = 99999999999999999999;");
        // идентификатор, `=`, затем литерал: начало на колонке 9, 20 цифр
        assert_eq!(d.code, Code::Lexer);
        assert_eq!(range(&d), ((1, 9), (1, 29)));
        assert!(d.message.contains("int-литерал"), "{}", d.message);
    }

    #[test]
    fn unclosed_string_is_underlined_to_the_end_of_its_first_line() {
        let d = only("x = \"abc");
        assert_eq!((d.code, range(&d)), (Code::Lexer, ((1, 5), (1, 9))));

        // Строка тянется через несколько строк до конца файла: подчёркивается
        // только остаток строки, где она началась.
        let source = "ok = 1;\ns = \"abc\nvery long second line\nthird";
        let d = only(source);
        assert_eq!(d.code, Code::Lexer);
        assert_eq!(range(&d), ((2, 5), (2, 9)), "диагностика: {d:?}");
        assert!(d.message.contains("незакрыт"), "{}", d.message);

        // Незакрытый escape в конце файла — тот же диапазон правила
        let d = only("\"abc\\");
        assert_eq!(range(&d), ((1, 1), (1, 6)));
    }

    #[test]
    fn lexer_error_in_a_crlf_document_is_clamped_to_the_line() {
        // `\r` остаётся в строке и считается символом: конец диапазона —
        // колонка сразу после него; клиент обрежет её до конца строки.
        let d = only("s = \"abc\r\nnext");
        assert_eq!(range(&d), ((1, 5), (1, 10)));
    }

    #[test]
    fn a_degenerate_lexer_error_range_is_widened_to_one_character() {
        // Настоящий лексер всегда потребляет хотя бы один символ ошибки, но
        // `lex_diagnostic` обещает диапазон не короче одного символа и для
        // вручную собранной `LexError` (поля публичны): конец не раньше и не
        // равен началу.
        let error = |line, col, end_line, end_col| LexError {
            message: "m".to_string(),
            line,
            col,
            end_line,
            end_col,
        };
        let widened = |e: LexError| {
            let d = lex_diagnostic("abcdef\nxy", &e);
            ((d.start_line, d.start_col), (d.end_line, d.end_col))
        };
        assert_eq!(
            widened(error(1, 3, 1, 3)),
            ((1, 3), (1, 4)),
            "пустой диапазон"
        );
        assert_eq!(
            widened(error(1, 3, 1, 1)),
            ((1, 3), (1, 4)),
            "конец раньше начала"
        );
        assert_eq!(widened(error(2, 2, 2, 2)), ((2, 2), (2, 3)));
        // строка, которой нет в тексте: конец строки — колонка 1, диапазон
        // всё равно в один символ
        assert_eq!(widened(error(9, 1, 9, 1)), ((9, 1), (9, 2)));
        // нормальный диапазон не трогается
        assert_eq!(widened(error(1, 2, 1, 5)), ((1, 2), (1, 5)));
    }

    #[test]
    fn parser_is_not_run_when_the_lexer_fails() {
        // Вторая ошибка (парсера) не должна просочиться: лексер упал первым.
        let source = format!("{} @", kw("LET"));
        let d = only(&source);
        assert_eq!(d.code, Code::Lexer);
        assert_eq!(range(&d), ((1, 5), (1, 6)));
    }

    // ── ошибки парсера ───────────────────────────────────────────────

    #[test]
    fn parse_error_underlines_the_offending_token() {
        let let_kw = kw("LET");
        // число вместо имени: колонка 5, ширина 5
        let d = only(&format!("{let_kw} 12345 = 1;"));
        assert_eq!(d.code, Code::Parser);
        assert_eq!(range(&d), ((1, 5), (1, 10)));
        assert!(d.message.contains("идентификатор"), "{}", d.message);

        // слово из SGA-букв вместо имени: три символа
        let d = only(&format!("{let_kw} {let_kw} = 1;"));
        assert_eq!((d.code, range(&d)), (Code::Parser, ((1, 5), (1, 8))));

        // строка вместо имени: вместе с кавычками
        let d = only(&format!("{let_kw} \"ab\" = 1;"));
        assert_eq!((d.code, range(&d)), (Code::Parser, ((1, 5), (1, 9))));

        // двухсимвольные операторы
        let d = only(&format!("{let_kw} x = == 1;"));
        assert_eq!((d.code, range(&d)), (Code::Parser, ((1, 9), (1, 11))));
        let d = only(&format!("{let_kw} x = -> 1;"));
        assert_eq!((d.code, range(&d)), (Code::Parser, ((1, 9), (1, 11))));
    }

    #[test]
    fn parse_error_at_end_of_text_is_an_empty_range() {
        let let_kw = kw("LET");
        // `LET x` без `=`: парсер остановился на Eof сразу после `x`
        let d = only(&format!("{let_kw} x"));
        assert_eq!(d.code, Code::Parser);
        assert_eq!(range(&d), ((1, 6), (1, 6)));

        // Eof после завершающего перевода строки — пустая последняя строка
        let d = only(&format!("{let_kw} x\n"));
        assert_eq!(range(&d), ((2, 1), (2, 1)));
    }

    #[test]
    fn parse_error_on_a_multiline_string_token_spans_its_lines() {
        let let_kw = kw("LET");
        // строка-токен занимает две строки; парсер остановился на ней
        let d = only(&format!("{let_kw} \"ab\ncd\" = 1;"));
        assert_eq!(d.code, Code::Parser);
        assert_eq!(range(&d), ((1, 5), (2, 4)));
    }

    #[test]
    fn parse_message_has_no_position_prefix() {
        let d = only(&format!("{} 1", kw("LET")));
        assert!(!d.message.starts_with('['), "{}", d.message);
        assert!(!d.message.contains("ошибка парсера"), "{}", d.message);
        let d = only("@");
        assert!(!d.message.starts_with('['), "{}", d.message);
        assert!(!d.message.contains("ошибка лексера"), "{}", d.message);
    }

    // ── semantic и типы (T017) ───────────────────────────────────────

    #[test]
    fn semantic_error_is_underlined_at_the_offending_node() {
        let (let_kw, print_kw) = (kw("LET"), kw("PRINT"));
        let d = only(&format!("{let_kw} a = 1; {print_kw}(b);"));
        assert_eq!((d.severity, d.code), (Severity::Error, Code::Semantic));
        assert_eq!(range(&d), ((1, 18), (1, 19)));
        // сообщение — без префикса `[строка:колонка] ошибка семантики:`
        assert_eq!(d.message, "неопределённая переменная 'b'");
    }

    #[test]
    fn type_error_is_underlined_at_the_offending_node() {
        let let_kw = kw("LET");
        let d = only(&format!("{let_kw} x: int = \"s\";"));
        assert_eq!((d.severity, d.code), (Severity::Error, Code::Type));
        // диапазон — строковый литерал целиком, с кавычками
        assert_eq!(range(&d), ((1, 14), (1, 17)));
        assert!(
            d.message
                .starts_with("переменная 'x' объявлена с типом int"),
            "{}",
            d.message
        );
        assert!(!d.message.contains("ошибка типов"), "{}", d.message);
    }

    #[test]
    fn a_multiline_node_gives_a_multiline_range() {
        let (let_kw, struct_kw) = (kw("LET"), kw("STRUCT"));
        let source = format!("{struct_kw} P {{ x }}\n{let_kw} p = P {{\n    y: 1\n}};");
        let d = only(&source);
        assert_eq!(d.code, Code::Semantic);
        // весь литерал: от имени типа (2:9) до закрывающей скобки (4:1)
        assert_eq!(range(&d), ((2, 9), (4, 2)));
        assert!(d.message.contains("не имеет поля 'y'"), "{}", d.message);
    }

    #[test]
    fn semantic_ranges_in_a_crlf_document_use_the_lexer_columns() {
        let (let_kw, print_kw) = (kw("LET"), kw("PRINT"));
        let d = only(&format!("{let_kw} a = 1;\r\n{print_kw}(b);\r\n"));
        assert_eq!((d.code, range(&d)), (Code::Semantic, ((2, 7), (2, 8))));
    }

    #[test]
    fn stages_run_in_priority_order() {
        let (let_kw, print_kw) = (kw("LET"), kw("PRINT"));
        // ошибка типов стоит РАНЬШЕ в тексте, но semantic — предыдущая стадия:
        // она и найдена, типы не проверялись
        let type_then_semantic = format!("{let_kw} x: int = \"s\";\n{print_kw}(b);");
        let d = only(&type_then_semantic);
        assert_eq!((d.code, d.start_line), (Code::Semantic, 2));
        // синтаксическая ошибка скрывает и semantic, и типы
        let d = only(&format!("{type_then_semantic}\n{let_kw} = 1;"));
        assert_eq!(d.code, Code::Parser);
        // ошибка лексера — всё остальное
        let d = only(&format!("{type_then_semantic}\n@"));
        assert_eq!(d.code, Code::Lexer);
    }

    #[test]
    fn long_semantic_messages_are_truncated() {
        let print_kw = kw("PRINT");
        let name = "a".repeat(5000);
        let d = only(&format!("{print_kw}({name});"));
        assert_eq!(d.code, Code::Semantic);
        assert_eq!(d.message.chars().count(), MAX_MESSAGE_CHARS);
        assert!(d.message.ends_with('…'));
        assert!(d.message.starts_with("неопределённая переменная 'aaa"));
        // диапазон — весь идентификатор, сколько бы он ни был длинным
        assert_eq!(range(&d), ((1, 7), (1, 5007)));
    }

    #[test]
    fn a_node_without_a_position_gets_an_empty_range_at_the_start() {
        // Защитная ветка: у узла, построенного из текста, позиция есть всегда
        let table = [TokenSpan {
            line: 3,
            col: 4,
            end_line: 3,
            end_col: 9,
        }];
        for span in [Span::NONE, Span::new(5, 9)] {
            let d = node_diagnostic(&table, span, Severity::Error, Code::Semantic, "m");
            assert_eq!(range(&d), ((1, 1), (1, 1)), "{span:?}");
        }
        let d = node_diagnostic(&table, Span::new(0, 0), Severity::Error, Code::Type, "m");
        assert_eq!(range(&d), ((3, 4), (3, 9)));
    }

    #[test]
    fn every_code_has_its_protocol_string() {
        let all = [
            (Code::Lexer, "lexer"),
            (Code::Parser, "parser"),
            (Code::Semantic, "semantic"),
            (Code::Type, "type"),
            (Code::Limit, "limit"),
            (Code::Internal, "internal"),
        ];
        for (code, text) in all {
            assert_eq!(code.as_str(), text);
        }
    }

    // ── IMPORT (T017) ────────────────────────────────────────────────

    #[test]
    fn a_document_with_import_gets_one_notice_instead_of_name_and_type_errors() {
        let (import_kw, let_kw, print_kw) = (kw("IMPORT"), kw("LET"), kw("PRINT"));
        // Ниже — необъявленное имя и ошибка типов: модуль мог бы объявить
        // имя, а ошибки в остальном файле не показываются без разбора модуля.
        let source =
            format!("{import_kw} \"m.sga\";\n{let_kw} x: int = \"s\";\n{print_kw}(from_module);");
        let d = only(&source);
        assert_eq!(
            (d.severity, d.code),
            (Severity::Information, Code::Limit),
            "{d:?}"
        );
        assert_eq!(range(&d), ((1, 1), (1, 16)));
        assert!(d.message.contains("IMPORT"), "{}", d.message);
    }

    #[test]
    fn the_import_notice_is_on_the_first_import_wherever_it_is() {
        let (import_kw, let_kw) = (kw("IMPORT"), kw("LET"));
        let source = format!("{let_kw} a = 1;\n\n  {import_kw} \"a.sga\";\n{import_kw} \"b.sga\";");
        let d = only(&source);
        assert_eq!((d.severity, d.code), (Severity::Information, Code::Limit));
        assert_eq!(range(&d), ((3, 3), (3, 18)));
    }

    #[test]
    fn syntax_errors_take_priority_over_the_import_notice() {
        let (import_kw, let_kw) = (kw("IMPORT"), kw("LET"));
        let d = only(&format!("{import_kw} \"m.sga\";\n{let_kw} = 1;"));
        assert_eq!(d.code, Code::Parser);
        // IMPORT вне верхнего уровня парсер отвергает сам: ошибка, не замечание
        let nested = format!("{} {} {{ {import_kw} \"m.sga\"; }}", kw("IF"), kw("TRUE"));
        let d = only(&nested);
        assert_eq!((d.severity, d.code), (Severity::Error, Code::Parser));
    }

    #[test]
    fn a_document_without_import_is_analyzed_completely() {
        // ровно те же имена, что в тесте с IMPORT, но без него: ошибка семантики
        let (let_kw, print_kw) = (kw("LET"), kw("PRINT"));
        let d = only(&format!("{let_kw} x: int = 1;\n{print_kw}(from_module);"));
        assert_eq!((d.severity, d.code), (Severity::Error, Code::Semantic));
    }

    // ── предел длины сообщения ───────────────────────────────────────

    #[test]
    fn long_messages_are_truncated_with_an_ellipsis() {
        // Парсер вставляет в сообщение Debug-представление токена целиком.
        let huge = "a".repeat(50_000);
        let d = only(&format!("{} \"{huge}\" = 1;", kw("LET")));
        assert_eq!(d.code, Code::Parser);
        assert_eq!(d.message.chars().count(), MAX_MESSAGE_CHARS);
        assert!(d.message.ends_with('…'), "{}", d.message);
        assert!(d.message.starts_with("ожидался идентификатор"));

        // слово из SGA-букв целиком попадает в сообщение лексера
        let long_word = "\u{EB40}".repeat(5000);
        let d = only(&long_word);
        assert_eq!(d.code, Code::Lexer);
        assert_eq!(d.message.chars().count(), MAX_MESSAGE_CHARS);
        assert!(d.message.ends_with('…'));
    }

    #[test]
    fn short_messages_are_not_touched_and_truncation_is_char_safe() {
        assert_eq!(bounded_message("коротко"), "коротко");
        let exactly = "я".repeat(MAX_MESSAGE_CHARS);
        assert_eq!(bounded_message(&exactly), exactly);
        let one_more = "я".repeat(MAX_MESSAGE_CHARS + 1);
        let cut = bounded_message(&one_more);
        assert_eq!(cut.chars().count(), MAX_MESSAGE_CHARS);
        assert_eq!(
            cut.chars().filter(|c| *c == 'я').count(),
            MAX_MESSAGE_CHARS - 1
        );
        assert!(cut.ends_with('…'));
        // многобайтные символы вне BMP режутся по границе символа
        let astral = "😀".repeat(MAX_MESSAGE_CHARS + 5);
        assert_eq!(bounded_message(&astral).chars().count(), MAX_MESSAGE_CHARS);
    }

    // ── глубина (защита парсера, T016) ───────────────────────────────

    /// Запускает `f` в потоке со стеком `mib` МиБ. Переполнение стека
    /// завершает весь процесс тестов, а не один тест, поэтому размер задаёт
    /// проверяемую границу.
    fn on_stack<R: Send + 'static>(mib: usize, f: impl FnOnce() -> R + Send + 'static) -> R {
        std::thread::Builder::new()
            .stack_size(mib * 1024 * 1024)
            .spawn(f)
            .expect("поток")
            .join()
            .expect("поток не должен падать")
    }

    /// Поток со стеком в 2 МиБ — столько получает поток тестового харнесса;
    /// на нём защита глубины парсера обязана срабатывать раньше, чем кончится
    /// стек.
    fn on_small_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        on_stack(2, f)
    }

    #[test]
    fn long_unary_and_assignment_chains_are_a_parse_error_not_a_stack_overflow() {
        let let_kw = kw("LET");
        let not_kw = kw("NOT");
        let inputs = [
            format!("{let_kw} x = {}1;", "-".repeat(100_000)),
            format!("{let_kw} x = {}y;", format!("{not_kw} ").repeat(50_000)),
            format!("{}1;", "a = ".repeat(100_000)),
        ];
        for source in inputs {
            let diags = on_small_stack(move || analyze_source(&source));
            assert_eq!(diags.len(), 1, "{diags:?}");
            assert_eq!(diags[0].code, Code::Parser);
            assert!(
                diags[0].message.contains("максимальная глубина"),
                "{}",
                diags[0].message
            );
        }
    }

    #[test]
    fn reasonable_unary_and_assignment_chains_still_parse() {
        let let_kw = kw("LET");
        let not_kw = kw("NOT");
        let sources = [
            format!("{let_kw} x = - - - 1;"),
            format!("{let_kw} x = -----------------------1;"),
            format!("{let_kw} x = {not_kw} {not_kw} {not_kw} y;"),
            format!("{let_kw} x = {not_kw} - {not_kw} - y;"),
            "a = b = c = d = 1;".to_string(),
            // запас относительно предела: 60 операторов подряд
            format!("{let_kw} x = {}1;", "-".repeat(60)),
            format!("{}1;", "a = ".repeat(60)),
        ];
        for source in sources {
            let diags = on_small_stack({
                let source = source.clone();
                move || analyze_source(&source)
            });
            // Тест защит ПАРСЕРА: важно, что цепочки разбираются. Осмысленность
            // программ для semantic и типов (необъявленные `a`, `y`) здесь
            // ни при чём — стадии после парсера не должны считаться.
            let syntax: Vec<&Diagnostic> = diags
                .iter()
                .filter(|d| matches!(d.code, Code::Lexer | Code::Parser))
                .collect();
            assert!(syntax.is_empty(), "источник {source:?}: {syntax:?}");
        }
    }

    #[test]
    fn reasonable_chains_pass_the_whole_analysis() {
        // Те же цепочки унарных операторов и присваиваний, но в корректных
        // программах (имена объявлены, типы согласованы): весь конвейер —
        // лексер, парсер, semantic, типы — не даёт ни одной диагностики. Это
        // прежняя (T016) строгость теста выше — «диагностик нет вовсе».
        let (let_kw, var_kw, not_kw) = (kw("LET"), kw("VAR"), kw("NOT"));
        let sources = [
            format!("{let_kw} x = - - - 1;"),
            format!("{let_kw} x = -----------------------1;"),
            format!("{let_kw} y = 1; {let_kw} x = {not_kw} {not_kw} {not_kw} y;"),
            format!("{let_kw} y = 1; {let_kw} x = {not_kw} {not_kw} - - y;"),
            format!(
                "{var_kw} a = 0; {var_kw} b = 0; {var_kw} c = 0; {var_kw} d = 0; a = b = c = d = 1;"
            ),
            // запас относительно предела: 60 операторов подряд
            format!("{let_kw} x = {}1;", "-".repeat(60)),
            format!("{var_kw} a = 0; {}1;", "a = ".repeat(60)),
        ];
        for source in sources {
            let diags = on_small_stack({
                let source = source.clone();
                move || analyze_source(&source)
            });
            assert!(diags.is_empty(), "источник {source:?}: {diags:?}");
        }
    }

    // ── глубина дерева для semantic и типов (T017) ───────────────────

    /// Виды конструкций, глубину AST которых парсер не ограничивает (цепочки
    /// строятся циклом), и глубокие вложения вокруг цепочки.
    #[derive(Debug, Clone, Copy)]
    enum Chain {
        Add,
        Index,
        Method,
        CallExpr,
        Field,
        /// цепочка внутри 70 вложенных `IF` (предел парсера — 80)
        UnderIfs,
        /// цепочка внутри 35 вложенных лямбд (каждая — два уровня глубины)
        UnderLambdas,
    }

    const CHAINS: [Chain; 7] = [
        Chain::Add,
        Chain::Index,
        Chain::Method,
        Chain::CallExpr,
        Chain::Field,
        Chain::UnderIfs,
        Chain::UnderLambdas,
    ];

    /// Исходник с цепочкой из `links` звеньев поверх `bottom` — самого
    /// левого (глубокого) операнда. Объявлены `a` (массив) и `f` (замыкание).
    fn chain_source(kind: Chain, links: usize, bottom: &str) -> String {
        let (let_kw, fn_kw, return_kw, if_kw, true_kw) =
            (kw("LET"), kw("FN"), kw("RETURN"), kw("IF"), kw("TRUE"));
        let chain = |links: usize| match kind {
            Chain::Add | Chain::UnderIfs | Chain::UnderLambdas => {
                format!("{bottom}{}", "+1".repeat(links))
            }
            Chain::Index => format!("{bottom}{}", "[0]".repeat(links)),
            Chain::Method => format!("{bottom}{}", ".m()".repeat(links)),
            Chain::CallExpr => format!("{bottom}{}", "()".repeat(links)),
            Chain::Field => format!("{bottom}{}", ".b".repeat(links)),
        };
        let prefix = format!("{let_kw} a = [1]; {let_kw} f = {fn_kw}() {{ {return_kw} 1; }};\n");
        let statement = match kind {
            Chain::UnderIfs => format!(
                "{}{let_kw} x = {};{}",
                format!("{if_kw} {true_kw} {{ ").repeat(70),
                chain(links),
                " }".repeat(70)
            ),
            Chain::UnderLambdas => format!(
                "{let_kw} x = {}{}{};",
                format!("{fn_kw}() {{ {return_kw} ").repeat(35),
                chain(links),
                "; }".repeat(35)
            ),
            _ => format!("{let_kw} x = {};", chain(links)),
        };
        format!("{prefix}{statement}")
    }

    /// Самый левый операнд, который объявлен (`a` — массив, `1` — число).
    fn defined_bottom(kind: Chain) -> &'static str {
        match kind {
            Chain::Add | Chain::UnderIfs | Chain::UnderLambdas => "1",
            _ => "a",
        }
    }

    /// Наибольшее число звеньев, при котором последний оператор не глубже
    /// `MAX_ANALYSIS_DEPTH` (по `ast::exceeds_depth`, то есть по тому же
    /// определению глубины, что у `analyze_source`).
    fn max_links_within_limit(kind: Chain) -> usize {
        let too_deep = |links: usize| {
            let source = chain_source(kind, links, "zz");
            let tokens = Lexer::new(&source).tokenize().expect("лексинг");
            let program = Parser::new(tokens).parse_program().expect("парсинг");
            exceeds_depth(program.last().expect("оператор"), MAX_ANALYSIS_DEPTH)
        };
        let (mut fits, mut too_much) = (1, MAX_ANALYSIS_DEPTH + 10);
        assert!(!too_deep(fits), "{kind:?}: одно звено глубже предела");
        assert!(too_deep(too_much), "{kind:?}: цепочка не растёт в глубину");
        while too_much - fits > 1 {
            let middle = (fits + too_much) / 2;
            if too_deep(middle) {
                too_much = middle;
            } else {
                fits = middle;
            }
        }
        fits
    }

    #[test]
    fn analysis_runs_at_exactly_the_depth_limit_and_is_skipped_one_level_deeper() {
        on_stack(64, || {
            for kind in CHAINS {
                let links = max_links_within_limit(kind);
                // На пределе анализ ВЫПОЛНЯЕТСЯ: необъявленное самое левое имя
                // `zz` (дно цепочки, самый глубокий узел) найдено semantic
                let source = chain_source(kind, links, "zz");
                let at_limit = analyze_source(&source);
                assert_eq!(at_limit.len(), 1, "{kind:?}: {at_limit:?}");
                assert_eq!(
                    (at_limit[0].severity, at_limit[0].code),
                    (Severity::Error, Code::Semantic),
                    "{kind:?}: {at_limit:?}"
                );
                // На единицу глубже анализа нет: одно предупреждение
                let source = chain_source(kind, links + 1, "zz");
                let deeper = analyze_source(&source);
                assert_eq!(deeper.len(), 1, "{kind:?}: {deeper:?}");
                let d = &deeper[0];
                assert_eq!(
                    (d.severity, d.code),
                    (Severity::Warning, Code::Limit),
                    "{kind:?}: {deeper:?}"
                );
                assert!(
                    d.message.contains(&MAX_ANALYSIS_DEPTH.to_string()),
                    "{}",
                    d.message
                );
                // диапазон — последний оператор целиком (вторая строка)
                assert_eq!((d.start_line, d.start_col), (2, 1), "{kind:?}");
                assert_eq!(d.end_line, 2, "{kind:?}");
                let last_line = source.split('\n').nth(1).unwrap();
                assert_eq!(d.end_col, last_line.chars().count() + 1, "{kind:?}");
            }
        });
    }

    #[test]
    fn the_depth_warning_is_on_the_first_deep_statement_and_hides_other_errors() {
        on_stack(64, || {
            let print_kw = kw("PRINT");
            let deep = chain_source(Chain::Add, MAX_ANALYSIS_DEPTH + 5, "zz");
            // `deep` — две строки (объявления + оператор); берём последнюю
            let deep_statement = deep.split('\n').nth(1).unwrap();
            // оператор 1: ошибка semantic; 2 и 3: слишком глубокие
            let source = format!("{print_kw}(b);\n{deep_statement}\n{deep_statement}");
            let diags = analyze_source(&source);
            assert_eq!(diags.len(), 1, "{diags:?}");
            let d = &diags[0];
            assert_eq!((d.severity, d.code), (Severity::Warning, Code::Limit));
            assert_eq!((d.start_line, d.start_col), (2, 1));
            assert_eq!(d.end_line, 2);
        });
    }

    #[test]
    fn a_syntax_error_beats_the_depth_warning() {
        on_stack(64, || {
            let deep = chain_source(Chain::Add, MAX_ANALYSIS_DEPTH + 5, "zz");
            let d = only(&format!("{deep}\n{} = 1;", kw("LET")));
            assert_eq!(d.code, Code::Parser);
        });
    }

    #[test]
    fn analysis_at_the_depth_limit_fits_into_a_quarter_of_the_fallback_stack() {
        // Запасной стек потока сервера — 64 МиБ. Предел глубины подобран так,
        // чтобы анализ на пределе занимал не больше четверти этого стека (запас
        // не меньше 4×). Переполнение стека завершило бы весь процесс тестов.
        for kind in CHAINS {
            let links = max_links_within_limit(kind);
            // дно объявлено: semantic проходит и типы доходят до самого дна
            let source = chain_source(kind, links, defined_bottom(kind));
            let diags = on_stack(16, move || analyze_source(&source));
            assert!(diags.len() <= 1, "{kind:?}: {diags:?}");
            assert!(
                diags.iter().all(|d| d.code != Code::Limit),
                "{kind:?}: анализ на пределе должен выполняться: {diags:?}"
            );
        }
    }

    // ── устойчивость к произвольному вводу ───────────────────────────

    /// Детерминированный генератор (xorshift64): тест воспроизводим.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const KEYWORDS: [&str; 23] = [
        "LET", "VAR", "CONST", "FN", "RETURN", "IF", "ELSE", "WHILE", "FOR", "IN", "TRUE", "FALSE",
        "STRUCT", "PRINT", "BREAK", "CONTINUE", "AND", "OR", "NOT", "NIL", "MUT", "IMPORT",
        "MATCH",
    ];

    const ASCII_FRAGMENTS: &[&str] = &[
        "+",
        "-",
        "*",
        "/",
        "%",
        "=",
        "==",
        "!=",
        "<",
        "<=",
        ">",
        ">=",
        "(",
        ")",
        "{",
        "}",
        "[",
        "]",
        ",",
        ";",
        ":",
        "->",
        ".",
        "..",
        "x",
        "foo",
        "_",
        "abc123",
        "0",
        "42",
        "3.14",
        "99999999999999999999",
        "1.",
        ".5",
        "\"abc\"",
        "\"unterminated",
        "\"esc\\\"aped\"",
        "\"\\\\\"",
        "\"\\",
        " ",
        "  ",
        "\n",
        "\r\n",
        "\r",
        "\t",
        "\0",
        "😀",
        "é",
        "\u{FEFF}",
        "// комментарий\n",
        "!",
        "@",
        "#",
    ];

    fn random_input(rng: &mut Rng) -> String {
        let mut out = String::new();
        for _ in 0..1 + rng.below(30) {
            match rng.below(10) {
                0..=2 => out.push_str(&encode_word(KEYWORDS[rng.below(KEYWORDS.len())])),
                3 => {
                    // случайные SGA-буквы: чаще всего неизвестное слово
                    for _ in 0..1 + rng.below(8) {
                        out.push(char::from_u32(0xEB40 + rng.below(26) as u32).unwrap());
                    }
                }
                4 => {
                    // глубокая вложенность / длинная цепочка
                    let depth = rng.below(200);
                    let unit = ["(", "{", "[", "-", "a = ", "f(", "[0]", "+1"][rng.below(8)];
                    out.push_str(&unit.repeat(depth));
                }
                5 => {
                    // очень длинная строка/идентификатор
                    let unit = ["a", "\"", "я", "😀", "\u{EB40}"][rng.below(5)];
                    out.push_str(&unit.repeat(rng.below(6000)));
                }
                _ => out.push_str(ASCII_FRAGMENTS[rng.below(ASCII_FRAGMENTS.len())]),
            }
        }
        out
    }

    /// Инварианты диагностики относительно текста (число строк — по `\n`).
    fn assert_well_formed(source: &str, diags: &[Diagnostic]) {
        assert!(diags.len() <= 1, "больше одной диагностики: {diags:?}");
        let lines: Vec<usize> = source.split('\n').map(|l| l.chars().count()).collect();
        for d in diags {
            assert!(!d.message.is_empty(), "пустое сообщение: {d:?}");
            assert!(d.message.chars().count() <= MAX_MESSAGE_CHARS);
            assert!(1 <= d.start_line && d.start_line <= d.end_line, "{d:?}");
            assert!(d.end_line <= lines.len(), "{d:?}: строк {}", lines.len());
            assert!(
                1 <= d.start_col && d.start_col <= lines[d.start_line - 1] + 1,
                "{d:?}: в строке {} символов",
                lines[d.start_line - 1]
            );
            assert!(
                1 <= d.end_col && d.end_col <= lines[d.end_line - 1] + 1,
                "{d:?}: в строке {} символов",
                lines[d.end_line - 1]
            );
            assert!(
                (d.start_line, d.start_col) <= (d.end_line, d.end_col),
                "начало после конца: {d:?}"
            );
        }
    }

    #[test]
    fn arbitrary_input_never_panics_and_yields_well_formed_diagnostics() {
        // Прогон идёт на потоке с 64 МиБ стека — запасной размер стека потока
        // сервера: защиты глубины парсера и `MAX_ANALYSIS_DEPTH` обязаны
        // выдерживать любой из входов, а анализ на пределе глубины требует
        // больше стека, чем даёт поток тестового харнесса.
        let outcome = on_stack(64, || {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
            let mut with_diag = 0usize;
            let mut clean = 0usize;
            for _ in 0..6000 {
                let input = random_input(&mut rng);
                let diags = analyze_source(&input);
                assert_well_formed(&input, &diags);
                if diags.is_empty() {
                    clean += 1;
                } else {
                    with_diag += 1;
                }
            }
            (with_diag, clean)
        });
        // Генератор не вырожден: есть и ошибочные входы, и корректные.
        assert!(
            outcome.0 > 1000,
            "слишком мало ошибочных входов: {outcome:?}"
        );
        assert!(outcome.1 > 10, "нет корректных входов: {outcome:?}");
    }

    #[test]
    fn sga_letters_helper_range_is_what_the_generator_assumes() {
        // Защита самого генератора: U+EB40..U+EB59 — именно SGA-буквы.
        for offset in 0..26u32 {
            assert!(is_sga_letter(char::from_u32(0xEB40 + offset).unwrap()));
        }
        assert!(!is_sga_letter('\u{EB5A}'));
    }
}
