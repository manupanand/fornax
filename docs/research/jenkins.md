# Jenkins (core + Pipeline plugin suite)

| | |
|---|---|
| Repos (all `github.com/jenkinsci/…`) | `jenkins` @ `e8ba485` (2026-11-19), `remoting` @ `e02b5cd`, `workflow-cps-plugin` @ `0c787bc`, `workflow-durable-task-step-plugin` @ `9779541`, `pipeline-model-definition-plugin` @ `6e7193c` |
| License | All **MIT** (`jenkins/LICENSE.txt`, `remoting/LICENSE.md`, `<licenses>` in each plugin's `pom.xml`) |
| Reuse rule | Ideas only; MIT is Apache-compatible if code is ever reused (attribution required). |

## 1. Architecture

- **Controller** (single JVM web app, `war/`, `core/`) holds UI, scheduling, config and build history. **Agents** are separate JVMs connected via **Remoting**.
- **Storage = filesystem under `JENKINS_HOME`**, XML via XStream: job config `config.xml` (`core/.../hudson/model/Items.java:417`), each build's `build.xml` (`core/.../hudson/model/Run.java:2066`), queue persisted to `queue.xml` (`Queue.java:532`). Builds are lazily loaded (`core/.../jenkins/model/lazy/`). No database.
- **Data model:** `Job` → `Run` (build); `Node` (config of a machine) / `Computer` (live state) / `Executor` (slot) (`core/.../hudson/model/`).
- **Queue / scheduler** (`core/.../hudson/model/Queue.java`): documented lifecycle in the class Javadoc (lines 153–175): items enter a **waiting list** (quiet period) → **blocked** (resource/concurrency) or **buildable** → **pending** (handed to an executor) → **left**. `maintain()` (line 1678) is the periodic scheduling pass; `LoadBalancer.java` maps buildable items to executor slots (`JobOffer`, line 249). It is an **in-memory, controller-push** scheduler, persisted to XML on save.
- **Agent protocol** (`remoting/docs/protocols.md`): **JNLP4-connect** (TCP with TLS upgrade before secrets are exchanged) or **WebSocket** over the HTTP(S) port. Remoting is a bidirectional RPC channel built on **Java object serialization + remote classloading** (`remoting/docs/class-loading.md` lines 5–7, 66) — the controller ships closures (`Callable`) to agents.

## 2. Pipeline definition

- Two flavours: **Scripted** (Groovy) and **Declarative** (`pipeline { agent … stages { stage { steps … } } }`), parsed by `pipeline-model-definition/.../parser/ModelParser.groovy` into a model then converted to Groovy.
- Execution engine: **CPS-transformed Groovy interpreter inside the controller** (`workflow-cps-plugin/README.md`); program state is serialized to disk at every async step so builds survive controller restarts; the README itself warns it is inefficient and should only be glue code — real work goes in `sh`/`bat`.
- Long-running processes on agents survive restarts/reconnects via **durable tasks** (`workflow-durable-task-step-plugin/README.md`): the process writes output/exit code to files the controller re-attaches to.
- Stages run sequentially; `parallel` for fan-out; matrix via declarative `matrix` directive. No first-class DAG with `needs`.
- Triggers: `hudson/triggers/SCMTrigger.java` (polling), `TimerTrigger.java` (cron), webhooks via plugins. Cron syntax with the **`H` hash** for load-spreading (`core/.../hudson/scheduler/CronTab.java`, `Hash.java`).
- Artifacts: archived to controller disk (or external via plugins); fingerprints track artifact usage (`core/.../jenkins/fingerprints/`).
- Secrets: **Credentials plugin** (not in core); core has `ConfidentialStore` / `ConfidentialKey` (`core/.../jenkins/security/`) for encrypting secrets at rest with a key in `JENKINS_HOME/secrets/`.
- Environments / approvals: `input` step for manual approval; no first-class environment model.

## 3. GitOps / CD

- None built in. CD = pipeline steps that push (kubectl/helm/ssh). No reconciliation, drift detection or promotion model; rollback = rerun an older build. (Jenkins X was a separate project — not studied.)

## 4. Scaling, HA, storage, upgrades

- **Single controller, no native HA** (filesystem state; active/passive only via shared storage — **assumption**, general knowledge). Scale = more agents or more controllers.
- Filesystem storage becomes slow with many builds; lazy loading exists for this reason.
- Upgrades: WAR swap; **plugin compatibility is the main upgrade risk** (dependency hell across ~2000 plugins — **assumption**, general knowledge).
- Controller diagnostics warn when executors run on the built-in node (`core/src/main/resources/jenkins/diagnostics/Messages.properties` lines 5–6) — the project now recommends building only on agents.

## 5. Security model & known weaknesses

- Agent → controller calls are filtered (`core/.../jenkins/security/s2m/AdminWhitelistRule.java`, `CallableDirectionChecker.java`; `SlaveToMasterCallable` vs `MasterToSlaveCallable`) because agents could otherwise run code on the controller.
- Java deserialization is restricted with class filters (`core/.../jenkins/security/ClassFilterImpl.java`, `CustomClassFilter.java`, `XStreamDeserializable.java`) — a long history of deserialization CVEs drove this.
- Groovy **script security sandbox** with an allowlist (`workflow-cps-plugin/.../cps/CpsWhitelist.java`, `GroovyClassLoaderWhitelist.java`, `SandboxContinuable.java`); sandbox escapes have been a recurring CVE class (**assumption**, general knowledge).
- Builds run as the controller's identity unless `QueueItemAuthenticator` is configured (`core/.../jenkins/security/QueueItemAuthenticator*.java`).
- Plugin ecosystem = large, uneven supply-chain surface.

## 6. Adopt / avoid / do differently in Rust

**Adopt**
- Explicit queue lifecycle (waiting → blocked → buildable → pending → left) — maps well onto DB job states and explains *why* a job isn't running (blocked reason).
- Durable process idea: agent writes output + exit status to local files so a job survives agent ↔ server disconnects (important for the **host executor**, D-008).
- Cron `H` (hash) token to spread scheduled jobs.
- Agents connect **outbound** over WebSocket on the HTTP(S) port — no extra inbound port.
- Encrypting secrets at rest with a server key in a separate directory.

**Avoid**
- A Turing-complete pipeline language executed **inside the controller** (CPS Groovy): performance, sandbox escapes, non-determinism.
- Remote code/class shipping and native object serialization on the agent channel — use a fixed, versioned protobuf/JSON protocol.
- XML-on-filesystem as the primary store; no HA story.
- Core functionality (credentials, Git, pipelines) living in plugins with independent versioning.

**Do differently**
- Declarative YAML with typed schema; logic runs only on agents.
- DB-backed queue with explicit blocked/waiting reasons (D-007).
- Small, versioned extension points (executors, Git providers, secret backends) compiled in or loaded out-of-process — no in-process third-party plugins in 1.0.
