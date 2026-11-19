//! Подсказки языкового сервера (T018, M007): `hover` и `complete` на доменных
//! типах — без JSON и протокола (JSON и маршрутизация — в `lsp::mod`).
//!
//! Движок — `lsp::scan` (токены, а не AST: набираемый документ почти всегда
//! синтаксически неверен), тексты — `lsp::vocabulary`. Ничего не исполняется
//! и не читается с диска; работа — проход по токенам документа.
//!
//! Позиции — позиции ЛЕКСЕРА (`(строка, колонка)` с единицы, колонка в
//! символах, конец диапазона исключительно); перевод в позиции протокола — в
//! `lsp::mod` через `position::to_lsp`.

use super::scan::{self, Document, Lex, Pos, PrefixKind, VarInfo, VarKind};
use super::vocabulary::{self, BuiltinDoc, KeywordDoc};
use crate::lexer::token::TokenKind;
use crate::sga_alphabet::encode_word;
use std::collections::HashSet;

// ── hover ────────────────────────────────────────────────────────────

/// Подсказка при наведении на слово.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    /// Текст в формате Markdown.
    pub markdown: String,
    /// Слово, к которому относится подсказка: начало (включительно) и конец
    /// (исключительно).
    pub start: Pos,
    pub end: Pos,
}

/// Блок кода Markdown без подсветки: синтаксис и сигнатуры показываются
/// моноширинно, а `_` и `*` внутри них не превращаются в разметку.
fn code_block(text: &str) -> String {
    format!("```text\n{text}\n```")
}

/// `Аргументов: 2` / `Аргументов: от 1 до 2`.
fn arity_line((min, max): (usize, usize)) -> String {
    if min == max {
        format!("Аргументов: {min}")
    } else {
        format!("Аргументов: от {min} до {max}")
    }
}

/// Описание ключевого слова без заголовка: форма записи и пояснение. Тот же
/// текст — `documentation` пункта автодополнения.
fn keyword_body(k: &KeywordDoc) -> String {
    format!("{}\n\n{}", code_block(k.syntax), k.summary)
}

fn keyword_hover(k: &KeywordDoc) -> String {
    format!(
        "**`{}`** — {}\n\nПишется буквами SGA: `{}`\n\n{}",
        k.mnemonic,
        k.title,
        encode_word(k.mnemonic),
        keyword_body(k)
    )
}

/// Пометка о функции документа, имя которой совпадает с builtin: при вызове
/// по имени builtin перехватывается раньше пользовательских функций (проверено
/// на `sga run`), хотя `semantic` проверяет арность по функции документа.
fn shadow_note(name: &str, user_hover: bool) -> String {
    if user_hover {
        format!(
            "**Внимание:** имя совпадает со встроенной функцией — вызов `{name}(…)` по имени выполняет встроенную, а не эту функцию."
        )
    } else {
        format!(
            "**Внимание:** в документе объявлена функция `{name}`, но вызов `{name}(…)` по имени выполняет встроенную — она перехватывается раньше функций документа."
        )
    }
}

fn builtin_hover(b: &BuiltinDoc, user_function_exists: bool) -> String {
    let mut out = format!(
        "**`{}`** — встроенная функция\n\n{}\n\n{}\n\n{}",
        b.name,
        code_block(&b.signature()),
        b.summary,
        arity_line(b.arity())
    );
    if user_function_exists {
        out.push_str("\n\n");
        out.push_str(&shadow_note(b.name, false));
    }
    out
}

fn function_hover(f: &scan::FnInfo, shadows_builtin: bool) -> String {
    let mut out = format!(
        "**`{}`** — функция документа\n\n{}\n\n{}",
        f.name,
        code_block(&format!("FN {}", f.signature())),
        arity_line((f.params.len(), f.params.len()))
    );
    if shadows_builtin {
        out.push_str("\n\n");
        out.push_str(&shadow_note(&f.name, true));
    }
    out
}

/// Подсказка для идентификатора под индексом `i`: builtin или функция
/// документа — в позиции вызова (`имя(`) и (функция) в объявлении
/// (`FN имя(`); после одиночной `.` (поле, метод) и во всех остальных
/// позициях — `None`.
fn identifier_hover(doc: &Document, i: usize, name: &str) -> Option<String> {
    let toks = &doc.tokens;
    if scan::single_dot_before(toks, i) {
        return None;
    }
    let call_follows = matches!(toks.get(i + 1).map(|t| &t.kind), Some(TokenKind::LParen));
    if !call_follows {
        return None;
    }
    let user = doc.globals.function(name);
    let builtin = vocabulary::builtin(name);
    let declaration = i > 0 && matches!(toks[i - 1].kind, TokenKind::Fn);
    if declaration {
        // `FN имя(`: объявление; builtin здесь не вызывается
        return user.map(|f| function_hover(f, builtin.is_some()));
    }
    // вызов по имени: builtin выигрывает у функции документа (и у переменной)
    if let Some(b) = builtin {
        return Some(builtin_hover(b, user.is_some()));
    }
    user.map(|f| function_hover(f, false))
}

/// Подсказка при наведении в позиции `pos`; `None` — подсказки нет (строки,
/// комментарии, числа, операторы, пробелы, переменные, поля, структуры и
/// типы, слова после `.`, текстовый режим). Слово определяется как у
/// `Document::word_at`: токен, содержащий позицию, а если он не слово —
/// слово, заканчивающееся в позиции.
pub fn hover(doc: &Document, pos: Pos) -> Option<Hover> {
    let i = doc.word_at(pos)?;
    let token = &doc.tokens[i];
    let markdown = match &token.kind {
        TokenKind::Ident(name) => identifier_hover(doc, i, name)?,
        kind => keyword_hover(vocabulary::keyword(scan::keyword_mnemonic(kind)?)?),
    };
    Some(Hover {
        markdown,
        start: token.start(),
        end: token.end(),
    })
}

