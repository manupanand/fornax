// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! GitHub-Actions-style filter patterns for branches, tags and paths (C-009, unit U2).
//!
//! Syntax (reference behaviour: Gitea `modules/actions/workflowpattern`, GitHub docs):
//!
//! | Token | Meaning |
//! |---|---|
//! | `*` | zero or more characters except `/` |
//! | `**` | zero or more of any character, including `/` |
//! | `**/` | at the start of a pattern or after `/`: zero or more whole directories, so `**/docs` matches `docs` and `a/b/docs` (but not `xdocs`) |
//! | `?` | zero or one of the preceding character (or class/group) |
//! | `+` | one or more of the preceding character (or class/group) |
//! | `[abc]`, `[a-z]`, `[!a-z]`, `[^a-z]` | character class (negated with `!` or `^`) |
//! | `{a,b}` | alternatives |
//! | `\x` | the literal character `x` |
//! | leading `!` | negation (only meaningful inside a [`PatternList`]) |
//!
//! Matching is anchored (the whole input must match) and works on Unicode characters.
//! It is implemented as a Thompson NFA, so matching time is linear in
//! `pattern length × input length` — no exponential backtracking.
//!
//! **Order-sensitive lists:** in a [`PatternList`] the *last* pattern that matches an input
//! decides: a positive pattern includes it, a `!` pattern excludes it, and an input no
//! pattern matches is not included.

use std::fmt;

/// Maximum pattern length in characters.
pub const MAX_PATTERN_LEN: usize = 512;
/// Maximum number of patterns in one [`PatternList`] (one `branches:`/`paths:`/… list).
///
/// Together with [`MAX_PATTERN_LEN`] and [`crate::trigger::MAX_FILTER_PATHS`] this bounds
/// the work of evaluating one path filter (paths × path length × patterns × NFA states).
pub const MAX_PATTERNS_PER_LIST: usize = 100;

