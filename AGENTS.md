# KubeWeasel contributor guide

These instructions apply throughout this repository.

## Project layout

- `src/projects.rs`: cluster-scoped project-folder persistence, transactional legacy migration, assignment labels, and empty-folder deletion rules.
- `src/clusters/`: encrypted kubeconfig storage, strict inline-credential validation, capability checks, connection management, scoped dispatch, and per-connection worker lifetimes.
- `dashboard/clusters.js`: cluster selection, connection dialogs, explicit scoped requests, and stale-response protection.
- `deploy/remote-cluster-*.yaml`, `deploy/remote-node-management.yaml`, `scripts/cluster-kubeconfig.py`: remote namespace permissions, separate node-administration permissions, and private expiring-token connection files.
- `src/auth.rs`: single-owner setup, SQLite-backed sessions, password hashing, request protection, and login rate limits.
- `src/main.rs`: minimal executable entry point; delegates to the library's `run` function.
- `src/lib.rs`, `src/server.rs`: module boundaries and startup orchestration (authentication, state, workers, listener, shutdown).
- `src/config.rs`, `src/health.rs`: environment/CLI configuration and side-effect-free health-check execution.
- `src/state.rs`: shared application state initialization and demo seeding.
- `src/applications/`: application models, HTTP route adapters, shared business services, and typed Kubernetes resource factories.
- `src/http/`: complete router composition, embedded static assets, and mutation-header validation; authentication wraps every feature API.
- `src/error.rs`, `src/validation.rs`: shared API errors and pure resource/deployment validation.
- `src/workloads.rs`: Deployment/StatefulSet operations, ownership checks, version preconditions, and single-replica storage guards.
- `src/storage.rs`: standalone retained PVCs, mount validation, claim-sharing checks, and explicit detach without deleting data.
- `src/git.rs`: public Git/Dockerfile builds, retained registry credentials, bounded rootless build Jobs, and restart-safe promotion.
- `src/registry.rs`: image-provider validation and image-pull credential creation before deployment or source changes.
- `src/history.rs`: bounded image/build history, rollout observation, digest-aware rollback, and pending-build supersession.
- `src/settings.rs`: application configuration, resource ownership checks, Ingress management, and temporary pod port forwarding.
- `src/nodes/`: node inspection, conditional scheduling/metadata patches, drain validation, controller verification and PDB-respecting eviction. `dashboard/nodes.js` owns the selected-cluster Nodes screen.
- `src/monitoring.rs`: application and whole-node usage, allocatable capacity, pod reservations, unit conversion, and metrics availability.
- `src/https.rs`: cert-manager discovery, issuer validation, ingress-shim annotations, and certificate status; never reads private keys.
- `src/diagnostics.rs`: networking checks, ingress-class discovery, DNS lookup, and HTTP/HTTPS connection testing.
- `dashboard/`: build-free HTML, CSS, and JavaScript embedded in the Rust binary. Rebuild and restart the server after editing assets.
- `charts/kubeweasel/`: Helm installation, persistent owner-account storage, namespace-scoped permissions, optional read-only cluster discovery/monitoring, and explicitly enabled node-administration permissions.
- `scripts/install.sh`: versioned install/upgrade/status/open/uninstall commands, explicit context selection, chart checksum checks, and retained account-volume reuse.
- `.github/workflows/`: CI checks and tagged releases with native AMD64/ARM64 images, chart/installer assets, and anonymous-pull verification before publication.
- `tests/`: authenticated opt-in live checks; `auth_client.py` reads the owner cookie from the test environment without printing it.

## Development

