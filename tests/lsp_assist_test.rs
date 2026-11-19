//! Подсказки языкового сервера против настоящего компилятора (T018, M007).
//!
//! Юнит-тесты `lsp::scan` и `lsp::assist` проверяют сканер на примерах,
//! которые написал тот же автор, что и сканер, — поэтому здесь он сверяется с
//! независимым источником истины:
//!
//! 1. ВИДИМОСТЬ ИМЁН. У набираемого текста нет AST, и сканер подсказок
//!    повторяет правила `semantic::Analyzer` на токенах. Повторение правил —
//!    место расхождений. В программе без ошибок на место использования имени
//!    (узлы `ExprKind::Ident` из настоящего AST) подставляется каждое
//!    объявленное в программе имя, а на каждую границу операторов вставляется
//!    `PRINT(имя);`. Решение сканера «имя предложено как локальное»
//!    обязано совпасть с решением `analyze_source` «неопределённой переменной
//!    нет». Корпус: примеры `examples/`, рукописные программы на каждое
//!    правило видимости и случайные программы. Тот же текст, обрезанный по
//!    строке и без закрывающих скобок (так выглядит набор), проверяется против
//!    дописанного закрывающими скобками.
//! 2. СЛОВАРЬ. Тип результата и арность каждого builtin из `lsp::vocabulary`
//!    проверяются ИСПОЛНЕНИЕМ и компиляцией настоящих вызовов.
//! 3. УСТОЙЧИВОСТЬ. Подсказки на испорченных программах и на документах
//!    предельного размера: без паник, с инвариантами результата и в бюджете
//!    времени. Те же 18 документов в 1 МиБ проходят и через навигацию (T019:
//!    `documentSymbol`, `definition`) — её содержательные проверки лежат в
//!    `tests/lsp_nav_test.rs`.
//!
//! Ключевые слова в шаблонах пишутся `@LET`, `@IF`, … — подставляются словами
//! из SGA-букв (идентификаторы и числа — обычный ASCII, как в языке).

use sga::ast::{Expr, ExprKind, MatchArm, Pattern, Program, Stmt, StmtKind};
use sga::lexer::token::TokenKind;
use sga::lexer::Lexer;
use sga::lsp::assist::{self, ItemKind};
use sga::lsp::json::Json;
use sga::lsp::nav::MAX_SYMBOL_DEPTH;
use sga::lsp::scan::{Document, Pos};
use sga::lsp::{vocabulary, Server, MAX_DOCUMENT_BYTES};
use sga::parser::Parser;
use sga::runtime::{self, Value};
use sga::sga_alphabet::{encode_word, is_sga_letter};
use sga::{diagnostics, semantic, RunOptions};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ── общие помощники ────────────────────────────────────────────────────────

/// Подставляет SGA-слова: `@LET` -> слово `LET` из SGA-букв. Слово — максимальная
/// последовательность заглавных ASCII-букв после `@`.
fn sga(template: &str) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '@' {
            out.push(c);
            continue;
        }
        let mut word = String::new();
        while let Some(&next) = chars.peek() {
            if !next.is_ascii_uppercase() {
                break;
            }
            word.push(next);
            chars.next();
        }
        assert!(!word.is_empty(), "`@` без слова в шаблоне {template:?}");
        out.push_str(&encode_word(&word));
    }
    out
}

fn is_code(diagnostic: &diagnostics::Diagnostic, code: &str) -> bool {
    diagnostic.code.as_str() == code
}

// ── обход настоящего AST ───────────────────────────────────────────────────

enum Node<'a> {
    Stmt(&'a Stmt),
    Expr(&'a Expr),
    Arm(&'a MatchArm),
}

fn stmts<'a>(list: &'a [Stmt]) -> impl Iterator<Item = Node<'a>> {
    list.iter().map(Node::Stmt)
}

fn exprs<'a>(list: &'a [Expr]) -> impl Iterator<Item = Node<'a>> {
    list.iter().map(Node::Expr)
}

/// Дети узла. Все варианты перечислены без `_`: новый вариант узла не
/// скомпилируется, пока его не учтут здесь.
fn children<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    match node {
        Node::Arm(arm) => vec![Node::Expr(&arm.body)],
        Node::Stmt(stmt) => match &stmt.kind {
            StmtKind::VarDecl { value, .. } => vec![Node::Expr(value)],
            StmtKind::ExprStmt(e) => vec![Node::Expr(e)],
            StmtKind::Print(args) => exprs(args).collect(),
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let mut kids = vec![Node::Expr(cond)];
                kids.extend(stmts(then_branch));
                if let Some(else_branch) = else_branch {
                    kids.extend(stmts(else_branch));
                }
                kids
            }
            StmtKind::While { cond, body } => {
                let mut kids = vec![Node::Expr(cond)];
                kids.extend(stmts(body));
                kids
            }
            StmtKind::ForIn {
                start, end, body, ..
            } => {
                let mut kids = vec![Node::Expr(start), Node::Expr(end)];
                kids.extend(stmts(body));
                kids
            }
            StmtKind::FnDecl { body, .. } => stmts(body).collect(),
            StmtKind::Return(value) => value.iter().map(Node::Expr).collect(),
            StmtKind::Block(body) => stmts(body).collect(),
            StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Import(_)
            | StmtKind::StructDecl { .. } => vec![],
        },
        Node::Expr(expr) => match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::Nil
            | ExprKind::Ident(_) => vec![],
            ExprKind::Array(items) => exprs(items).collect(),
            ExprKind::Index(target, index) => vec![Node::Expr(target), Node::Expr(index)],
            ExprKind::Unary(_, operand) => vec![Node::Expr(operand)],
            ExprKind::Binary(_, left, right) => vec![Node::Expr(left), Node::Expr(right)],
            ExprKind::Call(_, args) => exprs(args).collect(),
            ExprKind::CallExpr(callee, args) => {
                let mut kids = vec![Node::Expr(callee)];
                kids.extend(exprs(args));
                kids
            }
            ExprKind::Assign(_, value) => vec![Node::Expr(value)],
            ExprKind::IndexAssign(target, index, value) => {
                vec![Node::Expr(target), Node::Expr(index), Node::Expr(value)]
            }
            ExprKind::Lambda { body, .. } => stmts(body).collect(),
            ExprKind::StructLit { fields, .. } => {
                fields.iter().map(|(_, e)| Node::Expr(e)).collect()
            }
            ExprKind::FieldAccess(object, _) => vec![Node::Expr(object)],
            ExprKind::FieldAssign(object, _, value) => {
                vec![Node::Expr(object), Node::Expr(value)]
            }
            ExprKind::MethodCall(object, _, args) => {
                let mut kids = vec![Node::Expr(object)];
                kids.extend(exprs(args));
                kids
            }
            ExprKind::Match(scrutinee, arms) => {
                let mut kids = vec![Node::Expr(scrutinee)];
                kids.extend(arms.iter().map(Node::Arm));
                kids
            }
        },
    }
}

