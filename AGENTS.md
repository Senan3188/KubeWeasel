# KubeWeasel contributor guide

These instructions apply throughout this repository.

## Project layout

- `src/projects.rs`: project-folder persistence, assignment labels, and empty-folder deletion rules.
- `src/auth.rs`: single-owner setup, SQLite-backed sessions, password hashing, request protection, and login rate limits.
- `src/main.rs`: Axum server, embedded dashboard assets, deployment listing/creation, scaling, restarts, and logs.
- `src/settings.rs`: application configuration, resource ownership checks, Ingress management, and temporary pod port forwarding.
- `src/monitoring.rs`: application and whole-node usage, allocatable capacity, pod reservations, unit conversion, and metrics availability.
- `src/diagnostics.rs`: networking checks, ingress-class discovery, DNS lookup, and HTTP/HTTPS connection testing.
- `dashboard/`: build-free HTML, CSS, and JavaScript embedded in the Rust binary. Rebuild and restart the server after editing assets.
- `charts/kubeweasel/`: Helm installation, persistent owner-account storage, namespace-scoped permissions, and optional read-only cluster discovery/monitoring.
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

- Keep projects as organizational folders; store assignment in `kubeweasel.io/project`, preserve project IDs on rename, and reject deletion of non-empty folders. Moving apps must not restart pods or change namespaces.
- Keep demo and live behavior explicit. Never silently substitute sample data for cluster errors.
- Kubernetes resources are the source of truth for live configuration; demo settings remain in memory.
- Verify the `app.kubernetes.io/managed-by=kubeweasel` ownership label before changing existing resources.
- Preserve Secret/ConfigMap environment references when editing literal variables, and do not read Secret contents for diagnostics.
- Validate configuration on the server. Keep replicas, tunnels, connections, log output, and network timeouts bounded.
- Report partial failures for operations spanning multiple Kubernetes resources; do not claim they are atomic.
- Bind temporary forward listeners to loopback and cancel their tasks when stopped or dropped.
- Distinguish saved routing configuration, controller-class registration, DNS resolution, and actual connection success.
- Connection tests use the saved hostname/path, validate HTTPS certificates, do not follow redirects, and explain where the test runs.
- Report missing metrics as unavailable, not zero. Distinguish application usage from whole-node/VM usage, allocatable capacity, requests, and configured limits. Never imply these measurements are cloud billing totals.
- Keep browser chart history bounded and identify stale samples; node reservations span active scheduled pods across all namespaces.
- Render resource names, project names/descriptions, environment values, logs, and errors as text, never as HTML.
- Keep the dashboard build-free unless the user explicitly requests a framework migration.
- Update Helm RBAC and README documentation when adding API permissions or changing behavior. Keep permissions minimal and discovery failures explicit.

## Verification

- Run `cargo fmt --check`, `cargo test --locked`, and `cargo clippy --locked --all-targets -- -D warnings`.
- Run `node --check` for every `dashboard/*.js` file after frontend changes, including authentication and project navigation.
- For UI changes, inspect the running dashboard, app navigation, forms, error states, and responsive layout in a browser.
- For chart changes, run `helm lint charts/kubeweasel` and render the chart with `ingressClassDiscovery` and `nodeMonitoring` enabled and disabled, plus a configured HTTPS `publicOrigin`. Verify account-volume mounts and the optional Service.
- Live Python checks require `KUBEWEASEL_SESSION_COOKIE`; never put it in tracked files, command arguments, or test output.
- Run `python3 tests/live_settings.py <BASE_URL> <NAMESPACE>` only against an explicitly selected test namespace. It creates and removes `kw-settings-check`; do not use that name for existing applications.
- Run `python3 tests/live_networking.py <BASE_URL> <APP_NAME> <INGRESS_PORT>` to check an existing saved route and local ingress tunnel without modifying cluster resources.
- Explain verification limits; do not claim a live cluster, Docker build, or Helm installation was tested unless it was.
