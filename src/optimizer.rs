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

use crate::ast::{BinOp, Expr, MatchArm, Program, Stmt, UnOp};
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
        let is_terminator = matches!(optimized, Stmt::Return(_) | Stmt::Break | Stmt::Continue);
        out.push(optimized);
        if is_terminator {
            break;
        }
    }
    out
}

fn optimize_stmt(stmt: Stmt) -> Stmt {
    match stmt {
        Stmt::VarDecl {
            name,
            value,
            mutable,
            ty,
        } => Stmt::VarDecl {
            name,
            value: fold_expr(value),
            mutable,
            ty,
        },
        Stmt::ExprStmt(e) => Stmt::ExprStmt(fold_expr(e)),
        Stmt::Print(exprs) => Stmt::Print(exprs.into_iter().map(fold_expr).collect()),
        Stmt::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let cond = fold_expr(cond);
            let then_branch = optimize_stmts(then_branch);
            let else_branch = else_branch.map(optimize_stmts);
            match const_bool(&cond) {
                Some(true) => Stmt::Block(then_branch),
                Some(false) => Stmt::Block(else_branch.unwrap_or_default()),
                None => Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                },
            }
        }
        Stmt::While { cond, body } => {
            let cond = fold_expr(cond);
            let body = optimize_stmts(body);
            match const_bool(&cond) {
                Some(false) => Stmt::Block(vec![]),
                _ => Stmt::While { cond, body },
            }
        }
        Stmt::ForIn {
            var,
            start,
            end,
            body,
        } => Stmt::ForIn {
            var,
            start: fold_expr(start),
            end: fold_expr(end),
            body: optimize_stmts(body),
        },
        Stmt::FnDecl {
            name,
            params,
            body,
            return_ty,
        } => Stmt::FnDecl {
            name,
            params,
            body: optimize_stmts(body),
            return_ty,
        },
        Stmt::Return(opt_e) => Stmt::Return(opt_e.map(fold_expr)),
        Stmt::Break => Stmt::Break,
        Stmt::Continue => Stmt::Continue,
        Stmt::Block(stmts) => Stmt::Block(optimize_stmts(stmts)),
        other @ (Stmt::Import(_) | Stmt::StructDecl { .. }) => other,
    }
}

fn const_bool(e: &Expr) -> Option<bool> {
    match e {
        Expr::Bool(b) => Some(*b),
        _ => None,
    }
}

fn fold_expr(expr: Expr) -> Expr {
    match expr {
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Nil
        | Expr::Ident(_) => expr,
        Expr::Array(items) => Expr::Array(items.into_iter().map(fold_expr).collect()),
        Expr::Index(arr, idx) => Expr::Index(Box::new(fold_expr(*arr)), Box::new(fold_expr(*idx))),
        Expr::Unary(op, inner) => {
            let inner = fold_expr(*inner);
            match fold_unary(&op, &inner) {
                Some(folded) => folded,
                None => Expr::Unary(op, Box::new(inner)),
            }
        }
        Expr::Binary(op, l, r) => {
            let l = fold_expr(*l);
            let r = fold_expr(*r);
            match fold_binary(&op, &l, &r) {
                Some(folded) => folded,
                None => Expr::Binary(op, Box::new(l), Box::new(r)),
            }
        }
        Expr::Call(name, args) => Expr::Call(name, args.into_iter().map(fold_expr).collect()),
        Expr::CallExpr(callee, args) => Expr::CallExpr(
            Box::new(fold_expr(*callee)),
            args.into_iter().map(fold_expr).collect(),
        ),
        Expr::Assign(name, value) => Expr::Assign(name, Box::new(fold_expr(*value))),
        Expr::IndexAssign(arr, idx, value) => Expr::IndexAssign(
            Box::new(fold_expr(*arr)),
            Box::new(fold_expr(*idx)),
            Box::new(fold_expr(*value)),
        ),
        Expr::Lambda { params, body } => Expr::Lambda {
            params,
            body: optimize_stmts(body),
        },
        Expr::StructLit { type_name, fields } => Expr::StructLit {
            type_name,
            fields: fields.into_iter().map(|(k, v)| (k, fold_expr(v))).collect(),
        },
        Expr::FieldAccess(obj, field) => Expr::FieldAccess(Box::new(fold_expr(*obj)), field),
        Expr::FieldAssign(obj, field, value) => Expr::FieldAssign(
            Box::new(fold_expr(*obj)),
            field,
            Box::new(fold_expr(*value)),
        ),
        Expr::MethodCall(obj, method, args) => Expr::MethodCall(
            Box::new(fold_expr(*obj)),
            method,
            args.into_iter().map(fold_expr).collect(),
        ),
        Expr::Match(scrutinee, arms) => Expr::Match(
            Box::new(fold_expr(*scrutinee)),
            arms.into_iter()
                .map(|arm| MatchArm {
                    pattern: arm.pattern,
                    body: fold_expr(arm.body),
                })
                .collect(),
        ),
    }
}

