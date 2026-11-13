# KubeWeasel

Deploy and manage apps on Kubernetes through a focused, self-hosted dashboard.

This is a working early MVP, written in Rust with Axum and kube-rs. The dashboard is embedded in the binary and uses plain JavaScript and CSS, with no frontend build step. It uses optional Google Fonts with local fallbacks.

## Try the demo

```sh
cargo run --locked -- --demo
```

Open http://127.0.0.1:7878. Demo applications, scaling, restarts, and sample logs do not contact Kubernetes. Demo changes are held in memory and reset on restart.

## Use a cluster

```sh
# Uses your current kubeconfig; in-cluster service accounts are also supported.
KUBEWEASEL_NAMESPACE=my-project cargo run --locked
```

On first startup, use the setup token printed in the terminal to create your owner account in the browser. Your email/password and sessions persist in `kubeweasel.db` (passwords are hashed, not stored in plaintext).

Live mode fails at startup if no Kubernetes configuration can be loaded; it never silently falls back to demo data. The dashboard only lists deployments labeled `app.kubernetes.io/managed-by=kubeweasel`. It polls every five seconds. Kubernetes persists the actual resources, so restarting the dashboard preserves applications.

The Rustls TLS backend uses the explicitly enabled `ring` crypto provider. A regression test constructs an HTTPS Kubernetes client without contacting a cluster, covering the TLS initialization path that demo mode skips.

Supported operations:

- Create an image-based Deployment and internal ClusterIP Service.
- See desired and ready replica counts and basic rollout status.
- Scale from zero to twenty replicas and request rolling restarts.
- Read snapshots of the last 100 log lines from up to five pods.
- Search and filter applications.

Deployment names must be lowercase DNS labels of at most 63 characters. New applications default to one replica and port 80. Containers request 100m CPU and 64Mi memory, with limits of 1 CPU and 512Mi memory. Creating an existing name fails rather than overwriting resources. Deployment and Service creation are separate operations; if the latter fails, the error explicitly reports the partial creation so it can be inspected.

## Install with Helm

Build and push your own image to a registry your cluster can pull from:

```sh
docker build -t YOUR_REGISTRY/kubeweasel:0.1.0 .
docker push YOUR_REGISTRY/kubeweasel:0.1.0
helm upgrade --install kubeweasel ./charts/kubeweasel \
  --namespace kubeweasel --create-namespace \
  --set image.repository=YOUR_REGISTRY/kubeweasel
kubectl -n kubeweasel port-forward deployment/kubeweasel 7878:7878
```

The chart grants namespace-scoped access only. Applications are deployed in the installation namespace. The dashboard listens on pod loopback; Kubernetes port-forward provides access without exposing a network Service or ingress. Your local kubeconfig may grant broader permissions than the chart.

## Access model and current limits

This version supports one owner account with full access, without teams or project-specific permissions. `KUBEWEASEL_BIND` defaults to `127.0.0.1:7878`. Every application API requires an owner session. Mutation requests also require a session-bound CSRF token and reject unexpected origins. Public access requires an HTTPS reverse proxy and KUBEWEASEL_PUBLIC_ORIGIN; see owner setup below.

This release does not yet implement Git builds, project-level permissions or environments, automatic HTTPS provisioning, secrets editing, private registry setup, rollback history, a custom-resource controller, or multi-cluster support. Existing cluster networking, registry credentials, and admission policies still apply. Health indicates replica readiness; it is not an application-level health probe. Cluster changes occur when you deploy, scale, restart, or save application settings through the UI.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
node --check dashboard/dashboard.js
```

`src/main.rs` contains the API and Kubernetes operations; `dashboard/` contains the embedded UI; `charts/kubeweasel/` contains the installation chart.

## Application configuration

Click an application card or name to open its configuration screen. Links such as `/#apps/hello-weasel/networking` can be bookmarked. The screen includes General, Networking, Environment, Resources, and Logs tabs.

- **General:** change the container image and replica count. Saving applies immediately; changing the image rolls out new pods.
- **Networking:** change the container port and Service target port, keeping the Service frontend port stable. The process must actually listen on the chosen port. Start/stop a temporary port forward on `127.0.0.1` of the machine running KubeWeasel. Forwarding requires a ready pod, supports at most 10 tunnels and 32 connections per tunnel, and stops when the server stops. Each connection is limited to five minutes. In-cluster installs should use the provided kubectl command on the user's computer.
- **Domains:** configure one hostname/path route per application, an optional ingress class, and an optional existing TLS Secret. This creates or updates an owned `networking.k8s.io/v1` Ingress; clearing the hostname removes it. An ingress controller, matching DNS, and any HTTPS certificate must already exist. Saving an Ingress does not prove external routing works.
- **Environment:** edit literal variables, including multiline values. Saving replaces literal variables while preserving Secret/ConfigMap references. Literal values are stored in the Deployment and returned to dashboard users; use Secrets for credentials.
- **Resources:** change positive CPU and memory requests/limits; requests cannot exceed limits. CPU accepts cores or millicores; memory accepts bytes, Ki, Mi, or Gi.
- **Logs:** refresh recent output without leaving the app screen.