/// Списки операторов, непосредственно вложенные в узел.
fn lists<'a>(node: &Node<'a>) -> Vec<&'a [Stmt]> {
    match node {
        Node::Arm(_) => vec![],
        Node::Stmt(stmt) => match &stmt.kind {
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                let mut found = vec![then_branch.as_slice()];
                if let Some(else_branch) = else_branch {
                    found.push(else_branch.as_slice());
                }
                found
            }
            StmtKind::While { body, .. }
            | StmtKind::ForIn { body, .. }
            | StmtKind::FnDecl { body, .. } => vec![body.as_slice()],
            StmtKind::Block(body) => vec![body.as_slice()],
            StmtKind::VarDecl { .. }
            | StmtKind::ExprStmt(_)
            | StmtKind::Print(_)
            | StmtKind::Return(_)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Import(_)
            | StmtKind::StructDecl { .. } => vec![],
        },
        Node::Expr(expr) => match &expr.kind {
            ExprKind::Lambda { body, .. } => vec![body.as_slice()],
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::Nil
            | ExprKind::Ident(_)
            | ExprKind::Array(_)
            | ExprKind::Index(..)
            | ExprKind::Unary(..)
            | ExprKind::Binary(..)
            | ExprKind::Call(..)
            | ExprKind::CallExpr(..)
            | ExprKind::Assign(..)
            | ExprKind::IndexAssign(..)
            | ExprKind::StructLit { .. }
            | ExprKind::FieldAccess(..)
            | ExprKind::FieldAssign(..)
            | ExprKind::MethodCall(..)
            | ExprKind::Match(..) => vec![],
        },
    }
}

/// Что программа даёт для проверки видимости.
#[derive(Default)]
struct Facts {
    /// Индексы токенов-идентификаторов, прочитанных как переменные.
    ident_uses: Vec<usize>,
    /// Индексы токенов, перед которыми допустим новый оператор (с учётом
    /// предыдущего токена — см. `statement_boundaries`).
    boundaries: Vec<usize>,
    /// Все имена, объявленные в программе (переменные, параметры, функции,
    /// структуры, переменные `FOR`, привязки `MATCH`).
    names: BTreeSet<String>,
}

fn walk(node: Node, facts: &mut Facts) {
    match &node {
        Node::Stmt(stmt) => match &stmt.kind {
            StmtKind::VarDecl { name, .. } | StmtKind::StructDecl { name, .. } => {
                facts.names.insert(name.clone());
            }
            StmtKind::ForIn { var, .. } => {
                facts.names.insert(var.clone());
            }
            StmtKind::FnDecl { name, params, .. } => {
                facts.names.insert(name.clone());
                facts.names.extend(params.iter().map(|p| p.name.clone()));
            }
            _ => {}
        },
        Node::Expr(expr) => match &expr.kind {
            ExprKind::Ident(_) => {
                assert_eq!(
                    expr.span.first, expr.span.last,
                    "идентификатор — один токен"
                );
                facts.ident_uses.push(expr.span.first as usize);
            }
            ExprKind::Lambda { params, .. } => facts.names.extend(params.iter().cloned()),
            _ => {}
        },
        Node::Arm(arm) => {
            if let Pattern::Bind(name) = &arm.pattern {
                facts.names.insert(name.clone());
            }
        }
    }
    for list in lists(&node) {
        list_boundaries(list, &mut facts.boundaries);
    }
    for child in children(&node) {
        walk(child, facts);
    }
}

/// Границы списка операторов: перед каждым оператором и после последнего.
fn list_boundaries(list: &[Stmt], out: &mut Vec<usize>) {
    for stmt in list {
        out.push(stmt.span.first as usize);
    }
    if let Some(last) = list.last() {
        out.push(last.span.last as usize + 1);
    }
}

fn collect_facts(program: &Program) -> Facts {
    let mut facts = Facts::default();
    list_boundaries(program, &mut facts.boundaries);
    for stmt in program {
        walk(Node::Stmt(stmt), &mut facts);
    }
    facts
}

// ── правка текста по позициям лексера ──────────────────────────────────────

struct Source {
    chars: Vec<char>,
    line_starts: Vec<usize>,
}

impl Source {
    fn new(text: &str) -> Source {
        let chars: Vec<char> = text.chars().collect();
        let mut line_starts = vec![0];
        for (i, c) in chars.iter().enumerate() {
            if *c == '\n' {
                line_starts.push(i + 1);
            }
        }
        Source { chars, line_starts }
    }

    fn offset(&self, (line, col): Pos) -> usize {
        (self.line_starts[line - 1] + col - 1).min(self.chars.len())
    }

    /// Текст, в котором `[start, end)` заменён на `with`.
    fn splice(&self, start: Pos, end: Pos, with: &str) -> String {
        let mut out: String = self.chars[..self.offset(start)].iter().collect();
        out.push_str(with);
        out.extend(&self.chars[self.offset(end)..]);
        out
    }
}

// ── вердикты ───────────────────────────────────────────────────────────────

/// Вердикт настоящего анализатора: `Some(true)` — переменная видна (ошибки
/// «неопределённая переменная» нет), `Some(false)` — не видна, `None` — текст
/// не разбирается (проверка места пропускается).
fn compiler_says_visible(text: &str, name: &str) -> Option<bool> {
    let found = diagnostics::analyze_source(text);
    if found
        .iter()
        .any(|d| is_code(d, "lexer") || is_code(d, "parser"))
    {
        return None;
    }
    let undefined = format!("неопределённая переменная '{name}'");
    Some(!found.iter().any(|d| d.message.contains(&undefined)))
}

/// Вердикт подсказок: имя предложено автодополнением как локальное (переменная
/// или константа) в позиции `cursor`.
fn scanner_says_visible(text: &str, cursor: Pos, name: &str) -> bool {
    let doc = Document::new(text);
    assist::complete(&doc, text, cursor)
        .items
        .iter()
        .any(|i| i.label == name && matches!(i.kind, ItemKind::Variable | ItemKind::Constant))
}

#[derive(Default, Debug)]
struct Stats {
    programs: usize,
    checked: usize,
    skipped: usize,
    visible: usize,
    hidden: usize,
}

/// Сколько мест и имён проверяется в одной программе (равномерная выборка).
const MAX_SITES: usize = 48;
const MAX_NAMES: usize = 12;

fn sample<T: Clone>(items: &[T], limit: usize) -> Vec<T> {
    if items.len() <= limit {
        return items.to_vec();
    }
    (0..limit)
        .map(|i| items[i * items.len() / limit].clone())
        .collect()
}

/// Первая строка текста вокруг позиции — для сообщения о расхождении.
fn line_of(text: &str, pos: Pos) -> String {
    text.split('\n')
        .nth(pos.0 - 1)
        .unwrap_or_default()
        .to_string()
}

#[derive(Clone, Copy)]
enum Site {
    /// Токен-идентификатор заменяется кандидатом.
    Use { start: Pos, end: Pos },
    /// Перед позицией вставляется оператор `PRINT(кандидат);`.
    Insert { at: Pos },
}

