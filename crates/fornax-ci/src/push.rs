// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Push → pipelines.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use fornax_db::{Db, NewJob, NewPipeline, Pipeline as DbPipeline, Repo};
use fornax_git::{GitError, RepoStore};
use fornax_spec::{Context, Diagnostic, Event, Pipeline, Spec, load};

use crate::CiError;
use crate::payload::{JobPayload, PAYLOAD_VERSION, StepPayload};
use crate::refs::RefUpdate;

/// Branch/tag updates processed per push; further updates are listed in
/// [`PushOutcome::skipped_refs`] and start no pipelines (bounds the work one push can cause).
pub const MAX_REF_UPDATES: usize = 20;
/// Pipelines created per ref update; further triggered pipelines are reported as a
/// diagnostic in [`PushOutcome::errors`].
pub const MAX_PIPELINES_PER_REF: usize = 20;

/// Jobs created per push, over all refs and pipelines; a pipeline that would go past it is
/// not created and is reported as a diagnostic. (Without it one push could create
/// 20 refs × 20 pipelines × 256 jobs.)
pub const MAX_JOBS_PER_PUSH: usize = 1000;
/// Steps per job (also a spec validation error; checked again before jobs are created).
pub const MAX_STEPS_PER_JOB: usize = fornax_spec::model::MAX_STEPS_PER_JOB;
/// Encoded [`JobPayload`] size per job in bytes (256 KiB); a pipeline with a larger job is
/// not created and is reported as a diagnostic.
pub const MAX_JOB_PAYLOAD_BYTES: usize = 256 * 1024;

/// Retries are not configurable in the spec yet: every job runs once.
const DEFAULT_MAX_ATTEMPTS: u32 = 1;

/// What [`on_push`] did.
#[derive(Debug, Clone, Default)]
pub struct PushOutcome {
    /// Pipelines created, in ref order and then spec file order.
    pub created: Vec<DbPipeline>,
    /// Problems per full ref name: invalid or unreadable `.fornax/` files, or pipelines not
    /// started because of [`MAX_PIPELINES_PER_REF`]. A ref with spec errors starts nothing.
    pub errors: Vec<(String, Vec<Diagnostic>)>,
    /// Full ref names not processed because the push updated more than
    /// [`MAX_REF_UPDATES`] branches/tags.
    pub skipped_refs: Vec<String>,
}

/// Create the pipelines triggered by a push.
///
/// For every created or updated branch (`refs/heads/…`) or tag (`refs/tags/…`) — deleted
/// refs and other namespaces are ignored — this reads `.fornax/` at the new commit, loads
/// the spec and creates one pipeline per spec pipeline whose `on.push` filter matches
/// [`Event::Push`]. `changed_paths` is the diff against the old commit for updated
/// branches, and `None` (path filters pass) for new refs, tags, diffs too large to list
/// ([`fornax_git::MAX_CHANGED_PATHS`]) or diffs that cannot be computed.
///
/// Jobs: each job's `if:` is evaluated with `event`, `ref`, `branch`/`tag`, `sha`, `repo`
/// (`owner/name`) and the pipeline `variables` as `vars.*`. **A job whose condition is
/// false is left out of the pipeline, and so is every job that needs it, directly or
/// transitively** — such a job could never run, and leaving it out keeps the stored DAG
/// valid (`needs` only name existing jobs). A pipeline whose jobs are all left out is not
/// created. Pushes are trusted (only users with write access can push); each job runs once
/// (`max_attempts = 1`) on runners offering all its `runs-on` labels.
///
/// Limits: [`MAX_REF_UPDATES`], [`MAX_PIPELINES_PER_REF`], [`MAX_JOBS_PER_PUSH`],
/// [`MAX_STEPS_PER_JOB`] and [`MAX_JOB_PAYLOAD_BYTES`]. Loading specs and evaluating
/// triggers and conditions is CPU work on untrusted input, so it runs on the blocking
/// thread pool (`spawn_blocking`), never on an async worker.
///
/// Spec problems never fail the push; they are returned in [`PushOutcome::errors`].
/// Errors are returned only for database or repository failures; pipelines created before
/// such a failure stay created. The caller should wake waiting runners afterwards.
pub async fn on_push(
    db: &Db,
    repos: &RepoStore,
    repo: &Repo,
    pusher: Option<i64>,
    updates: &[RefUpdate],
    now: i64,
) -> Result<PushOutcome, CiError> {
    let (owner, name) = repo.storage_key();
    let path = repos.repo_path(&owner, &name);
    let full_name = repo.full_name();
    let repo_id = repo.id;
    let mut outcome = PushOutcome::default();
    let mut jobs_created = 0usize;

    let candidates = updates
        .iter()
        .filter(|u| !u.is_delete() && (u.branch().is_some() || u.tag().is_some()));
    for (i, update) in candidates.enumerate() {
        if i >= MAX_REF_UPDATES {
            outcome.skipped_refs.push(update.ref_name.clone());
            continue;
        }
        let Some(new) = update.new.as_deref() else {
            continue;
        };
        let sha = match repos.resolve_commit(&path, new).await {
            Ok(sha) => sha,
            // The ref points at something other than a commit (e.g. a tag of a tree).
            Err(GitError::NotFound(_)) => continue,
            Err(e) => return Err(e.into()),
        };

        let fornax = repos.fornax_files(&path, &sha).await?;
        let mut diags: Vec<Diagnostic> = fornax
            .problems
            .iter()
            .map(|p| Diagnostic::new(&p.path, 1, 1, &p.message))
            .collect();
        if fornax.files.is_empty() && diags.is_empty() {
            continue;
        }
        let files = fornax.files;
        let loaded = tokio::task::spawn_blocking(move || load(&files)).await?;
        let spec = match loaded {
            Ok(spec) if diags.is_empty() => spec,
            Ok(_) => {
                outcome.errors.push((update.ref_name.clone(), diags));
                continue;
            }
            Err(mut errs) => {
                diags.append(&mut errs);
                outcome.errors.push((update.ref_name.clone(), diags));
                continue;
            }
        };

        let event = Event::Push {
            ref_name: update.ref_name.clone(),
            changed_paths: changed_paths(repos, &path, update, &sha).await,
        };
        let budget = MAX_JOBS_PER_PUSH.saturating_sub(jobs_created);
        let (planned, mut more) = {
            let (full_name, sha) = (full_name.clone(), sha.clone());
            tokio::task::spawn_blocking(move || {
                plan_all(&spec, &event, &full_name, repo_id, pusher, &sha, budget)
            })
            .await??
        };
        diags.append(&mut more);
        for new_pipeline in planned {
            jobs_created += new_pipeline.jobs.len();
            outcome
                .created
                .push(db.create_pipeline(&new_pipeline, now).await?);
        }
        if !diags.is_empty() {
            outcome.errors.push((update.ref_name.clone(), diags));
        }
    }
    Ok(outcome)
}

