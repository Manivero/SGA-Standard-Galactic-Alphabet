//! Позиции узлов AST (T017, M007): каждый `Expr`, `Stmt`, `MatchArm` и `Param`
//! знает диапазон токенов, из которых построен, а диапазон переводится в
//! координаты исходного текста таблицей `Lexer::tokenize_with_spans`.
//!
//! Проверки — по ТЕКСТУ под диапазоном: тест берёт узел по пути в дереве и
//! сравнивает кусок исходника, который покрывает его `Span`, с ожидаемым.
//! Ключевые слова в шаблонах пишутся `@LET`, `@IF`, … — они подставляются
//! словами из SGA-букв (идентификаторы и числа — обычный ASCII, как в языке).
//!
//! Помимо точечных проверок правил диапазонов (раздел «Правила диапазонов» в
//! Feature Contract T017) здесь два сквозных инварианта на любом дереве:
//! диапазон ребёнка лежит внутри диапазона родителя, а соседи не пересекаются и
//! идут в порядке исходного текста. Они проверяются на всех 17 примерах из
//! `examples/` и на программе, в которой есть каждый вариант `ExprKind` и
//! `StmtKind` — список детей составлен БЕЗ `_`, поэтому новый вариант узла не
//! скомпилируется, пока его не учтут здесь.

use sga::ast::{Expr, ExprKind, MatchArm, Param, Program, Span, Stmt, StmtKind};
use sga::lexer::token::TokenSpan;
use sga::lexer::Lexer;
use sga::parser::Parser;
use sga::sga_alphabet::encode_word;

// ── помощники ──────────────────────────────────────────────────────────────

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

/// Текст исходника под границами `at` (строки и колонки с единицы, колонки — в
/// символах, конец исключающий).
fn slice(src: &str, at: TokenSpan) -> String {
    let lines: Vec<Vec<char>> = src.split('\n').map(|l| l.chars().collect()).collect();
    let (first_line, first_col) = (at.line - 1, at.col - 1);
    let (last_line, last_col) = (at.end_line - 1, at.end_col - 1);
    let mut out = String::new();
    if first_line == last_line {
        out.extend(&lines[first_line][first_col..last_col]);
        return out;
    }
    out.extend(&lines[first_line][first_col..]);
    for line in &lines[first_line + 1..last_line] {
        out.push('\n');
        out.extend(line);
    }
    out.push('\n');
    out.extend(&lines[last_line][..last_col]);
    out
}

/// Разобранная программа вместе с исходным текстом и таблицей границ токенов.
struct Parsed {
    src: String,
    program: Program,
    table: Vec<TokenSpan>,
}

impl Parsed {
    fn new(template: &str) -> Parsed {
        let src = sga(template);
        let (tokens, table) = Lexer::new(&src)
            .tokenize_with_spans()
            .unwrap_or_else(|e| panic!("лексинг {template:?}: {e}"));
        let program = Parser::new(tokens)
            .parse_program()
            .unwrap_or_else(|e| panic!("парсинг {template:?}: {e}"));
        Parsed {
            src,
            program,
            table,
        }
    }

    /// `@LET v = <expr>;` — разобранное выражение удобно доставать через `value()`.
    fn of_expr(expr_template: &str) -> Parsed {
        Parsed::new(&format!("@LET v = {expr_template};"))
    }

    /// Текст под диапазоном узла.
    fn text(&self, span: Span) -> String {
        let at = span
            .resolve(&self.table)
            .unwrap_or_else(|| panic!("диапазон {span:?} не переводится в позиции"));
        slice(&self.src, at)
    }

    /// Значение первого оператора `LET v = …;`.
    fn value(&self) -> &Expr {
        match &self.program[0].kind {
            StmtKind::VarDecl { value, .. } => value,
            other => panic!("ожидался VarDecl, получено {other:?}"),
        }
    }

    fn text_of_value(&self) -> String {
        self.text(self.value().span)
    }
}

fn args_of(expr: &Expr) -> &[Expr] {
    match &expr.kind {
        ExprKind::Call(_, args)
        | ExprKind::CallExpr(_, args)
        | ExprKind::MethodCall(_, _, args)
        | ExprKind::Array(args) => args,
        other => panic!("у {other:?} нет списка аргументов"),
    }
}

