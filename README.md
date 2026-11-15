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

## Install from the command line

Installer v0.2.1 supports both Helm 3.12+ and Helm 4; it selects the correct release-listing flags for each version.

Requires a running Kubernetes cluster, `kubectl`, Helm 3.12+ (or Helm 4), Bash, curl, and a default StorageClass (or a class you choose). No Rust or Docker installation is needed on the user's computer. The release image supports Linux AMD64 and ARM64.

```sh
curl -fsSL https://github.com/Senan3188/KubeWeasel/releases/latest/download/install.sh | bash
```

The installer uses your current Kubernetes context, prints the selected context and namespace, downloads the versioned Helm chart, verifies its SHA-256 checksum, and waits for account storage and dashboard readiness. It does not install Kubernetes, Helm, ingress controllers, Metrics Server, or cert-manager. Default access stays local through Kubernetes port-forwarding; it does not expose a public dashboard. The release's published checksum detects a corrupt/mismatched chart; it is not independent verification of a compromised release publisher.

For a reviewable, pinned installation and explicit cluster selection:

```sh
curl -fsSLo install.sh https://github.com/Senan3188/KubeWeasel/releases/download/v0.2.1/install.sh
# Review install.sh before executing it.
bash install.sh install --context YOUR_CONTEXT --namespace kubeweasel --version 0.2.1
```

Use `--storage-class YOUR_CLASS` if the cluster has no default, and `--storage-size 2Gi` to choose the size of a new account volume. If optional cluster-level discovery/monitoring permissions are unavailable, use `--namespace-only` with namespace permissions and an existing namespace. This disables node monitoring and cluster-wide class/issuer discovery. In this mode the installer does not create a namespace; an administrator must create it and grant the namespace permissions first. The default installation grants namespace-scoped application mutations and optional read-only cluster monitoring/discovery roles.

After installation, run the printed port-forward command and open the dashboard:

```sh
bash install.sh open --context YOUR_CONTEXT --namespace kubeweasel
# Open http://127.0.0.1:7878 while the command remains running.
```

For first-run setup, use the token in the dashboard pod's logs (the installer prints the exact command). Create the owner account in the browser. The token changes after restart until account setup is completed. Existing owners sign in normally. Account data and projects live in the persistent account volume; application settings and history live in Kubernetes resources.

```sh
bash install.sh status --context YOUR_CONTEXT --namespace kubeweasel
bash install.sh upgrade --context YOUR_CONTEXT --namespace kubeweasel --version 0.2.1
bash install.sh uninstall --context YOUR_CONTEXT --namespace kubeweasel
```

Before upgrading from v0.1.0, back up the account SQLite database while the dashboard is stopped or using a SQLite-aware backup. v0.2.1 migrates existing project folders into the local cluster without changing their IDs. After connecting remote clusters, back up the matching `/data/kubeweasel.db.cluster-key` alongside the database; restore both together. See [release notes](docs/releases/0.2.1.md).

Upgrade preserves the release's existing Helm values and account data. The dashboard uses a single-instance Recreate strategy for SQLite, so an upgrade briefly interrupts dashboard access. Application workloads keep running. Failed readiness checks leave resources in place and print inspection steps; use `upgrade` to retry a failed install. No automatic uninstall or namespace/data deletion is performed on failure.

Uninstall removes the dashboard and its permissions, **retaining the namespace, account PVC, deployed applications, their volumes and registry credentials**. A chart-created certificate issuer is also retained so existing certificate renewal can continue. Existing ingress controllers/cert-manager are not removed. Reinstalling with the same namespace/release automatically reuses the retained account PVC. Old releases without an explicit PVC retention policy must be upgraded before this installer will uninstall them. Back up the account database and application databases independently; namespace/PVC deletion by an administrator can still destroy data.

### Helm directly

