# GCP Cloud Source Repositories (CSR) and Secure Source Manager (SSM) — integration surface

Sources: official Google Cloud docs (checked 2026-11-19, via web search summaries, **not** a
full read of every page — verify field names against the live docs before implementing).

## Cloud Source Repositories (first integration, D-010)

| Concern | Finding | Source |
|---|---|---|
| Availability | Closed to new customers since **2024-06-17**; existing orgs can keep using it, including on new projects in that org; shutdown date to be announced ≥ 1 year ahead | [release notes](https://cloud.google.com/source-repositories/docs/release-notes), [migration guide](https://docs.cloud.google.cn/source-repositories/docs/migration-guides?hl=en) |
| Change events | **Pub/Sub notifications** (not HTTP webhooks). Topic configured per project or per repo via `gcloud source project-configs` / `repos update`; message format JSON or protobuf (`--message-format`) | [Notifications for CSR](https://docs.cloud.google.com/source-repositories/docs/pubsub-notifications), [Configuring notifications](https://docs.cloud.google.com/source-repositories/docs/configuring-notifications) |
| Event shape | `refUpdateEvent` with a map of ref name → ref update (ref name, old/new commit, update type); also repo create/delete events. Protobuf shape mirrored in Chromium's `SourceRepoEvent` bindings (third-party, reference only) | same as above; [luci-go `source_repo_event.pb.go`](https://chromium.googlesource.com/infra/luci/luci-go/+/b00442272dfc/common/proto/gerrit/source_repo_event.pb.go) |
| Permissions | `source.repos.updateRepo` (repo) or `source.repos.updateProjectConfig` (project) to configure; a service account allowed to publish to the topic | Notifications doc |
| Git transport | HTTPS clone `https://source.developers.google.com/p/<project>/r/<repo>`; auth with Google credentials (gcloud credential helper / OAuth access token for a service account) — **assumption**, standard usage, not re-read | — |
| Commit status / PR API | None: CSR has no pull requests or commit-status API (**assumption**, consistent with product scope) | — |

### Implications for Fornax
- Ingest = **Pub/Sub pull subscription** (Fornax server holds a subscriber with a service-account key or Workload Identity) → normalized `RefUpdated` event → pipeline trigger. Fall back to **periodic `git ls-remote` polling** when Pub/Sub isn't configured.
- No status reporting back to CSR → results visible only in Fornax UI (and optionally Cloud Build-style badges).
- Because CSR is closed to new customers, the integration should be thin and isolated: `fornax-git-provider-csr` behind a `GitProvider` trait.

## Secure Source Manager (successor recommended by Google)

| Concern | Finding | Source |
|---|---|---|
| Webhooks | HTTP webhooks configured in the SSM web UI; events: push, pull-request state change, pull-request comment | [Webhooks overview](https://docs.cloud.google.com/secure-source-manager/docs/webhooks-overview), [Set up webhooks](https://docs.cloud.google.com/secure-source-manager/docs/set-up-webhooks) |
| Webhook auth | Either a secret in a sensitive query string, or **Google-signed OIDC ID token** for the repo's service account as a bearer token | Webhooks overview |
| Payload | GitHub-like push payload (`ref`, `before`, `after`, `commits`, `repository`), event-type header | Webhooks overview |
| Built on Gitea? | **Unverified.** None of the official pages found mention Gitea. Keep as an assumption; do not design on it | — |

### Implications for Fornax
- SSM fits the normal **webhook** provider path (same as GitHub/Gitea), with OIDC-token verification as an extra auth mode — worth supporting generically (Google-signed OIDC is also how Pub/Sub push and Cloud Scheduler authenticate).
- Recommendation for A-005 (user decision): keep CSR first if the BIAL/srfserver org already uses it; add SSM immediately after, since it shares GCP auth code.
