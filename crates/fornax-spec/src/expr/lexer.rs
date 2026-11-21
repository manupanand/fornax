// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Tokenizer for the expression language.

use super::ExprError;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Ident(String),
    Str(String),
    Num(f64),
    LParen,
    RParen,
    Comma,
    Dot,
    Not,
    Eq,
    Ne,
    And,
    Or,
    Eof,
}

/// A token with its character offset.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub(crate) tok: Tok,
    pub(crate) offset: usize,
}

pub(crate) fn tokenize(src: &str) -> Result<Vec<Token>, ExprError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        let tok = match c {
            ' ' | '\t' | '\n' | '\r' => {
                i += 1;
                continue;
            }
            '(' => {
                i += 1;
                Tok::LParen
            }
            ')' => {
                i += 1;
                Tok::RParen
            }
            ',' => {
                i += 1;
                Tok::Comma
            }
            '.' if !chars.get(i + 1).is_some_and(char::is_ascii_digit) => {
                i += 1;
                Tok::Dot
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    i += 2;
                    Tok::Ne
                } else {
                    i += 1;
                    Tok::Not
                }
            }
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    i += 2;
                    Tok::Eq
                } else {
                    return Err(ExprError::new(i, "unexpected `=`; use `==` to compare"));
                }
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    i += 2;
                    Tok::And
                } else {
                    return Err(ExprError::new(i, "unexpected `&`; use `&&`"));
                }
            }
            '|' => {
                if chars.get(i + 1) == Some(&'|') {
                    i += 2;
                    Tok::Or
                } else {
                    return Err(ExprError::new(i, "unexpected `|`; use `||`"));
                }
            }
            '\'' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(ExprError::new(start, "unterminated string")),
                        Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                            s.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                Tok::Str(s)
            }
            '"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err(ExprError::new(start, "unterminated string")),
                        Some('"') => {
                            i += 1;
                            break;
                        }
                        Some('\\') => {
                            let esc = match chars.get(i + 1) {
                                Some('"') => '"',
                                Some('\\') => '\\',
                                Some('\'') => '\'',
                                Some('n') => '\n',
                                Some('t') => '\t',
                                Some('r') => '\r',
                                Some(other) => {
                                    return Err(ExprError::new(
                                        i,
                                        format!("unknown escape `\\{other}`"),
                                    ));
                                }
                                None => {
                                    return Err(ExprError::new(start, "unterminated string"));
                                }
                            };
                            s.push(esc);
                            i += 2;
                        }
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                Tok::Str(s)
            }
            c if c.is_ascii_digit() || c == '-' || c == '.' => {
                let mut j = i;
                if chars[j] == '-' {
                    j += 1;
                }
                while j < chars.len()
                    && (chars[j].is_ascii_digit()
                        || chars[j] == '.'
                        || chars[j] == 'e'
                        || chars[j] == 'E'
                        || ((chars[j] == '+' || chars[j] == '-')
                            && matches!(chars.get(j.wrapping_sub(1)), Some('e' | 'E'))))
                {
                    j += 1;
                }
                let text: String = chars[i..j].iter().collect();
                let value: f64 = text
                    .parse()
                    .map_err(|_| ExprError::new(i, format!("invalid number `{text}`")))?;
                if !value.is_finite() {
                    return Err(ExprError::new(
                        i,
                        format!("number `{text}` is out of range"),
                    ));
                }
                i = j;
                Tok::Num(value)
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut j = i;
                while j < chars.len()
                    && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '-')
                {
                    j += 1;
                }
                let text: String = chars[i..j].iter().collect();
                i = j;
                Tok::Ident(text)
            }
            other => {
                return Err(ExprError::new(i, format!("unexpected character `{other}`")));
            }
        };
        out.push(Token { tok, offset: start });
    }
    out.push(Token {
        tok: Tok::Eof,
        offset: chars.len(),
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        tokenize(src)
            .expect("tokenize")
            .into_iter()
            .map(|t| t.tok)
            .collect()
    }

    #[test]
    fn operators_and_literals() {
        assert_eq!(
            toks("ref == 'a''b' && !x || \"q\\\"\" != -1.5e2"),
            vec![
                Tok::Ident("ref".into()),
                Tok::Eq,
                Tok::Str("a'b".into()),
                Tok::And,
                Tok::Not,
                Tok::Ident("x".into()),
                Tok::Or,
                Tok::Str("q\"".into()),
                Tok::Ne,
                Tok::Num(-150.0),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn dotted_path() {
        assert_eq!(
            toks("vars.MY_VAR"),
            vec![
                Tok::Ident("vars".into()),
                Tok::Dot,
                Tok::Ident("MY_VAR".into()),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn errors() {
        for bad in [
            "'abc", "a = b", "a & b", "a | b", "\"\\q\"", "#", "1.2.3", "1e999",
        ] {
            assert!(tokenize(bad).is_err(), "{bad:?}");
        }
    }
}