fn expr_to_value(e: &Expr) -> Option<Value> {
    match e {
        Expr::Int(n) => Some(Value::Int(*n)),
        Expr::Float(f) => Some(Value::Float(*f)),
        Expr::Str(s) => Some(Value::Str(s.clone())),
        Expr::Bool(b) => Some(Value::Bool(*b)),
        Expr::Nil => Some(Value::Nil),
        _ => None,
    }
}

fn value_to_expr(v: Value) -> Option<Expr> {
    match v {
        Value::Int(n) => Some(Expr::Int(n)),
        Value::Float(f) => Some(Expr::Float(f)),
        Value::Str(s) => Some(Expr::Str(s)),
        Value::Bool(b) => Some(Expr::Bool(b)),
        Value::Nil => Some(Expr::Nil),
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

fn fold_binary(op: &BinOp, l: &Expr, r: &Expr) -> Option<Expr> {
    let lv = expr_to_value(l)?;
    let rv = expr_to_value(r)?;
    match op {
        BinOp::And => return value_to_expr(Value::Bool(lv.is_truthy() && rv.is_truthy())),
        BinOp::Or => return value_to_expr(Value::Bool(lv.is_truthy() || rv.is_truthy())),
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
    result.ok().and_then(value_to_expr)
}

fn fold_unary(op: &UnOp, inner: &Expr) -> Option<Expr> {
    let v = expr_to_value(inner)?;
    match (op, v) {
        (UnOp::Neg, Value::Int(n)) => n.checked_neg().and_then(|r| value_to_expr(Value::Int(r))),
        (UnOp::Neg, Value::Float(f)) => value_to_expr(Value::Float(-f)),
        (UnOp::Not, v) => value_to_expr(Value::Bool(!v.is_truthy())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary(op, Box::new(l), Box::new(r))
    }

    #[test]
    fn test_fold_int_arithmetic() {
        assert_eq!(
            fold_expr(bin(BinOp::Add, Expr::Int(1), Expr::Int(2))),
            Expr::Int(3)
        );
        assert_eq!(
            fold_expr(bin(BinOp::Mul, Expr::Int(6), Expr::Int(7))),
            Expr::Int(42)
        );
    }

    #[test]
    fn test_fold_nested_arithmetic() {
        let expr = bin(
            BinOp::Mul,
            bin(BinOp::Add, Expr::Int(1), Expr::Int(2)),
            bin(BinOp::Add, Expr::Int(3), Expr::Int(4)),
        );
        assert_eq!(fold_expr(expr), Expr::Int(21));
    }

    #[test]
    fn test_fold_mixed_int_float_promotes_to_float() {
        assert_eq!(
            fold_expr(bin(BinOp::Add, Expr::Int(1), Expr::Float(2.5))),
            Expr::Float(3.5)
        );
    }

    #[test]
    fn test_fold_string_concat() {
        assert_eq!(
            fold_expr(bin(
                BinOp::Add,
                Expr::Str("a".into()),
                Expr::Str("b".into())
            )),
            Expr::Str("ab".into())
        );
    }

    #[test]
    fn test_fold_comparisons() {
        assert_eq!(
            fold_expr(bin(BinOp::Lt, Expr::Int(1), Expr::Int(2))),
            Expr::Bool(true)
        );
        assert_eq!(
            fold_expr(bin(BinOp::Eq, Expr::Int(1), Expr::Int(1))),
            Expr::Bool(true)
        );
        assert_eq!(
            fold_expr(bin(
                BinOp::NotEq,
                Expr::Str("a".into()),
                Expr::Str("b".into())
            )),
            Expr::Bool(true)
        );
    }

    #[test]
    fn test_fold_and_or_no_short_circuit_both_literal() {
        assert_eq!(
            fold_expr(bin(BinOp::And, Expr::Bool(true), Expr::Bool(false))),
            Expr::Bool(false)
        );
        assert_eq!(
            fold_expr(bin(BinOp::Or, Expr::Bool(false), Expr::Bool(true))),
            Expr::Bool(true)
        );
    }

    #[test]
    fn test_fold_unary_neg_and_not() {
        assert_eq!(
            fold_expr(Expr::Unary(UnOp::Neg, Box::new(Expr::Int(5)))),
            Expr::Int(-5)
        );
        assert_eq!(
            fold_expr(Expr::Unary(UnOp::Neg, Box::new(Expr::Float(2.5)))),
            Expr::Float(-2.5)
        );
        assert_eq!(
            fold_expr(Expr::Unary(UnOp::Not, Box::new(Expr::Bool(true)))),
            Expr::Bool(false)
        );
    }

    #[test]
    fn test_neg_i64_min_does_not_fold() {
        let e = Expr::Unary(UnOp::Neg, Box::new(Expr::Int(i64::MIN)));
        let folded = fold_expr(e.clone());
        assert_eq!(folded, e);
    }

    #[test]
    fn test_overflow_does_not_fold() {
        let e = bin(BinOp::Add, Expr::Int(i64::MAX), Expr::Int(1));
        assert_eq!(fold_expr(e.clone()), e);
    }

    #[test]
    fn test_division_by_zero_does_not_fold() {
        let e = bin(BinOp::Div, Expr::Int(1), Expr::Int(0));
        assert_eq!(fold_expr(e.clone()), e);
    }

    #[test]
    fn test_non_literal_does_not_fold() {
        let e = bin(BinOp::Add, Expr::Ident("x".into()), Expr::Int(1));
        assert_eq!(fold_expr(e.clone()), e);
    }

    #[test]
    fn test_dce_unreachable_after_return() {
        let stmts = vec![
            Stmt::Return(Some(Expr::Int(1))),
            Stmt::ExprStmt(Expr::Int(2)),
        ];
        assert_eq!(
            optimize_stmts(stmts),
            vec![Stmt::Return(Some(Expr::Int(1)))]
        );
    }

    #[test]
    fn test_dce_if_true_becomes_then_branch() {
        let stmt = Stmt::If {
            cond: Expr::Bool(true),
            then_branch: vec![Stmt::ExprStmt(Expr::Int(1))],
            else_branch: Some(vec![Stmt::ExprStmt(Expr::Int(2))]),
        };
        assert_eq!(
            optimize_stmt(stmt),
            Stmt::Block(vec![Stmt::ExprStmt(Expr::Int(1))])
        );
    }

    #[test]
    fn test_dce_if_false_becomes_else_branch() {
        let stmt = Stmt::If {
            cond: Expr::Bool(false),
            then_branch: vec![Stmt::ExprStmt(Expr::Int(1))],
            else_branch: Some(vec![Stmt::ExprStmt(Expr::Int(2))]),
        };
        assert_eq!(
            optimize_stmt(stmt),
            Stmt::Block(vec![Stmt::ExprStmt(Expr::Int(2))])
        );
    }

    #[test]
    fn test_dce_if_false_without_else_becomes_empty_block() {
        let stmt = Stmt::If {
            cond: Expr::Bool(false),
            then_branch: vec![Stmt::ExprStmt(Expr::Int(1))],
            else_branch: None,
        };
        assert_eq!(optimize_stmt(stmt), Stmt::Block(vec![]));
    }

    #[test]
    fn test_dce_while_false_becomes_empty_block() {
        let stmt = Stmt::While {
            cond: Expr::Bool(false),
            body: vec![Stmt::ExprStmt(Expr::Int(1))],
        };
        assert_eq!(optimize_stmt(stmt), Stmt::Block(vec![]));
    }

    #[test]
    fn test_while_true_is_not_touched() {
        let stmt = Stmt::While {
            cond: Expr::Bool(true),
            body: vec![Stmt::Break],
        };
        assert_eq!(
            optimize_stmt(stmt),
            Stmt::While {
                cond: Expr::Bool(true),
                body: vec![Stmt::Break],
            }
        );
    }
}