Demo settings remain in memory. Demo mode cannot start real port forwards. Live configuration is stored in Kubernetes resources and survives dashboard restarts. Networking updates span multiple resources and are not transactional; errors report partial Service/Deployment updates. The Helm Role includes Service reads/patches, managed Ingress operations, and pod port forwarding within its namespace. Kubernetes RBAC cannot restrict these permissions by resource label; the application checks ownership before mutation.

To repeat the opt-in live settings check against a running local dashboard:

```sh
python3 tests/live_settings.py http://127.0.0.1:7879 kubeweasel-test
```

The check creates a dedicated `kw-settings-check` application, exercises settings and nginx forwarding, and removes its resources afterward. Do not use that name for an existing app.

## Networking diagnostics

The Networking tab now checks application readiness, the internal Service, the selected/default IngressClass, the saved Ingress, DNS resolution, any advertised ingress address, and HTTPS configuration. Checks run on the KubeWeasel server and use saved settings. Recheck after changing DNS or installing a controller. Saving a domain refreshes the checks automatically.

**Test connection** sends an HTTP GET to the saved hostname and path on the selected port. Redirects are not followed, ambient HTTP proxies are disabled, and tests time out after eight seconds. Successful responses do not prove the endpoint is the intended application; inspect its content separately. HTTPS tests validate the served certificate using public trust roots. TLS Secret contents are never read by diagnostics; a configured Secret alone is not reported as verified HTTPS.

For a local Traefik tunnel, select **Local test**, enter **8082**, and keep `kubectl -n traefik port-forward service/traefik 8082:80` running. This connects to loopback on the KubeWeasel server while retaining the saved hostname for routing and TLS. It bypasses DNS only for that test. Your browser still needs a hosts-file entry or working DNS. In-cluster loopback refers to the dashboard pod, not your Mac.

The Helm chart's `ingressClassDiscovery` option (default `true`) adds only cluster-wide `get/list` permissions for IngressClass discovery. Set it to `false` to omit this ClusterRole and binding. Discovery is reported as **unknown** when access is denied, rather than claiming that no controller exists. Namespace-scoped mutation permissions are unchanged. Controller-class registration is not proof of a running controller; connection tests provide separate reachability evidence.

## Live monitoring

Open **Monitoring** in the sidebar (`/#monitoring`) for CPU and memory trends, ready pod counts, restart totals, and per-pod usage. It reads Kubernetes Metrics Server through `/api/monitoring` and refreshes every five seconds while the page is visible. CPU is shown in millicores (1,000 mCPU = one core), memory in MiB. Only pods labeled `app.kubernetes.io/managed-by=kubeweasel` in the configured namespace are included. This is application monitoring, not whole-node or Mac host usage.

The page keeps up to five minutes of chart history in the current browser tab; reloading clears it. Metrics Server itself provides recent readings, not durable monitoring history. Missing samples are shown as unavailable, never fabricated as zero. The workload list shows measurement coverage; stale readings older than two minutes are identified. Disk and network I/O need additional collectors and are not supplied by this page.

Install Metrics Server once per cluster if it is not already available:

```sh
helm upgrade --install metrics-server metrics-server \
  --repo https://kubernetes-sigs.github.io/metrics-server/ \
  --namespace kube-system
```

The KubeWeasel Helm Role includes namespace-scoped `get/list` on `metrics.k8s.io/pods`. Missing metrics providers, denied permissions, and temporarily unavailable metrics APIs have distinct messages; pod health remains available without CPU/memory readings. Demo mode explicitly reports that usage readings require a live cluster.

Keep kubelet certificate verification enabled for normal installations. Docker Desktop may use kubelet certificates without IP SANs; configure trusted, matching serving certificates where possible. A local development cluster can use Metrics Server's `--kubelet-insecure-tls` option only as an explicit development exception. This weakens verification of the kubelet connection and is not recommended for production.

### Node and VM capacity

Switch to **Monitoring → Nodes** (`/#monitoring/nodes`) to see all Kubernetes nodes, not just KubeWeasel-managed pods. Each node shows CPU and memory usage versus allocatable capacity, percentages, readiness, five-minute browser-session trends, and an indicator at 80% usage. This includes system activity inside the node. Docker Desktop reports its Linux VM, not macOS or the physical host.