/// Дети узла в порядке исходного текста. Варианты перечислены без `_`.
enum Node<'a> {
    Stmt(&'a Stmt),
    Expr(&'a Expr),
    Arm(&'a MatchArm),
    Param(&'a Param),
}

fn span_of(node: &Node) -> Span {
    match node {
        Node::Stmt(s) => s.span,
        Node::Expr(e) => e.span,
        Node::Arm(a) => a.span,
        Node::Param(p) => p.span,
    }
}

fn stmts<'a>(list: &'a [Stmt]) -> impl Iterator<Item = Node<'a>> {
    list.iter().map(Node::Stmt)
}

fn exprs<'a>(list: &'a [Expr]) -> impl Iterator<Item = Node<'a>> {
    list.iter().map(Node::Expr)
}

fn children<'a>(node: &Node<'a>) -> Vec<Node<'a>> {
    match node {
        Node::Param(_) => vec![],
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
            StmtKind::FnDecl { params, body, .. } => {
                let mut kids: Vec<Node> = params.iter().map(Node::Param).collect();
                kids.extend(stmts(body));
                kids
            }
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

/// Как должны выглядеть диапазоны всего дерева.
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    /// Каждый узел имеет настоящий диапазон внутри таблицы токенов.
    Real,
    /// Каждый узел — `Span::NONE` (`Parser::detached`).
    None,
}

/// Проверяет инварианты поддерева; возвращает число узлов.
fn check_tree(node: &Node, table_len: usize, expect: Expect, path: &str) -> usize {
    let span = span_of(node);
    if expect == Expect::None {
        assert!(
            span.is_none(),
            "{path}: ожидался Span::NONE, получено {span:?}"
        );
        let kids = children(node);
        return 1 + kids
            .iter()
            .enumerate()
            .map(|(i, kid)| check_tree(kid, table_len, expect, &format!("{path}/{i}")))
            .sum::<usize>();
    }
    assert!(!span.is_none(), "{path}: у узла нет позиции");
    assert!(
        span.first <= span.last,
        "{path}: перевёрнутый диапазон {span:?}"
    );
    // `Eof` — последний токен таблицы; ни один узел его не содержит
    assert!(
        (span.last as usize) < table_len - 1,
        "{path}: диапазон {span:?} заходит на Eof или за таблицу ({table_len} токенов)"
    );
    let mut count = 1;
    let mut previous_last: Option<u32> = None;
    for (i, kid) in children(node).iter().enumerate() {
        let kid_span = span_of(kid);
        assert!(
            !kid_span.is_none(),
            "{path}/{i}: у ребёнка нет позиции (родитель {span:?})"
        );
        assert!(
            span.first <= kid_span.first && kid_span.last <= span.last,
            "{path}/{i}: ребёнок {kid_span:?} вне родителя {span:?}"
        );
        if let Some(previous) = previous_last {
            assert!(
                previous < kid_span.first,
                "{path}/{i}: соседи пересекаются или идут не по порядку ({previous} >= {})",
                kid_span.first
            );
        }
        previous_last = Some(kid_span.last);
        count += check_tree(kid, table_len, expect, &format!("{path}/{i}"));
    }
    count
}

fn check_program(parsed: &Parsed, expect: Expect) -> usize {
    let mut count = 0;
    let mut previous_last: Option<u32> = None;
    for (i, stmt) in parsed.program.iter().enumerate() {
        let node = Node::Stmt(stmt);
        count += check_tree(&node, parsed.table.len(), expect, &format!("#{i}"));
        if expect == Expect::Real {
            if let Some(previous) = previous_last {
                assert!(previous < stmt.span.first, "#{i}: операторы пересекаются");
            }
            previous_last = Some(stmt.span.last);
        }
    }
    count
}

// ── Span и его перевод в координаты ───────────────────────────────────────

#[test]
fn spans_are_token_indexes_inclusive() {
    // токены: LET(0) x(1) =(2) 1(3) ;(4) Eof(5)
    let parsed = Parsed::new("@LET x = 1;");
    let stmt = &parsed.program[0];
    assert_eq!(stmt.span, Span::new(0, 4));
    let StmtKind::VarDecl { value, .. } = &stmt.kind else {
        panic!()
    };
    assert_eq!(value.span, Span::new(3, 3));
    assert_eq!(parsed.table.len(), 6);
}

#[test]
fn resolve_gives_line_and_column_of_the_first_and_end_of_the_last_token() {
    let parsed = Parsed::new("@LET a = 1;\n@PRINT(a);");
    let first = parsed.program[0].span.resolve(&parsed.table).unwrap();
    // `LET a = 1;` — 10 символов: конец (исключающий) — колонка 11
    assert_eq!(
        (first.line, first.col, first.end_line, first.end_col),
        (1, 1, 1, 11)
    );
    let second = parsed.program[1].span.resolve(&parsed.table).unwrap();
    // `PRINT(a);` — 9 символов
    assert_eq!(
        (second.line, second.col, second.end_line, second.end_col),
        (2, 1, 2, 10)
    );
}

