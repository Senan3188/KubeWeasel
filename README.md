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

This version has no application login or multi-user authorization. Keep access local or use the Helm port-forward flow with trusted Kubernetes users. `KUBEWEASEL_BIND` defaults to `127.0.0.1:7878`. Do not expose the API publicly; anyone who can reach it can exercise its Kubernetes permissions. Mutation requests require a custom header to block ordinary cross-origin form submissions; this is not authentication.

This release does not yet implement Git builds, projects/environments, automatic HTTPS provisioning, secrets editing, private registry setup, rollback history, a custom-resource controller, or multi-cluster support. Existing cluster networking, registry credentials, and admission policies still apply. Health indicates replica readiness; it is not an application-level health probe. Cluster changes occur when you deploy, scale, restart, or save application settings through the UI.

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