/// Changed paths for path filters; `None` when unknown (see [`on_push`]).
async fn changed_paths(
    repos: &RepoStore,
    path: &Path,
    update: &RefUpdate,
    new_sha: &str,
) -> Option<Vec<String>> {
    // Path filters are not evaluated for tags.
    update.branch()?;
    let old = update.old.as_deref()?;
    if old.bytes().all(|b| b == b'0') {
        return None;
    }
    repos.changed_paths(path, old, new_sha).await.ok().flatten()
}

/// Plan the pipelines of `spec` that `event` triggers, in spec order, applying
/// [`MAX_PIPELINES_PER_REF`] and the remaining per-push job budget (`job_budget`, from
/// [`MAX_JOBS_PER_PUSH`]). Returns the pipelines to create and diagnostics for those left
/// out. Pipelines past [`MAX_PIPELINES_PER_REF`] are not planned at all, so the work (and
/// memory for payloads) per ref stays bounded.
///
/// CPU-bound (trigger filters, `if:` conditions, payload encoding): run it off the async
/// workers.
fn plan_all(
    spec: &Spec,
    event: &Event,
    repo_full_name: &str,
    repo_id: i64,
    pusher: Option<i64>,
    sha: &str,
    job_budget: usize,
) -> Result<(Vec<NewPipeline>, Vec<Diagnostic>), CiError> {
    let mut planned: Vec<NewPipeline> = Vec::new();
    let mut diags = Vec::new();
    let mut jobs_left = job_budget;
    for pipeline in spec.triggered_pipelines(event) {
        let not_started = |why: String| {
            Diagnostic::new(
                &pipeline.file,
                1,
                1,
                format!("pipeline {:?} not started: {why}", pipeline.name),
            )
        };
        if planned.len() == MAX_PIPELINES_PER_REF {
            diags.push(not_started(format!(
                "one push may start at most {MAX_PIPELINES_PER_REF} pipelines per ref"
            )));
            continue;
        }
        match plan_pipeline(repo_full_name, repo_id, pusher, pipeline, event, sha)? {
            Plan::Create(p) if p.jobs.len() > jobs_left => {
                diags.push(not_started(format!(
                    "one push may create at most {MAX_JOBS_PER_PUSH} jobs; this pipeline has \
                     {} and {jobs_left} are left",
                    p.jobs.len()
                )));
            }
            Plan::Create(p) => {
                jobs_left -= p.jobs.len();
                planned.push(p);
            }
            Plan::Nothing => {}
            Plan::Invalid(d) => diags.push(d),
        }
    }
    Ok((planned, diags))
}