/// An invalid pattern.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid pattern `{pattern}`: {message}")]
pub struct PatternError {
    /// The pattern as written.
    pub pattern: String,
    /// What is wrong with it.
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Class {
    negated: bool,
    items: Vec<ClassItem>,
}

impl Class {
    fn matches(&self, c: char) -> bool {
        let hit = self.items.iter().any(|item| match *item {
            ClassItem::Char(x) => x == c,
            ClassItem::Range(a, b) => a <= c && c <= b,
        });
        hit != self.negated
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Atom {
    Char(char),
    Class(Class),
    /// `*`
    Star,
    /// `**`
    DoubleStar,
    /// `**/` at a directory boundary: `(.*/)?`
    DirStar,
    Group(Vec<Vec<Piece>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quant {
    One,
    Optional,
    OneOrMore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Piece {
    atom: Atom,
    quant: Quant,
}

struct PatternParser<'a> {
    chars: &'a [char],
    pos: usize,
    source: &'a str,
}

impl PatternParser<'_> {
    fn err(&self, message: impl Into<String>) -> PatternError {
        PatternError {
            pattern: self.source.to_string(),
            message: message.into(),
        }
    }

    /// Parse alternatives until the end (or a closing `}` when `in_group`).
    fn parse_alternatives(&mut self, in_group: bool) -> Result<Vec<Vec<Piece>>, PatternError> {
        let mut alts: Vec<Vec<Piece>> = vec![Vec::new()];
        while let Some(&c) = self.chars.get(self.pos) {
            self.pos += 1;
            let current = alts.len() - 1;
            let seq = &mut alts[current];
            match c {
                '}' if in_group => return Ok(alts),
                ',' if in_group => alts.push(Vec::new()),
                '*' => {
                    if self.chars.get(self.pos) == Some(&'*') {
                        let star_start = self.pos - 1;
                        self.pos += 1;
                        let at_boundary =
                            star_start == 0 || self.chars.get(star_start - 1) == Some(&'/');
                        if at_boundary && self.chars.get(self.pos) == Some(&'/') {
                            self.pos += 1;
                            seq.push(Piece {
                                atom: Atom::DirStar,
                                quant: Quant::One,
                            });
                        } else {
                            seq.push(Piece {
                                atom: Atom::DoubleStar,
                                quant: Quant::One,
                            });
                        }
                    } else {
                        seq.push(Piece {
                            atom: Atom::Star,
                            quant: Quant::One,
                        });
                    }
                }
                '?' | '+' => {
                    let quant = if c == '?' {
                        Quant::Optional
                    } else {
                        Quant::OneOrMore
                    };
                    match seq.last_mut() {
                        Some(last)
                            if last.quant == Quant::One
                                && !matches!(
                                    last.atom,
                                    Atom::Star | Atom::DoubleStar | Atom::DirStar
                                ) =>
                        {
                            last.quant = quant;
                        }
                        _ => {
                            return Err(self.err(format!(
                                "`{c}` must follow a character, class or group (escape it as `\\{c}` to match it literally)"
                            )));
                        }
                    }
                }
                '[' => {
                    let class = self.parse_class()?;
                    seq.push(Piece {
                        atom: Atom::Class(class),
                        quant: Quant::One,
                    });
                }
                '{' => {
                    let group = self.parse_alternatives(true)?;
                    let current = alts.len() - 1;
                    alts[current].push(Piece {
                        atom: Atom::Group(group),
                        quant: Quant::One,
                    });
                }
                '\\' => {
                    let Some(&escaped) = self.chars.get(self.pos) else {
                        return Err(self.err("trailing `\\` has nothing to escape"));
                    };
                    self.pos += 1;
                    seq.push(Piece {
                        atom: Atom::Char(escaped),
                        quant: Quant::One,
                    });
                }
                other => seq.push(Piece {
                    atom: Atom::Char(other),
                    quant: Quant::One,
                }),
            }
        }
        if in_group {
            Err(self.err("unterminated `{` group"))
        } else {
            Ok(alts)
        }
    }

    fn parse_class(&mut self) -> Result<Class, PatternError> {
        let mut negated = false;
        if matches!(self.chars.get(self.pos), Some('!' | '^')) {
            negated = true;
            self.pos += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let Some(&c) = self.chars.get(self.pos) else {
                return Err(self.err("unterminated `[` character class"));
            };
            self.pos += 1;
            let lo = match c {
                ']' if !first => return Ok(Class { negated, items }),
                '\\' => {
                    let Some(&e) = self.chars.get(self.pos) else {
                        return Err(self.err("unterminated `[` character class"));
                    };
                    self.pos += 1;
                    e
                }
                other => other,
            };
            first = false;
            // Range `a-z` (a `-` right before `]` is literal).
            if self.chars.get(self.pos) == Some(&'-')
                && self.chars.get(self.pos + 1).is_some_and(|&n| n != ']')
            {
                self.pos += 1;
                let Some(&hi_raw) = self.chars.get(self.pos) else {
                    return Err(self.err("unterminated `[` character class"));
                };
                self.pos += 1;
                let hi = if hi_raw == '\\' {
                    let Some(&e) = self.chars.get(self.pos) else {
                        return Err(self.err("unterminated `[` character class"));
                    };
                    self.pos += 1;
                    e
                } else {
                    hi_raw
                };
                if lo > hi {
                    return Err(self.err(format!("character range `{lo}-{hi}` is reversed")));
                }
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Char(lo));
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Char(char, usize),
    Class(Class, usize),
    /// Any character except `/`.
    NonSep(usize),
    /// Any character.
    Any(usize),
    Split(usize, usize),
    Match,
}

/// Placeholder target while building loops.
const HOLE: usize = usize::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Program {
    states: Vec<State>,
    start: usize,
}

impl Program {
    fn compile(alts: &[Vec<Piece>]) -> Self {
        let mut p = Program {
            states: vec![State::Match],
            start: 0,
        };
        p.start = p.compile_alts(alts, 0);
        p
    }

    fn push(&mut self, s: State) -> usize {
        self.states.push(s);
        self.states.len() - 1
    }

    fn compile_alts(&mut self, alts: &[Vec<Piece>], next: usize) -> usize {
        let mut starts: Vec<usize> = alts.iter().map(|seq| self.compile_seq(seq, next)).collect();
        let mut cur = starts.pop().unwrap_or(next);
        while let Some(s) = starts.pop() {
            cur = self.push(State::Split(s, cur));
        }
        cur
    }

    fn compile_seq(&mut self, seq: &[Piece], next: usize) -> usize {
        let mut cur = next;
        for piece in seq.iter().rev() {
            cur = self.compile_piece(piece, cur);
        }
        cur
    }

    fn compile_piece(&mut self, piece: &Piece, next: usize) -> usize {
        match piece.quant {
            Quant::One => self.compile_atom(&piece.atom, next),
            Quant::Optional => {
                let a = self.compile_atom(&piece.atom, next);
                self.push(State::Split(a, next))
            }
            Quant::OneOrMore => {
                let s = self.push(State::Split(HOLE, next));
                let a = self.compile_atom(&piece.atom, s);
                self.states[s] = State::Split(a, next);
                a
            }
        }
    }

    fn star_loop(&mut self, any: bool, next: usize) -> usize {
        let s = self.push(State::Split(HOLE, next));
        let a = self.push(if any { State::Any(s) } else { State::NonSep(s) });
        self.states[s] = State::Split(a, next);
        s
    }

    fn compile_atom(&mut self, atom: &Atom, next: usize) -> usize {
        match atom {
            Atom::Char(c) => self.push(State::Char(*c, next)),
            Atom::Class(class) => self.push(State::Class(class.clone(), next)),
            Atom::Star => self.star_loop(false, next),
            Atom::DoubleStar => self.star_loop(true, next),
            Atom::DirStar => {
                let slash = self.push(State::Char('/', next));
                let body = self.star_loop(true, slash);
                self.push(State::Split(body, next))
            }
            Atom::Group(alts) => self.compile_alts(alts, next),
        }
    }

    /// Add `s` and everything reachable through `Split`s to `set`.
    fn add(&self, set: &mut Vec<usize>, seen: &mut [bool], s: usize) {
        let mut stack = vec![s];
        while let Some(s) = stack.pop() {
            if s >= self.states.len() || seen[s] {
                continue;
            }
            seen[s] = true;
            if let State::Split(a, b) = self.states[s] {
                stack.push(b);
                stack.push(a);
            } else {
                set.push(s);
            }
        }
    }

    fn matches(&self, input: &str) -> bool {
        let n = self.states.len();
        let mut seen = vec![false; n];
        let mut current = Vec::new();
        self.add(&mut current, &mut seen, self.start);
        for c in input.chars() {
            if current.is_empty() {
                return false;
            }
            let mut next = Vec::new();
            seen.iter_mut().for_each(|x| *x = false);
            for &s in &current {
                let target = match &self.states[s] {
                    State::Char(x, t) if *x == c => Some(*t),
                    State::Class(class, t) if class.matches(c) => Some(*t),
                    State::NonSep(t) if c != '/' => Some(*t),
                    State::Any(t) => Some(*t),
                    _ => None,
                };
                if let Some(t) = target {
                    self.add(&mut next, &mut seen, t);
                }
            }
            current = next;
        }
        current.iter().any(|&s| self.states[s] == State::Match)
    }
}

/// One compiled filter pattern.
#[derive(Clone, PartialEq, Eq)]
pub struct Pattern {
    source: String,
    negated: bool,
    program: Program,
}

impl fmt::Debug for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Pattern").field(&self.source).finish()
    }
}

impl Pattern {
    /// Compile a pattern. A leading `!` marks it as negated (see [`PatternList`]).
    pub fn new(source: &str) -> Result<Self, PatternError> {
        let chars: Vec<char> = source.chars().collect();
        if chars.is_empty() {
            return Err(PatternError {
                pattern: String::new(),
                message: "pattern is empty".into(),
            });
        }
        if chars.len() > MAX_PATTERN_LEN {
            return Err(PatternError {
                pattern: source.chars().take(40).collect::<String>() + "…",
                message: format!("pattern is longer than {MAX_PATTERN_LEN} characters"),
            });
        }
        let negated = chars.first() == Some(&'!');
        let body = if negated { &chars[1..] } else { &chars[..] };
        if negated && body.is_empty() {
            return Err(PatternError {
                pattern: source.to_string(),
                message: "`!` must be followed by a pattern".into(),
            });
        }
        let mut parser = PatternParser {
            chars: body,
            pos: 0,
            source,
        };
        let alts = parser.parse_alternatives(false)?;
        Ok(Self {
            source: source.to_string(),
            negated,
            program: Program::compile(&alts),
        })
    }

    /// The pattern as written (including any leading `!`).
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// True when the pattern starts with `!`.
    pub fn is_negated(&self) -> bool {
        self.negated
    }

    /// Whether the pattern body (without the leading `!`) matches the whole input.
    pub fn matches(&self, input: &str) -> bool {
        self.program.matches(input)
    }
}

/// The error for a list longer than [`MAX_PATTERNS_PER_LIST`].
fn too_many_patterns(n: usize) -> PatternError {
    PatternError {
        pattern: String::new(),
        message: format!("{n} patterns; a list may have at most {MAX_PATTERNS_PER_LIST} patterns"),
    }
}

/// An ordered list of patterns with GitHub-Actions "last match wins" semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatternList {
    patterns: Vec<Pattern>,
}

impl PatternList {
    /// Compile a list of patterns, collecting every invalid one. More than
    /// [`MAX_PATTERNS_PER_LIST`] patterns is an error.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<Self, Vec<PatternError>> {
        if patterns.len() > MAX_PATTERNS_PER_LIST {
            return Err(vec![too_many_patterns(patterns.len())]);
        }
        let mut out = Vec::with_capacity(patterns.len());
        let mut errors = Vec::new();
        for p in patterns {
            match Pattern::new(p.as_ref()) {
                Ok(p) => out.push(p),
                Err(e) => errors.push(e),
            }
        }
        if errors.is_empty() {
            Ok(Self { patterns: out })
        } else {
            Err(errors)
        }
    }

    /// Build a list from already-compiled patterns (at most [`MAX_PATTERNS_PER_LIST`]).
    pub fn from_patterns(patterns: Vec<Pattern>) -> Result<Self, PatternError> {
        if patterns.len() > MAX_PATTERNS_PER_LIST {
            return Err(too_many_patterns(patterns.len()));
        }
        Ok(Self { patterns })
    }

    /// True when the list has no patterns (no filtering).
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The compiled patterns in order.
    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    /// The verdict of the last pattern that matches `input`: `Some(true)` for a positive
    /// pattern, `Some(false)` for a `!` pattern, `None` when no pattern matches.
    pub fn decide(&self, input: &str) -> Option<bool> {
        self.patterns
            .iter()
            .rev()
            .find(|p| p.matches(input))
            .map(|p| !p.negated)
    }

    /// Whether `input` is included: the last matching pattern is positive.
    pub fn includes(&self, input: &str) -> bool {
        self.decide(input) == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, input: &str) -> bool {
        Pattern::new(pattern).expect("valid pattern").matches(input)
    }

    fn list(patterns: &[&str]) -> PatternList {
        PatternList::new(patterns).expect("valid patterns")
    }

    #[test]
    fn star_does_not_cross_slash() {
        assert!(m("*", "main"));
        assert!(!m("*", "path/with/slash"));
        assert!(m("feature/*", "feature/my-branch"));
        assert!(!m("feature/*", "feature/a/b"));
        assert!(m("*feature", "mona-feature"));
        assert!(m("*feature", "feature"));
        assert!(m("v2*", "v2"));
        assert!(m("v2*", "v2.9"));
    }

    #[test]
    fn double_star_crosses_slash() {
        assert!(m("**", "all/the/branches"));
        assert!(m("feature/**", "feature/mona/the/octocat"));
        assert!(!m("feature/**", "feature"));
        assert!(m("**.js", "src/js/app.js"));
        assert!(m("**.js", "index.js"));
        assert!(m("docs/**/*.md", "docs/README.md"));
        assert!(m("docs/**/*.md", "docs/a/markdown/file.md"));
    }

    #[test]
    fn leading_double_star_slash_matches_zero_dirs() {
        assert!(m("**/docs/**", "docs/hello.md"));
        assert!(m("**/docs/**", "space/docs/plan/space.doc"));
        assert!(m("**/README.md", "README.md"));
        assert!(m("**/README.md", "js/README.md"));
        assert!(!m("**/README.md", "fooREADME.md"));
        assert!(m("**/*src/**", "my-src/code/js/app.js"));
        assert!(m("**/*-post.md", "my-post.md"));
        assert!(m("**/migrate-*.sql", "db/sept/migrate-v1.sql"));
        assert!(m("a/**/b", "a/b"));
        assert!(m("a/**/b", "a/x/y/b"));
    }

    #[test]
    fn question_and_plus_apply_to_previous() {
        assert!(m("*.jsx?", "page.js"));
        assert!(m("*.jsx?", "page.jsx"));
        assert!(!m("*.jsx?", "page.jsxx"));
        assert!(m("v[12].[0-9]+.[0-9]+", "v1.10.1"));
        assert!(m("v[12].[0-9]+.[0-9]+", "v2.0.0"));
        assert!(!m("v[12].[0-9]+.[0-9]+", "v3.0.0"));
        assert!(!m("v[12].[0-9]+.[0-9]+", "v1..0"));
        assert!(m("ab+c", "abbbc"));
        assert!(!m("ab+c", "ac"));
    }

    #[test]
    fn literal_dot_and_escapes() {
        assert!(!m(".yml", "fyml"));
        assert!(m("\\!file", "!file"));
        assert!(m("escape\\\\backslash", "escape\\backslash"));
        assert!(m("a\\*b", "a*b"));
        assert!(!m("a\\*b", "axb"));
        assert!(m("1\\+2", "1+2"));
    }

    #[test]
    fn classes() {
        assert!(m("[abc]", "b"));
        assert!(!m("[abc]", "d"));
        assert!(m("[!abc]", "d"));
        assert!(m("[^abc]", "d"));
        assert!(m("[a-c-]", "-"));
        assert!(m("[]]", "]"));
        assert!(m("rel-[0-9][0-9]", "rel-42"));
    }

    #[test]
    fn groups() {
        assert!(m("{main,master}", "main"));
        assert!(m("{main,master}", "master"));
        assert!(!m("{main,master}", "mai"));
        assert!(m("release/{v1,v2}/*", "release/v2/x"));
        assert!(m("a,b", "a,b"));
        assert!(m("x{a,}+y", "xy"));
    }

    #[test]
    fn unicode() {
        assert!(m("fé*", "fé-ature"));
        assert!(m("é?", ""));
        assert!(m("é?", "é"));
        assert!(m("dö[a-zö]s", "döös"));
    }

    #[test]
    fn invalid_patterns() {
        for bad in [
            "", "!", "+a", "?", "*+", "**?", "[abc", "{a,b", "a\\", "[z-a]",
        ] {
            assert!(Pattern::new(bad).is_err(), "{bad:?} should be invalid");
        }
        let long = "a".repeat(MAX_PATTERN_LEN + 1);
        assert!(Pattern::new(&long).is_err());
    }

    #[test]
    fn last_match_wins() {
        let l = list(&["*.md", "!README.md"]);
        assert!(l.includes("hello.md"));
        assert!(!l.includes("README.md"));
        assert!(!l.includes("docs/hello.md"));
        let l = list(&["*.md", "!README.md", "README*"]);
        assert!(l.includes("hello.md"));
        assert!(l.includes("README.md"));
        assert!(l.includes("README.doc"));
        let l = list(&["!main", "*"]);
        assert!(l.includes("main"));
        assert_eq!(list(&[]).decide("x"), None);
    }

    #[test]
    fn gitea_reference_cases_for_lists() {
        // (patterns, input, included) — from Gitea workflow_pattern_test.go "skip" semantics.
        let cases: &[(&[&str], &str, bool)] = &[
            (&["path/a", "path/b", "path/c"], "path/b", true),
            (&["path/a", "path/b", "path/c"], "path/d", false),
            (
                &["main", "releases/mona-the-octocat"],
                "releases/mona-the-octocat",
                true,
            ),
            (&["docs/*"], "docs/file.txt", true),
            (&["**/docs/**"], "dir/docs/my-file.txt", true),
        ];
        for (patterns, input, expected) in cases {
            assert_eq!(
                list(patterns).includes(input),
                *expected,
                "{patterns:?} {input}"
            );
        }
    }

    #[test]
    fn pathological_pattern_is_fast() {
        let p = Pattern::new(&"a?".repeat(100)).expect("valid");
        let input = "a".repeat(100);
        assert!(p.matches(&input));
        let p = Pattern::new(&"*a".repeat(50)).expect("valid");
        assert!(!p.matches(&"a".repeat(49)));
    }
}
