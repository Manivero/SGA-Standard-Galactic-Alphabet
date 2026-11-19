//! Навигация по документу (T019, M007): структура файла
//! (`textDocument/documentSymbol`) и переход к определению
//! (`textDocument/definition`).
//!
//! Как и подсказки (`assist`), работает на ТОКЕНАХ и сканере областей (`scan`),
//! а не на AST: текст, который сейчас набирают, почти всегда неверен, а у
//! парсера нет восстановления после ошибок (TD-020). Правила разрешения имён
//! повторяют `semantic` и `codegen` — «какое объявление читает использование»
//! проверяется настоящим исполнением (`tests/lsp_nav_test.rs`), а диапазоны
//! символов — настоящим AST (`Stmt.span`).
//!
//! Позиции — позиции лексера (строка и колонка в символах, с единицы); в
//! UTF-16 их переводит `lsp::mod`. Ничего здесь не рекурсивно по размеру
//! документа.

use super::scan::{self, Document, Lookup, Pos, VarKind};
use super::vocabulary;
use crate::lexer::token::TokenKind;

/// Вид символа; соответствие числам протокола — в `lsp::mod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Field,
    Variable,
    Constant,
}

/// Символ документа: объявление функции, структуры, её поля или переменной
/// верхнего уровня.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// Сигнатура функции, мнемоника (и тип) переменной; у структуры и поля нет.
    pub detail: Option<String>,
    pub kind: SymbolKind,
    /// Всё объявление: от первого токена до последнего.
    pub start: Pos,
    pub end: Pos,
    /// Токен имени; всегда внутри `start..end`.
    pub name_start: Pos,
    pub name_end: Pos,
    pub children: Vec<Symbol>,
}

/// Наибольшая вложенность символов в ответе. Корректная программа вкладывает
/// не глубже двух уровней (структура и её поля); больше бывает только в
/// испорченном тексте (`FN a() { FN a() { …`), а глубокий ответ рекурсивно
/// обходили бы сериализация и освобождение памяти.
pub const MAX_SYMBOL_DEPTH: usize = 16;

/// Заготовка символа до построения дерева: индексы токенов начала и конца.
struct Entry {
    start: usize,
    end: usize,
    symbol: Symbol,
}

fn token_symbol(
    doc: &Document,
    name: &str,
    kind: SymbolKind,
    detail: Option<String>,
    start: usize,
    end: usize,
    name_at: usize,
) -> Entry {
    let toks = &doc.tokens;
    Entry {
        start,
        end,
        symbol: Symbol {
            name: name.to_string(),
            detail,
            kind,
            start: toks[start].start(),
            end: toks[end].end(),
            name_start: toks[name_at].start(),
            name_end: toks[name_at].end(),
            children: Vec::new(),
        },
    }
}

