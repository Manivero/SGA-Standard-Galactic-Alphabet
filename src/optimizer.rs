//! Constant folding + dead code elimination (DCE) — T010, M004. AST ->
//! AST проход, место в пайплайне — ПОСЛЕ `typechecker` (валидированный
//! AST), ДО `codegen`. Полный Feature Contract — см. PROJECT_STATUS.md.
//!
//! КЛЮЧЕВОЙ ПРИНЦИП: семантически ПРОЗРАЧЕН — поведение любой корректной
//! программы идентично с этим проходом и без него. Арифметика/сравнение
//! НЕ дублируются заново — переиспользуются `vm::arith`/`vm::compare`/
//! `vm::values_equal` (сделаны `pub(crate)` специально для этого), чтобы
//! свёрнутое-на-этапе-компиляции выражение не могло разойтись в
//! поведении с тем же выражением, вычисленным во время исполнения
//! (особенно важно для переполнения `Int` и деления на ноль — оба
//! ДОЛЖНЫ остаться ошибками времени выполнения, если сворачивание
//! невозможно, см. `fold_binary`/`fold_unary` ниже).
//!
//! ПОЗИЦИИ (T017): проход сохраняет `Span` узлов. Свёрнутая константа получает
//! диапазон ВСЕГО исходного выражения (`1 + 2` → `3` с диапазоном `1 + 2`),
//! а заменивший `IF`/`WHILE` блок — диапазон исходного оператора. Позиции
//! после этого прохода никто не читает (ошибки находят `semantic` и
//! `typechecker` РАНЬШЕ), но потерянные диапазоны сделали бы дерево
//! недостоверным для любого будущего потребителя (например, ошибок времени
//! выполнения с позицией).

use crate::ast::{BinOp, Expr, ExprKind, MatchArm, Program, Stmt, StmtKind, UnOp};
use crate::runtime::{OpCode, Value};
use crate::vm;

/// Точка входа — оптимизирует всю программу.
pub fn optimize(program: Program) -> Program {
    optimize_stmts(program)
}

/// Оптимизирует список statements: рекурсивно оптимизирует каждый, затем
/// отбрасывает всё, что идёт ПОСЛЕ `RETURN`/`BREAK`/`CONTINUE` в этом же
/// списке (AC-08) — такой код недостижим ни при каких значениях
/// переменных, семантический анализ уже проверил его ДО этого прохода
/// (см. doc-комментарий модуля), поэтому удаление безопасно.
fn optimize_stmts(stmts: Vec<Stmt>) -> Vec<Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        let optimized = optimize_stmt(stmt);
        let is_terminator = matches!(
            optimized.kind,
            StmtKind::Return(_) | StmtKind::Break | StmtKind::Continue
        );
        out.push(optimized);
        if is_terminator {
            break;
        }
    }
    out
}

fn optimize_stmt(stmt: Stmt) -> Stmt {
    let span = stmt.span;
    let kind = match stmt.kind {
        StmtKind::VarDecl {
            name,
            value,
            mutable,
            ty,
        } => StmtKind::VarDecl {
            name,
            value: fold_expr(value),
            mutable,
            ty,
        },
        StmtKind::ExprStmt(e) => StmtKind::ExprStmt(fold_expr(e)),
        StmtKind::Print(exprs) => StmtKind::Print(exprs.into_iter().map(fold_expr).collect()),
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let cond = fold_expr(cond);
            let then_branch = optimize_stmts(then_branch);
            let else_branch = else_branch.map(optimize_stmts);
            match const_bool(&cond) {
                Some(true) => StmtKind::Block(then_branch),
                Some(false) => StmtKind::Block(else_branch.unwrap_or_default()),
                None => StmtKind::If {
                    cond,
                    then_branch,
                    else_branch,
                },
            }
        }
        StmtKind::While { cond, body } => {
            let cond = fold_expr(cond);
            let body = optimize_stmts(body);
            match const_bool(&cond) {
                Some(false) => StmtKind::Block(vec![]),
                _ => StmtKind::While { cond, body },
            }
        }
        StmtKind::ForIn {
            var,
            start,
            end,
            body,
        } => StmtKind::ForIn {
            var,
            start: fold_expr(start),
            end: fold_expr(end),
            body: optimize_stmts(body),
        },
        StmtKind::FnDecl {
            name,
            params,
            body,
            return_ty,
        } => StmtKind::FnDecl {
            name,
            params,
            body: optimize_stmts(body),
            return_ty,
        },
        StmtKind::Return(opt_e) => StmtKind::Return(opt_e.map(fold_expr)),
        StmtKind::Break => StmtKind::Break,
        StmtKind::Continue => StmtKind::Continue,
        StmtKind::Block(stmts) => StmtKind::Block(optimize_stmts(stmts)),
        other @ (StmtKind::Import(_) | StmtKind::StructDecl { .. }) => other,
    };
    Stmt::new(kind, span)
}

