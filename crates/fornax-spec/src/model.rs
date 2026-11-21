// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The typed `.fornax/` spec produced by [`crate::load`].

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use crate::expr::{Context, Expression};
use crate::graph::{GraphError, topo_order};
use crate::trigger::{Event, On, on_matches};

/// Default job timeout in minutes.
pub const DEFAULT_TIMEOUT_MINUTES: u32 = 60;
/// Maximum job/step timeout in minutes (24 h).
pub const MAX_TIMEOUT_MINUTES: u32 = 1440;
/// Maximum number of combinations one matrix may expand to.
pub const MAX_MATRIX_COMBINATIONS: usize = 256;
/// Maximum number of concrete jobs in one pipeline, after matrix expansion.
pub const MAX_JOBS_PER_PIPELINE: usize = 256;
/// Maximum number of steps in one job.
pub const MAX_STEPS_PER_JOB: usize = 100;

/// Every pipeline and deployment found under `.fornax/`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spec {
    /// `kind: Pipeline` files, in input order.
    pub pipelines: Vec<Pipeline>,
    /// `kind: Deployment` files, in input order.
    pub deployments: Vec<Deployment>,
}

impl Spec {
    /// Pipelines whose `on:` section matches `event`.
    pub fn triggered_pipelines<'a>(
        &'a self,
        event: &'a Event,
    ) -> impl Iterator<Item = &'a Pipeline> + 'a {
        self.pipelines
            .iter()
            .filter(move |p| p.is_triggered_by(event))
    }

    /// Look up a pipeline by name.
    pub fn pipeline(&self, name: &str) -> Option<&Pipeline> {
        self.pipelines.iter().find(|p| p.name == name)
    }

    /// Look up a deployment by name.
    pub fn deployment(&self, name: &str) -> Option<&Deployment> {
        self.deployments.iter().find(|d| d.name == name)
    }
}

/// A `kind: Pipeline` file.
#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    /// `name:` or the file stem.
    pub name: String,
    /// Source file path.
    pub file: String,
    /// Triggers.
    pub on: On,
    /// Pipeline variables (also readable as `vars.NAME` in expressions).
    pub variables: BTreeMap<String, String>,
    /// Concrete jobs in declaration order, with matrix jobs expanded.
    pub jobs: Vec<Job>,
}

impl Pipeline {
    /// Whether `event` triggers this pipeline.
    pub fn is_triggered_by(&self, event: &Event) -> bool {
        on_matches(&self.on, event)
    }

    /// Look up a concrete job by (expanded) name.
    pub fn job(&self, name: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.name == name)
    }

    /// Jobs in dependency order: every job comes after all jobs it needs; among jobs that
    /// are ready at the same time, declaration order wins. Loaded pipelines are always
    /// acyclic; the error is only possible for hand-built values.
    pub fn topological_order(&self) -> Result<Vec<&Job>, GraphError> {
        let index: HashMap<&str, usize> = self
            .jobs
            .iter()
            .enumerate()
            .map(|(i, j)| (j.name.as_str(), i))
            .collect();
        let mut deps = Vec::with_capacity(self.jobs.len());
        for job in &self.jobs {
            let mut ds = Vec::with_capacity(job.needs.len());
            for need in &job.needs {
                match index.get(need.as_str()) {
                    Some(&i) => ds.push(i),
                    None => {
                        return Err(GraphError::UnknownNeed {
                            job: job.name.clone(),
                            need: need.clone(),
                        });
                    }
                }
            }
            deps.push(ds);
        }
        topo_order(&deps)
            .map(|order| order.into_iter().map(|i| &self.jobs[i]).collect())
            .map_err(|cycle| {
                GraphError::Cycle(
                    cycle
                        .into_iter()
                        .map(|i| self.jobs[i].name.clone())
                        .collect(),
                )
            })
    }
}

