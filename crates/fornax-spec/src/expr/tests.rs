// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

use super::*;

fn ctx() -> Context {
    let mut c = Context::for_event(&Event::Push {
        ref_name: "refs/heads/main".into(),
        changed_paths: None,
    });
    c.sha = Some("abc123".into());
    c.repo = Some("acme/app".into());
    c.vars.insert("DEPLOY".into(), "yes".into());
    c.vars.insert("EMPTY".into(), String::new());
    c.vars.insert("COUNT".into(), "3".into());
    c.matrix.insert("os".into(), "linux".into());
    c
}

fn t(src: &str) -> bool {
    Expression::parse(src)
        .unwrap_or_else(|e| panic!("{src:?}: {e}"))
        .is_true(&ctx())
}

fn v(src: &str) -> Value {
    Expression::parse(src)
        .unwrap_or_else(|e| panic!("{src:?}: {e}"))
        .evaluate(&ctx())
}

#[test]
fn wrapped_and_bare() {
    assert!(t("${{ ref == 'refs/heads/main' }}"));
    assert!(t("ref == 'refs/heads/main'"));
    assert!(t("  ${{branch == \"main\"}}  "));
    assert!(!t("${{ ref == 'refs/heads/dev' }}"));
}

#[test]
fn context_paths() {
    assert_eq!(v("event"), Value::String("push".into()));
    assert_eq!(v("branch"), Value::String("main".into()));
    assert_eq!(v("tag"), Value::Null);
    assert_eq!(v("sha"), Value::String("abc123".into()));
    assert_eq!(v("repo"), Value::String("acme/app".into()));
    assert_eq!(v("vars.DEPLOY"), Value::String("yes".into()));
    assert_eq!(v("vars.MISSING"), Value::Null);
    assert_eq!(v("matrix.os"), Value::String("linux".into()));
}

#[test]
fn operators_and_precedence() {
    assert!(t("true || false && false"));
    assert!(!t("(true || false) && false"));
    assert!(t("!false"));
    assert!(t("!!true"));
    assert!(t("!(branch == 'dev')"));
    assert!(t("branch != 'dev' && event == 'push'"));
    assert_eq!(
        v("vars.EMPTY || 'fallback'"),
        Value::String("fallback".into())
    );
    assert_eq!(v("vars.DEPLOY && 'x'"), Value::String("x".into()));
    assert_eq!(v("null && 'x'"), Value::Null);
}

#[test]
fn truthiness() {
    for falsy in [
        "''",
        "0",
        "null",
        "false",
        "vars.EMPTY",
        "vars.MISSING",
        "-0",
    ] {
        assert!(!t(falsy), "{falsy}");
    }
    for truthy in ["'a'", "1", "-1", "true", "'false'", "'0'", "0.5"] {
        assert!(t(truthy), "{truthy}");
    }
}

#[test]
fn loose_equality() {
    assert!(t("vars.COUNT == 3"));
    assert!(t("'3.0' == 3"));
    assert!(t("true == 1"));
    assert!(t("null == 0"));
    assert!(t("'' == 0"));
    assert!(!t("'abc' == 0"));
    assert!(t("null == null"));
    assert!(
        !t("'Main' == 'main'"),
        "string comparison is case-sensitive"
    );
    assert!(t("vars.MISSING == null"));
}

#[test]
fn functions() {
    assert!(t("startsWith(ref, 'refs/heads/')"));
    assert!(t("STARTSWITH(ref, 'refs/heads/')"));
    assert!(t("endsWith(repo, '/app')"));
    assert!(t("contains(ref, 'heads')"));
    assert!(!t("contains(ref, 'HEADS')"));
    assert!(t("matches(branch, 'ma*')"));
    assert!(t("matches(ref, 'refs/**')"));
    assert!(!t("matches(ref, 'refs/*')"));
    assert!(t("matches(branch, '!dev')"));
    assert!(t("matches(branch, vars.DEPLOY) == false"));
    assert!(t("contains(vars.COUNT, 3)"));
    assert!(t("startsWith(vars.MISSING, '')"));
}