- Run `cargo run --locked -- --demo` at `http://127.0.0.1:7878`; demo apps/projects reset on restart, but the owner account persists.
- First-run setup uses the token printed at startup. `KUBEWEASEL_AUTH_DB` stores the owner, hashed sessions, and live project folders; preserve its persistent volume during upgrades.
- Live mode uses the current kubeconfig or an in-cluster service account. Set `KUBEWEASEL_NAMESPACE` explicitly for testing.
- `KUBEWEASEL_BIND` defaults to loopback. Public binding requires an HTTPS public origin and reverse proxy; keep the backend private.
- Use a separate namespace for live tests. Confirm the Kubernetes context before making changes.
- Never commit kubeconfig files, credentials, real environment values, certificates, private keys, or authentication databases.
- Preserve owner authentication on every application API, session CSRF checks on mutations, and persistent account storage.

## Implementation rules

- Keep dependencies explicit: no file-level `use super::*` or shared root-module prelude. Test modules may import their parent. Keep startup and asset wiring out of feature services.
- Keep application HTTP adapters thin. Internal features call `applications::service` operations directly, without Axum extractors, request headers, or JSON response wrappers. Resource factories construct typed Kubernetes objects without API calls; services coordinate effects.
- Never delete PVCs or data during detach, scale, or workload updates. Keep storage-backed apps and StatefulSets single-replica; use Recreate for storage-backed Deployments and retain PVCs independently.
- Keep projects as organizational folders; store assignment in `kubeweasel.io/project`, preserve project IDs on rename, and reject deletion of non-empty folders. Moving apps must not restart pods or change namespaces.
- Keep demo and live behavior explicit. Never silently substitute sample data for cluster errors.
- Kubernetes resources are the source of truth for live configuration; demo settings remain in memory.
- Verify the `app.kubernetes.io/managed-by=kubeweasel` ownership label before changing existing application resources. Node administration is an explicit cluster-wide owner operation, governed by separate optional RBAC and node/pod UID/version preconditions.
- Preserve Secret/ConfigMap environment references when editing literal variables, and do not read Secret contents for diagnostics.
- Validate configuration on the server. Keep replicas, tunnels, connections, log output, and network timeouts bounded.
- Report partial failures for operations spanning multiple Kubernetes resources; do not claim they are atomic.
- Bind temporary forward listeners to loopback and cancel their tasks when stopped or dropped.
- Distinguish saved routing configuration, controller-class registration, DNS resolution, and actual connection success.
- Automatic HTTPS must preserve legacy TLS settings, remove obsolete issuer annotations on mode changes, and distinguish certificate readiness from public HTTPS reachability. Use staging or self-signed issuers for tests; never silently request production certificates.
- Connection tests use the saved hostname/path, validate HTTPS certificates, do not follow redirects, and explain where the test runs.
- Report missing metrics as unavailable, not zero. Distinguish application usage from whole-node/VM usage, allocatable capacity, requests, and configured limits. Never imply these measurements are cloud billing totals.
- Keep browser chart history bounded and identify stale samples; node reservations span active scheduled pods across all namespaces.
- Render resource names, project names/descriptions, environment values, logs, and errors as text, never as HTML.
- Keep the dashboard build-free unless the user explicitly requests a framework migration.
- Update Helm RBAC and README documentation when adding API permissions or changing behavior. Keep permissions minimal and discovery failures explicit.

## Deployment and release rules

