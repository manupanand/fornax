// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Interactive questions for the setup commands.
//!
//! Every question has an optional suggested value (from the existing configuration, else the
//! BLP example). `--yes` takes the suggestion without asking and fails when there is none.
//! Answers are validated and the question is asked again on bad input.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// Checks one answer; `Err` holds the reason shown before asking again.
pub type Validator<'a> = &'a dyn Fn(&str) -> std::result::Result<(), String>;

/// Something that asks the DevOps engineer questions.
///
/// Implementations provide [`Prompter::ask_raw`] / [`Prompter::confirm_raw`]; the provided
/// methods add suggestions, `--yes`, validation and re-asking.
pub trait Prompter {
    /// True when every question takes its suggestion (`--yes`).
    fn assume_yes(&self) -> bool;

    /// Ask once and return the raw answer (empty = take the suggestion).
    fn ask_raw(&self, question: &str, suggested: Option<&str>) -> Result<String>;

    /// Ask a yes/no question once.
    fn confirm_raw(&self, question: &str, default: bool) -> Result<bool>;

    /// Tell the engineer why an answer was rejected.
    fn warn(&self, message: &str) {
        eprintln!("  ! {message}");
    }

    /// A free-text answer, validated; empty input takes `suggested`.
    fn text(
        &self,
        question: &str,
        suggested: Option<&str>,
        validate: Validator<'_>,
    ) -> Result<String> {
        if self.assume_yes() {
            let Some(s) = suggested else {
                bail!("--yes: no suggested value for \"{question}\"; run without --yes");
            };
            validate(s).map_err(|e| {
                anyhow!("--yes: suggested value '{s}' for \"{question}\" is invalid: {e}")
            })?;
            println!("  {question}: {s}");
            return Ok(s.to_owned());
        }
        loop {
            let raw = self.ask_raw(question, suggested)?;
            let answer = match raw.trim() {
                "" => suggested.unwrap_or(""),
                a => a,
            };
            match validate(answer) {
                Ok(()) => return Ok(answer.to_owned()),
                Err(e) => self.warn(&format!("'{answer}': {e}; try again")),
            }
        }
    }

    /// A yes/no answer; `--yes` takes `default`.
    fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        if self.assume_yes() {
            println!("  {question} {}", if default { "yes" } else { "no" });
            return Ok(default);
        }
        self.confirm_raw(question, default)
    }

    /// An ordered list (comma- or space-separated, `none` = empty), each item validated.
    fn list(
        &self,
        question: &str,
        suggested: Vec<String>,
        validate_item: Validator<'_>,
    ) -> Result<Vec<String>> {
        let shown = if suggested.is_empty() {
            "none".to_owned()
        } else {
            suggested.join(", ")
        };
        let check = |answer: &str| -> std::result::Result<(), String> {
            for item in parse_list(answer)? {
                validate_item(&item).map_err(|e| format!("'{item}': {e}"))?;
            }
            Ok(())
        };
        let answer = self.text(
            &format!("{question} (in order, comma-separated; 'none' for an empty list)"),
            Some(&shown),
            &check,
        )?;
        parse_list(&answer).map_err(|e| anyhow!(e))
    }

    /// A path; when `must_be_absolute`, relative paths are rejected and asked again.
    fn path(
        &self,
        question: &str,
        suggested: Option<&Path>,
        must_be_absolute: bool,
    ) -> Result<PathBuf> {
        let suggested = suggested.map(|p| p.to_string_lossy().into_owned());
        let check = |answer: &str| -> std::result::Result<(), String> {
            if answer.is_empty() {
                Err("a path is required".into())
            } else if must_be_absolute && !Path::new(answer).is_absolute() {
                Err("must be an absolute path (starting with /)".into())
            } else {
                Ok(())
            }
        };
        self.text(question, suggested.as_deref(), &check)
            .map(PathBuf::from)
    }
}

/// Split a list answer; `none` (or `-`) is the empty list; duplicates are rejected.
pub fn parse_list(answer: &str) -> std::result::Result<Vec<String>, String> {
    let trimmed = answer.trim();
    if trimmed.eq_ignore_ascii_case("none") || trimmed == "-" {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for item in trimmed.split(|c: char| c == ',' || c.is_whitespace()) {
        if item.is_empty() {
            continue;
        }
        if out.iter().any(|o| o == item) {
            return Err(format!("'{item}' is listed twice"));
        }
        out.push(item.to_owned());
    }
    Ok(out)
}

/// Asks on the terminal with `dialoguer`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DialoguerPrompter {
    /// `--yes`: take every suggestion without asking.
    pub yes: bool,
}

impl Prompter for DialoguerPrompter {
    fn assume_yes(&self) -> bool {
        self.yes
    }

    fn ask_raw(&self, question: &str, suggested: Option<&str>) -> Result<String> {
        let mut input = dialoguer::Input::<String>::new()
            .with_prompt(question)
            .allow_empty(true);
        if let Some(s) = suggested {
            input = input.default(s.to_owned());
        }
        input
            .interact_text()
            .context("reading an answer from the terminal (use --yes when not interactive)")
    }

