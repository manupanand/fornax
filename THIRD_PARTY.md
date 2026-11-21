# THIRD_PARTY.md

Third-party projects consulted or used by Fornax, with license and attribution obligations.

**Current status: no third-party code has been copied into Fornax.** All entries below were
studied for *ideas and architecture only* (Step 2 research, 2026-11-19). If code is ever
reused, add the file path, the original copyright notice and (for Apache-2.0) any NOTICE
content here and in the source file header.

| Project | Repo @ commit studied | License | Use | Obligations if code is reused |
|---|---|---|---|---|
| GitLab CE (gitlab-foss) | gitlab.com/gitlab-org/gitlab-foss @ 0c0dfd35e | MIT Expat (outside `ee/`, `jh/`); `doc/` CC BY-SA 4.0 | Research only | Keep MIT notice. **Never copy `doc/` prose** (share-alike). **Never read/copy EE** |
| GitLab Runner | gitlab.com/gitlab-org/gitlab-runner @ 87a75c8 | MIT | Research only | Keep MIT notice |
| Jenkins core | github.com/jenkinsci/jenkins @ e8ba485 | MIT | Research only | Keep MIT notice |
| Jenkins Remoting | github.com/jenkinsci/remoting @ e02b5cd | MIT | Research only | Keep MIT notice |
| Pipeline: Groovy (workflow-cps) | github.com/jenkinsci/workflow-cps-plugin @ 0c787bc | MIT | Research only | Keep MIT notice |
| Pipeline: Nodes and Processes | github.com/jenkinsci/workflow-durable-task-step-plugin @ 9779541 | MIT | Research only | Keep MIT notice |
| Pipeline: Declarative | github.com/jenkinsci/pipeline-model-definition-plugin @ 6e7193c | MIT | Research only | Keep MIT notice |
| Gitea | github.com/go-gitea/gitea @ d4101fd | MIT | Research only | Keep MIT notice |
| act_runner (incl. embedded act) | gitea.com/gitea/act_runner @ bcc0b71 | MIT | Research only | Keep MIT notice |
| Woodpecker CI | github.com/woodpecker-ci/woodpecker @ f1f38e3 | Apache-2.0 | Research only | Keep license + NOTICE, state changes |
| Harness Open Source (ex-Drone) | github.com/harness/drone @ f38f54f | Apache-2.0 | Research only | Keep license + NOTICE, state changes |
| Argo CD (incl. gitops-engine) | github.com/argoproj/argo-cd @ 232fa2d | Apache-2.0 | Research only | Keep license + NOTICE, state changes |
| Flux (flux2, source-controller, kustomize-controller) | github.com/fluxcd/* @ 1380863 / 62314b9 / e3ae793 | Apache-2.0 | Research only | Keep license + NOTICE, state changes |
| Tekton Pipelines | github.com/tektoncd/pipeline @ 94305b7 | Apache-2.0 | Research only | Keep license + NOTICE, state changes |

| rust-anything (tool only) | github.com/erkamyaman/rust-anything @ 9029714 | MIT | Proposed parity-recording tool (Step 3); not linked or vendored | None unless code is copied |

## Parity corpora

None recorded yet. Proposed units U1–U5 are listed in `docs/research/rust-anything.md`.

Research clones live outside the repo at `~/code-personal/fornax-research/` and are not
committed.