/// Проверяет программу `src` (без ошибок): для каждого места и каждого имени
/// решения сканера и компилятора совпадают. Расхождения — в `failures`.
fn differential(label: &str, src: &str, stats: &mut Stats, failures: &mut Vec<String>) {
    let base = diagnostics::analyze_source(src);
    assert!(
        base.is_empty(),
        "{label}: исходная программа должна быть без ошибок, а анализатор говорит: {:?}",
        base.first().map(|d| &d.message)
    );
    let (tokens, table) = Lexer::new(src).tokenize_with_spans().expect("лексер");
    let program = Parser::new(tokens.clone()).parse_program().expect("парсер");
    let facts = collect_facts(&program);
    let source = Source::new(src);

    let mut sites: Vec<Site> = facts
        .ident_uses
        .iter()
        .map(|&i| Site::Use {
            start: (table[i].line, table[i].col),
            end: (table[i].end_line, table[i].end_col),
        })
        .collect();
    let insertable = |i: usize| {
        i == 0
            || matches!(
                tokens[i - 1].kind,
                TokenKind::LBrace | TokenKind::RBrace | TokenKind::Semicolon
            )
    };
    sites.extend(
        facts
            .boundaries
            .iter()
            .filter(|&&i| i < table.len() && insertable(i))
            .map(|&i| Site::Insert {
                at: (table[i].line, table[i].col),
            }),
    );
    let sites = sample(&sites, MAX_SITES);

    let mut names: Vec<String> = facts.names.iter().cloned().collect();
    names = sample(&names, MAX_NAMES);
    // не объявлено нигде, builtin — для них видимости быть не должно
    names.push("zzz_undefined".to_string());
    names.push("len".to_string());

    stats.programs += 1;
    for site in sites {
        for name in &names {
            let (text, cursor) = match site {
                Site::Use { start, end } => (
                    source.splice(start, end, name),
                    (start.0, start.1 + name.chars().count()),
                ),
                Site::Insert { at } => (
                    source.splice(at, at, &format!("{}({name}); ", encode_word("PRINT"))),
                    // `PRINT(` — пять букв и скобка
                    (at.0, at.1 + 6 + name.chars().count()),
                ),
            };
            let Some(expected) = compiler_says_visible(&text, name) else {
                stats.skipped += 1;
                continue;
            };
            let actual = scanner_says_visible(&text, cursor, name);
            stats.checked += 1;
            if expected {
                stats.visible += 1;
            } else {
                stats.hidden += 1;
            }
            if actual != expected {
                failures.push(format!(
                    "{label}: имя `{name}` в {}:{} — компилятор: {}, подсказки: {}\n    строка: {}",
                    cursor.0,
                    cursor.1,
                    if expected {
                        "видно"
                    } else {
                        "не видно"
                    },
                    if actual {
                        "видно"
                    } else {
                        "не видно"
                    },
                    line_of(&text, cursor),
                ));
            }
        }
    }
}

fn assert_no_failures(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "расхождений: {}\n{}",
        failures.len(),
        failures
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ── 1. видимость имён: примеры и рукописные программы ──────────────────────

fn example_sources() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut found: Vec<(String, String)> = std::fs::read_dir(&dir)
        .expect("каталог examples")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()? != "sga" {
                return None;
            }
            let name = path.file_name()?.to_string_lossy().to_string();
            Some((name, std::fs::read_to_string(&path).expect("пример")))
        })
        .collect();
    found.sort();
    found
}

#[test]
fn visibility_matches_the_compiler_on_the_examples() {
    let examples = example_sources();
    assert_eq!(examples.len(), 17, "в examples/ должно быть 17 примеров");
    let mut stats = Stats::default();
    let mut failures = Vec::new();
    let mut with_import = 0;
    for (name, text) in &examples {
        // `IMPORT` сервер не разрешает: анализ такого файла — одно
        // информационное сообщение, видимость не проверяется (границы T018)
        let first = diagnostics::analyze_source(text);
        if first.first().is_some_and(|d| is_code(d, "limit")) {
            with_import += 1;
            continue;
        }
        differential(name, text, &mut stats, &mut failures);
    }
    eprintln!("examples: {stats:?}");
    assert_no_failures(&failures);
    assert_eq!(with_import, 1, "IMPORT есть ровно в одном примере");
    assert_eq!(stats.programs, 16);
    assert!(
        stats.visible > 100 && stats.hidden > 100,
        "проверка не должна быть вырожденной: {stats:?}"
    );
}

/// Рукописные программы: каждая бьёт в одно правило видимости (названия — в
/// комментариях перед программой).
const HANDWRITTEN: &[(&str, &str)] = &[
    (
        "глобальные до и после функции",
        "@LET g1 = 1;\n@FN f(a, b) { @LET c = a + b; @RETURN c + g1; }\n@LET g2 = 2;\n@PRINT(f(g1, g2));\n",
    ),
    (
        "тело функции видит только ранее объявленные глобальные",
        "@LET a = 1;\n@FN f() { @RETURN a; }\n@LET b = 2;\n@FN g() { @RETURN a + b; }\n@PRINT(f() + g());\n",
    ),
    (
        "вложенные блоки и затенение",
        "@LET x = 1;\n{\n  @LET y = x + 1;\n  {\n    @LET x = y + 1;\n    @PRINT(x);\n  }\n  @PRINT(x + y);\n}\n@PRINT(x);\n",
    ),
    (
        "FOR: переменная цикла только в теле, вложенные циклы",
        "@VAR total = 0;\n@FOR i @IN 0..3 {\n  @FOR j @IN 0..i {\n    @LET k = i + j;\n    total = total + k;\n  }\n}\n@PRINT(total);\n",
    ),
    (
        "FOR затеняет глобальную переменную",
        "@LET i = 100;\n@FOR i @IN 0..3 { @PRINT(i); }\n@PRINT(i);\n",
    ),
    (
        "замыкания видят окружение и свои параметры",
        "@LET base = 10;\n@LET add = @FN(a) { @RETURN a + base; };\n@LET twice = @FN(f, v) { @RETURN f(f(v)); };\n@PRINT(twice(add, 1));\n@FN outer(p) {\n  @LET q = p * 2;\n  @LET inner = @FN(z) { @RETURN z + q + p; };\n  @RETURN inner(1);\n}\n@PRINT(outer(3));\n",
    ),
    (
        "лямбда в лямбде",
        "@LET outer = 1;\n@LET mk = @FN(a) {\n  @LET mid = a + outer;\n  @RETURN @FN(b) { @RETURN a + b + mid + outer; };\n};\n@PRINT(mk(1)(2));\n",
    ),
    (
        "привязка MATCH только в своём пункте",
        "@LET v = 3;\n@LET r = @MATCH v {\n  0 -> 100,\n  n -> n + v\n};\n@PRINT(r, @MATCH v { 1 -> 10, k -> k + 1 });\n",
    ),
    (
        "вложенный MATCH в функции",
        "@FN classify(v) {\n  @RETURN @MATCH v {\n    0 -> 0,\n    n -> @MATCH n { 1 -> n, m -> m + n }\n  };\n}\n@PRINT(classify(5));\n",
    ),
    (
        "IF / ELSE IF / ELSE и WHILE",
        "@VAR n = 3;\n@WHILE n > 0 {\n  @LET step = 1;\n  n = n - step;\n  @IF n == 1 {\n    @LET a = n;\n    @PRINT(a);\n  } @ELSE @IF n == 2 {\n    @LET b = n;\n    @PRINT(b);\n  } @ELSE {\n    @LET c = n;\n    @PRINT(c + step);\n  }\n}\n@PRINT(n);\n",
    ),
    (
        "struct, MUT-параметр и поля",
        "@STRUCT Point { x, y }\n@FN move(@MUT p, dx) { p.x = p.x + dx; }\n@VAR pt = Point { x: 1, y: 2 };\nmove(pt, 5);\n@PRINT(pt.x);\n",
    ),
    (
        "значения полей литерала struct",
        "@STRUCT P { a, b }\n@LET u = 1;\n@LET w = 2;\n@LET s = P { a: u, b: w + u };\n@PRINT(s.a);\n",
    ),
    (
        "функции не являются переменными",
        "@FN even(n) { @IF n == 0 { @RETURN @TRUE; } @RETURN odd(n - 1); }\n@FN odd(n) { @IF n == 0 { @RETURN @FALSE; } @RETURN even(n - 1); }\n@PRINT(even(4));\n",
    ),
    (
        "последовательные объявления",
        "@LET a = 1;\n@LET b = a + 1;\n@LET c = b + a;\n@PRINT(a, b, c);\n",
    ),
    (
        "CONST и VAR",
        "@CONST k = 5;\n@VAR acc = k;\nacc = acc + k;\n@PRINT(acc);\n",
    ),
    (
        "параметр затеняет глобальную",
        "@LET x = 1;\n@FN f(x) { @RETURN x + 1; }\n@PRINT(f(x));\n",
    ),
    (
        "замыкание внутри цикла внутри функции",
        "@FN run(limit) {\n  @VAR i = 0;\n  @WHILE i < limit {\n    @LET sq = @FN(v) { @RETURN v * v + i; };\n    @PRINT(sq(i));\n    i = i + 1;\n  }\n  @RETURN i;\n}\n@PRINT(run(3));\n",
    ),
    (
        "массивы, индексы и границы FOR",
        "@LET arr = [1, 2, 3];\n@LET idx = 1;\n@PRINT(arr[idx], len(arr));\n@FOR e @IN 0..len(arr) { @PRINT(arr[e] + idx); }\n",
    ),
    (
        "логические операции",
        "@LET a = @TRUE;\n@LET b = @FALSE;\n@LET c = a @AND @NOT b;\n@IF c @OR a { @PRINT(a); }\n",
    ),
    (
        "типы и аннотации",
        "@LET x: int = 1;\n@FN f(a: int, b: string) -> int { @RETURN a; }\n@PRINT(f(x, \"s\"));\n",
    ),
    (
        "строки и комментарии рядом с кодом",
        "// комментарий\n@LET s = \"text // not a comment\";\n@LET t = s + \"x\"; // хвост\n@PRINT(t);\n",
    ),
    (
        "без точек с запятой",
        "@LET a = 1\n@LET b = a +\n  2\n@PRINT(a, b)\n",
    ),
    (
        "RETURN, BREAK и CONTINUE",
        "@FN find(limit) {\n  @VAR i = 0;\n  @WHILE @TRUE {\n    i = i + 1;\n    @IF i < 2 { @CONTINUE; }\n    @IF i > limit { @BREAK; }\n  }\n  @RETURN i;\n}\n@PRINT(find(3));\n",
    ),
    (
        "блок как оператор после объявлений",
        "@LET a = 1;\n{ @LET b = a; { @LET c = b + a; @PRINT(c); } }\n@LET d = a;\n@PRINT(d);\n",
    ),
];

