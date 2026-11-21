// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Diagnostics reported while loading `.fornax/` files.

/// A problem found in a `.fornax/` file, with a 1-based position.
///
/// `line` and `column` point at the offending key or value where possible; problems that
/// concern a whole file (for example a duplicate pipeline name without a `name:` key) use
/// line 1, column 1.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{file}:{line}:{column}: {message}")]
pub struct Diagnostic {
    /// Path of the file as given to [`crate::load`].
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number (in characters).
    pub column: usize,
    /// Human-readable description of the problem.
    pub message: String,
}

impl Diagnostic {
    /// Create a diagnostic.
    pub fn new(
        file: impl Into<String>,
        line: usize,
        column: usize,
        message: impl Into<String>,
    ) -> Self {
        Self {
            file: file.into(),
            line: line.max(1),
            column: column.max(1),
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_file_line_column_message() {
        let d = Diagnostic::new(".fornax/ci.yaml", 3, 5, "boom");
        assert_eq!(d.to_string(), ".fornax/ci.yaml:3:5: boom");
    }

    #[test]
    fn positions_are_at_least_one() {
        let d = Diagnostic::new("f", 0, 0, "x");
        assert_eq!((d.line, d.column), (1, 1));
    }
}
