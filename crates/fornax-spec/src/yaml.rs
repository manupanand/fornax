// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! A small positioned YAML tree built from `saphyr-parser` events.
//!
//! We build our own tree (instead of deserializing with serde) so that every key and value
//! keeps its line/column, which lets the loader report many precise diagnostics at once.
//! Anchors/aliases are resolved by copying, bounded by [`MAX_NODES`] and by a byte budget for
//! alias expansion ([`alias_budget`]); nesting is bounded by [`MAX_DEPTH`], so hostile input
//! cannot exhaust memory or the stack.

use std::collections::HashMap;

use saphyr_parser::{Event, Parser, ScalarStyle, Span};

/// Maximum nesting depth of sequences/mappings.
pub(crate) const MAX_DEPTH: usize = 64;
/// Maximum number of nodes in one document (after alias expansion).
pub(crate) const MAX_NODES: usize = 100_000;
/// Maximum size of one file in bytes.
pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
/// Alias expansion budget: at least this many bytes ...
pub(crate) const MIN_ALIAS_BYTES: usize = 4 * 1024 * 1024;
/// ... or this many times the file size, whichever is larger.
pub(crate) const ALIAS_BYTES_PER_FILE_BYTE: usize = 4;
/// Bytes charged per node copied by an alias (on top of its scalar text).
const ALIAS_NODE_BYTES: usize = 16;

/// Bytes that alias expansion may copy for a file of `file_len` bytes. Counting nodes alone
/// is not enough: one anchored 600 KB scalar aliased 100k times stays under [`MAX_NODES`]
/// but would copy ~60 GB.
pub(crate) fn alias_budget(file_len: usize) -> usize {
    file_len
        .saturating_mul(ALIAS_BYTES_PER_FILE_BYTE)
        .max(MIN_ALIAS_BYTES)
}

/// A YAML node with its 1-based start position.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Node {
    pub(crate) kind: Kind,
    pub(crate) line: usize,
    pub(crate) column: usize,
}

/// The shape of a node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    /// A scalar; `plain` is false for quoted, block or tagged scalars.
    Scalar {
        value: String,
        plain: bool,
    },
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

impl Node {
    /// True for an empty plain scalar or `~`/`null`.
    pub(crate) fn is_null(&self) -> bool {
        matches!(&self.kind, Kind::Scalar { value, plain: true }
            if matches!(value.as_str(), "" | "~" | "null" | "Null" | "NULL"))
    }

    /// The scalar text, if this is a scalar.
    pub(crate) fn scalar(&self) -> Option<&str> {
        match &self.kind {
            Kind::Scalar { value, .. } => Some(value),
            _ => None,
        }
    }

    /// Short description of the node type for error messages.
    pub(crate) fn type_name(&self) -> &'static str {
        match &self.kind {
            Kind::Scalar { .. } if self.is_null() => "null",
            Kind::Scalar { .. } => "a scalar",
            Kind::Seq(_) => "a list",
            Kind::Map(_) => "a mapping",
        }
    }

    /// `(nodes, bytes)` of this subtree; bytes are scalar lengths plus
    /// [`ALIAS_NODE_BYTES`] per node.
    fn cost(&self) -> (usize, usize) {
        let add = |a: (usize, usize), b: (usize, usize)| {
            (a.0.saturating_add(b.0), a.1.saturating_add(b.1))
        };
        let own = (1, ALIAS_NODE_BYTES);
        match &self.kind {
            Kind::Scalar { value, .. } => (1, ALIAS_NODE_BYTES.saturating_add(value.len())),
            Kind::Seq(items) => items.iter().map(Node::cost).fold(own, add),
            Kind::Map(entries) => entries
                .iter()
                .map(|(k, v)| add(k.cost(), v.cost()))
                .fold(own, add),
        }
    }
}

/// A YAML syntax error (1-based position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YamlError {
    pub(crate) line: usize,
    pub(crate) column: usize,
    pub(crate) message: String,
}

impl YamlError {
    fn at(span: Span, message: impl Into<String>) -> Self {
        Self {
            line: span.start.line(),
            column: span.start.col() + 1,
            message: message.into(),
        }
    }
}

enum Frame {
    Seq {
        items: Vec<Node>,
        line: usize,
        column: usize,
        anchor: usize,
    },
    Map {
        entries: Vec<(Node, Node)>,
        key: Option<Node>,
        line: usize,
        column: usize,
        anchor: usize,
    },
}