fn const_bool(e: &Expr) -> Option<bool> {
    match &e.kind {
        ExprKind::Bool(b) => Some(*b),
        _ => None,
    }
}

fn fold_expr(expr: Expr) -> Expr {
    let span = expr.span;
    let kind = match expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::Nil
        | ExprKind::Ident(_) => return expr,
        ExprKind::Array(items) => ExprKind::Array(items.into_iter().map(fold_expr).collect()),
        ExprKind::Index(arr, idx) => {
            ExprKind::Index(Box::new(fold_expr(*arr)), Box::new(fold_expr(*idx)))
        }
        ExprKind::Unary(op, inner) => {
            let inner = fold_expr(*inner);
            match fold_unary(&op, &inner) {
                Some(folded) => folded,
                None => ExprKind::Unary(op, Box::new(inner)),
            }
        }
        ExprKind::Binary(op, l, r) => {
            let l = fold_expr(*l);
            let r = fold_expr(*r);
            match fold_binary(&op, &l, &r) {
                Some(folded) => folded,
                None => ExprKind::Binary(op, Box::new(l), Box::new(r)),
            }
        }
        ExprKind::Call(name, args) => {
            ExprKind::Call(name, args.into_iter().map(fold_expr).collect())
        }
        ExprKind::CallExpr(callee, args) => ExprKind::CallExpr(
            Box::new(fold_expr(*callee)),
            args.into_iter().map(fold_expr).collect(),
        ),
        ExprKind::Assign(name, value) => ExprKind::Assign(name, Box::new(fold_expr(*value))),
        ExprKind::IndexAssign(arr, idx, value) => ExprKind::IndexAssign(
            Box::new(fold_expr(*arr)),
            Box::new(fold_expr(*idx)),
            Box::new(fold_expr(*value)),
        ),
        ExprKind::Lambda { params, body } => ExprKind::Lambda {
            params,
            body: optimize_stmts(body),
        },
        ExprKind::StructLit { type_name, fields } => ExprKind::StructLit {
            type_name,
            fields: fields.into_iter().map(|(k, v)| (k, fold_expr(v))).collect(),
        },
        ExprKind::FieldAccess(obj, field) => {
            ExprKind::FieldAccess(Box::new(fold_expr(*obj)), field)
        }
        ExprKind::FieldAssign(obj, field, value) => ExprKind::FieldAssign(
            Box::new(fold_expr(*obj)),
            field,
            Box::new(fold_expr(*value)),
        ),
        ExprKind::MethodCall(obj, method, args) => ExprKind::MethodCall(
            Box::new(fold_expr(*obj)),
            method,
            args.into_iter().map(fold_expr).collect(),
        ),
        ExprKind::Match(scrutinee, arms) => ExprKind::Match(
            Box::new(fold_expr(*scrutinee)),
            arms.into_iter()
                .map(|arm| MatchArm {
                    pattern: arm.pattern,
                    body: fold_expr(arm.body),
                    span: arm.span,
                })
                .collect(),
        ),
    };
    Expr::new(kind, span)
}

