// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The `.fornax/` specification: pipeline (CI) and deployment (CD) files.
//!
//! This crate is a pure library (no I/O, no async). The server reads every
//! `*.yaml`/`*.yml` file under `.fornax/` at a commit and hands their contents to [`load`],
//! which returns a validated [`Spec`] or a list of [`Diagnostic`]s with file/line/column.
//!
//! Turning a push into jobs:
//!
//! ```
//! use fornax_spec::{load, Context, Event};
//!
//! let files = vec![(
//!     ".fornax/ci.yaml".to_string(),
//!     "kind: Pipeline\nversion: 1\non:\n  push: { branches: [main] }\njobs:\n  build:\n    steps:\n      - run: cargo build\n".to_string(),
//! )];
//! let spec = load(&files).expect("valid spec");
//! let event = Event::Push { ref_name: "refs/heads/main".into(), changed_paths: None };
//! for pipeline in spec.triggered_pipelines(&event) {
//!     let ctx = Context::for_event(&event).with_vars(&pipeline.variables);
//!     for job in pipeline.topological_order().expect("acyclic") {
//!         if job.condition_holds(&ctx) {
//!             // enqueue `job` ...
//!         }
//!     }
//! }
//! ```
//!
//! Modules:
//! - [`model`]: the typed spec ([`Pipeline`], [`Job`], [`Deployment`], …).
//! - [`expr`]: the `if:` expression language (LOG D-034).
//! - [`pattern`] and [`trigger`]: GitHub-Actions-style branch/tag/path filters (C-009, U2).
//! - [`cron`]: 5-field cron schedules evaluated in UTC (U1).
//! - [`schema`]: JSON Schema for editors.

#![deny(missing_docs)]

pub mod cron;
mod diag;
pub mod expr;
mod graph;
mod load;
pub mod model;
pub mod pattern;
pub mod schema;
pub mod trigger;
mod yaml;

pub use cron::{CronError, CronSchedule};
pub use diag::Diagnostic;
pub use expr::{Context, ExprError, Expression, Value};
pub use graph::GraphError;
pub use load::load;
pub use model::{
    Artifacts, Deployment, Job, Pipeline, Promotion, Source, Spec, Step, SyncPolicy, Target,
    TargetType,
};
pub use pattern::{Pattern, PatternError, PatternList};
pub use schema::json_schema;
pub use trigger::{Event, On, PullRequestFilter, PushFilter, on_matches};