#[test]
fn visibility_matches_the_compiler_on_handwritten_programs() {
    let mut stats = Stats::default();
    let mut failures = Vec::new();
    for (label, template) in HANDWRITTEN {
        differential(label, &sga(template), &mut stats, &mut failures);
    }
    eprintln!("handwritten: {stats:?}");
    assert_no_failures(&failures);
    assert_eq!(stats.programs, HANDWRITTEN.len());
    assert!(
        stats.visible > 100 && stats.hidden > 100,
        "проверка не должна быть вырожденной: {stats:?}"
    );
    assert!(
        stats.skipped * 20 < stats.checked,
        "слишком много пропущенных мест (текст не разбирается): {stats:?}"
    );
}

// ── 1б. видимость имён: случайные программы и набор «на лету» ───────────────

/// Детерминированный генератор псевдослучайных чисел (xorshift64*): одни и те
/// же зерна дают одни и те же программы, поэтому расхождение воспроизводимо.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        ((self.next() >> 33) as usize) % n
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

/// Случайная программа по строкам: после каждой строки известно, какие блоки
/// открыты и чем закрываются.
#[derive(Default)]
struct Generated {
    lines: Vec<String>,
    open_after: Vec<Vec<&'static str>>,
    names: Vec<String>,
}

impl Generated {
    fn template(&self) -> String {
        self.lines.join("\n") + "\n"
    }
}

#[derive(Clone, Copy)]
struct Ctx {
    in_loop: bool,
    top: bool,
}

struct Gen {
    rng: Rng,
    next_id: usize,
    /// Области видимости: имя и изменяемость (`VAR`).
    scopes: Vec<Vec<(String, bool)>>,
    /// Индекс первой области текущей функции: присваивать можно только
    /// `VAR` этой функции (изменять захваченные нельзя).
    fn_base: usize,
    functions: Vec<(String, usize)>,
    open: Vec<&'static str>,
    out: Generated,
}

impl Gen {
    fn new(seed: u64) -> Gen {
        Gen {
            rng: Rng::new(seed),
            next_id: 0,
            scopes: vec![Vec::new()],
            fn_base: 0,
            functions: Vec::new(),
            open: Vec::new(),
            out: Generated::default(),
        }
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        let name = format!("{prefix}{}", self.next_id);
        self.out.names.push(name.clone());
        name
    }

    fn emit(&mut self, text: String) {
        self.out.lines.push(text);
        self.out.open_after.push(self.open.clone());
    }

    fn open_block(&mut self, header: String, closer: &'static str) {
        self.open.push(closer);
        self.emit(header);
        self.scopes.push(Vec::new());
    }

    fn close_block(&mut self, text: &str) {
        self.scopes.pop();
        self.open.pop();
        self.emit(text.to_string());
    }