    fn confirm_raw(&self, question: &str, default: bool) -> Result<bool> {
        dialoguer::Confirm::new()
            .with_prompt(question)
            .default(default)
            .interact()
            .context("reading an answer from the terminal (use --yes when not interactive)")
    }
}

/// Answers questions from a fixed script (tests).
#[cfg(test)]
#[derive(Debug, Default)]
pub struct ScriptedPrompter {
    answers: std::cell::RefCell<std::collections::VecDeque<String>>,
    yes: bool,
    /// Every question asked, in order.
    pub asked: std::cell::RefCell<Vec<String>>,
}

#[cfg(test)]
impl ScriptedPrompter {
    /// Answers in order; `""` takes the suggestion; `y`/`n` for confirmations.
    pub fn new(answers: &[&str]) -> Self {
        Self {
            answers: std::cell::RefCell::new(answers.iter().map(|a| (*a).to_owned()).collect()),
            ..Self::default()
        }
    }

    /// Behaves like `--yes`.
    pub fn yes() -> Self {
        Self {
            yes: true,
            ..Self::default()
        }
    }

    /// Answers not used yet.
    pub fn remaining(&self) -> usize {
        self.answers.borrow().len()
    }

    fn next(&self, question: &str) -> Result<String> {
        self.asked.borrow_mut().push(question.to_owned());
        self.answers
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| anyhow!("no scripted answer left for \"{question}\""))
    }
}

#[cfg(test)]
impl Prompter for ScriptedPrompter {
    fn assume_yes(&self) -> bool {
        self.yes
    }

    fn ask_raw(&self, question: &str, _suggested: Option<&str>) -> Result<String> {
        self.next(question)
    }

    fn confirm_raw(&self, question: &str, default: bool) -> Result<bool> {
        match self.next(question)?.as_str() {
            "" => Ok(default),
            "y" | "yes" => Ok(true),
            "n" | "no" => Ok(false),
            other => bail!("scripted confirm answer must be y/n/empty, got '{other}'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn name_ok(s: &str) -> std::result::Result<(), String> {
        if config::is_valid_name(s) {
            Ok(())
        } else {
            Err("must match ^[a-z0-9][a-z0-9-]*$".into())
        }
    }

    #[test]
    fn yes_takes_suggestions_and_fails_without_one() {
        let p = ScriptedPrompter::yes();
        assert_eq!(
            p.text("Repo", Some("srf-blp-ui"), &name_ok).expect("text"),
            "srf-blp-ui"
        );
        assert!(p.confirm("Use AR?", true).expect("confirm"));
        assert!(!p.confirm("Shared?", false).expect("confirm"));
        assert_eq!(
            p.list("Repos", vec!["a".into(), "b".into()], &name_ok)
                .expect("list"),
            ["a", "b"]
        );
        assert_eq!(
            p.path("Key", Some(Path::new("/k.json")), true)
                .expect("path"),
            PathBuf::from("/k.json")
        );
        let e = p.path("Key", None, true).expect_err("no suggestion");
        assert!(e.to_string().contains("no suggested value"), "{e}");
        let e = p
            .text("Repo", Some("Bad_Name"), &name_ok)
            .expect_err("invalid suggestion");
        assert!(e.to_string().contains("invalid"), "{e}");
        assert!(p.asked.borrow().is_empty(), "--yes never asks");
    }

    #[test]
    fn invalid_answers_are_asked_again() {
        let p = ScriptedPrompter::new(&["Bad_Name", "-x", "good-name"]);
        assert_eq!(p.text("Repo", None, &name_ok).expect("text"), "good-name");
        assert_eq!(p.asked.borrow().len(), 3);

        let p = ScriptedPrompter::new(&["relative/key.json", "/abs/key.json"]);
        assert_eq!(
            p.path("Key", None, true).expect("path"),
            PathBuf::from("/abs/key.json")
        );
        assert_eq!(p.asked.borrow().len(), 2);

        let p = ScriptedPrompter::new(&["a, B", "a a", "a, b  c"]);
        assert_eq!(
            p.list("Repos", vec![], &name_ok).expect("list"),
            ["a", "b", "c"]
        );
        assert_eq!(p.asked.borrow().len(), 3);
    }

    #[test]
    fn empty_answer_takes_suggestion() {
        let p = ScriptedPrompter::new(&["", "", "none", "n"]);
        assert_eq!(
            p.text("Dir", Some(".fornax"), &|_| Ok(())).expect("text"),
            ".fornax"
        );
        assert_eq!(
            p.list("Repos", vec!["x".into()], &name_ok).expect("list"),
            ["x"]
        );
        assert!(
            p.list("Repos", vec!["x".into()], &name_ok)
                .expect("list")
                .is_empty()
        );
        assert!(!p.confirm("Ok?", true).expect("confirm"));
        assert_eq!(p.remaining(), 0);
    }

    #[test]
    fn parse_list_rules() {
        assert_eq!(parse_list(" a,b ,, c ").expect("ok"), ["a", "b", "c"]);
        assert!(parse_list("NONE").expect("ok").is_empty());
        assert!(parse_list("a,a").is_err());
    }
}