// ── выражения: по одному варианту ─────────────────────────────────────────

#[test]
fn literals_and_identifiers_cover_exactly_their_token() {
    for literal in [
        "42", "3.5", "\"hi\"", "@TRUE", "@FALSE", "@NIL", "abc", "x_1",
    ] {
        let parsed = Parsed::of_expr(literal);
        assert_eq!(parsed.text_of_value(), sga(literal), "{literal}");
    }
}

#[test]
fn a_string_literal_with_a_newline_covers_both_lines() {
    let parsed = Parsed::of_expr("\"ab\ncd\"");
    assert_eq!(parsed.text_of_value(), "\"ab\ncd\"");
    let at = parsed.value().span.resolve(&parsed.table).unwrap();
    assert_eq!((at.line, at.end_line), (1, 2));
}

#[test]
fn array_covers_the_brackets() {
    for array in ["[1, 2, 3]", "[]", "[[1], [2, 3]]"] {
        assert_eq!(Parsed::of_expr(array).text_of_value(), array);
    }
    let parsed = Parsed::of_expr("[[1], [2, 3]]");
    assert_eq!(parsed.text(args_of(parsed.value())[1].span), "[2, 3]");
}

#[test]
fn multi_line_array_covers_from_bracket_to_bracket() {
    let parsed = Parsed::new("@LET v = [\n    1,\n    2\n];");
    assert_eq!(parsed.text_of_value(), "[\n    1,\n    2\n]");
    assert_eq!(
        parsed.text(parsed.program[0].span),
        "@LET v = [\n    1,\n    2\n];".replace("@LET", &sga("@LET"))
    );
}

#[test]
fn index_covers_the_operand_and_the_closing_bracket() {
    let parsed = Parsed::of_expr("a[0][1]");
    assert_eq!(parsed.text_of_value(), "a[0][1]");
    let ExprKind::Index(inner, index) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(inner.span), "a[0]");
    assert_eq!(parsed.text(index.span), "1");
    let ExprKind::Index(operand, _) = &inner.kind else {
        panic!()
    };
    assert_eq!(parsed.text(operand.span), "a");
}

#[test]
fn unary_covers_the_operator_and_the_operand() {
    for (source, text) in [
        ("-x", "-x"),
        ("- x", "- x"),
        ("@NOT @TRUE", "@NOT @TRUE"),
        ("-(x)", "-(x)"),
        ("- -x", "- -x"),
    ] {
        assert_eq!(
            Parsed::of_expr(source).text_of_value(),
            sga(text),
            "{source}"
        );
    }
    let parsed = Parsed::of_expr("- -x");
    let ExprKind::Unary(_, inner) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(inner.span), "-x");
}

#[test]
fn binary_covers_from_the_left_operand_to_the_right_operand() {
    let parsed = Parsed::of_expr("a   +   b");
    assert_eq!(parsed.text_of_value(), "a   +   b");
    let ExprKind::Binary(_, left, right) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(left.span), "a");
    assert_eq!(parsed.text(right.span), "b");
}

#[test]
fn every_binary_precedence_level_gets_its_own_range() {
    // Or(a, And(b, Eq(c, Lt(d, Add(e, Mul(f, g))))))
    let parsed = Parsed::of_expr("a @OR b @AND c == d < e + f * g");
    let mut expected = vec![
        sga("a @OR b @AND c == d < e + f * g"),
        sga("b @AND c == d < e + f * g"),
        "c == d < e + f * g".to_string(),
        "d < e + f * g".to_string(),
        "e + f * g".to_string(),
        "f * g".to_string(),
    ];
    let mut node = parsed.value();
    while let ExprKind::Binary(_, _, right) = &node.kind {
        assert_eq!(parsed.text(node.span), expected.remove(0));
        node = right;
    }
    assert!(expected.is_empty(), "не все уровни найдены: {expected:?}");
    assert_eq!(parsed.text(node.span), "g");
}

#[test]
fn comparison_and_equality_operators_cover_their_operands() {
    for (source, text) in [
        ("a != b", "a != b"),
        ("a <= b", "a <= b"),
        ("a >= b", "a >= b"),
        ("a % b", "a % b"),
        ("a / b", "a / b"),
    ] {
        assert_eq!(Parsed::of_expr(source).text_of_value(), text);
    }
}