struct Builder {
    stack: Vec<Frame>,
    anchors: HashMap<usize, Node>,
    root: Option<Node>,
    nodes: usize,
    /// Bytes copied by aliases so far, against `alias_budget`.
    alias_bytes: usize,
    alias_budget: usize,
}

impl Builder {
    fn push(&mut self, node: Node, span: Span) -> Result<(), YamlError> {
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > MAX_NODES {
            return Err(YamlError::at(
                span,
                format!("document is too large (more than {MAX_NODES} nodes)"),
            ));
        }
        match self.stack.last_mut() {
            None => self.root = Some(node),
            Some(Frame::Seq { items, .. }) => items.push(node),
            Some(Frame::Map { entries, key, .. }) => match key.take() {
                Some(k) => entries.push((k, node)),
                None => *key = Some(node),
            },
        }
        Ok(())
    }

    fn remember(&mut self, anchor: usize, node: &Node) {
        if anchor != 0 {
            self.anchors.insert(anchor, node.clone());
        }
    }
}

/// Parse one YAML document. Returns `Ok(None)` for an empty file.
pub(crate) fn parse(src: &str) -> Result<Option<Node>, YamlError> {
    if src.len() > MAX_FILE_BYTES {
        return Err(YamlError {
            line: 1,
            column: 1,
            message: format!("file is too large (more than {MAX_FILE_BYTES} bytes)"),
        });
    }
    let mut parser = Parser::new_from_str(src);
    let mut b = Builder {
        stack: Vec::new(),
        anchors: HashMap::new(),
        root: None,
        nodes: 0,
        alias_bytes: 0,
        alias_budget: alias_budget(src.len()),
    };
    let mut documents = 0usize;
    while let Some(next) = parser.next_event() {
        let (event, span) = next.map_err(|e| YamlError {
            line: e.marker().line(),
            column: e.marker().col() + 1,
            message: e.info().to_string(),
        })?;
        let (line, column) = (span.start.line(), span.start.col() + 1);
        match event {
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentEnd => {}
            Event::DocumentStart(_) => {
                documents += 1;
                if documents > 1 {
                    return Err(YamlError::at(
                        span,
                        "multiple YAML documents in one file are not supported; use one file per pipeline or deployment",
                    ));
                }
            }
            Event::Scalar(value, style, anchor, tag) => {
                let plain = style == ScalarStyle::Plain && tag.is_none();
                let node = Node {
                    kind: Kind::Scalar {
                        value: value.into_owned(),
                        plain,
                    },
                    line,
                    column,
                };
                b.remember(anchor, &node);
                b.push(node, span)?;
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                if b.stack.len() >= MAX_DEPTH {
                    return Err(YamlError::at(
                        span,
                        format!("document is nested too deeply (more than {MAX_DEPTH} levels)"),
                    ));
                }
                let frame = if matches!(event, Event::SequenceStart(..)) {
                    Frame::Seq {
                        items: Vec::new(),
                        line,
                        column,
                        anchor,
                    }
                } else {
                    Frame::Map {
                        entries: Vec::new(),
                        key: None,
                        line,
                        column,
                        anchor,
                    }
                };
                b.stack.push(frame);
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let (node, anchor) = match b.stack.pop() {
                    Some(Frame::Seq {
                        items,
                        line,
                        column,
                        anchor,
                    }) => (
                        Node {
                            kind: Kind::Seq(items),
                            line,
                            column,
                        },
                        anchor,
                    ),
                    Some(Frame::Map {
                        entries,
                        line,
                        column,
                        anchor,
                        ..
                    }) => (
                        Node {
                            kind: Kind::Map(entries),
                            line,
                            column,
                        },
                        anchor,
                    ),
                    None => return Err(YamlError::at(span, "unbalanced YAML structure")),
                };
                b.remember(anchor, &node);
                b.push(node, span)?;
            }
            Event::Alias(id) => {
                let Some(target) = b.anchors.get(&id) else {
                    return Err(YamlError::at(span, "alias refers to an unknown anchor"));
                };
                // Charge the copy before making it.
                let (nodes, bytes) = target.cost();
                b.alias_bytes = b.alias_bytes.saturating_add(bytes);
                if b.alias_bytes > b.alias_budget {
                    return Err(YamlError::at(
                        span,
                        format!(
                            "document is too large: aliases expand to more than {} bytes",
                            b.alias_budget
                        ),
                    ));
                }
                b.nodes = b.nodes.saturating_add(nodes.saturating_sub(1));
                if b.nodes > MAX_NODES {
                    return Err(YamlError::at(
                        span,
                        format!("document is too large (more than {MAX_NODES} nodes)"),
                    ));
                }
                let mut node = target.clone();
                node.line = line;
                node.column = column;
                b.push(node, span)?;
            }
        }
    }
    Ok(b.root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(node: &Node) -> &[(Node, Node)] {
        match &node.kind {
            Kind::Map(entries) => entries,
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn positions_are_one_based() {
        let root = parse("a: 1\nbb:\n  - x\n").expect("parse").expect("doc");
        let entries = map(&root);
        assert_eq!((entries[0].0.line, entries[0].0.column), (1, 1));
        assert_eq!((entries[1].0.line, entries[1].0.column), (2, 1));
        let Kind::Seq(items) = &entries[1].1.kind else {
            panic!("seq")
        };
        assert_eq!((items[0].line, items[0].column), (3, 5));
    }

    #[test]
    fn empty_file_is_none() {
        assert_eq!(parse("").expect("parse"), None);
        assert_eq!(parse("# just a comment\n").expect("parse"), None);
    }

    #[test]
    fn syntax_error_has_position() {
        let err = parse("a: [1, 2\nb: 3\n").expect_err("error");
        assert!(err.line >= 1);
        assert!(!err.message.is_empty());
    }

    #[test]
    fn quoted_scalars_are_not_plain() {
        let root = parse("a: 'null'\nb: null\n").expect("parse").expect("doc");
        let entries = map(&root);
        assert!(!entries[0].1.is_null());
        assert!(entries[1].1.is_null());
    }

    #[test]
    fn aliases_are_copied() {
        let root = parse("a: &x [1, 2]\nb: *x\n").expect("parse").expect("doc");
        let entries = map(&root);
        assert_eq!(entries[0].1.kind, entries[1].1.kind);
        assert_eq!(entries[1].1.line, 2);
    }

    #[test]
    fn alias_bomb_is_rejected() {
        let mut src = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..12 {
            let p = i - 1;
            src.push_str(&format!(
                "a{i}: &a{i} [*a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}]\n"
            ));
        }
        let err = parse(&src).expect_err("too large");
        assert!(err.message.contains("too large"), "{}", err.message);
    }

    /// Few nodes but huge scalars: without a byte budget this expands to ~300 MB (and the
    /// same shape with 600 KB × 100k aliases to ~60 GB) from a file well under 1 MiB.
    #[test]
    fn alias_byte_bomb_is_rejected() {
        let big = "x".repeat(300_000);
        let mut src = format!("s: &a \"{big}\"\nl: [");
        src.push_str(&"*a, ".repeat(1_000));
        src.push_str("]\n");
        assert!(src.len() < MAX_FILE_BYTES);
        let start = std::time::Instant::now();
        let err = parse(&src).expect_err("too large");
        assert!(err.message.contains("aliases expand"), "{}", err.message);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn moderate_alias_use_is_fine() {
        // 1000 aliases of a 1 KB scalar: about 1 MB of expansion, inside the budget.
        let mut src = format!("s: &a \"{}\"\nl: [", "y".repeat(1_000));
        src.push_str(&"*a, ".repeat(1_000));
        src.push_str("]\n");
        let root = parse(&src).expect("parse").expect("doc");
        let Kind::Seq(items) = &map(&root)[1].1.kind else {
            panic!("seq")
        };
        assert_eq!(items.len(), 1_000);
    }

    #[test]
    fn deep_nesting_is_rejected() {
        let src = format!("{}{}", "[".repeat(200), "]".repeat(200));
        let err = parse(&src).expect_err("too deep");
        assert!(err.message.contains("nested too deeply"));
    }

    #[test]
    fn multiple_documents_are_rejected() {
        let err = parse("a: 1\n---\nb: 2\n").expect_err("multi");
        assert!(err.message.contains("multiple YAML documents"));
        assert_eq!(err.line, 2);
    }
}
