// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Pratt parser for the expression language.

use super::lexer::{Tok, Token, tokenize};
use super::{Ast, BinOp, ExprError, Func, MAX_DEPTH, Path, Value};
use crate::pattern::Pattern;

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}

pub(crate) fn parse(src: &str) -> Result<Ast, ExprError> {
    let tokens = tokenize(src)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        depth: 0,
    };
    if p.peek().tok == Tok::Eof {
        return Err(ExprError::new(0, "expression is empty"));
    }
    let (ast, _) = p.expr(0)?;
    let t = p.peek();
    if t.tok != Tok::Eof {
        return Err(ExprError::new(
            t.offset,
            format!("unexpected {}", describe(&t.tok)),
        ));
    }
    Ok(ast)
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Ident(s) => format!("`{s}`"),
        Tok::Str(_) => "string".into(),
        Tok::Num(_) => "number".into(),
        Tok::LParen => "`(`".into(),
        Tok::RParen => "`)`".into(),
        Tok::Comma => "`,`".into(),
        Tok::Dot => "`.`".into(),
        Tok::Not => "`!`".into(),
        Tok::Eq => "`==`".into(),
        Tok::Ne => "`!=`".into(),
        Tok::And => "`&&`".into(),
        Tok::Or => "`||`".into(),
        Tok::Eof => "end of expression".into(),
    }
}

/// Left/right binding powers of infix operators.
fn infix(tok: &Tok) -> Option<(BinOp, u8, u8)> {
    match tok {
        Tok::Or => Some((BinOp::Or, 1, 2)),
        Tok::And => Some((BinOp::And, 3, 4)),
        Tok::Eq => Some((BinOp::Eq, 5, 6)),
        Tok::Ne => Some((BinOp::Ne, 5, 6)),
        _ => None,
    }
}

const PREFIX_BP: u8 = 7;

impl Parser {
    fn peek(&self) -> Token {
        self.tokens.get(self.pos).cloned().unwrap_or(Token {
            tok: Tok::Eof,
            offset: 0,
        })
    }

    fn bump(&mut self) -> Token {
        let t = self.peek();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn too_deep(offset: usize) -> ExprError {
        ExprError::new(
            offset,
            format!("expression is nested too deeply (more than {MAX_DEPTH} levels)"),
        )
    }

    /// Parse an expression; returns the tree and its depth.
    fn expr(&mut self, min_bp: u8) -> Result<(Ast, usize), ExprError> {
        let start = self.peek().offset;
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(Self::too_deep(start));
        }
        let (mut lhs, mut lhs_depth) = self.prefix()?;
        while let Some((op, lbp, rbp)) = infix(&self.peek().tok) {
            if lbp < min_bp {
                break;
            }
            let op_tok = self.bump();
            let (rhs, rhs_depth) = self.expr(rbp)?;
            let depth = lhs_depth.max(rhs_depth) + 1;
            if depth > MAX_DEPTH {
                return Err(Self::too_deep(op_tok.offset));
            }
            lhs = Ast::Binary(op, Box::new(lhs), Box::new(rhs));
            lhs_depth = depth;
        }
        self.depth -= 1;
        Ok((lhs, lhs_depth))
    }

    fn prefix(&mut self) -> Result<(Ast, usize), ExprError> {
        let t = self.bump();
        match t.tok {
            Tok::Not => {
                let (inner, d) = self.expr(PREFIX_BP)?;
                if d + 1 > MAX_DEPTH {
                    return Err(Self::too_deep(t.offset));
                }
                Ok((Ast::Not(Box::new(inner)), d + 1))
            }
            Tok::LParen => {
                let (inner, d) = self.expr(0)?;
                let close = self.bump();
                if close.tok != Tok::RParen {
                    return Err(ExprError::new(
                        close.offset,
                        format!("expected `)`, found {}", describe(&close.tok)),
                    ));
                }
                Ok((inner, d))
            }
            Tok::Str(s) => Ok((Ast::Literal(Value::String(s)), 1)),
            Tok::Num(n) => Ok((Ast::Literal(Value::Number(n)), 1)),
            Tok::Ident(name) => self.ident(name, t.offset),
            other => Err(ExprError::new(
                t.offset,
                format!("expected a value, found {}", describe(&other)),
            )),
        }
    }

    fn ident(&mut self, name: String, offset: usize) -> Result<(Ast, usize), ExprError> {
        match name.as_str() {
            "true" => return Ok((Ast::Literal(Value::Bool(true)), 1)),
            "false" => return Ok((Ast::Literal(Value::Bool(false)), 1)),
            "null" => return Ok((Ast::Literal(Value::Null), 1)),
            _ => {}
        }
        if self.peek().tok == Tok::LParen {
            return self.call(&name, offset);
        }
        let path = match name.as_str() {
            "event" => Path::Event,
            "ref" => Path::Ref,
            "branch" => Path::Branch,
            "tag" => Path::Tag,
            "sha" => Path::Sha,
            "repo" => Path::Repo,
            "vars" | "matrix" => {
                let dot = self.bump();
                if dot.tok != Tok::Dot {
                    return Err(ExprError::new(
                        dot.offset,
                        format!("expected `.NAME` after `{name}`"),
                    ));
                }
                let key = self.bump();
                let Tok::Ident(key_name) = key.tok else {
                    return Err(ExprError::new(
                        key.offset,
                        format!("expected a name after `{name}.`"),
                    ));
                };
                if name == "vars" {
                    Path::Var(key_name)
                } else {
                    Path::Matrix(key_name)
                }
            }
            _ => {
                return Err(ExprError::new(
                    offset,
                    format!(
                        "unknown name `{name}`; expected event, ref, branch, tag, sha, repo, vars.NAME, matrix.NAME, true, false or null"
                    ),
                ));
            }
        };
        if self.peek().tok == Tok::Dot {
            return Err(ExprError::new(
                self.peek().offset,
                format!("`{name}` has no fields"),
            ));
        }
        Ok((Ast::Path(path), 1))
    }

    fn call(&mut self, name: &str, offset: usize) -> Result<(Ast, usize), ExprError> {
        let func = match name.to_ascii_lowercase().as_str() {
            "startswith" => Func::StartsWith,
            "endswith" => Func::EndsWith,
            "contains" => Func::Contains,
            "matches" => Func::Matches,
            _ => {
                return Err(ExprError::new(
                    offset,
                    format!(
                        "unknown function `{name}`; expected startsWith, endsWith, contains or matches"
                    ),
                ));
            }
        };
        self.bump(); // `(`
        let mut args = Vec::new();
        let mut depth = 1usize;
        if self.peek().tok != Tok::RParen {
            loop {
                let (arg, d) = self.expr(0)?;
                depth = depth.max(d + 1);
                args.push(arg);
                let t = self.bump();
                match t.tok {
                    Tok::Comma => continue,
                    Tok::RParen => break,
                    other => {
                        return Err(ExprError::new(
                            t.offset,
                            format!("expected `,` or `)`, found {}", describe(&other)),
                        ));
                    }
                }
            }
        } else {
            self.bump();
        }
        if depth > MAX_DEPTH {
            return Err(Self::too_deep(offset));
        }
        if args.len() != 2 {
            return Err(ExprError::new(
                offset,
                format!("`{name}` takes 2 arguments, got {}", args.len()),
            ));
        }
        let mut cached = None;
        if func == Func::Matches
            && let Some(Ast::Literal(Value::String(glob))) = args.get(1)
        {
            cached = Some(Pattern::new(glob).map_err(|e| ExprError::new(offset, e.to_string()))?);
        }
        Ok((Ast::Call(func, args, cached), depth))
    }
}