#[test]
fn left_associative_chain_nests_to_the_left_with_growing_ranges() {
    let parsed = Parsed::of_expr("1 - 2 - 3 - 4");
    assert_eq!(parsed.text_of_value(), "1 - 2 - 3 - 4");
    let ExprKind::Binary(_, left, right) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(right.span), "4");
    assert_eq!(parsed.text(left.span), "1 - 2 - 3");
    let ExprKind::Binary(_, left, _) = &left.kind else {
        panic!()
    };
    assert_eq!(parsed.text(left.span), "1 - 2");
}

#[test]
fn call_covers_the_name_and_the_closing_parenthesis() {
    let parsed = Parsed::of_expr("f(1, g(2), 3)");
    assert_eq!(parsed.text_of_value(), "f(1, g(2), 3)");
    let args = args_of(parsed.value());
    assert_eq!(parsed.text(args[0].span), "1");
    assert_eq!(parsed.text(args[1].span), "g(2)");
    assert_eq!(parsed.text(args[2].span), "3");
    assert_eq!(Parsed::of_expr("f()").text_of_value(), "f()");
}

#[test]
fn call_of_an_expression_covers_the_callee_and_the_arguments() {
    let parsed = Parsed::of_expr("g(1)(2)(3)");
    assert_eq!(parsed.text_of_value(), "g(1)(2)(3)");
    let ExprKind::CallExpr(callee, _) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(callee.span), "g(1)(2)");
    let parsed = Parsed::of_expr("fns[0](1)");
    assert_eq!(parsed.text_of_value(), "fns[0](1)");
    let parsed = Parsed::of_expr("(@FN() { })()");
    assert_eq!(parsed.text_of_value(), sga("(@FN() { })()"));
    let ExprKind::CallExpr(callee, _) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(callee.span), sga("@FN() { }"));
}

#[test]
fn assignment_covers_from_the_target_to_the_end_of_the_value() {
    let parsed = Parsed::of_expr("a = b = 1");
    assert_eq!(parsed.text_of_value(), "a = b = 1");
    let ExprKind::Assign(_, inner) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(inner.span), "b = 1");
    assert_eq!(Parsed::of_expr("(a) = 1").text_of_value(), "(a) = 1");
    assert_eq!(
        Parsed::of_expr("a = (1 + 2)").text_of_value(),
        "a = (1 + 2)"
    );
}

#[test]
fn index_assignment_and_field_assignment_cover_target_and_value() {
    let parsed = Parsed::of_expr("a[0] = b.c = d = 2");
    assert_eq!(parsed.text_of_value(), "a[0] = b.c = d = 2");
    let ExprKind::IndexAssign(target, index, value) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(target.span), "a");
    assert_eq!(parsed.text(index.span), "0");
    assert_eq!(parsed.text(value.span), "b.c = d = 2");
    let ExprKind::FieldAssign(object, _, value) = &value.kind else {
        panic!()
    };
    assert_eq!(parsed.text(object.span), "b");
    assert_eq!(parsed.text(value.span), "d = 2");
}

#[test]
fn lambda_covers_fn_through_the_closing_brace() {
    let parsed = Parsed::of_expr("@FN(x, y) { @RETURN x; }");
    assert_eq!(parsed.text_of_value(), sga("@FN(x, y) { @RETURN x; }"));
    let with_type = Parsed::of_expr("@FN(x) -> int { @RETURN x; }");
    assert_eq!(
        with_type.text_of_value(),
        sga("@FN(x) -> int { @RETURN x; }")
    );
    let multi_line = Parsed::new("@LET f = @FN(x) {\n    @RETURN x;\n};");
    assert_eq!(
        multi_line.text_of_value(),
        sga("@FN(x) {\n    @RETURN x;\n}")
    );
}

#[test]
fn struct_literal_covers_the_type_name_through_the_closing_brace() {
    for literal in [
        "P { x: 1, y: 2 }",
        "P {}",
        "P { x: 1, }",
        "P {\n    x: 1\n}",
    ] {
        assert_eq!(Parsed::of_expr(literal).text_of_value(), literal);
    }
}