#[test]
fn parse_errors() {
    let cases = [
        ("", "empty"),
        ("${{ }}", "empty"),
        ("${{ ref", "not closed"),
        ("x ${{ ref }}", "text around"),
        ("branhc == 'main'", "unknown name `branhc`"),
        ("vars", "expected `.NAME`"),
        ("vars.", "expected a name"),
        ("ref.x", "has no fields"),
        ("startsWith(ref)", "takes 2 arguments"),
        ("startsWith(ref, 'a', 'b')", "takes 2 arguments"),
        ("frob(ref, 'a')", "unknown function"),
        ("ref ==", "expected a value"),
        ("(ref == 'a'", "expected `)`"),
        ("ref 'a'", "unexpected string"),
        ("matches(ref, '[a')", "invalid pattern"),
        ("ref = 'a'", "use `==`"),
    ];
    for (src, want) in cases {
        let err = Expression::parse(src).expect_err(src).to_string();
        assert!(err.contains(want), "{src:?}: {err}");
    }
}

#[test]
fn error_columns() {
    let err = Expression::parse("ref == 'a' && branhc").expect_err("bad");
    assert_eq!(err.column, 15);
}

#[test]
fn limits() {
    let long = format!("'{}'", "a".repeat(MAX_EXPR_LEN));
    assert!(Expression::parse(&long).is_err());

    let deep = format!("{}true{}", "(".repeat(100), ")".repeat(100));
    let err = Expression::parse(&deep).expect_err("too deep").to_string();
    assert!(err.contains("nested too deeply"), "{err}");

    let nots = format!("{}true", "!".repeat(100));
    assert!(Expression::parse(&nots).is_err());

    let chain = vec!["true"; 100].join(" || ");
    assert!(Expression::parse(&chain).is_err());

    let ok_chain = vec!["true"; 20].join(" || ");
    assert!(Expression::parse(&ok_chain).is_ok());
    let ok_parens = format!("{}true{}", "(".repeat(30), ")".repeat(30));
    assert!(Expression::parse(&ok_parens).is_ok());
}

#[test]
fn pull_request_context_uses_base_branch() {
    let c = Context::for_event(&Event::PullRequest {
        base_branch: "refs/heads/main".into(),
        changed_paths: None,
    });
    assert_eq!(c.event.as_deref(), Some("pull_request"));
    assert_eq!(c.branch.as_deref(), Some("main"));
    assert_eq!(c.ref_name, None);
}

#[test]
fn number_display() {
    assert_eq!(Value::Number(3.0).to_string(), "3");
    assert_eq!(Value::Number(1.5).to_string(), "1.5");
    assert_eq!(Value::Null.to_string(), "");
}

/// Small deterministic PRNG (xorshift64*) so the property test needs no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn random_input_never_panics() {
    const PIECES: &[&str] = &[
        "ref",
        "branch",
        "tag",
        "event",
        "sha",
        "repo",
        "vars.",
        "vars.X",
        "matrix.os",
        "(",
        ")",
        "==",
        "!=",
        "&&",
        "||",
        "!",
        ",",
        ".",
        "'",
        "\"",
        "''",
        "'a'",
        "\"b\\\"\"",
        "1",
        "-2.5",
        "1e3",
        "true",
        "false",
        "null",
        "startsWith(",
        "endsWith(",
        "contains(",
        "matches(",
        "'**'",
        "'[a-'",
        "${{",
        "}}",
        " ",
        "\\",
        "\n",
        "é",
        "=",
        "&",
        "|",
        "-",
        "e",
        "{",
        "[",
    ];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let c = ctx();
    for _ in 0..20_000 {
        let n = rng.below(24);
        let mut s = String::new();
        for _ in 0..n {
            if rng.below(10) == 0 {
                // Arbitrary Unicode scalar.
                let cp = (rng.next() % 0x11_0000) as u32;
                s.push(char::from_u32(cp).unwrap_or('?'));
            } else {
                s.push_str(PIECES[rng.below(PIECES.len())]);
            }
        }
        if let Ok(e) = Expression::parse(&s) {
            let _ = e.evaluate(&c);
        }
    }
    // Mutations of a valid expression.
    let base = "${{ startsWith(ref, 'refs/heads/') && (branch == 'main' || matches(tag, 'v*')) }}";
    let chars: Vec<char> = base.chars().collect();
    for _ in 0..5_000 {
        let mut m = chars.clone();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(m.len());
            match rng.below(3) {
                0 => {
                    m.remove(i);
                }
                1 => m.insert(i, chars[rng.below(chars.len())]),
                _ => m[i] = chars[rng.below(chars.len())],
            }
            if m.is_empty() {
                break;
            }
        }
        let s: String = m.into_iter().collect();
        if let Ok(e) = Expression::parse(&s) {
            let _ = e.evaluate(&c);
        }
    }
}