Requests are calculated from active scheduled pods across all namespaces, including effective init-container requests, restartable sidecars, pod overhead, and pod-level request overrides. Configured-limit totals include application containers and restartable sidecars (or pod-level limits), excluding one-shot init containers. Containers without explicit limits are counted separately; configured totals are not an overall usage cap. Requests are reservations, not measurements. Limits may exceed capacity. Missing permissions or metrics are shown explicitly.

The Helm option `nodeMonitoring` defaults to `true` and adds read-only cluster-scoped `get/list` permissions for nodes, pods, and node metrics. Cluster-wide pod access is needed to calculate reservations across all workloads; it grants access to pod specifications, not Secret contents. Set `nodeMonitoring=false` to omit these permissions. Namespace-scoped application monitoring continues to work independently. Metrics do not represent the cloud bill: many services charge for provisioned capacity rather than actual usage.


## Owner account and HTTPS

KubeWeasel starts with a **Create owner account** screen. Enter the single-use setup token printed in the server terminal (or `kubectl logs deployment/kubeweasel` for Helm), your email, and a password of at least 12 characters. The token changes on restart until setup completes. Once the owner exists, account creation is disabled. There is no public registration or shared demo password. The owner has full application and cluster-monitoring access; teams, invitations, project permissions, password recovery, and MFA are not implemented yet. Keep the owner password in a password manager.

Passwords use Argon2id. Sessions use random HttpOnly, SameSite=Strict cookies and hashed server-side session identifiers, with a 30-minute idle timeout and a 12-hour absolute lifetime. Login rotates the session, logout revokes it, and **Sign out all sessions** revokes all owner sessions. Polling counts as activity. Sign-in/setup attempts are limited to 10 per source IP per 15 minutes, with at most two concurrent password-hashing operations. Proxy headers are not trusted for client IP; a reverse proxy should apply its own rate limits, since proxied users share the proxy address at the backend.

`KUBEWEASEL_AUTH_DB` defaults to `kubeweasel.db`. Protect and back up this SQLite database using a SQLite-aware backup or while the server is stopped; do not copy only the main file while WAL writes are active. On Unix the database permissions are set to owner-only. Database files are excluded from Git and Docker build contexts. Do not delete the database as a routine upgrade step. The Helm chart mounts a persistent volume at `/data` and uses a single replica with Recreate updates.

For public access, configure a trusted HTTPS reverse proxy that preserves the original Host header:

```sh
KUBEWEASEL_BIND=0.0.0.0:7878 \
KUBEWEASEL_PUBLIC_ORIGIN=https://deploy.example.com \
KUBEWEASEL_AUTH_DB=/data/kubeweasel.db \
kubeweasel
```

KubeWeasel does not terminate TLS itself. Keep the HTTP backend private to the proxy. Non-loopback binding without an HTTPS public origin is rejected. HTTPS deployments use Secure `__Host-` session cookies and HSTS; local loopback HTTP uses a separate non-Secure development cookie. Allowed Host and Origin values are checked explicitly. Setting `publicOrigin` in Helm enables a ClusterIP Service on port 7878 for your HTTPS proxy; the default remains local port-forward access. Choose `persistence.size` and `persistence.storageClass` for the account volume.

Opt-in Python live checks now require `KUBEWEASEL_SESSION_COOKIE` in the test process environment, containing an authenticated owner cookie. They fetch the session CSRF token automatically and never print credentials. Never commit this cookie or store it in shell history. Unit tests exercise signup, duplicate-owner rejection, login/logout, session revocation/expiry, rate limiting, CSRF, and cross-origin rejection without creating your real account.

## Projects

Use **Projects** in the sidebar to organize applications into simple folders. Create a named project with an optional description, open it to see its applications, and use **Edit project** to rename it. New deployments can choose a project; existing apps appear under **Unassigned** until moved through **General → Project**. All owner-account capabilities remain the same; projects do not add roles, environments, namespace isolation, or permissions.

Live project names/descriptions are stored alongside the owner account in the SQLite database, scoped to the configured namespace. Application assignment is stored on the Deployment as the `kubeweasel.io/project` label. Renaming keeps the project ID stable. Moving an app changes only this metadata label; pods, Services, domains, and namespaces are unchanged. Empty folders may be deleted; move all applications out first. Project operations and deployment assignments are serialized within one dashboard process. Direct external Kubernetes changes are outside that coordination.

Back up the SQLite database as well as Kubernetes resources to retain folder names and assignments. If project metadata is missing after a restore, applications with unknown project IDs are displayed in Unassigned. Demo project folders and assignments remain in memory and reset on restart; the owner account still persists.