#[test]
fn field_access_and_method_call_cover_the_object_and_the_member() {
    let parsed = Parsed::of_expr("p.q.x");
    assert_eq!(parsed.text_of_value(), "p.q.x");
    let ExprKind::FieldAccess(object, _) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(object.span), "p.q");

    let parsed = Parsed::of_expr("p.m(1, 2)");
    assert_eq!(parsed.text_of_value(), "p.m(1, 2)");
    assert_eq!(parsed.text(args_of(parsed.value())[1].span), "2");
    assert_eq!(Parsed::of_expr("p.m()").text_of_value(), "p.m()");
}

#[test]
fn postfix_chains_grow_from_the_same_first_token() {
    // FieldAccess(Index(MethodCall(Ident a, b, [1]), 2), c)
    let parsed = Parsed::of_expr("a.b(1)[2].c");
    let mut texts = vec!["a.b(1)[2].c", "a.b(1)[2]", "a.b(1)", "a"];
    let mut node = parsed.value();
    loop {
        assert_eq!(parsed.text(node.span), texts.remove(0));
        node = match &node.kind {
            ExprKind::FieldAccess(object, _) => object,
            ExprKind::Index(target, _) => target,
            ExprKind::MethodCall(object, _, _) => object,
            ExprKind::Ident(_) => break,
            other => panic!("неожиданный узел {other:?}"),
        };
    }
    assert!(texts.is_empty());
}

#[test]
fn match_covers_the_keyword_through_the_closing_brace_and_each_arm_its_own_text() {
    let parsed = Parsed::of_expr("@MATCH x { 1 -> 2, -1 -> 0, n -> n, }");
    assert_eq!(
        parsed.text_of_value(),
        sga("@MATCH x { 1 -> 2, -1 -> 0, n -> n, }")
    );
    let ExprKind::Match(scrutinee, arms) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(scrutinee.span), "x");
    let arm_texts: Vec<String> = arms.iter().map(|a| parsed.text(a.span)).collect();
    assert_eq!(arm_texts, ["1 -> 2", "-1 -> 0", "n -> n"]);
    assert_eq!(parsed.text(arms[0].body.span), "2");
    assert_eq!(parsed.text(arms[2].body.span), "n");
}

#[test]
fn match_arm_with_a_multi_line_body_covers_all_its_lines() {
    let parsed = Parsed::new("@LET v = @MATCH 1 {\n    _ -> [\n        1,\n        2\n    ]\n};");
    let ExprKind::Match(_, arms) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(
        parsed.text(arms[0].span),
        "_ -> [\n        1,\n        2\n    ]"
    );
}

// ── скобки ────────────────────────────────────────────────────────────────

#[test]
fn parentheses_give_no_node_of_their_own_but_enclosing_nodes_include_them() {
    // внутреннее выражение — без скобок
    for (source, inner) in [
        ("(a)", "a"),
        ("((a))", "a"),
        ("( a )", "a"),
        ("(1 + 2)", "1 + 2"),
    ] {
        assert_eq!(Parsed::of_expr(source).text_of_value(), inner, "{source}");
    }
    // охватывающие узлы — со скобками (и открывающей, и закрывающей)
    let parsed = Parsed::of_expr("(a + b) * c");
    assert_eq!(parsed.text_of_value(), "(a + b) * c");
    let ExprKind::Binary(_, left, _) = &parsed.value().kind else {
        panic!()
    };
    assert_eq!(parsed.text(left.span), "a + b");

    assert_eq!(
        Parsed::of_expr("a * (b + c)").text_of_value(),
        "a * (b + c)"
    );
    assert_eq!(
        Parsed::of_expr("(a + b) * (c + d)").text_of_value(),
        "(a + b) * (c + d)"
    );
    assert_eq!(Parsed::of_expr("-(a)").text_of_value(), "-(a)");
    assert_eq!(Parsed::of_expr("(a)[0]").text_of_value(), "(a)[0]");
    assert_eq!(Parsed::of_expr("(p).x").text_of_value(), "(p).x");
    assert_eq!(Parsed::of_expr("(p).m(1)").text_of_value(), "(p).m(1)");
    assert_eq!(Parsed::of_expr("(f)(1)").text_of_value(), "(f)(1)");

    let parsed = Parsed::of_expr("f((1), (2))");
    let args = args_of(parsed.value());
    assert_eq!(parsed.text(args[0].span), "1");
    assert_eq!(parsed.text(args[1].span), "2");
}

// ── операторы ─────────────────────────────────────────────────────────────

fn stmt_text(template: &str) -> String {
    let parsed = Parsed::new(template);
    parsed.text(parsed.program[0].span)
}