Each GitHub release includes `kubeweasel-VERSION.tgz`, `install.sh`, and `SHA256SUMS`. Download and verify them from the [releases page](https://github.com/Senan3188/KubeWeasel/releases), then:

```sh
helm upgrade --install kubeweasel ./kubeweasel-0.2.1.tgz \
  --namespace kubeweasel --create-namespace --wait --timeout 8m
kubectl -n kubeweasel port-forward deployment/kubeweasel 7878:7878
```

For a direct Helm reinstall using a retained account volume, add `--set persistence.existingClaim=kubeweasel-data`. The installer handles this detection automatically. Advanced HTTPS reverse-proxy setup and optional discovery/monitoring controls are described below.

For development, build an image with `docker build -t YOUR_REGISTRY/kubeweasel:0.2.1 .` and push it, or use the local Docker Desktop image store. The installer accepts `--chart charts/kubeweasel --image-repository YOUR_REGISTRY/kubeweasel --image-tag 0.2.1` for testing. These overrides do not download or verify the release chart.

## Multiple clusters

Use the sidebar cluster selector to switch environments, and **Manage → Connect cluster** to add a connection. The current installation remains **Local cluster** and existing project IDs are preserved. Multi-cluster connections and node management are included starting with v0.2.0. Helm 4 users should use installer v0.2.1 or newer.

Each connection has a name, a fixed application namespace, and a self-contained kubeconfig. The API server must be reachable **from the dashboard server**. A localhost address inside an installed dashboard container refers to that container, not your laptop. Create the target namespace first. Use one connection per cluster/namespace; connecting the same namespace twice exposes the same workloads with separate folder metadata.

Choose a file or paste the kubeconfig, optionally choose its context, and run **Test connection**. The report distinguishes API/TLS connectivity from individual Kubernetes permissions. Missing metrics, storage provisioners, ingress controllers or cert-manager are reported by their respective features; connecting does not install them. Uploaded configurations require HTTPS with certificate validation and embedded CA/token or client certificate/key data. Exec plugins, auth providers, file references, impersonation, proxy URLs and disabled TLS verification are rejected, including in unused contexts.

For a namespace-limited service account, apply the example permissions on the **remote** cluster, then create an expiring connection file:

```sh
kubectl --context REMOTE_CONTEXT create namespace kubeweasel
kubectl --context REMOTE_CONTEXT -n kubeweasel apply -f deploy/remote-cluster-rbac.yaml
python3 scripts/cluster-kubeconfig.py --context REMOTE_CONTEXT \
  --namespace kubeweasel --output remote-kubeconfig.json
```

The helper uses an embedded CA and a one-hour service-account token by default, writes a private file, and never prints the token. Upload the file, then remove it securely according to your operating practices. Use **Edit → Replacement kubeconfig** before token expiry to rotate credentials. A cluster administrator can choose a different duration, subject to the API server's token policy. The namespace role cannot create namespaces or inspect nodes/cluster-wide classes/issuers; optional read-only discovery permissions are in `deploy/remote-cluster-discovery.yaml`. Review them before applying. Do not use cluster-admin credentials for normal operation.

Project creation chooses a cluster; existing projects cannot move between clusters. Application settings, deployments, logs, monitoring, domains, storage, registry credentials, builds and rollback operate on the selected connection. Cluster selection is included in browser URLs and only the connection ID is saved in browser storage. Builds and rollout observers resume for saved connections after restart. Disconnect waits for active dashboard requests, stops connection workers and temporary forwards, and removes saved credentials. It leaves remote namespaces, workloads and volumes untouched. Project folder records remain in the account database but are no longer accessible through that removed connection; a newly added connection gets a new ID. Back up before disconnecting if you need to recover folder organization.

Saved kubeconfigs are encrypted with AES-256-GCM in the account database. The random encryption key defaults to `KUBEWEASEL_AUTH_DB.cluster-key` (for Helm, `/data/kubeweasel.db.cluster-key`); override with `KUBEWEASEL_CLUSTER_KEY_FILE`. On Unix the key must have owner-only permissions. **Back up the SQLite database and its matching key together**, keeping both private. A database-only copy cannot decrypt connections, and encryption does not protect against someone who obtains the database and key. A missing/wrong key causes startup to fail without replacing credentials. The existing Helm account volume persists both files. One owner has full access to every connection; this is not tenant isolation.

Opt-in local lifecycle check (Docker Desktop context only):

```sh
KUBEWEASEL_RUN_CLUSTERS_TEST=1 cargo test --locked \
  clusters::live_tests::connections_persist_and_disconnect_preserves_workloads_and_volumes -- --ignored
```

It creates two isolated namespaces, paused same-name applications and an unbound PVC, verifies restart/isolation/disconnect retention, then removes those test namespaces. It does not publish images or modify existing application resources.

## Node management

Open **Nodes** in the sidebar for the selected cluster. The list shows readiness, scheduling status, roles, Kubernetes version, node/VM details and live usage when Metrics Server is available. Open a node for conditions, addresses, labels, taints, pods across all namespaces and recent node events. Missing usage or event permissions do not masquerade as empty measurements.

Owner actions include **Cordon/Uncordon**, custom label/taint changes, and **Preview drain**. Cordon prevents ordinary new scheduling while existing pods continue running. System labels and taints under Kubernetes domains are read-only. A `NoExecute` custom taint can evict existing pods; review its explicit confirmation before applying it. Operations check node UID and resource version, so stale pages cannot silently overwrite concurrent changes.

Drain previews every pod, requires typing the node name, and rechecks the preview on the server before cordoning. It uses the Kubernetes `policy/v1` Eviction API and preserves pod grace periods and disruption budgets. It skips DaemonSets, static mirror pods and completed pods, and blocks unmanaged/unsupported-controller pods, missing controllers, `emptyDir`, `hostPath` and generic ephemeral volumes. PVCs are never deleted, but storage topology can prevent replacements starting on another node. There is no force/delete-data option. The bounded operation reports accepted evictions, refusals and remaining/terminating pods; an incomplete drain leaves the node cordoned and must be inspected/retried. A completed drain means eligible pods left this node, not that replacement applications are healthy elsewhere. If the dashboard runs on this node, draining may interrupt the dashboard itself. Control-plane or single-node maintenance can interrupt services; do not shut down a node while the report is incomplete.

Node actions affect the **whole cluster**, including workloads outside KubeWeasel's application namespace. They require additional Kubernetes permissions. Helm's read-only `nodeMonitoring` option is unchanged; mutation permissions are **disabled by default**. An administrator can enable them when installing/upgrading the chart:

```sh
helm upgrade --install kubeweasel charts/kubeweasel --namespace kubeweasel \
  --reuse-values --set nodeManagement=true
```

For an existing remote connection service account, review and apply `deploy/remote-node-management.yaml` on that remote cluster (edit its subject namespace/name if different). This grants node get/list/patch, all-namespace pod/event reads, standard workload-controller reads, and pod eviction creation; it does not grant node deletion, pod deletion, Secret reads or VM access. Namespace-only installer mode explicitly disables this role. Cluster administrators using a broader kubeconfig may already have these permissions. The connection test reports node mutation and drain capabilities separately from monitoring.

Adding machines, cloud autoscaling, SSH, reboot and VM deletion remain the responsibility of your Kubernetes/cloud provider. This screen administers Kubernetes node state.

## Access model and current limits

This version supports one owner account with full access, without teams or project-specific permissions. `KUBEWEASEL_BIND` defaults to `127.0.0.1:7878`. Every application API requires an owner session. Mutation requests also require a session-bound CSRF token and reject unexpected origins. Public access requires an HTTPS reverse proxy and KUBEWEASEL_PUBLIC_ORIGIN; see owner setup below.

This release does not yet implement private Git repository access, automatic deployment webhooks, project-level permissions or environments, secrets editing, or a custom-resource controller. Existing cluster networking, registry credentials, and admission policies still apply. Health indicates replica readiness; it is not an application-level health probe. Cluster changes occur when you deploy, scale, restart, or save application settings through the UI.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
node --check dashboard/dashboard.js
```

`src/main.rs` only starts the application. The Rust library separates responsibilities:

- `server.rs`, `config.rs`, and `health.rs`: startup, runtime configuration, and health-check execution.
- `state.rs`: shared state and demo initialization.
- `http/`: route composition, authentication coverage, embedded assets, and request guards.
- `applications/`: models, thin HTTP adapters, application services, and typed Kubernetes resource construction.
- Feature modules (`clusters`, `nodes`, `auth`, `projects`, `settings`, `git`, `registry`, `storage`, `history`, `https`, `monitoring`, `diagnostics`): their respective capabilities, with explicit imports.
- `workloads.rs`: shared Kubernetes ownership checks, updates, and concurrency/storage guards.
- `error.rs` and `validation.rs`: common errors and pure validation.

`dashboard/` contains the embedded UI; `charts/kubeweasel/` contains the installation chart. Internal callers use services directly; they do not call HTTP handlers. The complete router is tested for protected APIs, asset delivery, and authenticated deployment/source-change flows.

## Application configuration

Click an application card or name to open its configuration screen. Links such as `/#apps/hello-weasel/networking` can be bookmarked. The screen includes General, History, Networking, Environment, Storage, Resources, and Logs tabs.

- **General:** change the container image and replica count. Saving applies immediately; changing the image rolls out new pods.
- **Networking:** change the container port and Service target port, keeping the Service frontend port stable. The process must actually listen on the chosen port. Start/stop a temporary port forward on `127.0.0.1` of the machine running KubeWeasel. Forwarding requires a ready pod, supports at most 10 tunnels and 32 connections per tunnel, and stops when the server stops. Each connection is limited to five minutes. In-cluster installs should use the provided kubectl command on the user's computer.
- **Domains:** configure one hostname/path route per application, an optional ingress class, and HTTP-only, automatic HTTPS, or an existing TLS Secret. This creates or updates an owned `networking.k8s.io/v1` Ingress; clearing the hostname removes it. An ingress controller and matching DNS must already exist; existing-certificate mode also requires a TLS Secret. Automatic mode uses cert-manager to obtain it. Saving an Ingress does not prove external routing works.
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

**Projects** is the dashboard home and the sidebar entry point for applications. Create a named project with an optional description, open it to see its applications, and use **Edit project** to rename it. New applications are deployed from a project, with a project selection required in the form; existing apps without a project appear under **Unassigned** (shown only when needed) until moved through **General → Project**. All owner-account capabilities remain the same; projects do not add roles, environments, namespace isolation, or permissions.

Live project names/descriptions are stored alongside the owner account in the SQLite database, scoped to the configured namespace. Application assignment is stored on the Deployment as the `kubeweasel.io/project` label. Renaming keeps the project ID stable. Moving an app changes only this metadata label; pods, Services, domains, and namespaces are unchanged. Empty folders may be deleted; move all applications out first. Project operations and deployment assignments are serialized within one dashboard process. Direct external Kubernetes changes are outside that coordination.

Back up the SQLite database as well as Kubernetes resources to retain folder names and assignments. If project metadata is missing after a restore, applications with unknown project IDs are displayed in Unassigned. Demo project folders and assignments remain in memory and reset on restart; the owner account still persists.

## Automatic HTTPS with cert-manager

Networking offers **HTTP only**, **Automatic HTTPS**, and **Existing certificate Secret**. Legacy routes with a TLS Secret continue to use that existing certificate. Automatic mode lets you choose a ready namespace-scoped Issuer or ClusterIssuer and an explicit ingress class. KubeWeasel adds cert-manager ingress-shim annotations and a unique, stable TLS Secret reference to the app's owned Ingress. cert-manager creates the Certificate, issues the key/certificate, and renews it. KubeWeasel never reads Secret contents or private keys.

Install cert-manager once per cluster if needed:

```sh
helm upgrade --install cert-manager cert-manager \
  --repo https://charts.jetstack.io --namespace cert-manager \
  --create-namespace --set crds.enabled=true
```

Configure a namespaced Let's Encrypt issuer through the KubeWeasel Helm chart:

```sh
helm upgrade --install kubeweasel ./charts/kubeweasel \
  --namespace kubeweasel --create-namespace \
  --set image.repository=YOUR_REGISTRY/kubeweasel \
  --set certificateIssuer.create=true \
  --set certificateIssuer.email=YOU@example.com \
  --set certificateIssuer.ingressClass=traefik \
  --set certificateIssuer.staging=true
```

cert-manager must already be installed before enabling issuer creation. Use your real certificate contact email. The staging default produces untrusted test certificates and avoids production issuance during setup. After verifying a real domain, use a separate issuer name (for example `letsencrypt-production`) with `certificateIssuer.staging=false`, then select that issuer in the app. This registers an ACME account with Let's Encrypt; review its terms and applicable rate limits before using production.

For Let's Encrypt HTTP-01 validation, the public domain must resolve to the ingress controller and inbound port **80** must reach the cluster. HTTPS traffic then uses port **443**. A local `/etc/hosts` entry or loopback port-forward does not satisfy public ACME validation. Reserved local names such as `hello.test` cannot use Let's Encrypt; use an explicitly configured local test issuer or an existing certificate instead. Self-signed and staging certificates will not be trusted by normal browser/connection checks.

The certificate status panel detects missing APIs and discovery permissions, lists available issuers, and refreshes every ten seconds while Networking is visible. It shows waiting/issuing, condition messages, issuance failures, readiness, expiration, and planned renewal when cert-manager supplies them. A stale Ready condition or mismatched certificate request is not reported as ready. Certificate readiness is separate from DNS, ingress reachability, and the certificate actually served; use **Test connection** for that final check.

Switching modes removes the managed issuer annotations. HTTP-only mode also removes the route's TLS reference. Certificate objects and Secrets are deliberately retained, including when a route is removed; an existing Certificate may continue renewing until its owner garbage collection or administrator cleanup. Inspect dependencies before deleting unused certificates. Other Ingress annotations are preserved, and unmanaged Ingresses are never overwritten.

The chart grants read-only `get/list` on namespace Certificates and Issuers; it does not grant the app Secret access or issuer-writing privileges. `clusterIssuerDiscovery=true` adds optional read-only ClusterIssuer discovery. Set it to `false` for namespace issuers only. HTTP-only and existing-certificate routes remain usable without cert-manager.

An opt-in live test creates a temporary namespace, a self-signed issuer, and a zero-replica application; verifies real certificate issuance and switching back to HTTP; and deletes the namespace afterward. Run only on an explicitly selected test cluster:

```sh
KUBEWEASEL_RUN_HTTPS_TEST=1 cargo test --locked \
  https::live_tests::cert_manager_issues_and_modes_round_trip -- --ignored
```

## Persistent volumes and database workloads

When deploying a database or other stateful service, choose **Database / persistent service · StatefulSet**. StatefulSets are separate workload resources; existing Deployments are not automatically converted. This release supports zero or one replica for StatefulSets, with a headless governing Service. Replication, clustering, database backups, and high availability need database-specific configuration and are not supplied by increasing replicas.

Open **Storage** on an app to create a PVC or attach an existing filesystem claim in the same namespace. Set the claim name, requested size (1–4096 GiB for new claims), optional StorageClass, and container mount path. New claims use ReadWriteOnce and the cluster default class when no class is specified. Up to eight mounts are supported on the `app` container. The page shows claim status, size, class, and mount path; a Pending claim may be awaiting provisioning or its first scheduled consumer. `storageClassDiscovery=false` disables the optional read-only cluster-wide class-discovery permission; manually entering a class name still works.

Choose the mount path required by the particular database image/version; mounting an empty volume does not migrate data already stored in a container. Configure required database passwords and other image settings before using it. The Storage tab does not grant or manage filesystem ownership for you; some non-root images require an administrator to configure `fsGroup` or existing volume permissions. Database Services remain internal. PostgreSQL/MySQL require their normal TCP port and client; HTTP Ingress domains do not expose these database protocols.

Attaching or detaching a volume replaces pods. Storage-backed Deployments use Recreate updates and are restricted to zero or one replica; new databases should use StatefulSets. Workload patches and storage changes are serialized within the dashboard process and use resource-version preconditions. Storage attachment refuses claims referenced by another workload or mounted by another active pod. Kubernetes users and controllers outside KubeWeasel can still change resources; this is not a substitute for database locking or storage fencing. ReadWriteOnce restricts nodes, not the number of pods on a node; use storage and database architecture appropriate for production.

**Detach · retain data** removes only the pod mount/reference. PVCs are created independently without workload owner references. StatefulSet retention is also explicitly Retain. KubeWeasel has no PVC/data deletion endpoint or PVC delete permission. A retained claim can be attached again later. Deleting a namespace or manually deleting a PVC can still destroy its backing data according to the StorageClass reclaim policy. Persistence is not a backup.

The Helm Role includes namespace-scoped StatefulSet operations and PVC get/list/create. Application listing, settings, logs, networking, projects, and restarts work for Deployments and StatefulSets. Demo mounts are explicitly simulated and disappear on restart; they do not provision storage.

An opt-in live test creates an isolated namespace, StatefulSet, and 1 GiB PVC, writes a marker, verifies it survives pod replacement and detach/reattach, checks replica restrictions, and removes the test namespace (including its test data):

```sh
KUBEWEASEL_RUN_STORAGE_TEST=1 cargo test --locked \
  storage::live_tests::data_survives_pod_replacement_and_detach -- --ignored
```

## Deploying from Git

In a project, choose **Deploy application → Git repository**. Supply a public HTTPS repository on GitHub, GitLab, Codeberg, or Bitbucket, its branch, a Dockerfile path relative to the repository root, and the container port. The build context is the repository root; Dockerfiles in subdirectories still use that context. Private Git repositories, build arguments/secrets, custom contexts, webhooks, and automatic builds on push are not implemented in this first release. Use **General → Deploy settings → Build & deploy** after pushing changes. Container-image deployment continues to work.

Choose an image registry:

- **Docker Hub:** `docker.io/YOUR_USER/YOUR_APP`, Docker Hub username, and an access token with read/write permission.
- **GitHub Container Registry:** `ghcr.io/YOUR_OWNER/YOUR_APP`, GitHub username, and a package token with `read:packages` and `write:packages` permission (plus organization authorization when required).
- **Custom registry:** `registry.example.com[:PORT]/TEAM/APP`, registry username and token/password. The registry must have HTTPS certificates trusted by both the builder and the cluster nodes. Insecure HTTP registries and custom CA installation are not configured by KubeWeasel.

Enter a repository without a tag. Each build gets a unique image tag. Registry credentials are sent only to your dashboard and saved as namespace Kubernetes `kubernetes.io/dockerconfigjson` Secrets. The dashboard creates them without reading them back or exposing them in the Source API. Tokens are not stored in workload annotations, browser storage, or logs. Leave the token empty on later builds to reuse the app's saved credentials; enter credentials again if changing the registry/image repository. Kubernetes uses the same Secret to pull the finished image. Credentials should have access only to the required image repository. Replacing credentials retains the old Secret so existing pods can continue pulling; administrators should revoke obsolete tokens and remove unused Secrets after checking references. Kubernetes Secret encryption at rest is a cluster configuration responsibility.

Builds use one rootless `moby/buildkit:v0.33.0-rootless` Kubernetes Job per build, without a Docker socket, host mounts, or a mounted Kubernetes service-account token. The Job has CPU, memory and ephemeral-disk bounds, a 30-minute deadline, and no automatic retries. At most two unfinished KubeWeasel builds run in the namespace. Finished Jobs/logs expire after seven days; registry image cleanup is separate. Only the latest build's last 250 log lines (up to 64 KiB) are shown. Logs refresh every five seconds while General is open.

The rootless runner follows [BuildKit's Kubernetes example](https://github.com/moby/buildkit/blob/master/examples/kubernetes/job.rootless.yaml): it requires unprivileged user namespaces, unconfined seccomp/AppArmor profiles, and BuildKit's `--oci-worker-no-process-sandbox`. Clusters enforcing the restricted Pod Security profile may reject it. Only build trusted source: Dockerfiles execute code and can access the builder's network and registry credentials. For untrusted/multi-tenant builds, use a separately isolated builder infrastructure. The build runs for its scheduled node architecture; multi-platform image builds are not configured.

Build configuration and current build IDs live in workload annotations; Kubernetes Jobs are the source of build progress. A dashboard background worker resumes observing them after restart. New Git applications remain at zero replicas until the first successful build. Successful builds publish the image and request a rollout; **Deploying** is distinct from ready/running pods. Failed builds retain the previous image, runtime configuration, storage and replica count. Existing applications retain their replica count on a successful rebuild, including a paused app. If Job creation fails after creating the application, the error reports the partial result; open that application's General → Deploy settings panel to retry.

The Helm Role adds namespace Job `get/list/create` and Secret **create only**, not Secret read/list/delete. Upgrade the chart before using Git builds in an existing Helm installation. Rootless runtime compatibility and registry push/pull connectivity must also be available.

An opt-in local-cluster check performs a real public Git checkout and Dockerfile build into an OCI archive, exercises successful rollout handling with a known public image, verifies failed-build preservation, and removes its isolated test namespace. It does not publish to a registry and needs no registry credentials:

```sh
KUBEWEASEL_RUN_GIT_TEST=1 cargo test --locked \
  git::live_tests::public_git_build_and_rollout_recovery -- --ignored
```

### Pulling existing container images

For **Container image** deployments, choose Docker Hub, GitHub Container Registry, or Custom registry. Docker Hub accepts short names such as `nginx:stable`; GitHub accepts `owner/app:tag` or a full `ghcr.io` reference. Custom registries require a hostname (and optional port) in the image reference. The server rejects images whose hostname conflicts with the chosen provider. Public images need no credentials. Expand **Private image · registry credentials** to supply a username and token/password; credentials are saved in a Kubernetes image-pull Secret and attached before the first pod starts. This also works for StatefulSets. Credential Secrets are retained on partial deployment failures; administrators can remove unused Secrets after checking references. Demo mode simulates public-image deployments and does not save credentials.

Source configuration lives in **General → Deploy settings**, for both existing and new applications. Switch between a container image and a public Git repository there; container images offer Docker Hub, GitHub Container Registry, and custom registries. Image source changes preserve environment, networking, storage and replicas; blank credentials preserve existing pull credentials. Replica changes use their own Save replicas action in the same panel. Switching from Git to an image clears the saved Git source and pending promotion under the build lock: an already running builder may finish, but its output will not replace the chosen image. Build status and logs are available inside the same panel.

## Deployment history and rollback

Open an app's **History** tab to review its latest 30 image releases and Git build attempts. Each entry shows its observation/request time, image, action, result, and rollout message. Git builds progress through Building → Deploying → Running (or Failed); paused workloads are shown separately. Kubernetes readiness is verified for the updated rollout, not inferred from an older ready pod. Readiness does not guarantee that your application is functionally correct.

History is stored in a bounded workload annotation and survives dashboard restarts. Existing apps begin with an **Existing image** baseline; deployments from before this feature cannot be reconstructed. History tracks image changes/builds, not complete snapshots of every settings edit. Git images use the unique build tag; the commit SHA is not currently captured. An observation worker refreshes readiness and records a pullable image digest when Kubernetes reports one. Arbitrary container-runtime configuration IDs are not treated as pullable manifest digests.

**Rollback to this image** is available for an earlier release that reached readiness. It restores that image (pinned to the observed manifest digest when available) and saved image-pull Secret references. When no pullable digest was captured, rollback uses the recorded reference: mutable tags may no longer point to the old image. Keep released images/digests and their credential Secrets in your registry; registry retention and expired credentials can make rollback fail. The rollback itself creates a new history entry and must reach readiness again.

Rollback preserves current replicas, environment variables, resources, networking, mounts, PVCs, and database data. It does not revert migrations or guarantee compatibility with an older application version. Rolling back clears the saved Git source and pending Git promotion, so a build that is already running cannot later overwrite the rollback. Reconfigure Git in General → Deploy settings to build again. Paused apps remain paused. History and rollback require live mode; demo mode does not simulate successful rollbacks.

An opt-in live check deploys a working image with a test PVC, changes to a broken image, observes failure, rolls back, verifies readiness, and confirms that the original PVC and runtime settings are preserved. It removes its isolated namespace afterward:

```sh
KUBEWEASEL_RUN_HISTORY_TEST=1 cargo test --locked \
  history::live_tests::rollback_recovers_image_and_preserves_storage_and_configuration -- --ignored
```
