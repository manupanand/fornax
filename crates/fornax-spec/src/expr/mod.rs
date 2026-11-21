// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The `if:` expression language (LOG D-034).
//!
//! A small, side-effect-free language, written bare or wrapped in `${{ … }}`:
//!
//! - literals: `'single'` (with `''` as an escaped quote), `"double"` (with `\"`, `\\`,
//!   `\n`, `\t` escapes), numbers (`1`, `-2.5`, `1e3`), `true`, `false`, `null`;
//! - context paths: `event`, `ref`, `branch`, `tag`, `sha`, `repo`, `vars.NAME`,
//!   `matrix.NAME` (Fornax extension for matrix jobs);
//! - operators (lowest to highest precedence): `||`, `&&`, `==` / `!=`, prefix `!`;
//!   parentheses;
//! - functions (names are case-insensitive): `startsWith(a, b)`, `endsWith(a, b)`,
//!   `contains(a, b)`, `matches(value, glob)` — glob syntax as in [`crate::pattern`].
//!
//! Semantics follow GitHub Actions where it matters: `&&`/`||` short-circuit and return an
//! operand, truthiness treats `null`, `false`, `0`, `NaN` and `''` as false, and `==`
//! between different types compares them as numbers. Unlike GitHub, **string comparison and
//! the string functions are case-sensitive** (Git refs are case-sensitive). Missing context
//! values (for example an unset `vars.X`) evaluate to `null`; an unknown root name (a typo
//! such as `branhc`) is a parse error.
//!
//! Limits: at most [`MAX_EXPR_LEN`] bytes and [`MAX_DEPTH`] levels of nesting. Parsing and
//! evaluation never panic.

mod eval;
mod lexer;
mod parser;

use std::collections::BTreeMap;
use std::fmt;

pub use eval::Value;

use crate::pattern::Pattern;
use crate::trigger::Event;

/// Maximum expression length in bytes (including any `${{ }}` wrapper).
pub const MAX_EXPR_LEN: usize = 4096;
/// Maximum nesting depth of the expression tree.
pub const MAX_DEPTH: usize = 64;

/// An invalid expression.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("at column {column}: {message}")]
pub struct ExprError {
    /// 1-based character column in the `if:` text as written.
    pub column: usize,
    /// What is wrong.
    pub message: String,
}

impl ExprError {
    pub(crate) fn new(offset: usize, message: impl Into<String>) -> Self {
        Self {
            column: offset + 1,
            message: message.into(),
        }
    }
}

/// A context value an expression can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Path {
    Event,
    Ref,
    Branch,
    Tag,
    Sha,
    Repo,
    Var(String),
    Matrix(String),
}

/// A built-in function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Func {
    StartsWith,
    EndsWith,
    Contains,
    Matches,
}

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Eq,
    Ne,
    And,
    Or,
}

/// Expression syntax tree.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ast {
    Literal(Value),
    Path(Path),
    Not(Box<Ast>),
    Binary(BinOp, Box<Ast>, Box<Ast>),
    /// A function call; for `matches` with a literal glob the compiled pattern is cached.
    Call(Func, Vec<Ast>, Option<Pattern>),
}

/// A parsed `if:` expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Expression {
    source: String,
    ast: Ast,
}

impl Expression {
    /// Parse an expression, bare or wrapped in `${{ … }}`.
    pub fn parse(source: &str) -> Result<Self, ExprError> {
        if source.len() > MAX_EXPR_LEN {
            return Err(ExprError::new(
                0,
                format!("expression is longer than {MAX_EXPR_LEN} bytes"),
            ));
        }
        let (inner, offset) = strip_wrapper(source)?;
        let shift = source.get(..offset).map_or(0, |p| p.chars().count());
        let ast = parser::parse(inner).map_err(|mut e| {
            e.column += shift;
            e
        })?;
        Ok(Self {
            source: source.to_string(),
            ast,
        })
    }

    /// The expression as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Evaluate to a value.
    pub fn evaluate(&self, ctx: &Context) -> Value {
        eval::eval(&self.ast, ctx)
    }

    /// Evaluate and apply truthiness.
    pub fn is_true(&self, ctx: &Context) -> bool {
        self.evaluate(ctx).is_truthy()
    }
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// The expression text and its byte offset in `source`.
fn strip_wrapper(source: &str) -> Result<(&str, usize), ExprError> {
    let lead = source.len() - source.trim_start().len();
    let trimmed = source.trim();
    if let Some(rest) = trimmed.strip_prefix("${{") {
        let Some(inner) = rest.strip_suffix("}}") else {
            return Err(ExprError::new(
                source.len(),
                "`${{` is not closed with `}}`",
            ));
        };
        if inner.contains("${{") {
            return Err(ExprError::new(
                0,
                "only one `${{ … }}` expression is allowed in `if:`",
            ));
        }
        Ok((inner, lead + 3))
    } else if trimmed.contains("${{") {
        Err(ExprError::new(
            0,
            "text around `${{ … }}` is not allowed in `if:`; write the whole condition as one expression",
        ))
    } else {
        Ok((trimmed, lead))
    }
}

/// The values an expression is evaluated against.
///
/// Every field is optional: unset values read as `null`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    /// Event name: `push`, `pull_request`, `schedule` or `manual`.
    pub event: Option<String>,
    /// Full ref, e.g. `refs/heads/main`.
    pub ref_name: Option<String>,
    /// Branch name without `refs/heads/`.
    pub branch: Option<String>,
    /// Tag name without `refs/tags/`.
    pub tag: Option<String>,
    /// Commit SHA.
    pub sha: Option<String>,
    /// Repository, e.g. `owner/name`.
    pub repo: Option<String>,
    /// `vars.NAME` values (pipeline `variables` plus any server-side variables).
    pub vars: BTreeMap<String, String>,
    /// `matrix.NAME` values of the current matrix job.
    pub matrix: BTreeMap<String, String>,
}

impl Context {
    /// A context filled from a trigger event: `event`, and for pushes `ref`, `branch` or
    /// `tag`. For pull requests `branch` is the **base** branch. `sha`, `repo` and `vars`
    /// are left for the caller.
    pub fn for_event(event: &Event) -> Self {
        let mut ctx = Context {
            event: Some(event.name().to_string()),
            ..Context::default()
        };
        match event {
            Event::Push { ref_name, .. } => {
                ctx.ref_name = Some(ref_name.clone());
                if let Some(b) = ref_name.strip_prefix("refs/heads/") {
                    ctx.branch = Some(b.to_string());
                } else if let Some(t) = ref_name.strip_prefix("refs/tags/") {
                    ctx.tag = Some(t.to_string());
                }
            }
            Event::PullRequest { base_branch, .. } => {
                let b = base_branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(base_branch);
                ctx.branch = Some(b.to_string());
            }
            Event::Schedule | Event::Manual => {}
        }
        ctx
    }

    /// Add `vars.NAME` values (existing names are overwritten).
    pub fn with_vars(mut self, vars: &BTreeMap<String, String>) -> Self {
        self.vars
            .extend(vars.iter().map(|(k, v)| (k.clone(), v.clone())));
        self
    }

    /// Set the `matrix.NAME` values.
    pub fn with_matrix<'a>(
        mut self,
        matrix: impl IntoIterator<Item = &'a (String, String)>,
    ) -> Self {
        self.matrix = matrix
            .into_iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self
    }
}

#[cfg(test)]
mod tests;