#[test]
fn variable_declarations_include_the_semicolon_when_present() {
    for template in [
        "@LET x = 1;",
        "@VAR x = 1 + 2;",
        "@CONST x = 1;",
        "@LET x: int = 1;",
        "@LET x = 1", // без `;` и без перевода строки: `Eof` сразу за значением
        "@LET x = 1\n",
    ] {
        assert_eq!(
            stmt_text(template),
            sga(template.trim_end_matches('\n')),
            "{template:?}"
        );
    }
}

#[test]
fn an_expression_statement_includes_its_optional_semicolon() {
    let parsed = Parsed::new("f(1) g(2);");
    assert_eq!(parsed.text(parsed.program[0].span), "f(1)");
    assert_eq!(parsed.text(parsed.program[1].span), "g(2);");
    assert_eq!(stmt_text("f(1);"), "f(1);");
    assert_eq!(stmt_text("a = 1;"), "a = 1;");
}

#[test]
fn print_return_break_continue_and_import_include_the_semicolon() {
    assert_eq!(stmt_text("@PRINT(1, 2);"), sga("@PRINT(1, 2);"));
    assert_eq!(stmt_text("@PRINT();"), sga("@PRINT();"));
    assert_eq!(stmt_text("@IMPORT \"m.sga\";"), sga("@IMPORT \"m.sga\";"));
    assert_eq!(stmt_text("@IMPORT \"m.sga\""), sga("@IMPORT \"m.sga\""));

    let parsed = Parsed::new("@FN f() { @RETURN 1; @RETURN; @RETURN 2 }");
    let StmtKind::FnDecl { body, .. } = &parsed.program[0].kind else {
        panic!()
    };
    let texts: Vec<String> = body.iter().map(|s| parsed.text(s.span)).collect();
    assert_eq!(
        texts,
        [sga("@RETURN 1;"), sga("@RETURN;"), sga("@RETURN 2")]
    );

    let parsed = Parsed::new("@WHILE a { @BREAK; @CONTINUE }");
    let StmtKind::While { body, .. } = &parsed.program[0].kind else {
        panic!()
    };
    let texts: Vec<String> = body.iter().map(|s| parsed.text(s.span)).collect();
    assert_eq!(texts, [sga("@BREAK;"), sga("@CONTINUE")]);
}

#[test]
fn control_flow_statements_end_at_their_closing_brace() {
    for template in [
        "@WHILE a { @BREAK; }",
        "@FOR i @IN 0..3 { }",
        // после имени в условии пустые `{ }` читались бы как struct-литерал
        // `a {}` (существующая неоднозначность грамматики) — блок не пустой
        "@IF a { 1; } @ELSE { }",
        "@IF a { 1; }",
        "{ @LET a = 1; }",
        "@STRUCT P { x, y }",
        "@STRUCT P { x, y, }",
    ] {
        // следующий оператор в диапазон не попадает
        let parsed = Parsed::new(&format!("{template} 7;"));
        assert_eq!(
            parsed.text(parsed.program[0].span),
            sga(template),
            "{template}"
        );
        assert_eq!(parsed.text(parsed.program[1].span), "7;");
    }
}

#[test]
fn an_else_if_ladder_gives_the_nested_if_its_own_range() {
    let parsed = Parsed::new("@IF a { 1; } @ELSE @IF b { 2; } @ELSE { 3; }");
    let outer = &parsed.program[0];
    assert_eq!(
        parsed.text(outer.span),
        sga("@IF a { 1; } @ELSE @IF b { 2; } @ELSE { 3; }")
    );
    let StmtKind::If { else_branch, .. } = &outer.kind else {
        panic!()
    };
    let nested = &else_branch.as_ref().unwrap()[0];
    assert_eq!(parsed.text(nested.span), sga("@IF b { 2; } @ELSE { 3; }"));
    let StmtKind::If { else_branch, .. } = &nested.kind else {
        panic!()
    };
    // последняя ветка — блок: его операторы имеют свои диапазоны
    assert_eq!(parsed.text(else_branch.as_ref().unwrap()[0].span), "3;");
}

#[test]
fn function_declaration_and_its_parameters_have_ranges() {
    let parsed = Parsed::new("@FN f(@MUT a: int, b, c: string) -> int {\n    @RETURN a;\n}");
    let decl = &parsed.program[0];
    assert_eq!(
        parsed.text(decl.span),
        sga("@FN f(@MUT a: int, b, c: string) -> int {\n    @RETURN a;\n}")
    );
    let StmtKind::FnDecl { params, body, .. } = &decl.kind else {
        panic!()
    };
    let texts: Vec<String> = params.iter().map(|p| parsed.text(p.span)).collect();
    assert_eq!(
        texts,
        [sga("@MUT a: int"), "b".to_string(), "c: string".to_string()]
    );
    assert_eq!(parsed.text(body[0].span), sga("@RETURN a;"));
}