- Projects are the dashboard home; create apps inside projects. Keep source selection in General → Deploy settings, with container-image and public-Git options and Docker Hub/GitHub/custom registries. Do not reintroduce a separate Source or sidebar Applications tab.
- Never read registry Secret contents back into the dashboard or persist tokens in annotations, browser storage, fixtures, or logs. Image-pull references must be attached before the first pod starts; report retained credentials on partial failures.
- Git builds run trusted Dockerfiles in bounded rootless Jobs without host Docker sockets, host mounts, or service-account tokens. Keep failed builds from changing the running image. Preserve restart-safe observation and explicit unavailable demo behavior.
- Source switches and rollbacks must coordinate with Git promotion through the per-cluster build gate so an older build cannot overwrite a manual image selection. Preserve current environment, networking, resources, replicas, mounts, PVCs, and database data during image rollback.
- Keep deployment history bounded to 30 entries. Record image/build outcomes without environment values or Secret contents. Verify updated-rollout readiness; older ready pods do not prove a new release succeeded. Use only pullable manifest digests for image pinning, never arbitrary runtime configuration IDs.
- Helm 3 release listing requires `--all` to include every state; Helm 4 defaults to all states and rejects that flag. Preserve major-version handling, fail closed on list errors, and test actual Helm 3/4 command parsing in CI as well as mocked install/upgrade behavior.
- The installer must pin every cluster command to the selected context, verify downloaded chart checksums, wait for readiness, and leave failed installs inspectable. Namespace-only installation uses an existing namespace and disables optional cluster-wide discovery roles.
- Uninstall must preserve account storage, deployed workloads, application volumes, and registry credentials. Retain chart-created certificate issuers. Never add automatic namespace/PVC deletion; reinstall must reuse the retained account claim. Reject uninstall of legacy releases lacking PVC retention.
- Keep health-check execution independent of owner setup/database initialization; probes must work for both loopback and HTTPS reverse-proxy configurations without weakening Host/Origin protection.
- Version 0.2.0 includes cluster connections and node administration. Keep release notes in `docs/releases/`, document database/key backup requirements and migration behavior, and never move the existing v0.1.0 or v0.2.0 tags for fixes. Future source-only changes must not be advertised as part of an older installer release.
- Publish immutable version tags. Keep Cargo version, chart version/appVersion, image tag, and installer default version aligned. Publish AMD64/ARM64 images and installer/chart checksums; verify anonymous multi-platform pulls before making a release public. Do not move existing release tags for fixes.

## Cluster boundaries

- Preserve the local connection and existing project IDs. Every project lookup, assignment and uniqueness check includes cluster ID and namespace. Projects cannot move between clusters.
- Dashboard feature requests use `/api/clusters/{id}/...`; unscoped legacy feature routes remain local-only. Keep owner authentication, Host/Origin and CSRF checks on the outer router, including every cluster management API. Clear outer path captures before nested feature routing.
- Never infer a connection from global browser/server state in a backend request. Each connection owns its client, settings, forwards, storage state and build gate. Suppress stale responses and clear node/application chart history on browser cluster changes; block switching while mutations are pending.
- Saved kubeconfigs use authenticated encryption bound to connection ID. Keep the matching private key on the retained account volume and require database-plus-key backups. Fail closed for missing/wrong keys; never regenerate a key over saved credentials.
- Reject exec/auth-provider plugins, impersonation, file references, proxy URLs and disabled TLS verification in every uploaded context. Never return kubeconfigs/tokens in API responses, logs or browser storage. Keep parse errors redacted and input/network timeouts bounded.
- Connection tests distinguish API reachability from individual capabilities. Missing optional cluster discovery/metrics permissions must not prevent namespace-only use. Do not install controllers or grant cluster-admin automatically.
- Credential replacement/disconnect must drain active scoped dashboard requests, stop forwards and abort cached workers. Saved connections resume their build/history observers after restart. Disconnect removes credentials, never Kubernetes workloads, namespaces or PVCs; retained folder records do not automatically reattach to a new connection ID.
- Keep remote namespace RBAC aligned with chart application permissions. Optional cluster discovery remains a separate reviewed manifest. Generated connection files must be new, owner-only, contain only the selected endpoint/CA and scoped token, and be excluded from build contexts/Git.

## Node administration