/// Структура файла: каждая функция `FN имя (`, структура `STRUCT Имя {` с
/// полями и оператор `LET`/`VAR`/`CONST` верхнего уровня, в порядке
/// появления; повторные объявления перечисляются все. Символ, начавшийся
/// внутри диапазона ещё открытого предыдущего, — его ребёнок (поля структуры;
/// в испорченном тексте — функция внутри незакрытой функции).
pub fn document_symbols(doc: &Document) -> Vec<Symbol> {
    let toks = &doc.tokens;
    let Some(last) = toks.len().checked_sub(1) else {
        return Vec::new();
    };
    let decls = scan::collect_declarations(toks);
    let variables = doc.top_level_variables();

    // `{` тел функций и структур по возрастанию индекса — для парных `}`
    let mut opens: Vec<usize> = decls
        .functions
        .iter()
        .filter_map(|f| f.open)
        .chain(decls.structs.iter().map(|s| s.name_at + 1))
        .collect();
    opens.sort_unstable();
    let closers = doc.matching_closers(&opens);
    let close_of = |open: usize| -> usize {
        opens
            .binary_search(&open)
            .ok()
            .and_then(|n| closers[n])
            .unwrap_or(last)
    };

    let mut entries: Vec<Entry> = Vec::new();
    for f in &decls.functions {
        let end = match f.open {
            Some(open) => close_of(open),
            None => f.last,
        };
        entries.push(token_symbol(
            doc,
            &f.name,
            SymbolKind::Function,
            Some(f.signature_tail()),
            f.name_at - 1,
            end,
            f.name_at,
        ));
    }
    for s in &decls.structs {
        let end = close_of(s.name_at + 1);
        entries.push(token_symbol(
            doc,
            &s.name,
            SymbolKind::Struct,
            None,
            s.name_at - 1,
            end,
            s.name_at,
        ));
        for (field, &at) in s.fields.iter().zip(&s.fields_at) {
            entries.push(token_symbol(
                doc,
                field,
                SymbolKind::Field,
                None,
                at,
                at,
                at,
            ));
        }
    }
    for v in &variables {
        let mnemonic = match v.kind {
            VarKind::Let => "LET",
            VarKind::Var => "VAR",
            _ => "CONST",
        };
        let detail = match &v.ty {
            Some(ty) => format!("{mnemonic}: {ty}"),
            None => mnemonic.to_string(),
        };
        let kind = if v.kind == VarKind::Const {
            SymbolKind::Constant
        } else {
            SymbolKind::Variable
        };
        entries.push(token_symbol(
            doc,
            &v.name,
            kind,
            Some(detail),
            v.start,
            v.end,
            v.at,
        ));
    }
    entries.sort_by_key(|e| e.start);

    // вложенность по вхождению диапазонов, один проход со стеком открытых
    // символов; глубже `MAX_SYMBOL_DEPTH` символ не вкладывается дальше, а
    // остаётся ребёнком самого глубокого разрешённого родителя
    let n = entries.len();
    let mut parent: Vec<Option<usize>> = vec![None; n];
    let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut stack: Vec<usize> = Vec::new();
    for k in 0..n {
        while stack
            .last()
            .is_some_and(|&top| entries[top].end < entries[k].start)
        {
            stack.pop();
        }
        if let Some(&top) = stack.last() {
            parent[k] = Some(top);
            children_of[top].push(k);
        }
        if stack.len() + 1 < MAX_SYMBOL_DEPTH {
            stack.push(k);
        }
    }

    // сборка с конца: дети (индексы больше) готовы раньше родителя
    let mut slots: Vec<Option<Symbol>> = entries.into_iter().map(|e| Some(e.symbol)).collect();
    let mut built: Vec<Option<Symbol>> = (0..n).map(|_| None).collect();
    for k in (0..n).rev() {
        let mut symbol = slots[k].take().expect("символ собирается один раз");
        symbol.children = children_of[k]
            .iter()
            .map(|&child| built[child].take().expect("ребёнок собран раньше родителя"))
            .collect();
        built[k] = Some(symbol);
    }
    (0..n)
        .filter(|&k| parent[k].is_none())
        .map(|k| built[k].take().expect("корень собран"))
        .collect()
}

/// Место определения: токен имени в объявлении.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub start: Pos,
    pub end: Pos,
}

fn token_location(doc: &Document, at: usize) -> Location {
    let token = &doc.tokens[at];
    Location {
        start: token.start(),
        end: token.end(),
    }
}

