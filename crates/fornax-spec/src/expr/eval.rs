// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Evaluator for the expression language.

use std::fmt;

use super::{Ast, BinOp, Context, Func, Path};
use crate::pattern::Pattern;

/// A runtime value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null` (also the value of missing context entries).
    Null,
    /// `true` / `false`.
    Bool(bool),
    /// A number (IEEE 754 double).
    Number(f64),
    /// A string.
    String(String),
}

impl Value {
    /// GitHub-Actions truthiness: `null`, `false`, `0`, `NaN` and `''` are false.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => *n != 0.0 && !n.is_nan(),
            Value::String(s) => !s.is_empty(),
        }
    }

    /// Numeric conversion used when comparing values of different types.
    fn to_number(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Bool(b) => f64::from(u8::from(*b)),
            Value::Number(n) => *n,
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    0.0
                } else {
                    t.parse().unwrap_or(f64::NAN)
                }
            }
        }
    }

    fn loose_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            _ => self.to_number() == other.to_number(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{n}")
                }
            }
            Value::String(s) => f.write_str(s),
        }
    }
}

fn opt(v: Option<&String>) -> Value {
    v.map_or(Value::Null, |s| Value::String(s.clone()))
}

pub(crate) fn eval(ast: &Ast, ctx: &Context) -> Value {
    match ast {
        Ast::Literal(v) => v.clone(),
        Ast::Path(p) => match p {
            Path::Event => opt(ctx.event.as_ref()),
            Path::Ref => opt(ctx.ref_name.as_ref()),
            Path::Branch => opt(ctx.branch.as_ref()),
            Path::Tag => opt(ctx.tag.as_ref()),
            Path::Sha => opt(ctx.sha.as_ref()),
            Path::Repo => opt(ctx.repo.as_ref()),
            Path::Var(name) => opt(ctx.vars.get(name)),
            Path::Matrix(name) => opt(ctx.matrix.get(name)),
        },
        Ast::Not(inner) => Value::Bool(!eval(inner, ctx).is_truthy()),
        Ast::Binary(op, lhs, rhs) => match op {
            BinOp::And => {
                let l = eval(lhs, ctx);
                if l.is_truthy() { eval(rhs, ctx) } else { l }
            }
            BinOp::Or => {
                let l = eval(lhs, ctx);
                if l.is_truthy() { l } else { eval(rhs, ctx) }
            }
            BinOp::Eq => Value::Bool(eval(lhs, ctx).loose_eq(&eval(rhs, ctx))),
            BinOp::Ne => Value::Bool(!eval(lhs, ctx).loose_eq(&eval(rhs, ctx))),
        },
        Ast::Call(func, args, cached) => {
            let a = args
                .first()
                .map(|x| eval(x, ctx))
                .unwrap_or(Value::Null)
                .to_string();
            let b_val = args.get(1).map(|x| eval(x, ctx)).unwrap_or(Value::Null);
            let b = b_val.to_string();
            let result = match func {
                Func::StartsWith => a.starts_with(&b),
                Func::EndsWith => a.ends_with(&b),
                Func::Contains => a.contains(&b),
                Func::Matches => match cached {
                    Some(p) => p.matches(&a) != p.is_negated(),
                    None => Pattern::new(&b).is_ok_and(|p| p.matches(&a) != p.is_negated()),
                },
            };
            Value::Bool(result)
        }
    }
}