    fn visible(&self) -> Vec<String> {
        self.scopes
            .iter()
            .flatten()
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn declare(&mut self, name: &str, mutable: bool) {
        self.scopes
            .last_mut()
            .expect("область")
            .push((name.to_string(), mutable));
    }

    fn expr(&mut self, depth: usize) -> String {
        let roll = self.rng.below(100);
        let vars = self.visible();
        if depth >= 3 || roll < 25 {
            return self.rng.below(10).to_string();
        }
        if roll < 55 && !vars.is_empty() {
            return vars[self.rng.below(vars.len())].clone();
        }
        if roll < 75 {
            let op = ["+", "-", "*"][self.rng.below(3)];
            return format!("({} {op} {})", self.expr(depth + 1), self.expr(depth + 1));
        }
        if roll < 85 && !self.functions.is_empty() {
            let (name, arity) = self.functions[self.rng.below(self.functions.len())].clone();
            let args: Vec<String> = (0..arity).map(|_| self.expr(depth + 1)).collect();
            return format!("{name}({})", args.join(", "));
        }
        if roll < 92 {
            return format!("len([{}, {}])", self.expr(depth + 1), self.expr(depth + 1));
        }
        // MATCH: привязка видна только в своём пункте
        let scrutinee = self.expr(depth + 1);
        let first = self.expr(depth + 1);
        let bind = self.fresh("b");
        self.scopes.push(vec![(bind.clone(), false)]);
        let body = self.expr(depth + 1);
        self.scopes.pop();
        format!("@MATCH {scrutinee} {{ 0 -> {first}, {bind} -> {body} }}")
    }

    fn cond(&mut self) -> String {
        let op = ["<", ">", "==", "!="][self.rng.below(4)];
        format!("{} {op} {}", self.expr(1), self.expr(1))
    }

    fn body(&mut self, depth: usize, ctx: Ctx) {
        for _ in 0..=self.rng.below(3) {
            self.stmt(depth, Ctx { top: false, ..ctx });
        }
    }

    fn params(&mut self, count: usize) -> Vec<String> {
        (0..count).map(|_| self.fresh("p")).collect()
    }

    fn stmt(&mut self, depth: usize, ctx: Ctx) {
        let roll = self.rng.below(100);
        let simple = depth >= 3;
        if roll < 28 || simple && roll < 60 {
            // объявление: имя появляется ПОСЛЕ инициализатора
            let init = self.expr(0);
            let (word, mutable) = match self.rng.below(4) {
                0 => ("@VAR", true),
                1 => ("@CONST", false),
                _ => ("@LET", false),
            };
            let shadow = depth > 0 && self.rng.chance(12);
            let outer: Vec<String> = self
                .scopes
                .iter()
                .take(self.scopes.len() - 1)
                .flatten()
                .map(|(n, _)| n.clone())
                .collect();
            let name = if shadow && !outer.is_empty() {
                outer[self.rng.below(outer.len())].clone()
            } else {
                self.fresh("v")
            };
            self.emit(format!("{word} {name} = {init};"));
            self.declare(&name, mutable);
        } else if roll < 42 || simple && roll < 80 {
            let e = self.expr(0);
            self.emit(format!("@PRINT({e});"));
        } else if roll < 50 || simple {
            let targets: Vec<String> = self.scopes[self.fn_base..]
                .iter()
                .flatten()
                .filter(|(_, mutable)| *mutable)
                .map(|(n, _)| n.clone())
                .collect();
            if targets.is_empty() {
                let e = self.expr(0);
                self.emit(format!("@PRINT({e});"));
            } else {
                let target = targets[self.rng.below(targets.len())].clone();
                let e = self.expr(0);
                self.emit(format!("{target} = {e};"));
            }
        } else if roll < 60 {
            let c = self.cond();
            self.open_block(format!("@IF {c} {{"), "}");
            self.body(depth + 1, ctx);
            if self.rng.chance(50) {
                self.close_block("");
                self.out.lines.pop();
                self.out.open_after.pop();
                self.open.push("}");
                self.emit("} @ELSE {".to_string());
                self.scopes.push(Vec::new());
                self.body(depth + 1, ctx);
            }
            self.close_block("}");
        } else if roll < 66 {
            let c = self.cond();
            self.open_block(format!("@WHILE {c} {{"), "}");
            self.body(
                depth + 1,
                Ctx {
                    in_loop: true,
                    ..ctx
                },
            );
            self.close_block("}");
        } else if roll < 74 {
            let var = self.fresh("i");
            let bound = if self.rng.chance(50) {
                self.rng.below(5).to_string()
            } else {
                self.expr(2)
            };
            self.open_block(format!("@FOR {var} @IN 0..{bound} {{"), "}");
            self.declare(&var, false);
            self.body(
                depth + 1,
                Ctx {
                    in_loop: true,
                    ..ctx
                },
            );
            self.close_block("}");
        } else if roll < 80 {
            self.open_block("{".to_string(), "}");
            self.body(depth + 1, ctx);
            self.close_block("}");
        } else if roll < 90 {
            // лямбда: видит окружение и свои параметры; сама себе не видна
            let name = self.fresh("f");
            let count = self.rng.below(3);
            let params = self.params(count);
            self.open_block(format!("@LET {name} = @FN({}) {{", params.join(", ")), "};");
            for p in &params {
                self.declare(p, false);
            }
            let saved = self.fn_base;
            self.fn_base = self.scopes.len() - 1;
            self.body(
                depth + 1,
                Ctx {
                    in_loop: false,
                    top: false,
                },
            );
            let e = self.expr(0);
            self.emit(format!("@RETURN {e};"));
            self.fn_base = saved;
            self.close_block("};");
            self.declare(&name, false);
        } else if roll < 96 && ctx.top {
            // именованная функция: глобальные, объявленные РАНЬШЕ, и параметры
            let name = self.fresh("fn");
            let count = self.rng.below(3);
            let params = self.params(count);
            self.open_block(format!("@FN {name}({}) {{", params.join(", ")), "}");
            for p in &params {
                self.declare(p, false);
            }
            let saved = self.fn_base;
            self.fn_base = self.scopes.len() - 1;
            self.body(
                depth + 1,
                Ctx {
                    in_loop: false,
                    top: false,
                },
            );
            let e = self.expr(0);
            self.emit(format!("@RETURN {e};"));
            self.fn_base = saved;
            self.close_block("}");
            self.functions.push((name, params.len()));
        } else if roll < 98 && ctx.in_loop {
            let word = if self.rng.chance(50) {
                "@BREAK"
            } else {
                "@CONTINUE"
            };
            self.emit(format!("{word};"));
        } else {
            let e = self.expr(0);
            self.emit(format!("@PRINT({e});"));
        }
    }
}

fn generate(seed: u64) -> Generated {
    let mut gen = Gen::new(seed);
    let top = Ctx {
        in_loop: false,
        top: true,
    };
    for _ in 0..(5 + gen.rng.below(10)) {
        gen.stmt(0, top);
    }
    gen.out
}

/// Закрывающие скобки, дописывающие обрезанную по строке программу.
fn closers(open: &[&'static str]) -> String {
    open.iter().rev().map(|c| format!("{c}\n")).collect()
}

/// Обрезанная по строкам программа, в конце которой набирают `PRINT(имя`:
/// сканер видит текст с незакрытыми блоками и скобкой, компилятор — тот же
/// текст, дописанный до корректного.
fn typed_cuts(label: &str, generated: &Generated, stats: &mut Stats, failures: &mut Vec<String>) {
    let mut names: Vec<String> = generated.names.clone();
    names.sort();
    names.dedup();
    let mut names = sample(&names, 6);
    names.push("zzz_undefined".to_string());
    let cuts: Vec<usize> = (0..generated.lines.len()).collect();
    for cut in sample(&cuts, 30) {
        let prefix = sga(&generated.lines[..=cut].join("\n"));
        for name in &names {
            let typed = format!("{prefix}\n{}({name}", encode_word("PRINT"));
            let finished = format!(
                "{prefix}\n{}({name});\n{}",
                encode_word("PRINT"),
                closers(&generated.open_after[cut])
            );
            let Some(expected) = compiler_says_visible(&finished, name) else {
                stats.skipped += 1;
                continue;
            };
            let cursor = (
                typed.split('\n').count(),
                encode_word("PRINT").chars().count() + 2 + name.chars().count(),
            );
            let actual = scanner_says_visible(&typed, cursor, name);
            stats.checked += 1;
            if expected {
                stats.visible += 1;
            } else {
                stats.hidden += 1;
            }
            if actual != expected {
                failures.push(format!(
                    "{label}: набор после строки {} — имя `{name}`: компилятор: {}, подсказки: {}\n{}",
                    cut + 1,
                    if expected { "видно" } else { "не видно" },
                    if actual { "видно" } else { "не видно" },
                    typed
                ));
            }
        }
    }
}

fn random_programs(seeds: std::ops::Range<u64>, minimum_valid: usize) {
    let mut stats = Stats::default();
    let mut failures = Vec::new();
    let mut invalid = 0;
    for seed in seeds {
        let generated = generate(seed);
        let src = sga(&generated.template());
        if !diagnostics::analyze_source(&src).is_empty() {
            invalid += 1;
            continue;
        }
        let label = format!("зерно {seed}");
        differential(&label, &src, &mut stats, &mut failures);
        typed_cuts(&label, &generated, &mut stats, &mut failures);
    }
    eprintln!("random: {stats:?}, отброшено программ: {invalid}");
    assert_no_failures(&failures);
    assert!(
        stats.programs >= minimum_valid,
        "генератор даёт слишком мало корректных программ: {} (отброшено {invalid})",
        stats.programs
    );
    assert!(stats.visible > 100 && stats.hidden > 100, "{stats:?}");
}

#[test]
fn visibility_matches_the_compiler_on_random_programs_a() {
    random_programs(1..41, 15);
}

#[test]
fn visibility_matches_the_compiler_on_random_programs_b() {
    random_programs(1_000..1_040, 15);
}

// ── 2. словарь: типы результатов и арность builtin ─────────────────────────

/// Один пример вызова builtin: программа `setup; type_of(call);`.
struct Sample {
    setup: String,
    call: String,
    stdin: Option<&'static str>,
    /// Программе нужен каталог с файлами (`read_file`, `write_file`).
    files: bool,
}

fn plain(call: &str) -> Sample {
    Sample {
        setup: String::new(),
        call: call.to_string(),
        stdin: None,
        files: false,
    }
}

fn after(setup: &str, call: &str) -> Sample {
    Sample {
        setup: setup.to_string(),
        ..plain(call)
    }
}

fn with_stdin(call: &str, stdin: &'static str) -> Sample {
    Sample {
        stdin: Some(stdin),
        ..plain(call)
    }
}

fn in_directory(call: &str) -> Sample {
    Sample {
        files: true,
        ..plain(call)
    }
}

/// Примеры вызовов каждого builtin: вместе они показывают КАЖДЫЙ из
/// заявленных в словаре типов результата. Новый builtin без записи здесь
/// ломает тест — словарь не может разойтись с компилятором молча.
fn samples(name: &str) -> Vec<Sample> {
    match name {
        "len" => vec![plain("len([1, 2])"), plain("len(\"ab\")")],
        "push" => vec![after("@VAR a = [1];", "push(a, 2)")],
        "keys" => vec![after("@STRUCT P { x }", "keys(P { x: 1 })")],
        "range" => vec![plain("range(3)"), plain("range(1, 3)")],
        "to_string" => vec![plain("to_string(1)")],
        "to_int" => vec![plain("to_int(\"5\")"), plain("to_int(2.7)")],
        "to_float" => vec![plain("to_float(1)"), plain("to_float(\"2.5\")")],
        "type_of" => vec![plain("type_of(1)")],
        "sqrt" => vec![plain("sqrt(4)"), plain("sqrt(2.25)")],
        "floor" => vec![plain("floor(2.5)")],
        "ceil" => vec![plain("ceil(2.5)")],
        "abs" => vec![plain("abs(-1)"), plain("abs(-1.5)")],
        "min" | "max" => vec![
            plain(&format!("{name}(1, 2)")),
            plain(&format!("{name}(1, 2.5)")),
        ],
        "pow" => vec![plain("pow(2, 3)"), plain("pow(2.0, 3)")],
        "str_split" => vec![plain("str_split(\"a,b\", \",\")")],
        "str_contains" => vec![plain("str_contains(\"abc\", \"b\")")],
        "str_trim" => vec![plain("str_trim(\" a \")")],
        "str_starts_with" => vec![plain("str_starts_with(\"abc\", \"a\")")],
        "str_ends_with" => vec![plain("str_ends_with(\"abc\", \"c\")")],
        "str_replace" => vec![plain("str_replace(\"abc\", \"b\", \"x\")")],
        "str_upper" => vec![plain("str_upper(\"abc\")")],
        "str_lower" => vec![plain("str_lower(\"ABC\")")],
        "json_stringify" => vec![plain("json_stringify([1, 2])")],
        "json_parse" => vec![
            plain("json_parse(\"1\")"),
            plain("json_parse(\"1.5\")"),
            plain("json_parse(\"true\")"),
            plain("json_parse(\"null\")"),
            plain("json_parse(\"[1]\")"),
            plain("json_parse(\"\\\"s\\\"\")"),
        ],
        "read_file" => vec![in_directory("read_file(\"data.txt\")")],
        "write_file" => vec![in_directory("write_file(\"out.txt\", \"x\")")],
        "read_line" => vec![
            with_stdin("read_line()", "abc\n"),
            with_stdin("read_line()", ""),
        ],
        "read_stdin" => vec![with_stdin("read_stdin()", "abc")],
        "args" => vec![plain("args()")],
        "time_now_ms" => vec![plain("time_now_ms()")],
        "time_monotonic_ms" => vec![plain("time_monotonic_ms()")],
        other => panic!("у builtin `{other}` нет примеров вызова в тесте словаря"),
    }
}

fn scratch_directory(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sga_t018_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("временный каталог");
    dir
}

/// Выполняет пример и возвращает то, что напечатал бы `type_of(вызов)`.
fn observed_type(sample: &Sample) -> String {
    let src = sga(&format!("{}\ntype_of({});\n", sample.setup, sample.call));
    let options = RunOptions::new()
        .with_stdin_bytes(sample.stdin.unwrap_or(""))
        .with_allow_write(sample.files);
    let result = if sample.files {
        let dir = scratch_directory("sample");
        std::fs::write(dir.join("data.txt"), "hello").expect("data.txt");
        std::fs::write(dir.join("main.sga"), &src).expect("main.sga");
        let result = sga::run_source_file_with_options(&dir.join("main.sga"), options);
        let _ = std::fs::remove_dir_all(&dir);
        result
    } else {
        sga::run_source_with_options(&src, options)
    };
    match result {
        Ok(Value::Str(name)) => name,
        other => panic!("пример `{}` не выполнился: {other:?}", sample.call),
    }
}

#[test]
fn builtin_return_types_in_the_vocabulary_match_real_execution() {
    // типы значений, которые builtin `json_parse` может вернуть
    const JSON_TYPES: [&str; 6] = ["int", "float", "bool", "string", "nil", "array"];
    for b in vocabulary::BUILTINS {
        let observed: BTreeSet<String> = samples(b.name).iter().map(observed_type).collect();
        let declared: BTreeSet<&str> = b.return_alternatives().collect();
        if declared.contains("any") {
            assert_eq!(
                declared.len(),
                1,
                "{}: `any` не сочетается с другими",
                b.name
            );
            assert!(
                observed.iter().all(|t| JSON_TYPES.contains(&t.as_str())),
                "{}: {observed:?}",
                b.name
            );
            assert_eq!(
                observed.len(),
                JSON_TYPES.len(),
                "{}: примеры должны показать все типы: {observed:?}",
                b.name
            );
        } else {
            let observed: BTreeSet<&str> = observed.iter().map(String::as_str).collect();
            assert_eq!(
                observed, declared,
                "{}: словарь заявляет {declared:?}, исполнение показало {observed:?}",
                b.name
            );
        }
    }
}

#[test]
fn builtin_arity_in_the_vocabulary_matches_the_compiler_checks() {
    for b in vocabulary::BUILTINS {
        let (min, max) = b.arity();
        assert_eq!(
            semantic::builtin_arity(b.name),
            Some((min, max)),
            "{}",
            b.name
        );
        assert_eq!(
            runtime::builtin_arity(b.name),
            Some((min, max)),
            "{}",
            b.name
        );
        // компилятор жалуется на число аргументов ровно за пределами `min..=max`
        let complains = |count: usize| {
            let args = vec!["x"; count].join(", ");
            let src = sga(&format!("@LET x = 1;\n{}({args});\n", b.name));
            diagnostics::analyze_source(&src)
                .iter()
                .any(|d| is_code(d, "semantic") && d.message.contains("ожидает"))
        };
        for count in min..=max {
            assert!(
                !complains(count),
                "{}: {count} аргументов допустимы",
                b.name
            );
        }
        assert!(
            complains(max + 1),
            "{}: {} аргументов лишние",
            b.name,
            max + 1
        );
        if min > 0 {
            assert!(
                complains(min - 1),
                "{}: {} аргументов мало",
                b.name,
                min - 1
            );
        }
    }
}

// ── 3. устойчивость: испорченные программы ─────────────────────────────────

/// Портит текст: удаление, дублирование, вставка мусора, обрезка, замена.
fn damage(text: &str, rng: &mut Rng) -> String {
    const JUNK: [&str; 16] = [
        "(", ")", "{", "}", "[", "]", "\"", "//", ".", "..", ",", ":", "->", "@", "😀", "\r\n",
    ];
    let mut chars: Vec<char> = text.chars().collect();
    for _ in 0..=rng.below(3) {
        if chars.is_empty() {
            break;
        }
        let at = rng.below(chars.len());
        let len = 1 + rng.below(40.min(chars.len() - at));
        match rng.below(5) {
            0 => {
                chars.drain(at..at + len);
            }
            1 => {
                let copy: Vec<char> = chars[at..at + len].to_vec();
                for (k, c) in copy.into_iter().enumerate() {
                    chars.insert(at + len + k, c);
                }
            }
            2 => {
                let junk: Vec<char> = JUNK[rng.below(JUNK.len())].chars().collect();
                for (k, c) in junk.into_iter().enumerate() {
                    chars.insert(at + k, c);
                }
            }
            3 => chars.truncate(at),
            _ => {
                let junk: Vec<char> = JUNK[rng.below(JUNK.len())].chars().collect();
                chars.splice(at..at + 1, junk);
            }
        }
    }
    chars.into_iter().collect()
}

/// Проверяет инварианты ответов `hover` и `complete` в позиции `pos`.
fn check_answers(doc: &Document, text: &str, pos: Pos) {
    if let Some(h) = assist::hover(doc, pos) {
        assert!(h.start < h.end && h.start.0 == h.end.0, "{h:?}");
        assert!(
            h.start <= pos && pos <= h.end,
            "слово не у курсора: {h:?} {pos:?}"
        );
        assert!(!h.markdown.is_empty());
    }
    let c = assist::complete(doc, text, pos);
    assert_eq!(c.end, pos);
    assert!(c.start <= c.end && c.start.0 == c.end.0, "{c:?} {pos:?}");
    assert!(
        c.items.windows(2).all(|w| w[0].sort_text < w[1].sort_text),
        "порядок не строгий: {pos:?}"
    );
    for item in &c.items {
        assert!(!item.label.is_empty() && !item.new_text.is_empty());
        if item.kind == ItemKind::Keyword {
            assert!(item.new_text.chars().all(is_sga_letter), "{item:?}");
        }
    }
}

/// Позиции для проверки: случайные, концы и начала строк, за пределами текста.
fn positions(text: &str, rng: &mut Rng, random: usize) -> Vec<Pos> {
    let lines: Vec<usize> = text.split('\n').map(|l| l.chars().count()).collect();
    let mut found = vec![(1, 1), (lines.len() + 3, 40), (1, lines[0] + 5)];
    for (i, width) in lines.iter().enumerate().take(60) {
        found.push((i + 1, 1));
        found.push((i + 1, width + 1));
    }
    for _ in 0..random {
        let line = rng.below(lines.len());
        found.push((line + 1, 1 + rng.below(lines[line] + 2)));
    }
    found
}

#[test]
fn hover_and_completion_survive_damaged_programs() {
    let mut corpus: Vec<String> = example_sources().into_iter().map(|(_, t)| t).collect();
    corpus.extend(HANDWRITTEN.iter().map(|(_, t)| sga(t)));
    corpus.push("@ ".repeat(40) + "len(x)\n@LET a = 1;\nа");
    corpus.push("😀\r\n\"😀\" len(\"\r\n".to_string());
    let mut rng = Rng::new(2026);
    let mut checked = 0;
    for original in &corpus {
        let doc = Document::new(original);
        for pos in positions(original, &mut rng, 30) {
            check_answers(&doc, original, pos);
        }
        for _ in 0..8 {
            let damaged = damage(original, &mut rng);
            let doc = Document::new(&damaged);
            for pos in positions(&damaged, &mut rng, 30) {
                check_answers(&doc, &damaged, pos);
                checked += 1;
            }
        }
    }
    assert!(checked > 10_000, "{checked}");
}

// ── 3б. устойчивость: документы предельного размера ────────────────────────

fn repeated_until_limit(unit: &str) -> String {
    filled("", unit, "")
}

/// `префикс + единица × k + суффикс` — самый длинный текст в пределах
/// `MAX_DOCUMENT_BYTES`.
fn filled(prefix: &str, unit: &str, suffix: &str) -> String {
    let room = MAX_DOCUMENT_BYTES - prefix.len() - suffix.len();
    let mut text = String::from(prefix);
    text.push_str(&unit.repeat(room / unit.len()));
    text.push_str(suffix);
    text
}

fn numbered_until_limit(template: impl Fn(usize) -> String) -> String {
    let mut text = String::new();
    let mut i = 0;
    loop {
        let piece = template(i);
        if text.len() + piece.len() > MAX_DOCUMENT_BYTES {
            return text;
        }
        text.push_str(&piece);
        i += 1;
    }
}

fn big_documents() -> Vec<(&'static str, String)> {
    vec![
        (
            "много переменных",
            numbered_until_limit(|i| sga(&format!("@LET v{i} = {i};\n"))),
        ),
        (
            "много функций",
            numbered_until_limit(|i| sga(&format!("@FN f{i}(a, b) {{ @RETURN a; }}\n"))),
        ),
        (
            "много структур",
            numbered_until_limit(|i| sga(&format!("@STRUCT S{i} {{ x, y }}\n"))),
        ),
        ("открывающие скобки", repeated_until_limit("(")),
        ("открывающие фигурные", repeated_until_limit("{")),
        ("открывающие квадратные", repeated_until_limit("[")),
        ("цепочка операторов", filled("", "1+", "1")),
        ("мусор (текстовый режим)", repeated_until_limit("@")),
        ("незакрытая строка", filled("\"", "a", "")),
        ("один идентификатор", repeated_until_limit("a")),
        ("пустые строки", repeated_until_limit("\n")),
        (
            "слова IF подряд",
            repeated_until_limit(&format!("{} ", encode_word("IF"))),
        ),
        (
            "FOR подряд",
            repeated_until_limit(&format!("{} i ", encode_word("FOR"))),
        ),
        (
            "объявления без точки с запятой",
            repeated_until_limit(&sga("@LET a = 1 ")),
        ),
        (
            "один огромный MATCH",
            filled(&sga("@MATCH x { "), "n -> 1, ", "_ -> 0 }"),
        ),
        (
            "литерал struct с тысячами полей",
            filled(&sga("@LET p = P { "), "a: 1, ", "z: 1 };"),
        ),
        (
            "заголовок функции с тысячами параметров",
            filled(&sga("@FN f("), "a, ", "z) { }\nf(1);"),
        ),
        (
            "вложенные блоки с переменными",
            repeated_until_limit(&sga("{ @LET a = 1; ")),
        ),
    ]
}

fn json_request(id: i64, method: &str, uri: &str, line: usize, character: usize) -> String {
    Json::object([
        ("jsonrpc", Json::string("2.0")),
        ("id", Json::Int(id)),
        ("method", Json::string(method)),
        (
            "params",
            Json::object([
                ("textDocument", Json::object([("uri", Json::string(uri))])),
                (
                    "position",
                    Json::object([
                        ("line", Json::Int(line as i64)),
                        ("character", Json::Int(character as i64)),
                    ]),
                ),
            ]),
        ),
    ])
    .to_string()
}

fn ask(server: &mut Server, id: i64, method: &str, line: usize, character: usize) -> Json {
    let body = json_request(id, method, "file:///big.sga", line, character);
    let out = server.handle_message(body.as_bytes());
    assert_eq!(out.len(), 1);
    assert!(
        out[0].get("error").is_none(),
        "{}",
        out[0].to_string().len()
    );
    out[0].get("result").cloned().expect("result")
}

/// Бюджет одного запроса на документе в 1 МиБ в отладочной сборке. Измерено
/// (T018): с исправленным кодом самый медленный запрос — доли секунды
/// (текстовый режим: 17 проходов лексера), а с квадратичной проверкой
/// дубликатов имён в `collect_globals` и `Document::context_at` те же
/// запросы занимали 3–7 с на документах из десятков тысяч объявлений. Бюджет
/// отделяет одно от другого с запасом в несколько раз на шум машины.
const REQUEST_BUDGET: Duration = Duration::from_secs(3);

/// Бюджет одного запроса навигации на документе в 1 МиБ в отладочной сборке
/// (AC-08 T019: не больше 5 с). Измерено: самый тяжёлый запрос — иерархический
/// `documentSymbol` на структуре с ≈ 130 тысячами полей, около секунды; до
/// таблицы строк (`position::LineTable`) такой запрос на 16 тысячах функций
/// занимал 19 с.
const NAVIGATION_BUDGET: Duration = Duration::from_secs(5);

fn document_request(id: i64, method: &str) -> String {
    Json::object([
        ("jsonrpc", Json::string("2.0")),
        ("id", Json::Int(id)),
        ("method", Json::string(method)),
        (
            "params",
            Json::object([(
                "textDocument",
                Json::object([("uri", Json::string("file:///big.sga"))]),
            )]),
        ),
    ])
    .to_string()
}

/// Число символов и наибольшая глубина дерева `DocumentSymbol` — обходом по
/// уровням, без рекурсии.
fn symbol_tree_size(list: &[Json]) -> (usize, usize) {
    let (mut total, mut depth) = (0, 0);
    let mut level: Vec<&Json> = list.iter().collect();
    while !level.is_empty() {
        depth += 1;
        total += level.len();
        level = level
            .iter()
            .filter_map(|symbol| symbol.get("children").and_then(Json::as_array))
            .flatten()
            .collect();
    }
    (total, depth)
}

#[test]
fn navigation_on_a_one_mebibyte_document_is_fast_and_does_not_recurse() {
    // те же 18 документов и тот же малый стек, что у подсказок: рекурсия по
    // размеру документа или квадратичная работа на любом из них видна сразу
    let handle = std::thread::Builder::new()
        .name("small-stack-navigation".to_string())
        .stack_size(512 * 1024)
        .spawn(|| {
            let mut timings = Vec::new();
            for (label, text) in big_documents() {
                assert!(text.len() <= MAX_DOCUMENT_BYTES, "{label}");
                let lines = text.split('\n').count();
                let last_line = text.split('\n').next_back().unwrap_or("");
                let end = (lines - 1, last_line.encode_utf16().count());
                let mut sizes = Vec::new();
                for hierarchical in [true, false] {
                    let mut server = Server::with_analyzer(Box::new(|_| Vec::new()));
                    let init = format!(
                        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"capabilities":{{"textDocument":{{"documentSymbol":{{"hierarchicalDocumentSymbolSupport":{hierarchical}}}}}}}}}}}"#
                    );
                    server.handle_message(init.as_bytes());
                    let open = Json::object([
                        ("jsonrpc", Json::string("2.0")),
                        ("method", Json::string("textDocument/didOpen")),
                        (
                            "params",
                            Json::object([(
                                "textDocument",
                                Json::object([
                                    ("uri", Json::string("file:///big.sga")),
                                    ("version", Json::Int(1)),
                                    ("text", Json::string(text.as_str())),
                                ]),
                            )]),
                        ),
                    ])
                    .to_string();
                    server.handle_message(open.as_bytes());

                    let started = Instant::now();
                    let out = server.handle_message(
                        document_request(2, "textDocument/documentSymbol").as_bytes(),
                    );
                    let took = started.elapsed();
                    assert_eq!(out.len(), 1, "{label}");
                    assert!(out[0].get("error").is_none(), "{label}");
                    let answer = out[0].get("result").expect("result");
                    let list = answer
                        .as_array()
                        .unwrap_or_else(|| panic!("{label}: documentSymbol не массив"));
                    let size = if hierarchical {
                        let (total, depth) = symbol_tree_size(list);
                        assert!(
                            depth <= MAX_SYMBOL_DEPTH,
                            "{label}: глубина ответа {depth} больше {MAX_SYMBOL_DEPTH}"
                        );
                        total
                    } else {
                        list.len()
                    };
                    assert!(
                        took < NAVIGATION_BUDGET,
                        "{label}: documentSymbol занял {took:?} (бюджет {NAVIGATION_BUDGET:?})"
                    );
                    timings.push((label, "documentSymbol", took));
                    sizes.push(size);
                    drop(out);

                    if hierarchical {
                        for (what, (line, character)) in [
                            ("definition в начале", (0, 0)),
                            ("definition в середине", (lines / 2, 0)),
                            ("definition в конце", end),
                        ] {
                            let started = Instant::now();
                            let result = ask(&mut server, 3, "textDocument/definition", line, character);
                            let took = started.elapsed();
                            assert!(
                                took < NAVIGATION_BUDGET,
                                "{label}: {what} занял {took:?} (бюджет {NAVIGATION_BUDGET:?})"
                            );
                            assert!(
                                result.is_null() || result.get("range").is_some(),
                                "{label}: {what}: ответ — не null и не Location"
                            );
                            timings.push((label, what, took));
                        }
                    }
                }
                assert_eq!(
                    sizes[0], sizes[1],
                    "{label}: иерархия и плоский список не совпали по числу символов"
                );
                // там, где число объявлений известно из построения документа
                let pieces = lines - 1;
                match label {
                    "много переменных" | "много функций" => {
                        assert_eq!(sizes[0], pieces, "{label}")
                    }
                    "много структур" => assert_eq!(sizes[0], 3 * pieces, "{label}"),
                    "мусор (текстовый режим)" | "пустые строки" | "один идентификатор" => {
                        assert_eq!(sizes[0], 0, "{label}")
                    }
                    _ => {}
                }
            }
            timings
        })
        .expect("поток");
    let timings = handle.join().expect("запросы не должны ронять поток");
    let mut slowest = timings;
    slowest.sort_by_key(|t| std::cmp::Reverse(t.2));
    eprintln!("самые медленные запросы навигации: {:?}", &slowest[..3]);
}