- Keep node mutation RBAC (`nodeManagement`) separate from read-only `nodeMonitoring`, disabled by default, and explicitly false in namespace-only installer mode. Remote permissions are a separate reviewed manifest. Never grant node/pod deletion, Secret reads, SSH or VM access for this feature.
- Node list/details and every mutation use the request's selected cluster client. Require owner authentication and mutation CSRF. Node names are validated DNS subdomains before Kubernetes requests. Preserve system labels/taints and unrelated metadata; use UID/resource-version preconditions for patches.
- Drain must preview and revalidate pod identities before/after cordon, verify standard workload controllers and their UIDs, and block unmanaged/unsupported-controller pods and local/ephemeral data. Skip DaemonSets, static mirror pods and completed pods. Never force-delete, shorten grace periods, bypass disruption budgets, delete PVCs or claim replacement apps are healthy from drain completion.
- Send typed `policy/v1` Eviction requests with pod UID/resource-version preconditions. Keep evictions bounded and report partial/uncertain results, pending/terminating pods, PDB refusals and the node's remaining cordoned state. Leave incomplete drains inspectable. A disconnect must wait for an active scoped maintenance request to finish.
- Verify dangerous operations with a mock Kubernetes API. Do not cordon, taint or drain the user's existing local node just to test UI controls. Live checks should read real nodes and use disposable account state; mutation tests need explicitly disposable nodes.

## Verification

- Run `cargo fmt --check`, `cargo test --locked`, and `cargo clippy --locked --all-targets -- -D warnings`.
- After changing router composition or application services, keep the full-router tests passing: anonymous API rejection, Host/CSRF checks, embedded asset access, and authenticated deploy/scale/source-change flows. Verify `--healthcheck` without opening account storage or Kubernetes configuration.
- Run `python3 -m unittest discover -s tests -p 'test_*.py'` for installer and connection-file helper checks. Opt-in cluster lifecycle testing uses `KUBEWEASEL_RUN_CLUSTERS_TEST=1` and `clusters::live_tests::connections_persist_and_disconnect_preserves_workloads_and_volumes`; confirm Docker Desktop and inspect its temporary namespace scope first.
- Run `node --check` for every `dashboard/*.js` file after frontend changes, including authentication and project navigation.
- For UI changes, inspect the running dashboard, app navigation, forms, error states, and responsive layout in a browser.
- For chart changes, run `helm lint charts/kubeweasel` and render the chart with `ingressClassDiscovery`, `nodeMonitoring`, and `nodeManagement` enabled and disabled, plus a configured HTTPS `publicOrigin`. Verify account-volume mounts and the optional Service.
- For installer changes, run `bash -n scripts/install.sh` and `python3 -m unittest discover -s tests -p 'test_installer.py'`. Verify install/upgrade, checksum failure, namespace-only permissions, safe uninstall, and retained-volume reinstall.
- For chart changes, also render an existing account claim, all optional discovery permissions disabled, and a staging certificate issuer. Verify PVC/issuer retention and health probes. A full installer lifecycle test must use an isolated namespace and disposable owner account; confirm login, account PVC identity, and application survival across upgrade/uninstall/reinstall.
- Opt-in Rust live checks use `KUBEWEASEL_RUN_GIT_TEST=1`, `KUBEWEASEL_RUN_HISTORY_TEST=1`, `KUBEWEASEL_RUN_SOURCE_TEST=1`, `KUBEWEASEL_RUN_STORAGE_TEST=1`, or `KUBEWEASEL_RUN_HTTPS_TEST=1` with the corresponding ignored test. Inspect each test's scope before running it. Git tests export locally rather than publishing to a user's registry.
- Live Python checks require `KUBEWEASEL_SESSION_COOKIE`; never put it in tracked files, command arguments, or test output.
- Run `python3 tests/live_settings.py <BASE_URL> <NAMESPACE>` only against an explicitly selected test namespace. It creates and removes `kw-settings-check`; do not use that name for existing applications.
- Run `python3 tests/live_networking.py <BASE_URL> <APP_NAME> <INGRESS_PORT>` to check an existing saved route and local ingress tunnel without modifying cluster resources.
- Explain verification limits; do not claim a live cluster, Docker build, or Helm installation was tested unless it was.
