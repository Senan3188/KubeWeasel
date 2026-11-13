# KubeWeasel contributor guide

These instructions apply throughout this repository.

## Project layout

- `src/projects.rs`: project-folder persistence, assignment labels, and empty-folder deletion rules.
- `src/auth.rs`: single-owner setup, SQLite-backed sessions, password hashing, request protection, and login rate limits.
- `src/main.rs`: Axum server, embedded dashboard assets, deployment listing/creation, scaling, restarts, and logs.
- `src/workloads.rs`: Deployment/StatefulSet operations, ownership checks, version preconditions, and single-replica storage guards.
- `src/storage.rs`: standalone retained PVCs, mount validation, claim-sharing checks, and explicit detach without deleting data.
- `src/git.rs`: public Git/Dockerfile builds, retained registry credentials, bounded rootless build Jobs, and restart-safe promotion.
- `src/registry.rs`: image-provider validation and image-pull credential creation before deployment or source changes.
- `src/history.rs`: bounded image/build history, rollout observation, digest-aware rollback, and pending-build supersession.
- `src/settings.rs`: application configuration, resource ownership checks, Ingress management, and temporary pod port forwarding.
- `src/monitoring.rs`: application and whole-node usage, allocatable capacity, pod reservations, unit conversion, and metrics availability.
- `src/https.rs`: cert-manager discovery, issuer validation, ingress-shim annotations, and certificate status; never reads private keys.
- `src/diagnostics.rs`: networking checks, ingress-class discovery, DNS lookup, and HTTP/HTTPS connection testing.
- `dashboard/`: build-free HTML, CSS, and JavaScript embedded in the Rust binary. Rebuild and restart the server after editing assets.
- `charts/kubeweasel/`: Helm installation, persistent owner-account storage, namespace-scoped permissions, and optional read-only cluster discovery/monitoring.
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

- Never delete PVCs or data during detach, scale, or workload updates. Keep storage-backed apps and StatefulSets single-replica; use Recreate for storage-backed Deployments and retain PVCs independently.
- Keep projects as organizational folders; store assignment in `kubeweasel.io/project`, preserve project IDs on rename, and reject deletion of non-empty folders. Moving apps must not restart pods or change namespaces.
- Keep demo and live behavior explicit. Never silently substitute sample data for cluster errors.
- Kubernetes resources are the source of truth for live configuration; demo settings remain in memory.
- Verify the `app.kubernetes.io/managed-by=kubeweasel` ownership label before changing existing resources.
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
- Source switches and rollbacks must coordinate with Git promotion through the build gate so an older build cannot overwrite a manual image selection. Preserve current environment, networking, resources, replicas, mounts, PVCs, and database data during image rollback.
- Keep deployment history bounded to 30 entries. Record image/build outcomes without environment values or Secret contents. Verify updated-rollout readiness; older ready pods do not prove a new release succeeded. Use only pullable manifest digests for image pinning, never arbitrary runtime configuration IDs.
- The installer must pin every cluster command to the selected context, verify downloaded chart checksums, wait for readiness, and leave failed installs inspectable. Namespace-only installation uses an existing namespace and disables optional cluster-wide discovery roles.
- Uninstall must preserve account storage, deployed workloads, application volumes, and registry credentials. Retain chart-created certificate issuers. Never add automatic namespace/PVC deletion; reinstall must reuse the retained account claim. Reject uninstall of legacy releases lacking PVC retention.
- Keep health-check execution independent of owner setup/database initialization; probes must work for both loopback and HTTPS reverse-proxy configurations without weakening Host/Origin protection.
- Publish immutable version tags. Keep Cargo version, chart version/appVersion, image tag, and installer default version aligned. Publish AMD64/ARM64 images and installer/chart checksums; verify anonymous multi-platform pulls before making a release public. Do not move existing release tags for fixes.

## Verification

- Run `cargo fmt --check`, `cargo test --locked`, and `cargo clippy --locked --all-targets -- -D warnings`.
- Run `node --check` for every `dashboard/*.js` file after frontend changes, including authentication and project navigation.
- For UI changes, inspect the running dashboard, app navigation, forms, error states, and responsive layout in a browser.
- For chart changes, run `helm lint charts/kubeweasel` and render the chart with `ingressClassDiscovery` and `nodeMonitoring` enabled and disabled, plus a configured HTTPS `publicOrigin`. Verify account-volume mounts and the optional Service.
- For installer changes, run `bash -n scripts/install.sh` and `python3 -m unittest discover -s tests -p 'test_installer.py'`. Verify install/upgrade, checksum failure, namespace-only permissions, safe uninstall, and retained-volume reinstall.
- For chart changes, also render an existing account claim, all optional discovery permissions disabled, and a staging certificate issuer. Verify PVC/issuer retention and health probes. A full installer lifecycle test must use an isolated namespace and disposable owner account; confirm login, account PVC identity, and application survival across upgrade/uninstall/reinstall.
- Opt-in Rust live checks use `KUBEWEASEL_RUN_GIT_TEST=1`, `KUBEWEASEL_RUN_HISTORY_TEST=1`, `KUBEWEASEL_RUN_SOURCE_TEST=1`, `KUBEWEASEL_RUN_STORAGE_TEST=1`, or `KUBEWEASEL_RUN_HTTPS_TEST=1` with the corresponding ignored test. Inspect each test's scope before running it. Git tests export locally rather than publishing to a user's registry.
- Live Python checks require `KUBEWEASEL_SESSION_COOKIE`; never put it in tracked files, command arguments, or test output.
- Run `python3 tests/live_settings.py <BASE_URL> <NAMESPACE>` only against an explicitly selected test namespace. It creates and removes `kw-settings-check`; do not use that name for existing applications.
- Run `python3 tests/live_networking.py <BASE_URL> <APP_NAME> <INGRESS_PORT>` to check an existing saved route and local ingress tunnel without modifying cluster resources.
- Explain verification limits; do not claim a live cluster, Docker build, or Helm installation was tested unless it was.