/// A concrete job (one matrix combination, or the job itself without a matrix).
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// Concrete name, e.g. `build` or `build (os=linux, rust=1.88)`.
    pub name: String,
    /// The key under `jobs:`.
    pub base_name: String,
    /// This job's matrix values in key order (empty without a matrix).
    pub matrix: Vec<(String, String)>,
    /// Runner labels; empty means any runner.
    pub runs_on: Vec<String>,
    /// Concrete names of the jobs this job needs (a matrix job expands to all its
    /// combinations).
    pub needs: Vec<String>,
    /// `if:` condition.
    pub condition: Option<Expression>,
    /// Job environment.
    pub env: BTreeMap<String, String>,
    /// Timeout in minutes (1..=1440, default 60).
    pub timeout_minutes: u32,
    /// Steps (never empty).
    pub steps: Vec<Step>,
    /// `strategy.max-parallel`: how many combinations of `base_name` may run at once.
    pub max_parallel: Option<u32>,
    /// Artifacts to keep.
    pub artifacts: Option<Artifacts>,
    /// Environment this job deploys to.
    pub environment: Option<String>,
    /// Deployment to trigger after the job succeeds.
    pub trigger_deployment: Option<String>,
}

impl Job {
    /// Evaluate `if:` (true when there is none). The job's matrix values are added to the
    /// context as `matrix.NAME`.
    pub fn condition_holds(&self, ctx: &Context) -> bool {
        match &self.condition {
            None => true,
            Some(expr) if self.matrix.is_empty() => expr.is_true(ctx),
            Some(expr) => expr.is_true(&ctx.clone().with_matrix(&self.matrix)),
        }
    }
}

/// One step of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Display name.
    pub name: Option<String>,
    /// Shell script to run on the runner.
    pub run: String,
    /// Step environment (overrides the job's).
    pub env: BTreeMap<String, String>,
    /// Step timeout in minutes.
    pub timeout_minutes: Option<u32>,
    /// Working directory relative to the checkout.
    pub working_directory: Option<String>,
}

/// `artifacts:` of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifacts {
    /// Paths/globs relative to the checkout.
    pub paths: Vec<String>,
    /// How long to keep them (`expire-in`, e.g. `7d`).
    pub expire_in: Option<Duration>,
}

/// A `kind: Deployment` file (parsed and validated; reconciled by the GitOps engine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    /// `name:` or the file stem.
    pub name: String,
    /// Source file path.
    pub file: String,
    /// Target environment name.
    pub environment: String,
    /// What to deploy.
    pub source: Source,
    /// Where to deploy.
    pub target: Target,
    /// Sync policy.
    pub sync: SyncPolicy,
    /// Promotion to another environment.
    pub promotion: Option<Promotion>,
}

/// `source:` of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Directory in the repository, relative to the root.
    pub path: String,
}

/// Deployment target kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetType {
    /// A plain host (files + commands).
    Host,
    /// A Docker Compose project.
    DockerCompose,
    /// A Kubernetes cluster.
    Kubernetes,
}

impl TargetType {
    /// The YAML spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            TargetType::Host => "host",
            TargetType::DockerCompose => "docker-compose",
            TargetType::Kubernetes => "kubernetes",
        }
    }
}

/// `target:` of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// `type:`.
    pub kind: TargetType,
    /// Registered host name (required for `host`, optional for `docker-compose`).
    pub host: Option<String>,
    /// Kubernetes namespace (`kubernetes` only).
    pub namespace: Option<String>,
    /// Compose project name (`docker-compose` only).
    pub project: Option<String>,
}

/// `sync:` of a deployment (all default to false).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncPolicy {
    /// Apply new commits automatically.
    pub auto: bool,
    /// Delete resources that are no longer in the source.
    pub prune: bool,
    /// Revert drift automatically.
    pub self_heal: bool,
}

/// `promotion:` of a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promotion {
    /// Environment to promote to.
    pub to: String,
    /// Whether a person must approve.
    pub requires_approval: bool,
}