// ── completion ───────────────────────────────────────────────────────

/// Вид пункта автодополнения; соответствие числам протокола — в `lsp::mod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Function,
    Variable,
    Keyword,
    Constant,
    Struct,
}

/// Пункт автодополнения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub kind: ItemKind,
    /// Короткая строка рядом с именем (сигнатура, вид объявления).
    pub detail: Option<String>,
    /// Описание в формате Markdown.
    pub documentation: Option<String>,
    /// Текст, которым заменяется префикс (диапазон — `Completion::start..end`).
    pub new_text: String,
    /// Строка, по которой клиент отбирает пункты при дальнейшем наборе.
    pub filter_text: String,
    /// Порядок в списке: группа, затем имя.
    pub sort_text: String,
}

/// Результат автодополнения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Заменяемый диапазон — набранный префикс слова: от его начала
    /// (при пустом префиксе — позиция курсора) до курсора.
    pub start: Pos,
    pub end: Pos,
    /// Пустой список — подсказок в этой позиции нет.
    pub items: Vec<Item>,
}

// Группы порядка: ближние имена раньше дальних, слова языка — последними.
const GROUP_LOCAL: char = '0';
const GROUP_FUNCTION: char = '1';
const GROUP_STRUCT: char = '2';
const GROUP_BUILTIN: char = '3';
const GROUP_KEYWORD: char = '4';

fn sort_text(group: char, name: &str) -> String {
    format!("{group}{name}")
}

fn variable_detail(kind: VarKind) -> &'static str {
    match kind {
        VarKind::Let => "LET",
        VarKind::Var => "VAR",
        VarKind::Const => "CONST",
        VarKind::Param => "параметр",
        VarKind::ForVar => "переменная FOR",
        VarKind::MatchBind => "привязка MATCH",
    }
}

fn variable_item(v: &VarInfo) -> Item {
    Item {
        label: v.name.clone(),
        kind: if v.kind == VarKind::Const {
            ItemKind::Constant
        } else {
            ItemKind::Variable
        },
        detail: Some(variable_detail(v.kind).to_string()),
        documentation: None,
        new_text: v.name.clone(),
        filter_text: v.name.clone(),
        sort_text: sort_text(GROUP_LOCAL, &v.name),
    }
}

fn function_item(f: &scan::FnInfo) -> Item {
    Item {
        label: f.name.clone(),
        kind: ItemKind::Function,
        detail: Some(f.signature()),
        documentation: None,
        new_text: f.name.clone(),
        filter_text: f.name.clone(),
        sort_text: sort_text(GROUP_FUNCTION, &f.name),
    }
}

fn struct_item(s: &scan::StructInfo) -> Item {
    Item {
        label: s.name.clone(),
        kind: ItemKind::Struct,
        detail: Some(format!("STRUCT {} {{ {} }}", s.name, s.fields.join(", "))),
        documentation: None,
        new_text: s.name.clone(),
        filter_text: s.name.clone(),
        sort_text: sort_text(GROUP_STRUCT, &s.name),
    }
}

fn builtin_item(b: &BuiltinDoc) -> Item {
    Item {
        label: b.name.to_string(),
        kind: ItemKind::Function,
        detail: Some(b.signature()),
        documentation: Some(format!("{}\n\n{}", b.summary, arity_line(b.arity()))),
        new_text: b.name.to_string(),
        filter_text: b.name.to_string(),
        sort_text: sort_text(GROUP_BUILTIN, b.name),
    }
}

/// Ключевое слово: подпись — мнемоника, а вставляется слово из букв SGA
/// (в файле ключевые слова пишутся только ими). `typed_glyphs` — префикс уже
/// набран буквами SGA: тогда клиент отбирает пункты по самому слову, а не по
/// мнемонике.
fn keyword_item(k: &KeywordDoc, typed_glyphs: bool) -> Item {
    let glyphs = encode_word(k.mnemonic);
    Item {
        label: k.mnemonic.to_string(),
        kind: ItemKind::Keyword,
        detail: Some(k.title.to_string()),
        documentation: Some(keyword_body(k)),
        new_text: glyphs.clone(),
        filter_text: if typed_glyphs {
            glyphs
        } else {
            k.mnemonic.to_string()
        },
        sort_text: sort_text(GROUP_KEYWORD, k.mnemonic),
    }
}

