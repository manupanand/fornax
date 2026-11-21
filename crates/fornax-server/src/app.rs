// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! The HTTP application served on the main port: REST API, Git smart HTTP and web UI
//! (`fornax_api::router`) plus the gRPC runner protocol at `/fornax.runner.v1.Runner/*`.
//!
//! `axum::serve` (with axum's `http2` feature) negotiates HTTP/1.1 or HTTP/2 per connection
//! (h2c prior knowledge on plain TCP), so agents reach gRPC on the same port as browsers and
//! `git`. Behind a TLS-terminating proxy the proxy must forward HTTP/2 (gRPC) to this port.

use std::sync::Arc;

use axum::Router;
use fornax_api::AppState;
use fornax_runner::RunnerService;

/// The full router: API + git + UI, with the runner service merged in.
pub fn build(state: Arc<AppState>, runner: RunnerService) -> Router {
    fornax_api::router(state).merge(runner.router())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use fornax_api::ApiConfig;
    use fornax_db::Db;
    use fornax_git::RepoStore;
    use fornax_proto::runner::v1::NextRequest;
    use fornax_proto::runner::v1::runner_client::RunnerClient;
    use fornax_runner::{QueueNotifier, RunnerConfig};
    use tonic::Code;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn grpc_and_http_share_one_port() {
        let tmp = tempfile::tempdir().expect("tmp");
        let db = Db::open(&tmp.path().join("fornax.db")).await.expect("db");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let state = Arc::new(AppState {
            db: db.clone(),
            repos: RepoStore::new(tmp.path().join("repos")),
            config: ApiConfig {
                public_url: base.clone(),
                ..ApiConfig::default()
            },
            oidc: None,
            ci_wakeup: fornax_ci::Wakeup::noop(),
        });
        let runner = RunnerService::new(db.clone(), RunnerConfig::new(&base), QueueNotifier::new());
        let app = build(state, runner);
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        // HTTP/1.1 REST.
        let res = reqwest::get(format!("{base}/api/v1/health"))
            .await
            .expect("health");
        assert!(res.status().is_success());

        // gRPC over h2c on the same port.
        let (_, token) = fornax_runner::create_runner(&db, "r1", &["linux"], true, 0)
            .await
            .expect("runner");
        let mut anon = RunnerClient::connect(base.clone()).await.expect("connect");
        let err = anon
            .next(NextRequest { capacity_free: 1 })
            .await
            .expect_err("unauthenticated");
        assert_eq!(err.code(), Code::Unauthenticated);
        let mut req = tonic::Request::new(NextRequest { capacity_free: 1 });
        req.set_timeout(Duration::from_secs(2));
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().expect("ascii"),
        );
        let resp = anon.next(req).await.expect("next");
        assert!(resp.into_inner().job.is_none());
    }
}
