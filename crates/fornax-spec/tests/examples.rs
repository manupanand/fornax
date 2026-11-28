// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The documented examples under `examples/` must stay valid `.fornax/` configs.

use std::path::Path;

use fornax_spec::{TargetType, load};

fn fornax_files(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root.join(".fornax")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read .fornax") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
                let rel = path
                    .strip_prefix(root)
                    .expect("relative")
                    .to_string_lossy()
                    .into_owned();
                out.push((rel, std::fs::read_to_string(&path).expect("read")));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn compose_deploy_example_is_valid() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/compose-deploy");
    let spec = match load(&fornax_files(&root)) {
        Ok(spec) => spec,
        Err(diags) => panic!("example does not load: {diags:#?}"),
    };
    let d = spec.deployment("client-a-prod").expect("deployment");
    assert_eq!(d.environment, "client-a-prod");
    assert_eq!(d.source.path, "deploy/client-a");
    assert_eq!(d.target.kind, TargetType::DockerCompose);
    assert_eq!(d.target.project.as_deref(), Some("client-a"));
    assert!(d.sync.auto && !d.sync.prune && !d.sync.self_heal);
    assert!(root.join("deploy/client-a/docker-compose.yml").exists());
}
