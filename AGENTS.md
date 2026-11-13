# KubeWeasel contributor guide

These instructions apply throughout this repository.

## Project layout

- `src/main.rs`: Axum server, embedded dashboard assets, deployment listing/creation, scaling, restarts, and logs.
- `src/settings.rs`: application configuration, resource ownership checks, Ingress management, and temporary pod port forwarding.
- `src/diagnostics.rs`: networking checks, ingress-class discovery, DNS lookup, and HTTP/HTTPS connection testing.
- `dashboard/`: build-free HTML, CSS, and JavaScript embedded in the Rust binary. Rebuild and restart the server after editing assets.
- `charts/kubeweasel/`: Helm installation, namespace-scoped application permissions, and optional read-only IngressClass discovery.
- `tests/`: opt-in Python checks against a running dashboard and Kubernetes cluster.

## Development

- Run `cargo run --locked -- --demo` for an in-memory workspace at `http://127.0.0.1:7878`.
- Live mode uses the current kubeconfig or an in-cluster service account. Set `KUBEWEASEL_NAMESPACE` explicitly for testing.
- `KUBEWEASEL_BIND` defaults to loopback. Keep this default: the MVP has no application authentication.
- Use a separate namespace for live tests. Confirm the Kubernetes context before making changes.
- Never commit kubeconfig files, credentials, real environment values, certificates, or private keys.

## Implementation rules

- Keep demo and live behavior explicit. Never silently substitute sample data for cluster errors.
- Kubernetes resources are the source of truth for live configuration; demo settings remain in memory.
- Verify the `app.kubernetes.io/managed-by=kubeweasel` ownership label before changing existing resources.
- Preserve Secret/ConfigMap environment references when editing literal variables, and do not read Secret contents for diagnostics.
- Validate configuration on the server. Keep replicas, tunnels, connections, log output, and network timeouts bounded.
- Report partial failures for operations spanning multiple Kubernetes resources; do not claim they are atomic.
- Bind temporary forward listeners to loopback and cancel their tasks when stopped or dropped.
- Distinguish saved routing configuration, controller-class registration, DNS resolution, and actual connection success.
- Connection tests use the saved hostname/path, validate HTTPS certificates, do not follow redirects, and explain where the test runs.
- Render resource names, images, environment values, logs, and errors as text, never as HTML.
- Keep the dashboard build-free unless the user explicitly requests a framework migration.
- Update Helm RBAC and README documentation when adding API permissions or changing behavior. Keep permissions minimal and discovery failures explicit.

## Verification

- Run `cargo fmt --check`, `cargo test --locked`, and `cargo clippy --locked --all-targets -- -D warnings`.
- Run `node --check dashboard/dashboard.js` and `node --check dashboard/settings.js` after frontend changes.
- For UI changes, inspect the running dashboard, app navigation, forms, error states, and responsive layout in a browser.
- For chart changes, run `helm lint charts/kubeweasel` and render the chart with ingress-class discovery both enabled and disabled.
- Run `python3 tests/live_settings.py <BASE_URL> <NAMESPACE>` only against an explicitly selected test namespace. It creates and removes `kw-settings-check`; do not use that name for existing applications.
- Run `python3 tests/live_networking.py <BASE_URL> <APP_NAME> <INGRESS_PORT>` to check an existing saved route and local ingress tunnel without modifying cluster resources.
- Explain verification limits; do not claim a live cluster, Docker build, or Helm installation was tested unless it was.
