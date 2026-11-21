// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Loading `.fornax/` files into a validated [`Spec`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use crate::cron::CronSchedule;
use crate::diag::Diagnostic;
use crate::expr::Expression;
use crate::graph::find_cycles;
use crate::model::{
    Artifacts, DEFAULT_TIMEOUT_MINUTES, Deployment, Job, MAX_JOBS_PER_PIPELINE,
    MAX_MATRIX_COMBINATIONS, MAX_STEPS_PER_JOB, MAX_TIMEOUT_MINUTES, Pipeline, Promotion, Source,
    Spec, Step, SyncPolicy, Target, TargetType,
};
use crate::pattern::{MAX_PATTERNS_PER_LIST, Pattern, PatternList};
use crate::trigger::{On, PullRequestFilter, PushFilter};
use crate::yaml::{self, Kind, Node};

pub(crate) const PIPELINE_FIELDS: &[&str] = &["kind", "version", "name", "on", "variables", "jobs"];
pub(crate) const ON_FIELDS: &[&str] = &["push", "pull_request", "schedule", "manual"];
pub(crate) const PUSH_FIELDS: &[&str] = &[
    "branches",
    "branches-ignore",
    "tags",
    "tags-ignore",
    "paths",
    "paths-ignore",
];
pub(crate) const PULL_REQUEST_FIELDS: &[&str] =
    &["branches", "branches-ignore", "paths", "paths-ignore"];
pub(crate) const JOB_FIELDS: &[&str] = &[
    "runs-on",
    "needs",
    "if",
    "env",
    "timeout-minutes",
    "steps",
    "strategy",
    "artifacts",
    "environment",
    "trigger-deployment",
];
pub(crate) const STEP_FIELDS: &[&str] =
    &["name", "run", "env", "timeout-minutes", "working-directory"];
pub(crate) const STRATEGY_FIELDS: &[&str] = &["matrix", "max-parallel"];
pub(crate) const ARTIFACT_FIELDS: &[&str] = &["paths", "expire-in"];
pub(crate) const DEPLOYMENT_FIELDS: &[&str] = &[
    "kind",
    "version",
    "name",
    "environment",
    "source",
    "target",
    "sync",
    "promotion",
];
pub(crate) const SOURCE_FIELDS: &[&str] = &["path"];
pub(crate) const TARGET_FIELDS: &[&str] = &["type", "host", "namespace", "project"];
pub(crate) const SYNC_FIELDS: &[&str] = &["auto", "prune", "self-heal"];
pub(crate) const PROMOTION_FIELDS: &[&str] = &["to", "requires-approval"];

/// Allowed characters in pipeline/job names (besides ASCII alphanumerics).
pub(crate) const NAME_PUNCTUATION: &str = " _.()=,-";
/// Maximum pipeline/job name length.
pub(crate) const MAX_NAME_LEN: usize = 100;
/// Maximum label / environment / host name length.
pub(crate) const MAX_LABEL_LEN: usize = 64;
/// Maximum `strategy.max-parallel`.
pub(crate) const MAX_PARALLEL: u32 = 256;

/// Load and validate the contents of `.fornax/` files.
///
/// `files` are `(path, content)` pairs. Each file must declare `kind:` (`Pipeline` or
/// `Deployment`) and `version: 1`. All problems that can be found are returned together,
/// sorted by file (input order), line and column.
pub fn load(files: &[(String, String)]) -> Result<Spec, Vec<Diagnostic>> {
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut spec = Spec::default();
    // (file, line, column) of where each pipeline/deployment name came from.
    let mut pipeline_pos: Vec<(usize, usize)> = Vec::new();
    let mut deployment_pos: Vec<(usize, usize)> = Vec::new();
    // trigger-deployment references: (file, line, column, job, deployment).
    let mut triggers: Vec<(String, usize, usize, String, String)> = Vec::new();

    for (path, content) in files {
        let mut cx = Cx {
            file: path,
            diags: Vec::new(),
        };
        match yaml::parse(content) {
            Err(e) => cx.err_at(e.line, e.column, format!("invalid YAML: {}", e.message)),
            Ok(None) => cx.err_at(
                1,
                1,
                "file is empty; expected `kind: Pipeline` or `kind: Deployment`",
            ),
            Ok(Some(root)) => match file_kind(&mut cx, &root) {
                Some(FileKind::Pipeline) => {
                    if let Some(out) = pipeline(&mut cx, &root, path) {
                        spec.pipelines.push(out.pipeline);
                        pipeline_pos.push(out.name_pos);
                        triggers.extend(out.triggers);
                    }
                }
                Some(FileKind::Deployment) => {
                    if let Some((d, pos)) = deployment(&mut cx, &root, path) {
                        spec.deployments.push(d);
                        deployment_pos.push(pos);
                    }
                }
                None => {}
            },
        }
        diags.append(&mut cx.diags);
    }

    // Cross-file checks.
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for (p, &(line, column)) in spec.pipelines.iter().zip(&pipeline_pos) {
        if let Some(first) = seen.insert(&p.name, &p.file) {
            diags.push(Diagnostic::new(
                &p.file,
                line,
                column,
                format!(
                    "duplicate pipeline name `{}` (also used by {first})",
                    p.name
                ),
            ));
        }
    }
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for (d, &(line, column)) in spec.deployments.iter().zip(&deployment_pos) {
        if let Some(first) = seen.insert(&d.name, &d.file) {
            diags.push(Diagnostic::new(
                &d.file,
                line,
                column,
                format!(
                    "duplicate deployment name `{}` (also used by {first})",
                    d.name
                ),
            ));
        }
    }
    for (file, line, column, job, target) in triggers {
        if spec.deployment(&target).is_none() {
            diags.push(Diagnostic::new(
                file,
                line,
                column,
                format!("job `{job}` triggers unknown deployment `{target}`"),
            ));
        }
    }

    if diags.is_empty() {
        Ok(spec)
    } else {
        let order: HashMap<&str, usize> = files
            .iter()
            .enumerate()
            .map(|(i, (p, _))| (p.as_str(), i))
            .collect();
        diags.sort_by_key(|d| {
            (
                order.get(d.file.as_str()).copied().unwrap_or(usize::MAX),
                d.line,
                d.column,
            )
        });
        Err(diags)
    }
}

// ---------------------------------------------------------------------------------------
// Helpers