fn expr_to_value(e: &Expr) -> Option<Value> {
    match &e.kind {
        ExprKind::Int(n) => Some(Value::Int(*n)),
        ExprKind::Float(f) => Some(Value::Float(*f)),
        ExprKind::Str(s) => Some(Value::Str(s.clone())),
        ExprKind::Bool(b) => Some(Value::Bool(*b)),
        ExprKind::Nil => Some(Value::Nil),
        _ => None,
    }
}

fn value_to_kind(v: Value) -> Option<ExprKind> {
    match v {
        Value::Int(n) => Some(ExprKind::Int(n)),
        Value::Float(f) => Some(ExprKind::Float(f)),
        Value::Str(s) => Some(ExprKind::Str(s)),
        Value::Bool(b) => Some(ExprKind::Bool(b)),
        Value::Nil => Some(ExprKind::Nil),
        Value::Array(_) | Value::Struct { .. } | Value::Closure(_) => None,
    }
}

fn binop_to_opcode(op: &BinOp) -> OpCode {
    match op {
        BinOp::Add => OpCode::Add,
        BinOp::Sub => OpCode::Sub,
        BinOp::Mul => OpCode::Mul,
        BinOp::Div => OpCode::Div,
        BinOp::Mod => OpCode::Mod,
        BinOp::Eq => OpCode::Eq,
        BinOp::NotEq => OpCode::NotEq,
        BinOp::Lt => OpCode::Lt,
        BinOp::Gt => OpCode::Gt,
        BinOp::LtEq => OpCode::LtEq,
        BinOp::GtEq => OpCode::GtEq,
        BinOp::And => OpCode::And,
        BinOp::Or => OpCode::Or,
    }
}

fn fold_binary(op: &BinOp, l: &Expr, r: &Expr) -> Option<ExprKind> {
    let lv = expr_to_value(l)?;
    let rv = expr_to_value(r)?;
    match op {
        BinOp::And => return value_to_kind(Value::Bool(lv.is_truthy() && rv.is_truthy())),
        BinOp::Or => return value_to_kind(Value::Bool(lv.is_truthy() || rv.is_truthy())),
        _ => {}
    }
    let opcode = binop_to_opcode(op);
    let result = match op {
        BinOp::Eq => Ok(Value::Bool(vm::values_equal(&lv, &rv))),
        BinOp::NotEq => Ok(Value::Bool(!vm::values_equal(&lv, &rv))),
        BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => vm::compare(&opcode, lv, rv),
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            vm::arith(&opcode, lv, rv)
        }
        BinOp::And | BinOp::Or => unreachable!("обработаны выше через return"),
    };
    result.ok().and_then(value_to_kind)
}