#[test]
fn nested_statements_have_ranges_inside_their_parent() {
    let parsed =
        Parsed::new("@FN f(n) {\n    @FOR i @IN 0..n {\n        @IF i == 2 { @BREAK; }\n    }\n}");
    let StmtKind::FnDecl { body, .. } = &parsed.program[0].kind else {
        panic!()
    };
    assert_eq!(
        parsed.text(body[0].span),
        sga("@FOR i @IN 0..n {\n        @IF i == 2 { @BREAK; }\n    }")
    );
    let StmtKind::ForIn {
        start, end, body, ..
    } = &body[0].kind
    else {
        panic!()
    };
    assert_eq!(parsed.text(start.span), "0");
    assert_eq!(parsed.text(end.span), "n");
    assert_eq!(parsed.text(body[0].span), sga("@IF i == 2 { @BREAK; }"));
}

// ── Parser::detached ──────────────────────────────────────────────────────

/// Программа, в которой есть КАЖДЫЙ вариант `ExprKind` и `StmtKind`.
const KITCHEN_SINK: &str = "
@IMPORT \"m.sga\";
@STRUCT P { x, y }
@FN f(@MUT a: int, b) -> int {
    @LET s = \"text\";
    @VAR n = 1;
    @CONST pi = 3.14;
    @IF n < 2 and not @FALSE { @PRINT(s, [1, 2][0]); } @ELSE @IF n == 3 { n = -n; } @ELSE { }
    @WHILE n <= 10 { n = n + 1; @IF n > 5 { @BREAK; } @ELSE { @CONTINUE; } }
    @FOR i @IN 0..n { { @PRINT(i); } }
    @RETURN a;
}
@LET p = P { x: 1, y: @NIL };
@LET g = @FN(u) { @RETURN u; };
@LET m = @MATCH p.x { 1 -> \"one\", -2.5 -> \"neg\", @TRUE -> g(1)(2), z -> z };
p.x = 3;
p.y.q = f(1, 2);
@LET arr = [1, 2];
arr[0] = (arr[1] + p.x) * 2 % 3 / 4 - 5;
@PRINT(p.m(1, 2), (g)(3));
@RETURN;
";

fn kitchen_sink_template() -> String {
    // `AND`/`OR`/`NOT` в шаблоне пишутся строчными как слова для читаемости;
    // здесь превращаем их в SGA-слова
    KITCHEN_SINK
        .replace(" and ", " @AND ")
        .replace(" not ", " @NOT ")
}

/// Имя варианта узла. Варианты перечислены без `_`.
fn kind_name(node: &Node) -> &'static str {
    match node {
        Node::Param(_) => "Param",
        Node::Arm(_) => "MatchArm",
        Node::Stmt(stmt) => match &stmt.kind {
            StmtKind::VarDecl { .. } => "VarDecl",
            StmtKind::ExprStmt(_) => "ExprStmt",
            StmtKind::Print(_) => "Print",
            StmtKind::If { .. } => "If",
            StmtKind::While { .. } => "While",
            StmtKind::ForIn { .. } => "ForIn",
            StmtKind::FnDecl { .. } => "FnDecl",
            StmtKind::Return(_) => "Return",
            StmtKind::Break => "Break",
            StmtKind::Continue => "Continue",
            StmtKind::Block(_) => "Block",
            StmtKind::Import(_) => "Import",
            StmtKind::StructDecl { .. } => "StructDecl",
        },
        Node::Expr(expr) => match &expr.kind {
            ExprKind::Int(_) => "Int",
            ExprKind::Float(_) => "Float",
            ExprKind::Str(_) => "Str",
            ExprKind::Bool(_) => "Bool",
            ExprKind::Nil => "Nil",
            ExprKind::Ident(_) => "Ident",
            ExprKind::Array(_) => "Array",
            ExprKind::Index(..) => "Index",
            ExprKind::Unary(..) => "Unary",
            ExprKind::Binary(..) => "Binary",
            ExprKind::Call(..) => "Call",
            ExprKind::CallExpr(..) => "CallExpr",
            ExprKind::Assign(..) => "Assign",
            ExprKind::IndexAssign(..) => "IndexAssign",
            ExprKind::Lambda { .. } => "Lambda",
            ExprKind::StructLit { .. } => "StructLit",
            ExprKind::FieldAccess(..) => "FieldAccess",
            ExprKind::FieldAssign(..) => "FieldAssign",
            ExprKind::MethodCall(..) => "MethodCall",
            ExprKind::Match(..) => "Match",
        },
    }
}