struct Cx<'a> {
    file: &'a str,
    diags: Vec<Diagnostic>,
}

impl Cx<'_> {
    fn err(&mut self, node: &Node, message: impl Into<String>) {
        self.err_at(node.line, node.column, message);
    }

    fn err_at(&mut self, line: usize, column: usize, message: impl Into<String>) {
        self.diags
            .push(Diagnostic::new(self.file, line, column, message));
    }
}

struct Fields<'n> {
    items: Vec<(&'n str, &'n Node, &'n Node)>,
}

impl<'n> Fields<'n> {
    fn get(&self, name: &str) -> Option<&'n Node> {
        self.items
            .iter()
            .find(|(k, _, _)| *k == name)
            .map(|(_, _, v)| *v)
    }
}

/// Key/value entries of a mapping (null counts as empty), checking for non-string and
/// duplicate keys.
fn entries<'n>(
    cx: &mut Cx<'_>,
    node: &'n Node,
    what: &str,
) -> Option<Vec<(&'n str, &'n Node, &'n Node)>> {
    if node.is_null() {
        return Some(Vec::new());
    }
    let Kind::Map(map) = &node.kind else {
        cx.err(
            node,
            format!("{what} must be a mapping, found {}", node.type_name()),
        );
        return None;
    };
    let mut out = Vec::with_capacity(map.len());
    let mut seen = HashSet::new();
    for (k, v) in map {
        let Some(key) = k.scalar() else {
            cx.err(k, format!("keys in {what} must be strings"));
            continue;
        };
        if !seen.insert(key) {
            cx.err(k, format!("duplicate key `{key}` in {what}"));
            continue;
        }
        out.push((key, k, v));
    }
    Some(out)
}

/// A mapping with a fixed set of allowed keys; unknown keys are reported.
fn fields<'n>(cx: &mut Cx<'_>, node: &'n Node, what: &str, allowed: &[&str]) -> Option<Fields<'n>> {
    let items = entries(cx, node, what)?;
    let mut out = Vec::with_capacity(items.len());
    for (key, k, v) in items {
        if allowed.contains(&key) {
            out.push((key, k, v));
        } else {
            let hint = if allowed.is_empty() {
                "it takes no fields".to_string()
            } else {
                format!("expected one of: {}", allowed.join(", "))
            };
            cx.err(k, format!("unknown field `{key}` in {what}; {hint}"));
        }
    }
    Some(Fields { items: out })
}

fn string(cx: &mut Cx<'_>, node: &Node, what: &str) -> Option<String> {
    if node.is_null() {
        cx.err(node, format!("{what} must not be empty"));
        return None;
    }
    match node.scalar() {
        Some(s) => Some(s.to_string()),
        None => {
            cx.err(
                node,
                format!("{what} must be a string, found {}", node.type_name()),
            );
            None
        }
    }
}

fn nonblank(cx: &mut Cx<'_>, node: &Node, what: &str) -> Option<String> {
    let s = string(cx, node, what)?;
    if s.trim().is_empty() {
        cx.err(node, format!("{what} must not be empty"));
        return None;
    }
    Some(s)
}

/// A list of strings (a single string counts as a one-element list; null as empty).
fn string_list<'n>(cx: &mut Cx<'_>, node: &'n Node, what: &str) -> Option<Vec<(String, &'n Node)>> {
    if node.is_null() {
        return Some(Vec::new());
    }
    match &node.kind {
        Kind::Scalar { value, .. } => Some(vec![(value.clone(), node)]),
        Kind::Seq(items) => {
            let mut out = Vec::with_capacity(items.len());
            let mut ok = true;
            for item in items {
                match string(cx, item, &format!("each entry of {what}")) {
                    Some(s) => out.push((s, item)),
                    None => ok = false,
                }
            }
            ok.then_some(out)
        }
        Kind::Map(_) => {
            cx.err(node, format!("{what} must be a list, found a mapping"));
            None
        }
    }
}

fn boolean(cx: &mut Cx<'_>, node: &Node, what: &str) -> Option<bool> {
    if let Kind::Scalar { value, plain: true } = &node.kind {
        match value.as_str() {
            "true" | "True" | "TRUE" => return Some(true),
            "false" | "False" | "FALSE" => return Some(false),
            _ => {}
        }
    }
    cx.err(node, format!("{what} must be true or false"));
    None
}

fn int_in(cx: &mut Cx<'_>, node: &Node, what: &str, min: u32, max: u32) -> Option<u32> {
    let parsed = node
        .scalar()
        .filter(|_| !node.is_null())
        .and_then(|s| s.trim().parse::<u64>().ok());
    match parsed {
        Some(v) if v >= u64::from(min) && v <= u64::from(max) => u32::try_from(v).ok(),
        _ => {
            cx.err(
                node,
                format!("{what} must be a whole number from {min} to {max}"),
            );
            None
        }
    }
}

fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn env_map(cx: &mut Cx<'_>, node: &Node, what: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(items) = entries(cx, node, what) else {
        return out;
    };
    for (key, k, v) in items {
        if !is_env_name(key) {
            cx.err(
                k,
                format!("invalid variable name `{key}` in {what}; use letters, digits and `_`, not starting with a digit"),
            );
            continue;
        }
        let value = if v.is_null() {
            Some(String::new())
        } else if let Some(s) = v.scalar() {
            Some(s.to_string())
        } else {
            cx.err(
                v,
                format!(
                    "value of `{key}` in {what} must be a string, found {}",
                    v.type_name()
                ),
            );
            None
        };
        if let Some(value) = value {
            out.insert(key.to_string(), value);
        }
    }
    out
}

/// Pipeline / job name rule: 1–100 characters from `[A-Za-z0-9 _.()=,-]`, no leading or
/// trailing space.
pub(crate) fn check_name(s: &str) -> Result<(), String> {
    let len = s.chars().count();
    if len == 0 || len > MAX_NAME_LEN {
        return Err(format!("must be 1 to {MAX_NAME_LEN} characters long"));
    }
    if let Some(bad) = s
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && !NAME_PUNCTUATION.contains(*c))
    {
        return Err(format!(
            "contains `{bad}`; allowed are letters, digits, space and `_.()=,-`"
        ));
    }
    if s.starts_with(' ') || s.ends_with(' ') {
        return Err("must not start or end with a space".into());
    }
    Ok(())
}