/// Определение имени под курсором (правила — в Feature Contract T019 и
/// `docs/LSP.md`): объявление функции, структуры, поля структуры, переменной,
/// параметра, переменной `FOR` или привязки `MATCH`; на самом объявлении —
/// оно же. `None` — определения нет (после `.`, в аннотации типа, builtin,
/// неизвестное имя, не идентификатор, текстовый режим).
pub fn definition(doc: &Document, pos: Pos) -> Option<Location> {
    let i = doc.word_at(pos)?;
    let TokenKind::Ident(name) = &doc.tokens[i].kind else {
        return None;
    };
    let target = match doc.lookup_at(i) {
        Lookup::Nothing => None,
        Lookup::Declaration => Some(i),
        Lookup::Field { structure } => doc.globals.structure(&structure).and_then(|s| {
            s.fields
                .iter()
                .position(|f| f == name)
                .map(|n| s.fields_at[n])
        }),
        Lookup::Structure => doc.globals.structure(name).map(|s| s.name_at),
        // порядок как в `codegen`: builtin выигрывает у функции документа, а
        // она — у переменной (`known_functions` раньше `LoadVar`)
        Lookup::Call { variable } => {
            if vocabulary::builtin(name).is_some() {
                None
            } else {
                doc.globals.function(name).map(|f| f.name_at).or(variable)
            }
        }
        // имя функции в позиции значения — «неопределённая переменная»
        Lookup::Value { variable } => variable,
    };
    target.map(|at| token_location(doc, at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sga_alphabet::encode_word;

    /// Заменяет `<<СЛОВО>>` словом из букв SGA.
    fn enc(src: &str) -> String {
        let mut out = String::new();
        let mut rest = src;
        while let Some(i) = rest.find("<<") {
            out.push_str(&rest[..i]);
            let tail = &rest[i + 2..];
            let j = tail.find(">>").expect("незакрытый <<");
            out.push_str(&encode_word(&tail[..j]));
            rest = &tail[j + 2..];
        }
        out.push_str(rest);
        out
    }

    /// Текст без маркера курсора `|` и позиция курсора.
    fn parse(src: &str) -> (String, Pos) {
        let encoded = enc(src);
        let bar = encoded.find('|').expect("маркер курсора `|`");
        let before = &encoded[..bar];
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        (format!("{}{}", before, &encoded[bar + 1..]), (line, col))
    }

    /// Куда ведёт определение слова у курсора `|`: текст токена-цели и его
    /// позиция; `None` — определения нет.
    fn target(src: &str) -> Option<(String, Pos)> {
        let (text, pos) = parse(src);
        let doc = Document::new(&text);
        definition(&doc, pos).map(|l| {
            let word: String = text
                .lines()
                .nth(l.start.0 - 1)
                .expect("строка цели")
                .chars()
                .skip(l.start.1 - 1)
                .take(l.end.1 - l.start.1)
                .collect();
            (word, l.start)
        })
    }

    fn symbols(src: &str) -> Vec<Symbol> {
        document_symbols(&Document::new(&enc(src)))
    }

    /// Плоский список `(имя, вид, глубина)` в порядке обхода.
    fn outline(symbols: &[Symbol]) -> Vec<(String, SymbolKind, usize)> {
        fn walk(list: &[Symbol], depth: usize, out: &mut Vec<(String, SymbolKind, usize)>) {
            for s in list {
                out.push((s.name.clone(), s.kind, depth));
                walk(&s.children, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        walk(symbols, 0, &mut out);
        out
    }

    // ── documentSymbol ─────────────────────────────────────────────

    #[test]
    fn symbols_cover_functions_structs_fields_and_top_level_variables() {
        let list = symbols(
            "<<LET>> a = 1;\n<<FN>> f(x, y: int) -> int { <<RETURN>> x; }\n<<STRUCT>> P { px, py }\n<<VAR>> b: int = 2;\n<<CONST>> c = 3;",
        );
        assert_eq!(
            outline(&list),
            vec![
                ("a".to_string(), SymbolKind::Variable, 0),
                ("f".to_string(), SymbolKind::Function, 0),
                ("P".to_string(), SymbolKind::Struct, 0),
                ("px".to_string(), SymbolKind::Field, 1),
                ("py".to_string(), SymbolKind::Field, 1),
                ("b".to_string(), SymbolKind::Variable, 0),
                ("c".to_string(), SymbolKind::Constant, 0),
            ]
        );
        assert_eq!(list[0].detail.as_deref(), Some("LET"));
        assert_eq!(list[1].detail.as_deref(), Some("(x, y: int) -> int"));
        assert_eq!(list[2].detail, None);
        assert_eq!(list[3].detail.as_deref(), Some("VAR: int"));
        assert_eq!(list[4].detail.as_deref(), Some("CONST"));
    }

    #[test]
    fn symbol_ranges_run_from_the_keyword_to_the_end_of_the_declaration() {
        let list = symbols("<<LET>> a = 1;\n<<FN>> f() {\n  <<RETURN>> 1;\n}\n<<STRUCT>> P { x }");
        // LET … `;`
        assert_eq!((list[0].start, list[0].end), ((1, 1), (1, 11)));
        assert_eq!((list[0].name_start, list[0].name_end), ((1, 5), (1, 6)));
        // FN … `}` на третьей строке
        assert_eq!((list[1].start, list[1].end), ((2, 1), (4, 2)));
        assert_eq!((list[1].name_start, list[1].name_end), ((2, 4), (2, 5)));
        // STRUCT … `}`
        assert_eq!((list[2].start, list[2].end), ((5, 1), (5, 15)));
        assert_eq!(list[2].children.len(), 1);
        assert_eq!(
            (list[2].children[0].start, list[2].children[0].end),
            ((5, 12), (5, 13))
        );
    }

    #[test]
    fn an_unclosed_body_ends_at_the_last_token() {
        let list = symbols("<<FN>> f() {\n  <<LET>> a = 1;\n");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].end, (2, 13));
        let list = symbols("<<STRUCT>> P { x, y");
        assert_eq!(list[0].end, (1, 16));
    }

    #[test]
    fn an_unfinished_header_ends_at_its_last_token() {
        let list = symbols("<<FN>> f(a, b");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].end, (1, 10));
        assert_eq!(list[0].detail.as_deref(), Some("(a, b)"));
    }

    #[test]
    fn local_variables_parameters_and_lambdas_are_not_symbols() {
        let list = symbols(
            "<<FN>> f(p) { <<LET>> inner = 1; <<RETURN>> p; }\n<<LET>> g = <<FN>>(q) { <<RETURN>> q; };\n<<IF>> g { <<LET>> deep = 1; }",
        );
        assert_eq!(
            outline(&list),
            vec![
                ("f".to_string(), SymbolKind::Function, 0),
                ("g".to_string(), SymbolKind::Variable, 0),
            ]
        );
    }

    #[test]
    fn duplicate_declarations_are_all_listed() {
        let list = symbols("<<FN>> f() { }\n<<FN>> f() { }\n<<LET>> a = 1;\n<<LET>> a = 2;");
        assert_eq!(list.len(), 4);
    }

    #[test]
    fn a_variable_without_an_initializer_is_not_a_symbol_until_the_equals_sign() {
        assert!(symbols("<<LET>> a").is_empty());
        assert!(symbols("<<LET>> a: int").is_empty());
        let list = symbols("<<LET>> a =");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].end, (1, 8));
        // и посреди файла: оборванное объявление не попадает в список
        let list = symbols("<<LET>> a\n<<LET>> b = 1;");
        assert_eq!(
            outline(&list),
            vec![("b".to_string(), SymbolKind::Variable, 0)]
        );
    }

    #[test]
    fn a_variable_ends_before_the_next_statement_when_there_is_no_semicolon() {
        let list = symbols("<<LET>> a = 1\n<<LET>> b = 2\nprint_all(a)");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].end, (1, 10));
        assert_eq!(list[1].end, (2, 10));
    }

    #[test]
    fn a_function_inside_an_unclosed_function_becomes_its_child() {
        let list = symbols("<<FN>> a() {\n<<FN>> b() { }\n");
        assert_eq!(
            outline(&list),
            vec![
                ("a".to_string(), SymbolKind::Function, 0),
                ("b".to_string(), SymbolKind::Function, 1),
            ]
        );
        assert!(list[0].start <= list[0].children[0].start);
        assert!(list[0].children[0].end <= list[0].end);
    }

    #[test]
    fn nesting_is_capped_so_a_deep_answer_stays_shallow() {
        let text = "<<FN>> a() { ".repeat(200);
        let list = symbols(&text);
        let flat = outline(&list);
        assert_eq!(flat.len(), 200);
        assert_eq!(flat.iter().map(|f| f.2).max(), Some(MAX_SYMBOL_DEPTH - 1));
    }

    #[test]
    fn no_tokens_no_symbols() {
        assert!(symbols("").is_empty());
        assert!(symbols("// комментарий\n").is_empty());
        assert!(symbols("PRINT(1);").is_empty());
    }

    // ── definition ─────────────────────────────────────────────────

    #[test]
    fn a_variable_use_leads_to_its_declaration() {
        assert_eq!(
            target("<<LET>> a = 1;\n<<PRINT>>(|a);"),
            Some(("a".to_string(), (1, 5)))
        );
        assert_eq!(
            target("<<LET>> a = 1;\n<<PRINT>>(a|);"),
            Some(("a".to_string(), (1, 5)))
        );
    }

    #[test]
    fn the_declaration_name_leads_to_itself() {
        assert_eq!(target("<<LET>> a| = 1;"), Some(("a".to_string(), (1, 5))));
        assert_eq!(
            target("<<FN>> fo|o(p) { }"),
            Some(("foo".to_string(), (1, 4)))
        );
        assert_eq!(
            target("<<FN>> foo(|p) { }"),
            Some(("p".to_string(), (1, 8)))
        );
        assert_eq!(
            target("<<STRUCT>> Po|int { x }"),
            Some(("Point".to_string(), (1, 8)))
        );
        assert_eq!(
            target("<<STRUCT>> Point { |x, y }"),
            Some(("x".to_string(), (1, 16)))
        );
        assert_eq!(
            target("<<STRUCT>> Point { x, |y }"),
            Some(("y".to_string(), (1, 19)))
        );
        assert_eq!(
            target("<<FOR>> |i <<IN>> 0..3 { }"),
            Some(("i".to_string(), (1, 5)))
        );
        assert_eq!(
            target("<<MATCH>> 1 { |b -> b }"),
            Some(("b".to_string(), (1, 11)))
        );
        assert_eq!(
            target("<<LET>> f = <<FN>>(|q) { <<RETURN>> q; };"),
            Some(("q".to_string(), (1, 12)))
        );
    }

    #[test]
    fn scopes_decide_which_declaration_a_use_reads() {
        // внутренняя область затеняет внешнюю
        assert_eq!(
            target("<<LET>> a = 1;\n{ <<VAR>> a = 2; <<PRINT>>(|a); }"),
            Some(("a".to_string(), (2, 7)))
        );
        // после блока снова внешняя
        assert_eq!(
            target("<<LET>> a = 1;\n{ <<VAR>> a = 2; }\n<<PRINT>>(|a);"),
            Some(("a".to_string(), (1, 5)))
        );
        // имя не видно в собственном инициализаторе
        assert_eq!(
            target("<<LET>> a = 1;\n{ <<LET>> a = |a + 1; }"),
            Some(("a".to_string(), (1, 5)))
        );
        // параметр функции и тело — одна область
        assert_eq!(
            target("<<LET>> p = 0;\n<<FN>> f(p) { <<RETURN>> |p; }"),
            Some(("p".to_string(), (2, 6)))
        );
        // тело функции не видит глобальную переменную, объявленную ниже
        assert_eq!(
            target("<<FN>> f() { <<RETURN>> |late; }\n<<LET>> late = 1;"),
            None
        );
        // а объявленную выше — видит
        assert_eq!(
            target("<<LET>> early = 1;\n<<FN>> f() { <<RETURN>> |early; }"),
            Some(("early".to_string(), (1, 5)))
        );
        // переменная FOR и тело — одна область; в границах диапазона её нет
        assert_eq!(
            target("<<FOR>> i <<IN>> 0..3 { <<PRINT>>(|i); }"),
            Some(("i".to_string(), (1, 5)))
        );
        assert_eq!(target("<<FOR>> i <<IN>> 0..|i { }"), None);
        // привязка MATCH — область своего пункта
        assert_eq!(
            target("<<MATCH>> 1 { 0 -> 1, b -> |b }"),
            Some(("b".to_string(), (1, 19)))
        );
        // лямбда видит свои параметры
        assert_eq!(
            target("<<LET>> f = <<FN>>(q) { <<RETURN>> |q; };"),
            Some(("q".to_string(), (1, 12)))
        );
    }

    #[test]
    fn a_word_that_starts_the_next_statement_sees_the_previous_declaration() {
        assert_eq!(target("<<LET>> a = 1\n|a"), Some(("a".to_string(), (1, 5))));
    }

    #[test]
    fn calls_follow_the_codegen_order_builtin_function_variable() {
        // функция документа, в том числе объявленная ниже вызова
        assert_eq!(
            target("|foo(1);\n<<FN>> foo(a) { }"),
            Some(("foo".to_string(), (2, 4)))
        );
        // builtin — места в документе нет, даже если есть функция с тем же именем
        assert_eq!(target("|len([1]);"), None);
        assert_eq!(target("<<FN>> len(a) { }\n|len([1]);"), None);
        // функция выигрывает у переменной с тем же именем
        assert_eq!(
            target("<<LET>> foo = 1;\n<<FN>> foo(a) { }\n|foo(2);"),
            Some(("foo".to_string(), (2, 4)))
        );
        // переменная-функция — когда функции с таким именем нет
        assert_eq!(
            target("<<LET>> g = <<FN>>(x) { <<RETURN>> x; };\n|g(2);"),
            Some(("g".to_string(), (1, 5)))
        );
        // неизвестное имя
        assert_eq!(target("|nothing(1);"), None);
    }

    #[test]
    fn struct_names_and_keys_lead_to_the_struct_and_its_field() {
        let program = "<<STRUCT>> P { x, y }\n<<LET>> p = P { x: 1, y: 2 };";
        assert_eq!(
            target(&program.replace("= P {", "= |P {")),
            Some(("P".to_string(), (1, 8)))
        );
        assert_eq!(
            target(&program.replace("{ x: 1", "{ |x: 1")),
            Some(("x".to_string(), (1, 12)))
        );
        assert_eq!(
            target(&program.replace("y: 2", "|y: 2")),
            Some(("y".to_string(), (1, 15)))
        );
        // нет такого поля или структуры
        assert_eq!(target(&program.replace("y: 2", "|z: 2")), None);
        assert_eq!(target("<<LET>> p = Q { |x: 1 };"), None);
        // пустой литерал — тоже структура; в заголовке IF `имя { }` — не она
        assert_eq!(
            target("<<STRUCT>> E { }\n<<LET>> e = |E { };"),
            Some(("E".to_string(), (1, 8)))
        );
        assert_eq!(target("<<STRUCT>> E { }\n<<IF>> |E { }"), None);
    }

    #[test]
    fn places_without_a_definition_give_nothing() {
        // после `.` — поле или метод
        assert_eq!(target("<<LET>> p = 1;\np.|p;"), None);
        assert_eq!(target("<<LET>> a = [1];\na.|push(1);"), None);
        // аннотация типа
        assert_eq!(target("<<LET>> int = 1;\n<<LET>> a: |int = 2;"), None);
        assert_eq!(target("<<FN>> f(a: |int) -> int { }"), None);
        assert_eq!(target("<<FN>> f(a) -> |int { }"), None);
        // ключевые слова, числа, строки, комментарии
        assert_eq!(target("|<<LET>> a = 1;"), None);
        assert_eq!(target("<<LET>> a = |12;"), None);
        assert_eq!(target("<<LET>> a = \"x |a\";"), None);
        assert_eq!(target("<<LET>> a = 1; // |a"), None);
        // имя функции в позиции значения — «неопределённая переменная»
        assert_eq!(target("<<FN>> f() { }\n<<LET>> g = |f;"), None);
        // `_` — не привязка и не переменная
        assert_eq!(target("<<MATCH>> 1 { |_ -> 1 }"), None);
        // пустой документ и пробелы
        assert_eq!(target("|"), None);
        assert_eq!(target("<<LET>> a = 1;   |   "), None);
    }

    #[test]
    fn a_statement_inside_a_struct_body_declares_nothing() {
        // испорченный текст: `LET` в теле `STRUCT`; слова тела — имена полей, а
        // определение обязано вести только туда, где `definition` само отвечает
        // «объявление» (иначе переход заканчивался бы на «ничего»)
        let body = "<<STRUCT>> {\n    <<LET>> x = 2;\n    f(|x);\n}";
        assert_eq!(target(body), None);
        // тот же оператор вне тела структуры объявляет как обычно
        assert_eq!(
            target("<<LET>> x = 2;\nf(|x);"),
            Some(("x".to_string(), (1, 5)))
        );
        // а внутри блока в теле структуры — по-прежнему
        assert_eq!(
            target("<<STRUCT>> {\n    { <<LET>> x = 2; f(|x); }\n}"),
            Some(("x".to_string(), (2, 11)))
        );
    }

    #[test]
    fn a_repeated_declaration_in_one_scope_resolves_to_the_later_one() {
        // повтор имени в одной области — ошибка семантики, но пока её
        // исправляют, определение отвечает как автодополнение: побеждает позднее
        assert_eq!(
            target("<<LET>> x = 1;\n<<VAR>> x = 2;\n<<PRINT>>(|x);"),
            Some(("x".to_string(), (2, 5)))
        );
        assert_eq!(
            target("{ <<LET>> y = 1; <<LET>> y = 2; <<PRINT>>(|y); }"),
            Some(("y".to_string(), (1, 18)))
        );
        // а внутренняя область по-прежнему раньше внешней, даже с повтором снаружи
        assert_eq!(
            target("<<LET>> z = 1;\n<<LET>> z = 2;\n{ <<LET>> z = 3; <<PRINT>>(|z); }"),
            Some(("z".to_string(), (3, 7)))
        );
    }

    #[test]
    fn a_function_name_is_a_declaration_only_with_the_parenthesis_after_it() {
        // пока после имени нет `(`, функции нет: ни символа, ни определения
        assert_eq!(target("<<FN>> fo|o"), None);
        assert_eq!(target("<<FN>> fo|o bar"), None);
        assert_eq!(target("<<FN>> fo|o { }"), None);
        assert!(symbols("<<FN>> foo bar").is_empty());
        // со скобкой — есть, даже если заголовок не дописан
        assert_eq!(target("<<FN>> fo|o("), Some(("foo".to_string(), (1, 4))));
        assert_eq!(symbols("<<FN>> foo(").len(), 1);
    }

    #[test]
    fn the_word_where_a_function_header_breaks_has_no_definition() {
        // заголовок «сломался» на слове `y`: оно не параметр и не использование
        let before = "<<LET>> y = 1;\n";
        for header in [
            "<<FN>> f(x |y) { }",
            "<<FN>> f(x: int |y) { }",
            "<<FN>> f(x) -> int |y { }",
            "<<FN>> foo |y() { }",
        ] {
            assert_eq!(target(&format!("{before}{header}")), None, "{header}");
        }
        // а слова после него — уже обычный код
        assert_eq!(
            target("<<LET>> z = 1;\n<<FN>> f(x y |z) { }"),
            Some(("z".to_string(), (1, 5)))
        );
    }

    #[test]
    fn a_stray_mut_outside_a_function_header_declares_nothing() {
        assert_eq!(target("<<LET>> a = <<MUT>> |b;"), None);
        assert_eq!(target("<<MUT>> |b;"), None);
        // и после законченной функции: `MUT` — параметр только в заголовке
        assert_eq!(target("<<FN>> f(<<MUT>> p) { }\n<<MUT>> |p;"), None);
        assert_eq!(
            target("<<FN>> f(<<MUT>> |p) { }"),
            Some(("p".to_string(), (1, 10)))
        );
    }

    #[test]
    fn a_top_level_declaration_cut_off_inside_its_value_is_still_a_symbol() {
        // файл оборван посреди тела функции-значения: `f` — переменная верхнего
        // уровня и доходит до последнего токена, а `inner` внутри — не символ
        let list = symbols("<<LET>> f = <<FN>>(x) { <<LET>> inner = 1");
        assert_eq!(
            outline(&list),
            vec![("f".to_string(), SymbolKind::Variable, 0)]
        );
        assert_eq!((list[0].start, list[0].end), ((1, 1), (1, 30)));
        // то же внутри незакрытой скобки
        let list = symbols("<<LET>> a = (<<LET>> b = 1");
        assert_eq!(
            outline(&list),
            vec![("a".to_string(), SymbolKind::Variable, 0)]
        );
        assert_eq!((list[0].start, list[0].end), ((1, 1), (1, 19)));
    }

    #[test]
    fn the_word_ending_at_the_position_counts() {
        assert_eq!(
            target("<<LET>> abc = 1;\n<<PRINT>>(abc|);"),
            Some(("abc".to_string(), (1, 5)))
        );
    }

    #[test]
    fn the_text_mode_has_neither_symbols_nor_definitions() {
        let garbage = "@ ".repeat(40);
        let doc = Document::new(&garbage);
        assert!(doc.text_mode);
        assert!(document_symbols(&doc).is_empty());
        assert_eq!(definition(&doc, (1, 1)), None);
    }
}