/// Все виды узлов, которые знает парсер: 20 `ExprKind`, 13 `StmtKind`, `MatchArm`
/// и `Param`.
const ALL_KINDS: [&str; 35] = [
    "Int",
    "Float",
    "Str",
    "Bool",
    "Nil",
    "Ident",
    "Array",
    "Index",
    "Unary",
    "Binary",
    "Call",
    "CallExpr",
    "Assign",
    "IndexAssign",
    "Lambda",
    "StructLit",
    "FieldAccess",
    "FieldAssign",
    "MethodCall",
    "Match",
    "VarDecl",
    "ExprStmt",
    "Print",
    "If",
    "While",
    "ForIn",
    "FnDecl",
    "Return",
    "Break",
    "Continue",
    "Block",
    "Import",
    "StructDecl",
    "MatchArm",
    "Param",
];

fn collect_kinds(node: &Node, seen: &mut std::collections::BTreeSet<&'static str>) {
    seen.insert(kind_name(node));
    for kid in children(node) {
        collect_kinds(&kid, seen);
    }
}

#[test]
fn a_program_with_every_node_kind_satisfies_the_range_invariants() {
    // `@RETURN;` на верхнем уровне синтаксически допустим (ошибка — в semantic)
    let parsed = Parsed::new(&kitchen_sink_template());
    let nodes = check_program(&parsed, Expect::Real);
    assert!(nodes > 100, "ожидалось большое дерево, узлов: {nodes}");

    // программа действительно содержит КАЖДЫЙ вид узла — иначе проверка выше
    // ничего не говорит о недостающих
    let mut seen = std::collections::BTreeSet::new();
    for stmt in &parsed.program {
        collect_kinds(&Node::Stmt(stmt), &mut seen);
    }
    let expected: std::collections::BTreeSet<&str> = ALL_KINDS.into_iter().collect();
    assert_eq!(expected.len(), 35);
    assert_eq!(
        seen, expected,
        "в программе нет каких-то видов узлов или список устарел"
    );
}

#[test]
fn the_detached_parser_gives_span_none_to_every_node_and_the_same_tree() {
    let template = kitchen_sink_template();
    let attached = Parsed::new(&template);
    let src = sga(&template);
    let tokens = Lexer::new(&src).tokenize().unwrap();
    let detached = Parser::detached(tokens).parse_program().unwrap();
    let detached = Parsed {
        src: src.clone(),
        program: detached,
        table: attached.table.clone(),
    };
    let count = check_program(&detached, Expect::None);
    assert_eq!(count, check_program(&attached, Expect::Real));
    // форма дерева та же: `PartialEq` позиций не сравнивает
    assert_eq!(attached.program, detached.program);
}

#[test]
fn detached_parsing_still_reports_the_same_parse_errors() {
    let src = sga("@LET x = ;");
    let attached = Parser::new(Lexer::new(&src).tokenize().unwrap()).parse_program();
    let detached = Parser::detached(Lexer::new(&src).tokenize().unwrap()).parse_program();
    assert_eq!(
        attached.unwrap_err().to_string(),
        detached.unwrap_err().to_string()
    );
}

// ── сквозной инвариант на примерах репозитория ────────────────────────────

#[test]
fn every_example_program_satisfies_the_range_invariants() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("каталог examples") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("sga") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let (tokens, table) = Lexer::new(&src)
            .tokenize_with_spans()
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let program = Parser::new(tokens)
            .parse_program()
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let parsed = Parsed {
            src,
            program,
            table,
        };
        let nodes = check_program(&parsed, Expect::Real);
        assert!(nodes > 0, "{}", path.display());
        // каждый оператор верхнего уровня переводится в позиции и начинается на
        // своей строке не раньше предыдущего
        for stmt in &parsed.program {
            let at = stmt.span.resolve(&parsed.table).expect("перевод в позиции");
            assert!(at.line >= 1 && at.col >= 1, "{}: {at:?}", path.display());
            assert!(!parsed.text(stmt.span).is_empty());
        }
        checked += 1;
    }
    assert_eq!(checked, 17, "в examples/ ожидалось 17 программ");
}
