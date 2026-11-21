// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Golden tests over `tests/fixtures/`.
//!
//! - `valid/<dir>/`: every file in the directory is loaded together and must succeed.
//! - `invalid/<name>.yaml` or `invalid/<dir>/`: loading must fail with exactly the
//!   diagnostics in `invalid/<name>.expected` (one `file:line:column: message` per line).
//!
//! Set `FORNAX_BLESS=1` to rewrite the `.expected` files from the current output.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use fornax_spec::{Context, Event, Spec, TargetType, load};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Load a fixture file or directory as `.fornax/<file>` entries.
fn read(path: &Path) -> Vec<(String, String)> {
    let mut files: Vec<PathBuf> = if path.is_dir() {
        fs::read_dir(path)
            .expect("read dir")
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|e| e == "yaml" || e == "yml"))
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let name = p.file_name().expect("name").to_string_lossy().into_owned();
            (
                format!(".fornax/{name}"),
                fs::read_to_string(&p).expect("read fixture"),
            )
        })
        .collect()
}

fn load_valid(name: &str) -> Spec {
    let files = read(&fixtures().join("valid").join(name));
    match load(&files) {
        Ok(spec) => spec,
        Err(diags) => panic!(
            "{name} should be valid:\n{}",
            diags
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

#[test]
fn invalid_fixtures_match_expected_diagnostics() {
    let dir = fixtures().join("invalid");
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read invalid dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.is_dir() || p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    entries.sort();
    assert!(entries.len() >= 9, "fixtures missing");
    let bless = std::env::var_os("FORNAX_BLESS").is_some();
    let mut failures = Vec::new();
    for entry in entries {
        let stem = entry
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .into_owned();
        let expected_path = dir.join(format!("{stem}.expected"));
        let actual = match load(&read(&entry)) {
            Ok(_) => "<loaded successfully>\n".to_string(),
            Err(diags) => diags.iter().map(|d| format!("{d}\n")).collect::<String>(),
        };
        if bless {
            fs::write(&expected_path, &actual).expect("write expected");
            continue;
        }
        let expected = fs::read_to_string(&expected_path).unwrap_or_default();
        if expected != actual {
            failures.push(format!(
                "--- {stem}: expected\n{expected}--- actual\n{actual}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn full_example_loads() {
    let spec = load_valid("full");
    assert_eq!(spec.pipelines.len(), 1);
    assert_eq!(spec.deployments.len(), 1);
    let p = &spec.pipelines[0];
    assert_eq!(p.name, "ci");
    assert_eq!(
        p.variables.get("RUST_LOG").map(String::as_str),
        Some("info")
    );

    let names: Vec<&str> = p.jobs.iter().map(|j| j.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "build (os=linux, rust=1.88)",
            "build (os=linux, rust=1.90)",
            "build (os=macos, rust=1.88)",
            "build (os=macos, rust=1.90)",
            "test",
            "lint",
            "deploy-staging",
        ]
    );
    let build = &p.jobs[0];
    assert_eq!(build.base_name, "build");
    assert_eq!(build.max_parallel, Some(2));
    assert_eq!(build.runs_on, vec!["linux", "host"]);
    assert_eq!(build.steps[0].timeout_minutes, Some(30));
    assert_eq!(build.steps[0].working_directory.as_deref(), Some("app"));
    let artifacts = build.artifacts.as_ref().expect("artifacts");
    assert_eq!(artifacts.expire_in.map(|d| d.as_secs()), Some(7 * 86_400));

    let test = p.job("test").expect("test job");
    assert_eq!(test.needs.len(), 4, "needs all matrix combinations");
    assert_eq!(p.job("lint").expect("lint").timeout_minutes, 10);

    let order: Vec<&str> = p
        .topological_order()
        .expect("acyclic")
        .into_iter()
        .map(|j| j.name.as_str())
        .collect();
    assert_eq!(
        order,
        vec![
            "build (os=linux, rust=1.88)",
            "build (os=linux, rust=1.90)",
            "build (os=macos, rust=1.88)",
            "build (os=macos, rust=1.90)",
            "test",
            "lint",
            "deploy-staging",
        ]
    );

    let d = &spec.deployments[0];
    assert_eq!(d.name, "staging-app");
    assert_eq!(d.target.kind, TargetType::DockerCompose);
    assert!(d.sync.auto && !d.sync.prune && !d.sync.self_heal);
    let promo = d.promotion.as_ref().expect("promotion");
    assert_eq!(promo.to, "production");
    assert!(promo.requires_approval);
    assert_eq!(
        p.job("deploy-staging")
            .and_then(|j| j.trigger_deployment.as_deref()),
        Some("staging-app")
    );
}

#[test]
fn push_to_jobs_end_to_end() {
    let spec = load_valid("full");
    let push = |r: &str, paths: &[&str]| Event::Push {
        ref_name: r.into(),
        changed_paths: Some(paths.iter().map(|s| s.to_string()).collect()),
    };

    let main = push("refs/heads/main", &["src/lib.rs"]);
    let triggered: Vec<_> = spec.triggered_pipelines(&main).collect();
    assert_eq!(triggered.len(), 1);
    let ctx = Context::for_event(&main).with_vars(&triggered[0].variables);
    let deploy = triggered[0].job("deploy-staging").expect("job");
    assert!(deploy.condition_holds(&ctx));

    let release = push("refs/heads/release/1.0", &["src/lib.rs"]);
    let p = spec
        .triggered_pipelines(&release)
        .next()
        .expect("triggered");
    assert!(
        !p.job("deploy-staging")
            .expect("job")
            .condition_holds(&Context::for_event(&release))
    );

    assert_eq!(
        spec.triggered_pipelines(&push("refs/heads/main", &["README.md"]))
            .count(),
        0
    );
    assert_eq!(
        spec.triggered_pipelines(&push("refs/heads/dev", &["src/x"]))
            .count(),
        0
    );
    assert_eq!(
        spec.triggered_pipelines(&push("refs/tags/v1", &[])).count(),
        0
    );
    assert_eq!(
        spec.triggered_pipelines(&Event::PullRequest {
            base_branch: "anything".into(),
            changed_paths: None
        })
        .count(),
        1
    );
    assert_eq!(spec.triggered_pipelines(&Event::Manual).count(), 0);
    assert_eq!(spec.triggered_pipelines(&Event::Schedule).count(), 1);

    let cron = &spec.pipelines[0].on.schedule[0];
    let friday = Utc
        .with_ymd_and_hms(2026, 11, 20, 4, 0, 0)
        .single()
        .expect("time");
    assert_eq!(
        cron.next_after(friday),
        Utc.with_ymd_and_hms(2026, 11, 23, 3, 0, 0).single()
    );
}

/// Deterministic PRNG for the no-panic property test.
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
fn garbled_files_never_panic() {
    let base = fs::read_to_string(fixtures().join("valid/full/ci.yaml")).expect("fixture");
    let chars: Vec<char> = base.chars().collect();
    let extra: Vec<char> = "{}[]:,-&*!|>'\"#%@`\t\n ?".chars().collect();
    let mut rng = Rng(42);
    for _ in 0..1_500 {
        let mut m = chars.clone();
        for _ in 0..1 + rng.below(6) {
            if m.is_empty() {
                break;
            }
            let i = rng.below(m.len());
            match rng.below(4) {
                0 => {
                    m.remove(i);
                }
                1 => m.insert(i, extra[rng.below(extra.len())]),
                2 => m.insert(i, chars[rng.below(chars.len())]),
                _ => m[i] = extra[rng.below(extra.len())],
            }
        }
        let src: String = m.into_iter().collect();
        let _ = load(&[(".fornax/ci.yaml".to_string(), src)]);
    }
}