/// Label / environment / host rule: 1–64 characters from `[A-Za-z0-9_.-]`.
fn check_label(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > MAX_LABEL_LEN {
        return Err(format!("must be 1 to {MAX_LABEL_LEN} characters long"));
    }
    if let Some(bad) = s
        .chars()
        .find(|c| !c.is_ascii_alphanumeric() && !"_.-".contains(*c))
    {
        return Err(format!(
            "contains `{bad}`; allowed are letters, digits and `_.-`"
        ));
    }
    Ok(())
}

/// A relative path inside the repository / checkout.
fn check_rel_path(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        return Err("must not be empty".into());
    }
    if s.starts_with('/') || s.starts_with('\\') || s.as_bytes().get(1) == Some(&b':') {
        return Err("must be a relative path".into());
    }
    if s.contains('\0') {
        return Err("must not contain NUL".into());
    }
    if s.split(['/', '\\']).any(|c| c == "..") {
        return Err("must not contain `..`".into());
    }
    Ok(())
}

fn label(cx: &mut Cx<'_>, node: &Node, what: &str) -> Option<String> {
    let s = string(cx, node, what)?;
    match check_label(&s) {
        Ok(()) => Some(s),
        Err(e) => {
            cx.err(node, format!("{what} `{s}` {e}"));
            None
        }
    }
}

fn rel_path(cx: &mut Cx<'_>, node: &Node, what: &str) -> Option<String> {
    let s = string(cx, node, what)?;
    match check_rel_path(&s) {
        Ok(()) => Some(s),
        Err(e) => {
            cx.err(node, format!("{what} `{s}` {e}"));
            None
        }
    }
}

/// `30m`, `12h`, `7d`, `2w`, `90s`.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let unit = s.chars().last()?;
    let mult: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        'w' => 604_800,
        _ => return None,
    };
    let num = &s[..s.len() - 1];
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = num.parse().ok()?;
    let secs = n.checked_mul(mult)?;
    (secs > 0 && secs <= 3650 * 86_400).then(|| Duration::from_secs(secs))
}

fn file_stem(path: &str) -> &str {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => base,
    }
}

// ---------------------------------------------------------------------------------------
// Files

enum FileKind {
    Pipeline,
    Deployment,
}

fn find<'n>(node: &'n Node, key: &str) -> Option<(&'n Node, &'n Node)> {
    match &node.kind {
        Kind::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.scalar() == Some(key))
            .map(|(k, v)| (k, v)),
        _ => None,
    }
}

fn file_kind(cx: &mut Cx<'_>, root: &Node) -> Option<FileKind> {
    if !matches!(root.kind, Kind::Map(_)) {
        cx.err(
            root,
            format!(
                "a .fornax file must be a mapping, found {}",
                root.type_name()
            ),
        );
        return None;
    }
    let kind = match find(root, "kind") {
        None => {
            cx.err_at(
                1,
                1,
                "missing required field `kind` (Pipeline or Deployment)",
            );
            None
        }
        Some((_, v)) => match v.scalar() {
            Some("Pipeline") => Some(FileKind::Pipeline),
            Some("Deployment") => Some(FileKind::Deployment),
            _ => {
                cx.err(v, "`kind` must be `Pipeline` or `Deployment`");
                None
            }
        },
    };
    match find(root, "version") {
        None => {
            cx.err_at(1, 1, "missing required field `version` (use `version: 1`)");
            return None;
        }
        Some((_, v)) => {
            if v.scalar().map(str::trim) != Some("1") {
                cx.err(
                    v,
                    format!(
                        "unsupported version `{}`; only `version: 1` is supported",
                        v.scalar().unwrap_or("?")
                    ),
                );
                return None;
            }
        }
    }
    kind
}

struct PipelineOut {
    pipeline: Pipeline,
    name_pos: (usize, usize),
    triggers: Vec<(String, usize, usize, String, String)>,
}

fn spec_name(
    cx: &mut Cx<'_>,
    f: &Fields<'_>,
    path: &str,
    what: &str,
) -> Option<(String, (usize, usize))> {
    match f.get("name") {
        Some(n) => {
            let s = string(cx, n, "`name`")?;
            match check_name(&s) {
                Ok(()) => Some((s, (n.line, n.column))),
                Err(e) => {
                    cx.err(n, format!("{what} name `{s}` {e}"));
                    None
                }
            }
        }
        None => {
            let stem = file_stem(path).to_string();
            match check_name(&stem) {
                Ok(()) => Some((stem, (1, 1))),
                Err(e) => {
                    cx.err_at(
                        1,
                        1,
                        format!("{what} name `{stem}` (taken from the file name) {e}; set `name:`"),
                    );
                    None
                }
            }
        }
    }
}

fn pipeline(cx: &mut Cx<'_>, root: &Node, path: &str) -> Option<PipelineOut> {
    let before = cx.diags.len();
    let f = fields(cx, root, "pipeline", PIPELINE_FIELDS)?;
    let name = spec_name(cx, &f, path, "pipeline");
    let on = match f.get("on") {
        Some(n) => on(cx, n),
        None => {
            cx.err_at(1, 1, "missing required field `on`");
            None
        }
    };
    let variables = f
        .get("variables")
        .map(|n| env_map(cx, n, "`variables`"))
        .unwrap_or_default();
    let jobs = match f.get("jobs") {
        Some(n) => jobs(cx, n),
        None => {
            cx.err_at(1, 1, "missing required field `jobs`");
            None
        }
    };
    if cx.diags.len() > before {
        return None;
    }
    let ((name, name_pos), on, (jobs, triggers)) = (name?, on?, jobs?);
    let triggers = triggers
        .into_iter()
        .map(|(line, column, job, target)| (path.to_string(), line, column, job, target))
        .collect();
    Some(PipelineOut {
        pipeline: Pipeline {
            name,
            file: path.to_string(),
            on,
            variables,
            jobs,
        },
        name_pos,
        triggers,
    })
}

// ---------------------------------------------------------------------------------------
// on:

fn patterns(cx: &mut Cx<'_>, node: &Node, what: &str) -> PatternList {
    let mut out = Vec::new();
    if let Some(items) = string_list(cx, node, what) {
        if items.len() > MAX_PATTERNS_PER_LIST {
            cx.err(
                node,
                format!(
                    "{what} has {} patterns; a list may have at most {MAX_PATTERNS_PER_LIST} patterns",
                    items.len()
                ),
            );
            return PatternList::default();
        }
        for (s, n) in items {
            match Pattern::new(&s) {
                Ok(p) => out.push(p),
                Err(e) => cx.err(n, format!("{e} (in {what})")),
            }
        }
    }
    PatternList::from_patterns(out).unwrap_or_default()
}

fn exclusive(cx: &mut Cx<'_>, f: &Fields<'_>, a: &str, b: &str, event: &str) {
    if f.get(a).is_some()
        && let Some(n) = f.get(b)
    {
        cx.err(
            n,
            format!("`{a}` and `{b}` cannot both be set in `on.{event}`"),
        );
    }
}

fn push_filter(cx: &mut Cx<'_>, node: &Node) -> PushFilter {
    let Some(f) = fields(cx, node, "`on.push`", PUSH_FIELDS) else {
        return PushFilter::default();
    };
    exclusive(cx, &f, "branches", "branches-ignore", "push");
    exclusive(cx, &f, "tags", "tags-ignore", "push");
    exclusive(cx, &f, "paths", "paths-ignore", "push");
    let mut get = |key: &str| {
        f.get(key)
            .map(|n| patterns(cx, n, &format!("`on.push.{key}`")))
            .unwrap_or_default()
    };
    PushFilter {
        branches: get("branches"),
        branches_ignore: get("branches-ignore"),
        tags: get("tags"),
        tags_ignore: get("tags-ignore"),
        paths: get("paths"),
        paths_ignore: get("paths-ignore"),
    }
}

fn pull_request_filter(cx: &mut Cx<'_>, node: &Node) -> PullRequestFilter {
    let Some(f) = fields(cx, node, "`on.pull_request`", PULL_REQUEST_FIELDS) else {
        return PullRequestFilter::default();
    };
    exclusive(cx, &f, "branches", "branches-ignore", "pull_request");
    exclusive(cx, &f, "paths", "paths-ignore", "pull_request");
    let mut get = |key: &str| {
        f.get(key)
            .map(|n| patterns(cx, n, &format!("`on.pull_request.{key}`")))
            .unwrap_or_default()
    };
    PullRequestFilter {
        branches: get("branches"),
        branches_ignore: get("branches-ignore"),
        paths: get("paths"),
        paths_ignore: get("paths-ignore"),
    }
}

fn on(cx: &mut Cx<'_>, node: &Node) -> Option<On> {
    let before = cx.diags.len();
    let mut on = On::default();
    match &node.kind {
        Kind::Scalar { .. } | Kind::Seq(_) if !node.is_null() => {
            // Shorthand: `on: push` / `on: [push, manual]`.
            for (name, n) in string_list(cx, node, "`on`")? {
                match name.as_str() {
                    "push" => on.push = Some(PushFilter::default()),
                    "pull_request" => on.pull_request = Some(PullRequestFilter::default()),
                    "manual" => on.manual = true,
                    "schedule" => cx.err(n, "`schedule` needs a list of cron expressions: `on: { schedule: [\"0 3 * * *\"] }`"),
                    other => cx.err(n, format!(
                        "unknown event `{other}`; expected one of: {}",
                        ON_FIELDS.join(", ")
                    )),
                }
            }
        }
        _ => {
            let f = fields(cx, node, "`on`", ON_FIELDS)?;
            if let Some(n) = f.get("push") {
                on.push = Some(push_filter(cx, n));
            }
            if let Some(n) = f.get("pull_request") {
                on.pull_request = Some(pull_request_filter(cx, n));
            }
            if let Some(n) = f.get("schedule")
                && let Some(items) = string_list(cx, n, "`on.schedule`")
            {
                if items.is_empty() {
                    cx.err(n, "`on.schedule` must list at least one cron expression");
                }
                for (s, item) in items {
                    match CronSchedule::parse(&s) {
                        Ok(c) => on.schedule.push(c),
                        Err(e) => cx.err(item, format!("invalid cron expression `{s}`: {e}")),
                    }
                }
            }
            if let Some(n) = f.get("manual") {
                on.manual = match &n.kind {
                    Kind::Map(_) => fields(cx, n, "`on.manual`", &[]).is_some(),
                    _ if n.is_null() => true,
                    _ => boolean(cx, n, "`on.manual`").unwrap_or(false),
                };
            }
        }
    }
    if cx.diags.len() == before
        && on.push.is_none()
        && on.pull_request.is_none()
        && on.schedule.is_empty()
        && !on.manual
    {
        cx.err(
            node,
            "`on` enables no events; use push, pull_request, schedule or manual",
        );
    }
    (cx.diags.len() == before).then_some(on)
}

// ---------------------------------------------------------------------------------------
// jobs:

struct RawJob<'n> {
    key: &'n Node,
    needs: Vec<(String, &'n Node)>,
    matrix: Vec<(String, Vec<String>)>,
    trigger: Option<(usize, usize, String)>,
    template: Job,
}

type Triggers = Vec<(usize, usize, String, String)>;