#[test]
fn requests_on_a_one_mebibyte_document_are_fast_and_do_not_recurse() {
    // стек в 512 КиБ: рекурсия по размеру документа (скобки, операторы,
    // вложенные блоки) переполнила бы его на любом из документов
    let handle = std::thread::Builder::new()
        .name("small-stack".to_string())
        .stack_size(512 * 1024)
        .spawn(|| {
            let mut timings = Vec::new();
            for (label, text) in big_documents() {
                assert!(text.len() <= MAX_DOCUMENT_BYTES, "{label}");
                // без анализа диагностик: проверяется только путь подсказок
                let mut server = Server::with_analyzer(Box::new(|_| Vec::new()));
                let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
                server.handle_message(init.as_bytes());
                let open = Json::object([
                    ("jsonrpc", Json::string("2.0")),
                    ("method", Json::string("textDocument/didOpen")),
                    (
                        "params",
                        Json::object([(
                            "textDocument",
                            Json::object([
                                ("uri", Json::string("file:///big.sga")),
                                ("version", Json::Int(1)),
                                ("text", Json::string(text.as_str())),
                            ]),
                        )]),
                    ),
                ])
                .to_string();
                server.handle_message(open.as_bytes());

                let lines = text.split('\n').count();
                let last_line = text.split('\n').next_back().unwrap_or("");
                let end = (lines - 1, last_line.encode_utf16().count());
                let middle = (lines / 2, 0);
                for (what, method, (line, character)) in [
                    ("hover в середине", "textDocument/hover", middle),
                    ("completion в конце", "textDocument/completion", end),
                ] {
                    let started = Instant::now();
                    let result = ask(&mut server, 2, method, line, character);
                    let took = started.elapsed();
                    assert!(
                        took < REQUEST_BUDGET,
                        "{label}: {what} занял {took:?} (бюджет {REQUEST_BUDGET:?})"
                    );
                    timings.push((label, what, took));
                    drop(result);
                }
            }
            timings
        })
        .expect("поток");
    let timings = handle.join().expect("запросы не должны ронять поток");
    let mut slowest = timings;
    slowest.sort_by_key(|t| std::cmp::Reverse(t.2));
    eprintln!("самые медленные запросы: {:?}", &slowest[..3]);
}