/// Автодополнение в позиции `pos` документа `text` (`doc` — его разбор).
///
/// Пустой список: курсор в строке или комментарии, после одиночной `.`, в
/// позиции имени вводимого объявления, в заголовке функции, в теле `STRUCT`,
/// на ключе литерала структуры, в позиции аннотации типа и когда слово у
/// курсора начинается с цифры. Во всех остальных позициях: имена из области
/// видимости, функции и структуры документа (в том числе объявленные ниже
/// курсора), builtin и ключевые слова — те, что начинаются с набранного
/// префикса; префикс из букв SGA даёт только ключевые слова.
pub fn complete(doc: &Document, text: &str, pos: Pos) -> Completion {
    let info = doc.cursor_info(text, pos);
    let prefix = &info.prefix;
    let mut completion = Completion {
        start: prefix.start,
        end: pos,
        items: Vec::new(),
    };
    if info.lex != Lex::Code || info.context.blocked.is_some() || prefix.kind == PrefixKind::Digit {
        return completion;
    }

    let items = &mut completion.items;
    if prefix.kind == PrefixKind::Glyph {
        for k in scan::keywords_with_glyph_prefix(&prefix.text) {
            items.push(keyword_item(k, true));
        }
    } else {
        let wanted = prefix.text.to_ascii_lowercase();
        let matches = |name: &str| name.to_ascii_lowercase().starts_with(&wanted);
        // каждое имя — один раз: локальное > функция > структура > builtin
        let mut taken: HashSet<&str> = HashSet::new();
        for v in &info.context.variables {
            if matches(&v.name) && taken.insert(&v.name) {
                items.push(variable_item(v));
            }
        }
        for f in &doc.globals.functions {
            if matches(&f.name) && taken.insert(&f.name) {
                items.push(function_item(f));
            }
        }
        for s in &doc.globals.structs {
            if matches(&s.name) && taken.insert(&s.name) {
                items.push(struct_item(s));
            }
        }
        for b in vocabulary::BUILTINS {
            if matches(b.name) && taken.insert(b.name) {
                items.push(builtin_item(b));
            }
        }
        for k in scan::keywords_with_ascii_prefix(&prefix.text) {
            items.push(keyword_item(k, false));
        }
    }
    items.sort_by(|a, b| a.sort_text.cmp(&b.sort_text));
    completion
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic;

    // ── помощники ────────────────────────────────────────────────────

    /// `<<СЛОВО>>` -> слово из букв SGA.
    fn enc(src: &str) -> String {
        let mut out = String::new();
        let mut rest = src;
        while let Some(open) = rest.find("<<") {
            out.push_str(&rest[..open]);
            let after = &rest[open + 2..];
            let close = after.find(">>").expect("незакрытый маркер <<");
            out.push_str(&encode_word(&after[..close]));
            rest = &after[close + 2..];
        }
        out.push_str(rest);
        out
    }

    /// Текст (без маркера) и позиция курсора `|` в координатах лексера.
    fn parse(src: &str) -> (String, Pos) {
        let (mut line, mut col) = (1usize, 1usize);
        let mut cursor = None;
        let mut text = String::new();
        for c in enc(src).chars() {
            if c == '|' && cursor.is_none() {
                cursor = Some((line, col));
                continue;
            }
            text.push(c);
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (text, cursor.expect("в тексте нет курсора `|`"))
    }

    fn hover_at(src: &str) -> Option<Hover> {
        let (text, pos) = parse(src);
        hover(&Document::new(&text), pos)
    }

    fn complete_at(src: &str) -> Completion {
        let (text, pos) = parse(src);
        complete(&Document::new(&text), &text, pos)
    }

    fn labels(c: &Completion) -> Vec<&str> {
        c.items.iter().map(|i| i.label.as_str()).collect()
    }

    fn item<'a>(c: &'a Completion, label: &str) -> &'a Item {
        c.items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("нет пункта {label}: {:?}", labels(c)))
    }

    /// Имена из области видимости (группа «локальные») в порядке списка:
    /// внутри группы — по алфавиту.
    fn locals(src: &str) -> Vec<String> {
        complete_at(src)
            .items
            .iter()
            .filter(|i| i.sort_text.starts_with(GROUP_LOCAL))
            .map(|i| i.label.clone())
            .collect()
    }

    // ── hover: ключевые слова ────────────────────────────────────────

    #[test]
    fn every_keyword_has_a_hover_at_every_position_of_the_word() {
        assert_eq!(vocabulary::KEYWORDS.len(), 23);
        for k in vocabulary::KEYWORDS {
            let glyphs = encode_word(k.mnemonic);
            let n = glyphs.chars().count();
            let doc = Document::new(&format!("{glyphs} x"));
            // на каждой букве и сразу за словом (там пробел)
            for offset in 0..=n {
                let h = hover(&doc, (1, 1 + offset))
                    .unwrap_or_else(|| panic!("{}: нет подсказки, смещение {offset}", k.mnemonic));
                assert_eq!((h.start, h.end), ((1, 1), (1, 1 + n)), "{}", k.mnemonic);
                assert!(h.markdown.contains(&format!("**`{}`**", k.mnemonic)));
                assert!(h.markdown.contains(&format!("`{glyphs}`")), "написание");
                assert!(h.markdown.contains(k.title));
                assert!(h.markdown.contains(k.syntax), "форма записи");
                assert!(h.markdown.contains(k.summary), "описание");
            }
            // слово `x` после пробела — переменная: подсказки нет
            assert_eq!(hover(&doc, (1, n + 2)), None, "{}", k.mnemonic);
        }
    }

    #[test]
    fn keyword_hover_range_is_the_word_even_when_glued_to_other_tokens() {
        let h = hover_at("<<PRINT>>|(1)").expect("слово, заканчивающееся в позиции");
        assert_eq!((h.start, h.end), ((1, 1), (1, 6)));
        let h = hover_at("x = <<NIL>>|;").expect("подсказка");
        assert_eq!((h.start, h.end), ((1, 5), (1, 8)));
        let h = hover_at("\n  <<LE>>|<<T>> a = 1;").expect("подсказка на второй строке");
        assert_eq!((h.start, h.end), ((2, 3), (2, 6)));
    }

    #[test]
    fn an_ascii_spelling_of_a_keyword_is_an_identifier_and_has_no_hover() {
        // ключевые слова пишутся только буквами SGA: `LET` — обычное имя
        assert_eq!(hover_at("LE|T = 1;"), None);
    }

    // ── hover: builtin ───────────────────────────────────────────────

    #[test]
    fn every_builtin_has_a_hover_in_call_position() {
        assert_eq!(vocabulary::BUILTINS.len(), 32);
        for b in vocabulary::BUILTINS {
            let n = b.name.chars().count();
            let doc = Document::new(&format!("{}(", b.name));
            for offset in 0..=n {
                let h = hover(&doc, (1, 1 + offset))
                    .unwrap_or_else(|| panic!("{}: нет подсказки, смещение {offset}", b.name));
                assert_eq!((h.start, h.end), ((1, 1), (1, 1 + n)), "{}", b.name);
                assert!(h.markdown.contains(&b.signature()), "{}", b.name);
                assert!(h.markdown.contains(b.summary), "{}", b.name);
                let arity = semantic::builtin_arity(b.name).expect("builtin известен semantic");
                assert!(h.markdown.contains(&arity_line(arity)), "{}", b.name);
                assert!(h.markdown.contains("встроенная функция"));
                assert!(!h.markdown.contains("Внимание"), "{}", b.name);
            }
        }
    }

    #[test]
    fn arity_line_says_how_many_arguments_a_builtin_takes() {
        let h = hover_at("push|([], 1)").expect("push");
        assert!(h.markdown.contains("Аргументов: от 1 до 2"));
        let h = hover_at("len|(x)").expect("len");
        assert!(h.markdown.contains("Аргументов: 1"));
        let h = hover_at("args|()").expect("args");
        assert!(h.markdown.contains("Аргументов: 0"));
        let h = hover_at("str_replace|(a, b, c)").expect("str_replace");
        assert!(h.markdown.contains("Аргументов: 3"));
    }

    #[test]
    fn a_builtin_name_without_a_call_or_after_a_dot_has_no_hover() {
        assert_eq!(hover_at("<<LET>> len = 1; <<PRINT>>(le|n);"), None);
        assert_eq!(hover_at("<<LET>> le|n = 1;"), None);
        assert_eq!(hover_at("x = le|n;"), None);
        assert_eq!(hover_at("a.le|n(1)"), None, "метод/поле");
        assert_eq!(hover_at("a.b.le|n(1)"), None);
        assert_eq!(hover_at("a. le|n(1)"), None, "точка и пробел");
        // после `..` снова операнд выражения
        assert!(hover_at("<<FOR>> i <<IN>> 0..le|n(a) { }").is_some());
        assert!(hover_at("<<FOR>> i <<IN>> 0.. le|n(a) { }").is_some());
    }

    #[test]
    fn a_builtin_call_wins_over_a_variable_with_the_same_name() {
        // проверено на `sga run`: `len([1, 2, 3])` печатает 3 и при
        // `LET len = FN…`
        let h = hover_at("<<LET>> len = <<FN>>(x) { <<RETURN>> 1; };\nle|n([1])").expect("builtin");
        assert!(h.markdown.contains("встроенная функция"));
    }

    // ── hover: функции документа ─────────────────────────────────────

    const ADD: &str =
        "<<FN>> add(a: int, <<MUT>> b, c: string) -> int { <<RETURN>> a; }\n<<PRINT>>(add(1, 2, \"s\"));";

    #[test]
    fn a_document_function_has_a_hover_at_its_declaration_and_at_a_call() {
        let (text, _) = parse(&format!("|{ADD}"));
        let doc = Document::new(&text);
        let decl = hover(&doc, (1, 6)).expect("объявление"); // `add` после `FN␣`
        let call_col = text
            .lines()
            .nth(1)
            .and_then(|l| l.find("add"))
            .map(|byte| text.lines().nth(1).unwrap()[..byte].chars().count() + 2)
            .expect("вызов");
        let call = hover(&doc, (2, call_col)).expect("вызов");
        for h in [&decl, &call] {
            assert!(h
                .markdown
                .contains("FN add(a: int, MUT b, c: string) -> int"));
            assert!(h.markdown.contains("Аргументов: 3"));
            assert!(h.markdown.contains("функция документа"));
            assert!(!h.markdown.contains("Внимание"));
        }
        assert_eq!((decl.start, decl.end), ((1, 4), (1, 7)));
    }

    #[test]
    fn a_function_without_parameters_or_return_type() {
        let h = hover_at("<<FN>> f|() { }").expect("f");
        assert!(h.markdown.contains("FN f()\n"));
        assert!(h.markdown.contains("Аргументов: 0"));
        assert!(!h.markdown.contains("->"));
    }

    #[test]
    fn a_function_declared_below_is_found_from_a_call_above() {
        let h = hover_at("<<PRINT>>(g|(1));\n<<FN>> g(x) { }").expect("g");
        assert!(h.markdown.contains("FN g(x)"));
    }

    #[test]
    fn the_first_declaration_wins_for_a_duplicated_function() {
        let h = hover_at("<<FN>> f(a) { }\n<<FN>> f(a, b) { }\n<<PRINT>>(f|(1));").expect("f");
        assert!(h.markdown.contains("FN f(a)\n"));
        assert!(h.markdown.contains("Аргументов: 1"));
        assert!(!h.markdown.contains("Аргументов: 2"));
    }

    #[test]
    fn closures_variables_and_functions_without_a_call_have_no_hover() {
        // переменная-замыкание
        assert_eq!(
            hover_at("<<LET>> f = <<FN>>(a) { <<RETURN>> a; };\nf|(1);"),
            None
        );
        // сама лямбда: слово `FN` — ключевое слово, а не функция документа
        assert!(hover_at("<<LET>> f = <<FN>>|(a) { };").is_some());
        // имя функции без вызова — не переменная
        assert_eq!(hover_at("<<FN>> f() { }\n<<PRINT>>(f|);"), None);
        // тип и имя структуры
        assert_eq!(hover_at("<<STRUCT>> Po|int { x }"), None);
        assert_eq!(hover_at("<<LET>> p = Po|int { x: 1 };"), None);
        assert_eq!(hover_at("<<LET>> n: in|t = 1;"), None);
    }

    #[test]
    fn a_function_with_a_builtin_name_is_marked_in_both_places() {
        let src = "<<FN>> len(x) { <<RETURN>> 42; }\n<<PRINT>>(len([1]));";
        let decl = hover_at(&format!(
            "<<FN>> le|n{}",
            &src[src.find("n(").unwrap() + 1..]
        ))
        .expect("объявление");
        assert!(decl.markdown.contains("FN len(x)"));
        assert!(decl.markdown.contains("Внимание"));
        assert!(decl.markdown.contains("а не эту функцию"));

        let call = hover_at(&src.replace("<<PRINT>>(len", "<<PRINT>>(le|n")).expect("вызов");
        assert!(call.markdown.contains("встроенная функция"));
        assert!(call.markdown.contains("Внимание"));
        assert!(call.markdown.contains("выполняет встроенную"));
        assert!(!call.markdown.contains("FN len(x)"));
    }

    // ── hover: прочее и устойчивость ─────────────────────────────────

    #[test]
    fn nothing_else_has_a_hover() {
        for src in [
            "<<LET>> s = \"le|n(1)\";",
            "<<LET>> s = \"len(1)\"|;",
            "// le|n(1)",
            "<<LET>> n = 4|2;",
            "<<LET>> n = 1 +| 2;",
            "<<LET>> n = | 1;",
            "<<LET>> abc = 1; <<PRINT>>(a|bc);",
            "<<FOR>> i|tem <<IN>> 0..3 { }",
            "|",
            "   |   ",
        ] {
            assert_eq!(hover_at(src), None, "{src}");
        }
    }

    #[test]
    fn a_lexical_error_elsewhere_does_not_take_the_hover_away() {
        assert!(hover_at("# \nle|n(1)").is_some(), "ошибка до слова");
        assert!(hover_at("le|n(1)\n#").is_some(), "ошибка после слова");
        assert!(hover_at("le|n(1)\n\"не закрыта").is_some());
    }

    #[test]
    fn text_mode_has_no_hover() {
        let text = "@ ".repeat(scan::MAX_RELEX + 1) + "len(1)";
        let doc = Document::new(&text);
        assert!(doc.text_mode);
        assert_eq!(hover(&doc, (1, 2 * (scan::MAX_RELEX + 1) + 2)), None);
    }

    // ── completion: ключевые слова ───────────────────────────────────

    #[test]
    fn the_keyword_prefix_in_any_case_offers_let_with_the_sga_edit() {
        for src in ["le|", "LE|", "Le|"] {
            let c = complete_at(src);
            let let_item = item(&c, "LET");
            assert_eq!(let_item.kind, ItemKind::Keyword, "{src}");
            assert_eq!(let_item.new_text, encode_word("LET"));
            assert_eq!(let_item.filter_text, "LET");
            assert_eq!((c.start, c.end), ((1, 1), (1, 3)), "{src}");
            assert!(let_item
                .documentation
                .as_deref()
                .is_some_and(|d| d.contains(vocabulary::keyword("LET").expect("LET").syntax)));
            assert_eq!(
                let_item.detail.as_deref(),
                Some("неизменяемая переменная"),
                "{src}"
            );
            // `len` — builtin, он раньше слов языка
            assert_eq!(labels(&c), ["len", "LET"], "{src}");
        }
    }

    #[test]
    fn a_prefix_of_sga_letters_offers_only_keywords_filtered_by_the_word_itself() {
        let c = complete_at("<<LE>>|");
        assert_eq!(labels(&c), ["LET"]);
        assert_eq!(c.items[0].new_text, encode_word("LET"));
        assert_eq!(c.items[0].filter_text, encode_word("LET"));
        assert_eq!((c.start, c.end), ((1, 1), (1, 3)));
        // несколько слов: порядок по мнемонике
        assert_eq!(labels(&complete_at("<<I>>|")), ["IF", "IMPORT", "IN"]);
        // имена документа и builtin по префиксу из букв SGA не предлагаются
        let c = complete_at("<<LET>> len = 1;\n<<L>>|");
        assert_eq!(labels(&c), ["LET"]);
    }

    #[test]
    fn a_complete_keyword_typed_in_sga_letters_is_still_offered() {
        // Слово уже закончено (лексер видит ключевое слово), но курсор у его
        // конца: это набираемое слово, а не контекст «после LET» — подсказка
        // есть и предлагает то же слово.
        for k in ["LET", "VAR", "CONST", "STRUCT", "FOR", "FN", "MUT"] {
            let c = complete_at(&format!("<<{k}>>|"));
            assert_eq!(labels(&c), [k], "{k}");
            assert_eq!(c.items[0].new_text, encode_word(k));
        }
        // то же после готового оператора на той же строке
        assert_eq!(
            labels(&complete_at("<<LET>> a = 1; <<FOR>>|")),
            ["FOR"],
            "слово у курсора не делает позицию именем объявления"
        );
    }

    #[test]
    fn an_empty_prefix_offers_every_keyword_and_builtin() {
        let c = complete_at("|");
        let keywords = c.items.iter().filter(|i| i.kind == ItemKind::Keyword);
        let functions = c.items.iter().filter(|i| i.kind == ItemKind::Function);
        assert_eq!(keywords.count(), 23);
        assert_eq!(functions.count(), 32);
        assert_eq!(c.items.len(), 55);
        assert_eq!((c.start, c.end), ((1, 1), (1, 1)));
    }

    #[test]
    fn a_prefix_nothing_starts_with_offers_nothing() {
        assert!(complete_at("zzz|").items.is_empty());
        assert!(complete_at("<<ZZ>>|").items.is_empty());
        assert!(complete_at("_|").items.is_empty());
    }

    #[test]
    fn a_keyword_inserted_by_a_suggestion_is_lexed_as_that_keyword() {
        use crate::lexer::token::keyword_from_mnemonic;
        use crate::lexer::Lexer;
        for k in vocabulary::KEYWORDS {
            let c = complete_at(&format!("{}|", k.mnemonic.to_ascii_lowercase()));
            let inserted = &item(&c, k.mnemonic).new_text;
            let tokens = Lexer::new(inserted)
                .tokenize()
                .expect("лексер принимает слово");
            assert_eq!(
                Some(tokens[0].kind.clone()),
                keyword_from_mnemonic(k.mnemonic),
                "{}",
                k.mnemonic
            );
        }
    }

    // ── completion: builtin, функции, структуры ──────────────────────

    #[test]
    fn builtin_prefix_offers_the_matching_functions_with_signatures() {
        let c = complete_at("to_|");
        assert_eq!(labels(&c), ["to_float", "to_int", "to_string"]);
        for i in &c.items {
            assert_eq!(i.kind, ItemKind::Function);
            let doc = vocabulary::builtin(&i.label).expect("builtin");
            assert_eq!(i.detail.as_deref(), Some(doc.signature().as_str()));
            assert!(i
                .documentation
                .as_deref()
                .expect("описание")
                .contains(doc.summary));
            assert_eq!(i.new_text, i.label);
            assert_eq!(i.filter_text, i.label);
        }
    }

    #[test]
    fn a_builtin_item_documentation_is_the_summary_and_the_arity() {
        let c = complete_at("|");
        for i in c
            .items
            .iter()
            .filter(|i| i.sort_text.starts_with(GROUP_BUILTIN))
        {
            let doc = vocabulary::builtin(&i.label).expect("builtin");
            let expected = format!("{}\n\n{}", doc.summary, arity_line(doc.arity()));
            assert_eq!(
                i.documentation.as_deref(),
                Some(expected.as_str()),
                "{}",
                i.label
            );
        }
        let ends = |label: &str| item(&c, label).documentation.clone().expect("описание");
        assert!(ends("len").ends_with("\n\nАргументов: 1"));
        assert!(ends("range").ends_with("\n\nАргументов: от 1 до 2"));
        assert!(ends("args").ends_with("\n\nАргументов: 0"));
    }

    #[test]
    fn functions_and_structs_below_the_cursor_are_offered() {
        let c = complete_at("<<PRINT>>(he|);\n<<FN>> helper(a, <<MUT>> b: int) -> int { }\n<<STRUCT>> Hero { x, y }");
        assert_eq!(labels(&c), ["helper", "Hero"]);
        let f = item(&c, "helper");
        assert_eq!(f.kind, ItemKind::Function);
        assert_eq!(f.detail.as_deref(), Some("helper(a, MUT b: int) -> int"));
        let s = item(&c, "Hero");
        assert_eq!(s.kind, ItemKind::Struct);
        assert_eq!(s.detail.as_deref(), Some("STRUCT Hero { x, y }"));
    }

    #[test]
    fn a_name_is_offered_once_with_local_function_struct_builtin_priority() {
        let all = "<<LET>> abs = 1;\n<<FN>> abs(q) { }\n<<STRUCT>> abs { x }\nab|";
        let c = complete_at(all);
        assert_eq!(labels(&c), ["abs"]);
        assert_eq!(c.items[0].kind, ItemKind::Variable);

        let c = complete_at("<<FN>> abs(q) { }\n<<STRUCT>> abs { x }\nab|");
        assert_eq!(labels(&c), ["abs"]);
        assert_eq!(c.items[0].kind, ItemKind::Function);
        assert_eq!(c.items[0].detail.as_deref(), Some("abs(q)"));

        let c = complete_at("<<STRUCT>> abs { x }\nab|");
        assert_eq!(labels(&c), ["abs"]);
        assert_eq!(c.items[0].kind, ItemKind::Struct);

        let c = complete_at("ab|");
        assert_eq!(labels(&c), ["abs"]);
        assert_eq!(c.items[0].kind, ItemKind::Function);
        assert_eq!(c.items[0].detail.as_deref(), Some("abs(x) -> int | float"));
    }

    #[test]
    fn a_duplicated_function_is_offered_once_with_the_first_signature() {
        let c = complete_at("<<FN>> dup(a) { }\n<<FN>> dup(a, b) { }\ndu|");
        assert_eq!(labels(&c), ["dup"]);
        assert_eq!(c.items[0].detail.as_deref(), Some("dup(a)"));
    }

    #[test]
    fn a_user_name_may_coincide_with_a_keyword_mnemonic_in_ascii() {
        // `LET` ASCII — обычное имя: пункт переменной и пункт ключевого слова
        // различаются видом и вставляемым текстом
        let c = complete_at("<<LET>> LET = 1;\nLE|");
        let both: Vec<&Item> = c.items.iter().filter(|i| i.label == "LET").collect();
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].kind, ItemKind::Variable);
        assert_eq!(both[0].new_text, "LET");
        assert_eq!(both[1].kind, ItemKind::Keyword);
        assert_eq!(both[1].new_text, encode_word("LET"));
    }

    // ── completion: порядок ──────────────────────────────────────────

    #[test]
    fn items_are_ordered_by_group_then_name_and_the_order_is_deterministic() {
        let src = "<<LET>> zeta = 1;\n<<FN>> alpha() { }\n<<STRUCT>> Beta { x }\n|";
        let c = complete_at(src);
        let l = labels(&c);
        assert_eq!(&l[..3], ["zeta", "alpha", "Beta"]);
        assert_eq!(
            &c.items[3..35]
                .iter()
                .map(|i| i.label.as_str())
                .collect::<Vec<_>>(),
            &{
                let mut names: Vec<&str> = vocabulary::BUILTINS.iter().map(|b| b.name).collect();
                names.sort_unstable();
                names
            }
        );
        let tail: Vec<&str> = l[35..].to_vec();
        let mut sorted = tail.clone();
        sorted.sort_unstable();
        assert_eq!((tail.len(), tail), (23, sorted));
        // `sortText` строго возрастает, поэтому порядок не зависит от клиента
        assert!(c.items.windows(2).all(|w| w[0].sort_text < w[1].sort_text));
        assert_eq!(complete_at(src), c);
    }

    #[test]
    fn local_names_come_before_everything_else() {
        let c = complete_at("<<LET>> zzz = 1;\n<<FN>> f() { }\n|");
        assert_eq!(&labels(&c)[..2], ["zzz", "f"]);
    }

    // ── completion: области видимости ────────────────────────────────

    #[test]
    fn locals_follow_the_scope_rules_of_the_semantic_analyzer() {
        let cases: &[(&str, &[&str])] = &[
            ("<<LET>> abc = 1;\n<<PRINT>>(|);", &["abc"]),
            ("<<PRINT>>(|);\n<<LET>> abc = 1;", &[]),
            ("<<LET>> abc = |", &[]),
            ("<<LET>> abc = 1;\n<<LET>> abd = abc + |", &["abc"]),
            ("<<FN>> f(a, <<MUT>> b) { <<PRINT>>(|); }", &["a", "b"]),
            ("<<LET>> g = <<FN>>(a) { <<RETURN>> |; };", &["a"]),
            ("<<FOR>> i <<IN>> 0..3 { <<PRINT>>(|); }", &["i"]),
            ("<<FOR>> i <<IN>> 0..3 { }\n<<PRINT>>(|);", &[]),
            ("<<MATCH>> 1 { n -> |, _ -> 0 }", &["n"]),
            ("<<MATCH>> 1 { n -> 1, _ -> | }", &[]),
            ("{ <<LET>> inner = 1; }\n<<PRINT>>(|);", &[]),
            (
                "<<LET>> a = 1;\n{ <<LET>> b = 2; <<PRINT>>(|); }",
                &["a", "b"],
            ),
            (
                "<<LET>> early = 1;\n<<FN>> f() { <<PRINT>>(|); }\n<<LET>> late = 2;",
                &["early"],
            ),
            ("<<LET>> x = 1;\n<<FN>> f(x) { <<PRINT>>(|); }", &["x"]),
            (
                "<<CONST>> kk = 1;\n<<VAR>> vv = 2;\n<<PRINT>>(|);",
                &["kk", "vv"],
            ),
        ];
        for (src, expected) in cases {
            assert_eq!(&locals(src), expected, "{src}");
        }
    }

    #[test]
    fn local_items_describe_how_the_name_was_declared() {
        let c = complete_at(
            "<<CONST>> kk = 1;\n<<LET>> ll = 1;\n<<VAR>> vv = 1;\n<<FN>> f(pp) { <<FOR>> ii <<IN>> 0..2 { <<MATCH>> 1 { mm -> |, _ -> 0 } } }",
        );
        // области: пункт MATCH, тело FOR, тело функции — глобальные имена
        // тело функции не видит (они объявлены выше, поэтому видит)
        let by_name = |n: &str| item(&c, n).clone();
        assert_eq!(by_name("mm").detail.as_deref(), Some("привязка MATCH"));
        assert_eq!(by_name("ii").detail.as_deref(), Some("переменная FOR"));
        assert_eq!(by_name("pp").detail.as_deref(), Some("параметр"));
        assert_eq!(by_name("kk").detail.as_deref(), Some("CONST"));
        assert_eq!(by_name("kk").kind, ItemKind::Constant);
        assert_eq!(by_name("ll").detail.as_deref(), Some("LET"));
        assert_eq!(by_name("vv").detail.as_deref(), Some("VAR"));
        assert_eq!(by_name("vv").kind, ItemKind::Variable);
        assert_eq!(by_name("mm").kind, ItemKind::Variable);
    }

    // ── completion: места без подсказок ──────────────────────────────

    #[test]
    fn there_are_no_suggestions_where_a_name_cannot_be_typed() {
        for src in [
            "<<LET>> s = \"he|llo\";",
            "<<LET>> s = \"abc|",
            "<<LET>> s = \"a\\\"b|",
            "// le|",
            "<<LET>> s = 1; // x\n// le|",
            "a.|",
            "a.le|",
            "a.b.|",
            "<<LET>> |",
            "<<VAR>> ab|",
            "<<CONST>> |",
            "<<STRUCT>> |",
            "<<FOR>> |",
            "<<FN>> |",
            "<<MUT>> |",
            "<<FN>> f(a, |",
            "<<FN>> f(<<MUT>> |",
            "<<FN>> f(a) -> |",
            "<<STRUCT>> S { | }",
            "<<STRUCT>> S { x, | }",
            "<<LET>> p = P { | }",
            "<<LET>> p = P { x: 1, | }",
            "<<LET>> x: |",
            "<<LET>> x: in|",
            "<<LET>> x = 12|",
            "<<LET>> x = 12ab|",
            "<<LET>> x = 1.5|",
        ] {
            assert!(complete_at(src).items.is_empty(), "{src}");
        }
    }

    #[test]
    fn suggestions_exist_in_the_positions_next_to_the_empty_ones() {
        for src in [
            "<<PRINT>>(|",
            "<<FOR>> i <<IN>> 0..|",
            "<<FOR>> i <<IN>> 0..le|",
            "<<LET>> p = P { x: | }",
            "<<LET>> p = P { x: 1 + le| }",
            "<<MATCH>> 1 { 1 -> | }",
            "<<LET>> x = 1; |",
            "<<LET>> x |",
            "<<STRUCT>> S { x } |",
            "<<FN>> f(a) { | }",
            "<<FN>> f(a) -> int { | }",
            "<<LET>> s = \"a\"; le|",
            "x = 1 // c\nle|",
        ] {
            assert!(!complete_at(src).items.is_empty(), "{src}");
        }
    }

    // ── completion: набираемый документ ──────────────────────────────

    #[test]
    fn suggestions_work_in_a_document_that_is_being_typed() {
        // незакрытая скобка
        let c = complete_at("<<PRINT>>(le|");
        assert!(has_all(&c, &["len", "LET"]));
        // пропущенные `;`
        assert_eq!(
            locals("<<LET>> a = 1\n<<LET>> b = 2\n<<PRINT>>(|"),
            ["a", "b"]
        );
        // незакрытые блоки
        assert_eq!(
            locals("<<FN>> f(a) {\n  <<LET>> b = 1;\n  <<IF>> a {\n    |"),
            ["a", "b"]
        );
        // слово у курсора закрывает объявление без `;`
        assert_eq!(locals("<<LET>> abc = 1\nab|"), ["abc"]);
    }

    fn has_all(c: &Completion, wanted: &[&str]) -> bool {
        wanted.iter().all(|w| c.items.iter().any(|i| i.label == *w))
    }

    #[test]
    fn a_lexical_error_elsewhere_does_not_take_the_suggestions_away() {
        // стороннее слово (не слово языка) в буквах SGA — ошибка лексера
        for src in [
            "# \n<<LET>> abc = 1;\nab|",
            "<<LET>> abc = 1;\nab|\n#",
            "<<LET>> abc = 1; @ $ %\nab|",
            "<<LET>> abc = 1;\nab|\n\"не закрыта",
        ] {
            let c = complete_at(src);
            assert_eq!(item(&c, "abc").kind, ItemKind::Variable, "{src}");
        }
    }

    #[test]
    fn text_mode_offers_keywords_and_builtins_by_prefix_only() {
        let junk = "@ ".repeat(scan::MAX_RELEX + 1);
        let src = format!("{junk}\n<<LET>> abc = 1;\n<<FN>> myfn() {{ }}\nle|");
        let (text, pos) = parse(&src);
        let doc = Document::new(&text);
        assert!(doc.text_mode);
        let c = complete(&doc, &text, pos);
        assert_eq!(labels(&c), ["len", "LET"]);
        // имена документа в текстовом режиме неизвестны
        let (text, pos) = parse(&format!("{junk}\n<<LET>> abc = 1;\nab|"));
        let c = complete(&Document::new(&text), &text, pos);
        assert_eq!(labels(&c), ["abs"]);
        // а строки и комментарии по-прежнему распознаются по тексту
        let (text, pos) = parse(&format!("{junk}\n// le|"));
        assert!(complete(&Document::new(&text), &text, pos).items.is_empty());
    }

    // ── completion: диапазон ─────────────────────────────────────────

    #[test]
    fn the_edit_range_is_the_typed_prefix_counted_in_characters() {
        let (text, pos) = parse("<<LET>> s = \"😀\"; le|");
        let c = complete(&Document::new(&text), &text, pos);
        assert_eq!((c.start, c.end), ((1, pos.1 - 2), pos));
        // пустой префикс: пустой диапазон в позиции курсора
        let (text, pos) = parse("x = |");
        let c = complete(&Document::new(&text), &text, pos);
        assert_eq!((c.start, c.end), (pos, pos));
        // курсор внутри слова: заменяется только набранная часть
        let (text, pos) = parse("abc|def");
        let c = complete(&Document::new(&text), &text, pos);
        assert_eq!((c.start, c.end), ((1, 1), (1, 4)));
        // префикс из букв SGA
        let (text, pos) = parse("a = <<RET>>|");
        let c = complete(&Document::new(&text), &text, pos);
        assert_eq!((c.start, c.end), ((1, 5), (1, 8)));
        assert_eq!(labels(&c), ["RETURN"]);
    }

    // ── устойчивость ─────────────────────────────────────────────────

    #[test]
    fn hover_and_complete_never_panic_at_any_position_of_awkward_texts() {
        let corpus = [
            "",
            "\n",
            "\"",
            "\"abc",
            "// c",
            "😀 le😀n(\"😀\")",
            "<<LET>> x = [1, 2",
            "<<FN>> f(a, <<MUT>> b: int) -> { ",
            "<<MATCH>> x { 1 -> 2, ",
            "<<STRUCT>> S { x, y } <<LET>> s = S { x: 1,",
            "}}}} )))) ]]]]",
            "{{{{ (((( [[[[",
            "a.b.c..d...e",
            "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@ len(x)",
            "<<IF>> <<WHILE>> <<FOR>> <<ELSE>> <<MATCH>> <<FN>> <<STRUCT>>",
            "x\r\ny\r\n",
        ];
        for src in corpus {
            let text = enc(src);
            let doc = Document::new(&text);
            let lines: Vec<&str> = text.split('\n').collect();
            for (i, l) in lines.iter().enumerate() {
                for col in 1..=l.chars().count() + 3 {
                    let pos = (i + 1, col);
                    let _ = hover(&doc, pos);
                    let c = complete(&doc, &text, pos);
                    assert!(c.start <= c.end, "{src:?} {pos:?}");
                }
            }
            // и за концом текста
            let _ = hover(&doc, (lines.len() + 5, 99));
            let _ = complete(&doc, &text, (lines.len() + 5, 99));
        }
    }
}