fn jobs(cx: &mut Cx<'_>, node: &Node) -> Option<(Vec<Job>, Triggers)> {
    let before = cx.diags.len();
    let items = entries(cx, node, "`jobs`")?;
    if items.is_empty() {
        cx.err(node, "`jobs` must define at least one job");
        return None;
    }
    let mut raw: Vec<RawJob<'_>> = Vec::new();
    for (name, k, v) in items {
        if let Err(e) = check_name(name) {
            cx.err(k, format!("job name `{name}` {e}"));
            continue;
        }
        if let Some(j) = job(cx, k, name, v) {
            raw.push(j);
        }
    }
    if cx.diags.len() > before {
        return None;
    }

    // needs: existence and cycles (on declared job names).
    let index: HashMap<&str, usize> = raw
        .iter()
        .enumerate()
        .map(|(i, j)| (j.template.base_name.as_str(), i))
        .collect();
    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(raw.len());
    for j in &raw {
        let mut ds = Vec::new();
        for (need, n) in &j.needs {
            match index.get(need.as_str()) {
                Some(&i) => {
                    if !ds.contains(&i) {
                        ds.push(i);
                    }
                }
                None => cx.err(
                    n,
                    format!("job `{}` needs unknown job `{need}`", j.template.base_name),
                ),
            }
        }
        deps.push(ds);
    }
    for cycle in find_cycles(&deps, 10) {
        let names: Vec<&str> = cycle
            .iter()
            .map(|&i| raw[i].template.base_name.as_str())
            .collect();
        let first = raw[cycle[0]].key;
        cx.err(first, format!("dependency cycle: {}", names.join(" -> ")));
    }
    if cx.diags.len() > before {
        return None;
    }

    // Matrix expansion.
    let expanded: Vec<Vec<Vec<(String, String)>>> =
        raw.iter().map(|j| combinations(&j.matrix)).collect();
    let names: Vec<Vec<String>> = raw
        .iter()
        .zip(&expanded)
        .map(|(j, combos)| {
            combos
                .iter()
                .map(|c| job_name(&j.template.base_name, c))
                .collect()
        })
        .collect();
    let total: usize = names.iter().map(Vec::len).sum();
    if total > MAX_JOBS_PER_PIPELINE {
        cx.err(
            node,
            format!(
                "`jobs` expands to {total} jobs; a pipeline may have at most \
                 {MAX_JOBS_PER_PIPELINE} jobs (after matrix expansion)"
            ),
        );
        return None;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for (j, ns) in raw.iter().zip(&names) {
        for n in ns {
            if !j.matrix.is_empty()
                && let Err(e) = check_name(n)
            {
                cx.err(j.key, format!("matrix job name `{n}` {e}"));
            }
            if !seen.insert(n) {
                cx.err(j.key, format!("job name `{n}` is defined more than once"));
            }
        }
    }
    if cx.diags.len() > before {
        return None;
    }
    let mut out = Vec::new();
    let mut triggers = Vec::new();
    for (i, j) in raw.iter().enumerate() {
        let needs: Vec<String> = deps[i]
            .iter()
            .flat_map(|&d| names[d].iter().cloned())
            .collect();
        if let Some((line, column, target)) = &j.trigger {
            triggers.push((*line, *column, j.template.base_name.clone(), target.clone()));
        }
        for (combo, name) in expanded[i].iter().zip(&names[i]) {
            let mut job = j.template.clone();
            job.name = name.clone();
            job.matrix = combo.clone();
            job.needs = needs.clone();
            out.push(job);
        }
    }
    Some((out, triggers))
}

fn combinations(matrix: &[(String, Vec<String>)]) -> Vec<Vec<(String, String)>> {
    let mut out: Vec<Vec<(String, String)>> = vec![Vec::new()];
    for (key, values) in matrix {
        let mut next = Vec::with_capacity(out.len() * values.len());
        for prefix in &out {
            for v in values {
                let mut c = prefix.clone();
                c.push((key.clone(), v.clone()));
                next.push(c);
            }
        }
        out = next;
    }
    out
}

fn job_name(base: &str, combo: &[(String, String)]) -> String {
    if combo.is_empty() {
        return base.to_string();
    }
    let parts: Vec<String> = combo.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{base} ({})", parts.join(", "))
}

fn is_matrix_key(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn job<'n>(cx: &mut Cx<'_>, key: &'n Node, name: &str, node: &'n Node) -> Option<RawJob<'n>> {
    let before = cx.diags.len();
    let what = format!("job `{name}`");
    let f = fields(cx, node, &what, JOB_FIELDS)?;
    if node.is_null() {
        cx.err(key, format!("{what} must define `steps`"));
        return None;
    }

    let mut runs_on = Vec::new();
    if let Some(n) = f.get("runs-on")
        && let Some(items) = string_list(cx, n, "`runs-on`")
    {
        if items.is_empty() {
            cx.err(n, "`runs-on` must list at least one runner label");
        }
        for (s, item) in items {
            match check_label(&s) {
                Ok(()) => runs_on.push(s),
                Err(e) => cx.err(item, format!("runner label `{s}` {e}")),
            }
        }
    }

    let needs = f
        .get("needs")
        .and_then(|n| string_list(cx, n, "`needs`"))
        .unwrap_or_default();

    let condition = f.get("if").and_then(|n| {
        let s = string(cx, n, "`if`")?;
        match Expression::parse(&s) {
            Ok(e) => Some(e),
            Err(e) => {
                cx.err(n, format!("invalid `if` expression `{s}`: {e}"));
                None
            }
        }
    });

    let env = f
        .get("env")
        .map(|n| env_map(cx, n, "`env`"))
        .unwrap_or_default();

    let timeout_minutes = f
        .get("timeout-minutes")
        .and_then(|n| int_in(cx, n, "`timeout-minutes`", 1, MAX_TIMEOUT_MINUTES))
        .unwrap_or(DEFAULT_TIMEOUT_MINUTES);

    let mut steps_out = Vec::new();
    match f.get("steps") {
        None => cx.err(key, format!("{what} must define `steps`")),
        Some(n) => match &n.kind {
            Kind::Seq(items) if items.is_empty() => {
                cx.err(n, format!("{what} must have at least one step"));
            }
            Kind::Seq(items) if items.len() > MAX_STEPS_PER_JOB => {
                cx.err(
                    n,
                    format!(
                        "{what} has {} steps; a job may have at most {MAX_STEPS_PER_JOB} steps",
                        items.len()
                    ),
                );
            }
            Kind::Seq(items) => {
                for (i, item) in items.iter().enumerate() {
                    if let Some(s) = step(cx, item, i + 1) {
                        steps_out.push(s);
                    }
                }
            }
            _ if n.is_null() => cx.err(n, format!("{what} must have at least one step")),
            _ => cx.err(
                n,
                format!("`steps` must be a list, found {}", n.type_name()),
            ),
        },
    }

    let (matrix, max_parallel) = f
        .get("strategy")
        .map(|n| strategy(cx, n))
        .unwrap_or_default();

    let artifacts = f.get("artifacts").and_then(|n| artifacts(cx, n));
    let environment = f
        .get("environment")
        .and_then(|n| label(cx, n, "`environment`"));
    let trigger = f.get("trigger-deployment").and_then(|n| {
        let s = string(cx, n, "`trigger-deployment`")?;
        Some((n.line, n.column, s))
    });

    if cx.diags.len() > before {
        return None;
    }
    Some(RawJob {
        key,
        needs,
        matrix,
        trigger: trigger.clone(),
        template: Job {
            name: name.to_string(),
            base_name: name.to_string(),
            matrix: Vec::new(),
            runs_on,
            needs: Vec::new(),
            condition,
            env,
            timeout_minutes,
            steps: steps_out,
            max_parallel,
            artifacts,
            environment,
            trigger_deployment: trigger.map(|(_, _, s)| s),
        },
    })
}