fn fold_unary(op: &UnOp, inner: &Expr) -> Option<ExprKind> {
    let v = expr_to_value(inner)?;
    match (op, v) {
        (UnOp::Neg, Value::Int(n)) => n.checked_neg().and_then(|r| value_to_kind(Value::Int(r))),
        (UnOp::Neg, Value::Float(f)) => value_to_kind(Value::Float(-f)),
        (UnOp::Not, v) => value_to_kind(Value::Bool(!v.is_truthy())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;

    fn e(kind: ExprKind) -> Expr {
        Expr::unspanned(kind)
    }

    fn st(kind: StmtKind) -> Stmt {
        Stmt::unspanned(kind)
    }

    fn int(n: i64) -> Expr {
        e(ExprKind::Int(n))
    }

    fn float(f: f64) -> Expr {
        e(ExprKind::Float(f))
    }

    fn string(s: &str) -> Expr {
        e(ExprKind::Str(s.into()))
    }

    fn boolean(b: bool) -> Expr {
        e(ExprKind::Bool(b))
    }

    fn ident(name: &str) -> Expr {
        e(ExprKind::Ident(name.into()))
    }

    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        e(ExprKind::Binary(op, Box::new(l), Box::new(r)))
    }

    fn unary(op: UnOp, inner: Expr) -> Expr {
        e(ExprKind::Unary(op, Box::new(inner)))
    }

    fn expr_stmt(expr: Expr) -> Stmt {
        st(StmtKind::ExprStmt(expr))
    }

    #[test]
    fn test_fold_int_arithmetic() {
        assert_eq!(fold_expr(bin(BinOp::Add, int(1), int(2))), int(3));
        assert_eq!(fold_expr(bin(BinOp::Mul, int(6), int(7))), int(42));
    }

    #[test]
    fn test_fold_nested_arithmetic() {
        let expr = bin(
            BinOp::Mul,
            bin(BinOp::Add, int(1), int(2)),
            bin(BinOp::Add, int(3), int(4)),
        );
        assert_eq!(fold_expr(expr), int(21));
    }

    #[test]
    fn test_fold_mixed_int_float_promotes_to_float() {
        assert_eq!(fold_expr(bin(BinOp::Add, int(1), float(2.5))), float(3.5));
    }

    #[test]
    fn test_fold_string_concat() {
        assert_eq!(
            fold_expr(bin(BinOp::Add, string("a"), string("b"))),
            string("ab")
        );
    }

    #[test]
    fn test_fold_comparisons() {
        assert_eq!(fold_expr(bin(BinOp::Lt, int(1), int(2))), boolean(true));
        assert_eq!(fold_expr(bin(BinOp::Eq, int(1), int(1))), boolean(true));
        assert_eq!(
            fold_expr(bin(BinOp::NotEq, string("a"), string("b"))),
            boolean(true)
        );
    }

    #[test]
    fn test_fold_and_or_no_short_circuit_both_literal() {
        assert_eq!(
            fold_expr(bin(BinOp::And, boolean(true), boolean(false))),
            boolean(false)
        );
        assert_eq!(
            fold_expr(bin(BinOp::Or, boolean(false), boolean(true))),
            boolean(true)
        );
    }

    #[test]
    fn test_fold_unary_neg_and_not() {
        assert_eq!(fold_expr(unary(UnOp::Neg, int(5))), int(-5));
        assert_eq!(fold_expr(unary(UnOp::Neg, float(2.5))), float(-2.5));
        assert_eq!(fold_expr(unary(UnOp::Not, boolean(true))), boolean(false));
    }

    #[test]
    fn test_neg_i64_min_does_not_fold() {
        let expr = unary(UnOp::Neg, int(i64::MIN));
        let folded = fold_expr(expr.clone());
        assert_eq!(folded, expr);
    }

    #[test]
    fn test_overflow_does_not_fold() {
        let expr = bin(BinOp::Add, int(i64::MAX), int(1));
        assert_eq!(fold_expr(expr.clone()), expr);
    }

    #[test]
    fn test_division_by_zero_does_not_fold() {
        let expr = bin(BinOp::Div, int(1), int(0));
        assert_eq!(fold_expr(expr.clone()), expr);
    }

    #[test]
    fn test_non_literal_does_not_fold() {
        let expr = bin(BinOp::Add, ident("x"), int(1));
        assert_eq!(fold_expr(expr.clone()), expr);
    }

    #[test]
    fn test_dce_unreachable_after_return() {
        let stmts = vec![st(StmtKind::Return(Some(int(1)))), expr_stmt(int(2))];
        assert_eq!(
            optimize_stmts(stmts),
            vec![st(StmtKind::Return(Some(int(1))))]
        );
    }

    #[test]
    fn test_dce_if_true_becomes_then_branch() {
        let stmt = st(StmtKind::If {
            cond: boolean(true),
            then_branch: vec![expr_stmt(int(1))],
            else_branch: Some(vec![expr_stmt(int(2))]),
        });
        assert_eq!(
            optimize_stmt(stmt),
            st(StmtKind::Block(vec![expr_stmt(int(1))]))
        );
    }

    #[test]
    fn test_dce_if_false_becomes_else_branch() {
        let stmt = st(StmtKind::If {
            cond: boolean(false),
            then_branch: vec![expr_stmt(int(1))],
            else_branch: Some(vec![expr_stmt(int(2))]),
        });
        assert_eq!(
            optimize_stmt(stmt),
            st(StmtKind::Block(vec![expr_stmt(int(2))]))
        );
    }

    #[test]
    fn test_dce_if_false_without_else_becomes_empty_block() {
        let stmt = st(StmtKind::If {
            cond: boolean(false),
            then_branch: vec![expr_stmt(int(1))],
            else_branch: None,
        });
        assert_eq!(optimize_stmt(stmt), st(StmtKind::Block(vec![])));
    }

    #[test]
    fn test_dce_while_false_becomes_empty_block() {
        let stmt = st(StmtKind::While {
            cond: boolean(false),
            body: vec![expr_stmt(int(1))],
        });
        assert_eq!(optimize_stmt(stmt), st(StmtKind::Block(vec![])));
    }

    #[test]
    fn test_while_true_is_not_touched() {
        let stmt = st(StmtKind::While {
            cond: boolean(true),
            body: vec![st(StmtKind::Break)],
        });
        assert_eq!(
            optimize_stmt(stmt),
            st(StmtKind::While {
                cond: boolean(true),
                body: vec![st(StmtKind::Break)],
            })
        );
    }

    // ── T017: позиции сохраняются ───────────────────────────────────────

    #[test]
    fn test_folded_constant_keeps_the_span_of_the_whole_expression() {
        // `Expr` сравнивается без позиций, поэтому смотрим на `span` напрямую
        let whole = Span::new(4, 8);
        let folded = fold_expr(Expr::new(
            ExprKind::Binary(
                BinOp::Add,
                Box::new(Expr::new(ExprKind::Int(1), Span::new(4, 4))),
                Box::new(Expr::new(ExprKind::Int(2), Span::new(8, 8))),
            ),
            whole,
        ));
        assert_eq!(folded, int(3));
        assert_eq!(folded.span, whole);

        let unary = fold_expr(Expr::new(
            ExprKind::Unary(
                UnOp::Neg,
                Box::new(Expr::new(ExprKind::Int(5), Span::new(1, 1))),
            ),
            Span::new(0, 1),
        ));
        assert_eq!(unary, int(-5));
        assert_eq!(unary.span, Span::new(0, 1));
    }

    #[test]
    fn test_unfolded_nodes_keep_their_own_spans() {
        let tree = Expr::new(
            ExprKind::Binary(
                BinOp::Add,
                Box::new(Expr::new(ExprKind::Ident("x".into()), Span::new(2, 2))),
                Box::new(Expr::new(ExprKind::Int(1), Span::new(4, 4))),
            ),
            Span::new(2, 4),
        );
        let optimized = fold_expr(tree);
        assert_eq!(optimized.span, Span::new(2, 4));
        let ExprKind::Binary(_, l, r) = &optimized.kind else {
            panic!("узел не должен сворачиваться")
        };
        assert_eq!(l.span, Span::new(2, 2));
        assert_eq!(r.span, Span::new(4, 4));
    }

    #[test]
    fn test_dce_block_and_arms_keep_spans() {
        let stmt = Stmt::new(
            StmtKind::If {
                cond: boolean(true),
                then_branch: vec![Stmt::new(StmtKind::Break, Span::new(7, 8))],
                else_branch: None,
            },
            Span::new(3, 9),
        );
        let optimized = optimize_stmt(stmt);
        assert_eq!(optimized.span, Span::new(3, 9));
        let StmtKind::Block(inner) = &optimized.kind else {
            panic!("IF TRUE должен стать блоком")
        };
        assert_eq!(inner[0].span, Span::new(7, 8));

        let arm_span = Span::new(10, 14);
        let matched = fold_expr(e(ExprKind::Match(
            Box::new(int(1)),
            vec![MatchArm {
                pattern: crate::ast::Pattern::Wildcard,
                body: bin(BinOp::Add, int(1), int(1)),
                span: arm_span,
            }],
        )));
        let ExprKind::Match(_, arms) = &matched.kind else {
            panic!("MATCH остаётся MATCH")
        };
        assert_eq!(arms[0].span, arm_span);
        assert_eq!(arms[0].body, int(2));
    }
}