/// Result of planning one triggered spec pipeline.
enum Plan {
    /// Create this pipeline.
    Create(NewPipeline),
    /// Every job was left out.
    Nothing,
    /// The pipeline cannot be created: its job graph is unusable (not possible for loaded
    /// specs) or a job is over [`MAX_STEPS_PER_JOB`] / [`MAX_JOB_PAYLOAD_BYTES`].
    Invalid(Diagnostic),
}

/// Select the jobs of `pipeline` for `event` and build the database pipeline.
fn plan_pipeline(
    repo_full_name: &str,
    repo_id: i64,
    pusher: Option<i64>,
    pipeline: &Pipeline,
    event: &Event,
    sha: &str,
) -> Result<Plan, CiError> {
    let Event::Push { ref_name, .. } = event else {
        return Ok(Plan::Nothing);
    };
    let mut ctx = Context::for_event(event).with_vars(&pipeline.variables);
    ctx.sha = Some(sha.to_owned());
    ctx.repo = Some(repo_full_name.to_owned());

    let order = match pipeline.topological_order() {
        Ok(order) => order,
        Err(e) => {
            return Ok(Plan::Invalid(Diagnostic::new(
                &pipeline.file,
                1,
                1,
                e.to_string(),
            )));
        }
    };
    // Dependency order: every job's needs are decided before the job itself.
    let mut included: HashSet<&str> = HashSet::new();
    for job in order {
        if job.needs.iter().all(|n| included.contains(n.as_str())) && job.condition_holds(&ctx) {
            included.insert(job.name.as_str());
        }
    }
    if included.is_empty() {
        return Ok(Plan::Nothing);
    }

    let mut jobs = Vec::with_capacity(included.len());
    // Declaration order, so job ids follow the file.
    for job in pipeline
        .jobs
        .iter()
        .filter(|j| included.contains(j.name.as_str()))
    {
        if job.steps.len() > MAX_STEPS_PER_JOB {
            return Ok(Plan::Invalid(Diagnostic::new(
                &pipeline.file,
                1,
                1,
                format!(
                    "pipeline {:?} not started: job `{}` has {} steps; the limit is \
                     {MAX_STEPS_PER_JOB}",
                    pipeline.name,
                    job.name,
                    job.steps.len()
                ),
            )));
        }
        let mut env: BTreeMap<String, String> = pipeline.variables.clone();
        env.extend(job.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        let payload = JobPayload {
            v: PAYLOAD_VERSION,
            name: job.name.clone(),
            steps: job
                .steps
                .iter()
                .enumerate()
                .map(|(i, step)| StepPayload {
                    name: step
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("step {}", i + 1)),
                    run: step.run.clone(),
                    env: step.env.clone(),
                    timeout_secs: step.timeout_minutes.map(|m| u64::from(m) * 60),
                    working_directory: step.working_directory.clone(),
                })
                .collect(),
            env,
            timeout_secs: u64::from(job.timeout_minutes) * 60,
            repo_full_name: repo_full_name.to_owned(),
            sha: sha.to_owned(),
            git_ref: ref_name.clone(),
        };
        let payload = payload.to_json()?;
        if payload.len() > MAX_JOB_PAYLOAD_BYTES {
            return Ok(Plan::Invalid(Diagnostic::new(
                &pipeline.file,
                1,
                1,
                format!(
                    "pipeline {:?} not started: job `{}` has an encoded job payload of {} \
                     bytes; the limit is {MAX_JOB_PAYLOAD_BYTES}",
                    pipeline.name,
                    job.name,
                    payload.len()
                ),
            )));
        }
        jobs.push(NewJob {
            name: job.name.clone(),
            labels: job.runs_on.clone(),
            payload,
            needs: job.needs.clone(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        });
    }
    Ok(Plan::Create(NewPipeline {
        repo_id,
        file: pipeline.file.clone(),
        name: pipeline.name.clone(),
        git_ref: ref_name.clone(),
        sha: sha.to_owned(),
        event: event.name().to_owned(),
        trusted: true,
        triggered_by: pusher,
        jobs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_many_steps_are_reported() {
        let src = "kind: Pipeline\nversion: 1\non: push\njobs:\n  b:\n    steps: [{ run: x }]\n";
        let spec = load(&[(".fornax/ci.yaml".to_owned(), src.to_owned())]).expect("valid");
        let mut pipeline = spec.pipelines[0].clone();
        let step = pipeline.jobs[0].steps[0].clone();
        pipeline.jobs[0].steps = vec![step; 101];
        let event = Event::Push {
            ref_name: "refs/heads/main".into(),
            changed_paths: None,
        };
        let plan = plan_pipeline("o/r", 1, None, &pipeline, &event, "abc").expect("plan");
        match plan {
            Plan::Invalid(d) => assert!(d.message.contains("steps"), "{d:?}"),
            _ => panic!("expected Plan::Invalid"),
        }
    }
}