fn step(cx: &mut Cx<'_>, node: &Node, index: usize) -> Option<Step> {
    let before = cx.diags.len();
    let what = format!("step {index}");
    if !matches!(node.kind, Kind::Map(_)) {
        cx.err(
            node,
            format!(
                "{what} must be a mapping with `run:`, found {}",
                node.type_name()
            ),
        );
        return None;
    }
    let f = fields(cx, node, &what, STEP_FIELDS)?;
    let name = f.get("name").and_then(|n| nonblank(cx, n, "step `name`"));
    let run = match f.get("run") {
        Some(n) => nonblank(cx, n, "`run`"),
        None => {
            cx.err(node, format!("{what} must have `run:`"));
            None
        }
    };
    let env = f
        .get("env")
        .map(|n| env_map(cx, n, "step `env`"))
        .unwrap_or_default();
    let timeout_minutes = f
        .get("timeout-minutes")
        .and_then(|n| int_in(cx, n, "step `timeout-minutes`", 1, MAX_TIMEOUT_MINUTES));
    let working_directory = f
        .get("working-directory")
        .and_then(|n| rel_path(cx, n, "`working-directory`"));
    if cx.diags.len() > before {
        return None;
    }
    Some(Step {
        name,
        run: run?,
        env,
        timeout_minutes,
        working_directory,
    })
}

fn strategy(cx: &mut Cx<'_>, node: &Node) -> (Vec<(String, Vec<String>)>, Option<u32>) {
    let mut matrix = Vec::new();
    let Some(f) = fields(cx, node, "`strategy`", STRATEGY_FIELDS) else {
        return (matrix, None);
    };
    let max_parallel = f
        .get("max-parallel")
        .and_then(|n| int_in(cx, n, "`max-parallel`", 1, MAX_PARALLEL));
    let Some(m) = f.get("matrix") else {
        cx.err(node, "`strategy` must define `matrix`");
        return (matrix, max_parallel);
    };
    let Some(items) = entries(cx, m, "`strategy.matrix`") else {
        return (matrix, max_parallel);
    };
    if items.is_empty() {
        cx.err(m, "`strategy.matrix` must define at least one key");
    }
    let mut total: usize = 1;
    for (key, k, v) in items {
        if key == "include" || key == "exclude" {
            cx.err(k, format!("matrix `{key}` is not supported yet"));
            continue;
        }
        if !is_matrix_key(key) {
            cx.err(
                k,
                format!("invalid matrix key `{key}`; use letters, digits, `_` and `-`"),
            );
            continue;
        }
        let Kind::Seq(values) = &v.kind else {
            cx.err(
                v,
                format!(
                    "matrix `{key}` must be a list of values, found {}",
                    v.type_name()
                ),
            );
            continue;
        };
        if values.is_empty() {
            cx.err(v, format!("matrix `{key}` must have at least one value"));
            continue;
        }
        let mut vals: Vec<String> = Vec::new();
        for item in values {
            let Some(s) = string(cx, item, &format!("matrix `{key}` value")) else {
                continue;
            };
            if vals.contains(&s) {
                cx.err(item, format!("duplicate value `{s}` in matrix `{key}`"));
                continue;
            }
            vals.push(s);
        }
        total = total.saturating_mul(vals.len().max(1));
        matrix.push((key.to_string(), vals));
    }
    if total > MAX_MATRIX_COMBINATIONS {
        cx.err(
            m,
            format!(
                "matrix expands to {total} combinations; the limit is {MAX_MATRIX_COMBINATIONS}"
            ),
        );
    }
    (matrix, max_parallel)
}

fn artifacts(cx: &mut Cx<'_>, node: &Node) -> Option<Artifacts> {
    let before = cx.diags.len();
    let f = fields(cx, node, "`artifacts`", ARTIFACT_FIELDS)?;
    let mut paths = Vec::new();
    match f.get("paths") {
        None => cx.err(node, "`artifacts` must define `paths`"),
        Some(n) => {
            if let Some(items) = string_list(cx, n, "`artifacts.paths`") {
                if items.is_empty() {
                    cx.err(n, "`artifacts.paths` must list at least one path");
                }
                for (s, item) in items {
                    match check_rel_path(&s) {
                        Ok(()) => paths.push(s),
                        Err(e) => cx.err(item, format!("artifact path `{s}` {e}")),
                    }
                }
            }
        }
    }
    let expire_in = f.get("expire-in").and_then(|n| {
        let s = string(cx, n, "`expire-in`")?;
        let d = parse_duration(&s);
        if d.is_none() {
            cx.err(n, format!(
                "invalid `expire-in` `{s}`; use a number with s, m, h, d or w (e.g. `7d`), at most 3650 days"
            ));
        }
        d
    });
    (cx.diags.len() == before).then_some(Artifacts { paths, expire_in })
}

// ---------------------------------------------------------------------------------------
// Deployment

fn deployment(cx: &mut Cx<'_>, root: &Node, path: &str) -> Option<(Deployment, (usize, usize))> {
    let before = cx.diags.len();
    let f = fields(cx, root, "deployment", DEPLOYMENT_FIELDS)?;
    let name = spec_name(cx, &f, path, "deployment");
    let environment = match f.get("environment") {
        Some(n) => label(cx, n, "`environment`"),
        None => {
            cx.err_at(1, 1, "missing required field `environment`");
            None
        }
    };
    let source = match f.get("source") {
        None => {
            cx.err_at(1, 1, "missing required field `source`");
            None
        }
        Some(n) => fields(cx, n, "`source`", SOURCE_FIELDS).and_then(|sf| match sf.get("path") {
            Some(p) => rel_path(cx, p, "`source.path`").map(|path| Source { path }),
            None => {
                cx.err(n, "`source` must define `path`");
                None
            }
        }),
    };
    let target = match f.get("target") {
        None => {
            cx.err_at(1, 1, "missing required field `target`");
            None
        }
        Some(n) => target(cx, n),
    };
    let sync = f
        .get("sync")
        .and_then(|n| {
            let sf = fields(cx, n, "`sync`", SYNC_FIELDS)?;
            let mut get = |k: &str| {
                sf.get(k)
                    .and_then(|v| boolean(cx, v, &format!("`sync.{k}`")))
                    .unwrap_or(false)
            };
            Some(SyncPolicy {
                auto: get("auto"),
                prune: get("prune"),
                self_heal: get("self-heal"),
            })
        })
        .unwrap_or_default();
    let promotion = f.get("promotion").and_then(|n| {
        let pf = fields(cx, n, "`promotion`", PROMOTION_FIELDS)?;
        let to = match pf.get("to") {
            Some(t) => label(cx, t, "`promotion.to`"),
            None => {
                cx.err(n, "`promotion` must define `to`");
                None
            }
        };
        let requires_approval = pf
            .get("requires-approval")
            .and_then(|v| boolean(cx, v, "`promotion.requires-approval`"))
            .unwrap_or(false);
        if let (Some(t), Some(env)) = (&to, &environment)
            && t == env
        {
            cx.err(
                n,
                format!("`promotion.to` must differ from `environment` (`{env}`)"),
            );
        }
        Some(Promotion {
            to: to?,
            requires_approval,
        })
    });
    if cx.diags.len() > before {
        return None;
    }
    let (name, pos) = name?;
    Some((
        Deployment {
            name,
            file: path.to_string(),
            environment: environment?,
            source: source?,
            target: target?,
            sync,
            promotion,
        },
        pos,
    ))
}

fn target(cx: &mut Cx<'_>, node: &Node) -> Option<Target> {
    let before = cx.diags.len();
    let f = fields(cx, node, "`target`", TARGET_FIELDS)?;
    let kind = match f.get("type") {
        None => {
            cx.err(
                node,
                "`target` must define `type` (host, docker-compose or kubernetes)",
            );
            None
        }
        Some(t) => match t.scalar() {
            Some("host") => Some(TargetType::Host),
            Some("docker-compose") => Some(TargetType::DockerCompose),
            Some("kubernetes") => Some(TargetType::Kubernetes),
            _ => {
                cx.err(
                    t,
                    "`target.type` must be host, docker-compose or kubernetes",
                );
                None
            }
        },
    };
    let host = f.get("host").and_then(|n| label(cx, n, "`target.host`"));
    let namespace = f
        .get("namespace")
        .and_then(|n| label(cx, n, "`target.namespace`"));
    let project = f
        .get("project")
        .and_then(|n| label(cx, n, "`target.project`"));
    if let Some(kind) = kind {
        let ty = kind.as_str();
        if kind == TargetType::Host && f.get("host").is_none() {
            cx.err(node, "`target.host` is required for type `host`");
        }
        if kind == TargetType::Kubernetes
            && let Some(n) = f.get("host")
        {
            cx.err(n, "`target.host` is not used for type `kubernetes`");
        }
        if kind != TargetType::Kubernetes
            && let Some(n) = f.get("namespace")
        {
            cx.err(
                n,
                format!("`target.namespace` is only used for type `kubernetes`, not `{ty}`"),
            );
        }
        if kind != TargetType::DockerCompose
            && let Some(n) = f.get("project")
        {
            cx.err(
                n,
                format!("`target.project` is only used for type `docker-compose`, not `{ty}`"),
            );
        }
    }
    if cx.diags.len() > before {
        return None;
    }
    Some(Target {
        kind: kind?,
        host,
        namespace,
        project,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(src: &str) -> Result<Spec, Vec<Diagnostic>> {
        load(&[(".fornax/ci.yaml".to_string(), src.to_string())])
    }

    fn errors(src: &str) -> Vec<String> {
        one(src)
            .expect_err("should fail")
            .into_iter()
            .map(|d| format!("{}:{}: {}", d.line, d.column, d.message))
            .collect()
    }

    const HEAD: &str = "kind: Pipeline\nversion: 1\non: push\n";

    #[test]
    fn minimal_pipeline() {
        let spec = one(&format!(
            "{HEAD}jobs:\n  build:\n    steps:\n      - run: make\n"
        ))
        .expect("valid");
        let p = &spec.pipelines[0];
        assert_eq!(p.name, "ci");
        assert_eq!(p.jobs[0].timeout_minutes, 60);
        assert!(p.jobs[0].runs_on.is_empty());
        assert!(p.on.push.is_some());
    }

    #[test]
    fn names() {
        assert!(check_name("build (os=linux, rust=1.88)").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name(&"a".repeat(101)).is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name(" a").is_err());
        assert_eq!(file_stem(".fornax/pipelines/ci.yaml"), "ci");
        assert_eq!(file_stem("x.y.yml"), "x.y");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(7 * 86_400)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("0d"), None);
        assert_eq!(parse_duration("d"), None);
        assert_eq!(parse_duration("7x"), None);
        assert_eq!(parse_duration("99999999999999999999d"), None);
    }

    #[test]
    fn version_and_kind() {
        let e = errors("kind: Pipeline\nversion: 2\non: push\njobs: {}\n");
        assert!(e[0].contains("only `version: 1`"), "{e:?}");
        let e = errors("version: 1\n");
        assert!(e[0].contains("missing required field `kind`"), "{e:?}");
        let e = errors("kind: Job\nversion: 1\n");
        assert!(e[0].contains("`kind` must be"), "{e:?}");
        let e = errors("- a\n");
        assert!(e[0].contains("must be a mapping"), "{e:?}");
    }

    #[test]
    fn steps_and_fields() {
        let e = errors(&format!("{HEAD}jobs:\n  b:\n    steps:\n      - name: x\n"));
        assert!(
            e.iter().any(|m| m.contains("step 1 must have `run:`")),
            "{e:?}"
        );
        let e = errors(&format!("{HEAD}jobs:\n  b:\n    steps: []\n"));
        assert!(e.iter().any(|m| m.contains("at least one step")), "{e:?}");
        let e = errors(&format!(
            "{HEAD}jobs:\n  b:\n    runs-on: []\n    steps: [{{run: x}}]\n"
        ));
        assert!(
            e.iter().any(|m| m.contains("at least one runner label")),
            "{e:?}"
        );
        let e = errors(&format!(
            "{HEAD}jobs:\n  b:\n    timeout-minutes: 2000\n    steps: [{{run: x}}]\n"
        ));
        assert!(e.iter().any(|m| m.contains("from 1 to 1440")), "{e:?}");
        let e = errors(&format!(
            "{HEAD}jobs:\n  b:\n    steps: [{{run: x, working-directory: ../x}}]\n"
        ));
        assert!(
            e.iter().any(|m| m.contains("must not contain `..`")),
            "{e:?}"
        );
    }

    #[test]
    fn exclusive_filters() {
        let e = errors(
            "kind: Pipeline\nversion: 1\non:\n  push:\n    branches: [a]\n    branches-ignore: [b]\njobs:\n  b:\n    steps: [{run: x}]\n",
        );
        assert!(
            e.iter()
                .any(|m| m.starts_with("6:") && m.contains("cannot both be set")),
            "{e:?}"
        );
    }

    #[test]
    fn on_forms() {
        let spec = one(
            "kind: Pipeline\nversion: 1\non: [push, manual]\njobs:\n  b:\n    steps: [{run: x}]\n",
        )
        .expect("valid");
        assert!(spec.pipelines[0].on.manual);
        let e = errors(
            "kind: Pipeline\nversion: 1\non: schedule\njobs:\n  b:\n    steps: [{run: x}]\n",
        );
        assert!(e[0].contains("needs a list of cron"), "{e:?}");
        let e = errors("kind: Pipeline\nversion: 1\non: {}\njobs:\n  b:\n    steps: [{run: x}]\n");
        assert!(e[0].contains("enables no events"), "{e:?}");
        let e = errors(
            "kind: Pipeline\nversion: 1\non:\n  manual: { x: 1 }\njobs:\n  b:\n    steps: [{run: x}]\n",
        );
        assert!(e[0].contains("takes no fields"), "{e:?}");
    }

    #[test]
    fn matrix_limits() {
        let vals: Vec<String> = (0..17).map(|i| i.to_string()).collect();
        let list = vals.join(", ");
        let src = format!(
            "{HEAD}jobs:\n  b:\n    strategy:\n      matrix:\n        a: [{list}]\n        c: [{list}]\n    steps: [{{run: x}}]\n"
        );
        let e = errors(&src);
        assert!(e.iter().any(|m| m.contains("289 combinations")), "{e:?}");
        let src = format!(
            "{HEAD}jobs:\n  b:\n    strategy:\n      matrix:\n        os: [a/b]\n    steps: [{{run: x}}]\n"
        );
        let e = errors(&src);
        assert!(e.iter().any(|m| m.contains("matrix job name")), "{e:?}");
        let src = format!(
            "{HEAD}jobs:\n  b:\n    strategy:\n      matrix:\n        include: [x]\n    steps: [{{run: x}}]\n"
        );
        assert!(errors(&src).iter().any(|m| m.contains("not supported yet")));
    }

    #[test]
    fn pattern_limits() {
        let many: Vec<String> = (0..101).map(|i| format!("b{i}")).collect();
        let src = format!(
            "kind: Pipeline\nversion: 1\non:\n  push:\n    branches: [{}]\njobs:\n  b:\n    steps: [{{run: x}}]\n",
            many.join(", ")
        );
        let e = errors(&src);
        assert!(
            e.iter().any(|m| m.contains("at most 100 patterns")),
            "{e:?}"
        );
        let ok: Vec<String> = (0..100).map(|i| format!("b{i}")).collect();
        let src = format!(
            "kind: Pipeline\nversion: 1\non:\n  push:\n    paths: [{}]\njobs:\n  b:\n    steps: [{{run: x}}]\n",
            ok.join(", ")
        );
        one(&src).expect("100 patterns are fine");
        let long = "a".repeat(513);
        let src = format!(
            "kind: Pipeline\nversion: 1\non:\n  pull_request:\n    paths: [{long}]\njobs:\n  b:\n    steps: [{{run: x}}]\n"
        );
        let e = errors(&src);
        assert!(e.iter().any(|m| m.contains("longer than 512")), "{e:?}");
    }

    #[test]
    fn jobs_per_pipeline_limit() {
        let vals: Vec<String> = (0..200).map(|i| i.to_string()).collect();
        let list = vals.join(", ");
        let src = format!(
            "{HEAD}jobs:\n  a:\n    strategy:\n      matrix:\n        n: [{list}]\n    steps: [{{run: x}}]\n  b:\n    strategy:\n      matrix:\n        n: [{list}]\n    steps: [{{run: x}}]\n"
        );
        let e = errors(&src);
        assert!(e.iter().any(|m| m.contains("400 jobs")), "{e:?}");
        let src = format!(
            "{HEAD}jobs:\n  a:\n    strategy:\n      matrix:\n        n: [{list}]\n    steps: [{{run: x}}]\n  b:\n    steps: [{{run: x}}]\n"
        );
        assert_eq!(one(&src).expect("201 jobs").pipelines[0].jobs.len(), 201);
    }

    #[test]
    fn steps_per_job_limit() {
        let steps = |n: usize| "{run: x}, ".repeat(n);
        let e = errors(&format!("{HEAD}jobs:\n  b:\n    steps: [{}]\n", steps(101)));
        assert!(e.iter().any(|m| m.contains("at most 100 steps")), "{e:?}");
        one(&format!("{HEAD}jobs:\n  b:\n    steps: [{}]\n", steps(100))).expect("100 steps");
    }

    #[test]
    fn deployment_rules() {
        let d = "kind: Deployment\nversion: 1\nenvironment: staging\nsource: { path: deploy }\n";
        let ok = load(&[(
            ".fornax/d.yaml".into(),
            format!("{d}target: {{ type: kubernetes, namespace: app }}\n"),
        )])
        .expect("valid");
        assert_eq!(ok.deployments[0].target.kind, TargetType::Kubernetes);
        assert!(!ok.deployments[0].sync.auto);
        let e = load(&[(
            ".fornax/d.yaml".into(),
            format!("{d}target: {{ type: host }}\n"),
        )])
        .expect_err("host");
        assert!(e[0].message.contains("required for type `host`"));
        let e = load(&[(
            ".fornax/d.yaml".into(),
            format!("{d}target: {{ type: vm }}\npromotion: {{ to: staging }}\n"),
        )])
        .expect_err("bad");
        assert_eq!(e.len(), 2, "{e:?}");
    }
}
