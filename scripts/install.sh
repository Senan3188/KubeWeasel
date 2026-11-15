#!/usr/bin/env bash
# KubeWeasel installer: no sudo, no namespace deletion, no credential collection.
set -euo pipefail
VERSION=0.2.0
REPOSITORY=Senan3188/KubeWeasel
COMMAND=install
NAMESPACE=kubeweasel
RELEASE=kubeweasel
CONTEXT=
CHART=
IMAGE=ghcr.io/senan3188/kubeweasel
IMAGE_TAG=
STORAGE_CLASS=
STORAGE_SIZE=
TIMEOUT=8m
NAMESPACE_ONLY=false
LOCAL_PORT=7878
WORK=
cleanup() { if [[ -n "$WORK" ]]; then rm -rf -- "$WORK"; fi; }
trap cleanup EXIT
fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }
usage() {
 cat <<'HELP'
Usage: bash install.sh [install|upgrade|status|open|uninstall] [options]
  --version VERSION         Release version (default: this installer's version)
  --context CONTEXT         Kubernetes context (default: current context)
  --namespace NAME          Installation/app namespace (default: kubeweasel)
  --release NAME            Helm release (default: kubeweasel)
  --storage-class NAME      StorageClass for a new account volume
  --storage-size SIZE       Size of a new account volume (default: chart's 1Gi)
  --namespace-only          Disable optional cluster-wide discovery/monitoring
  --timeout DURATION        Readiness timeout (default: 8m)
  --chart PATH              Use a local chart directory/archive (development)
  --image-repository IMAGE  Override container repository (development)
  --image-tag TAG           Override container tag (development)
  --help                    Show this help
Requires kubectl and Helm 3.12+ or Helm 4, a running cluster, and persistent storage.
Only the selected context is used. No ingress, metrics server, or cluster is installed.
Uninstall preserves the namespace, account volume, and deployed applications.
HELP
}
if [[ $# -gt 0 && "$1" != --* ]]; then COMMAND=$1; shift; fi
while [[ $# -gt 0 ]]; do
 case "$1" in
  --help|-h) usage; exit 0 ;;
  --namespace-only) NAMESPACE_ONLY=true; shift ;;
  --version|--context|--namespace|--release|--storage-class|--storage-size|--timeout|--chart|--image-repository|--image-tag)
   [[ $# -ge 2 && -n "$2" ]] || fail "Missing value for $1"
   case "$1" in
    --version) VERSION=${2#v};; --context) CONTEXT=$2;; --namespace) NAMESPACE=$2;; --release) RELEASE=$2;;
    --storage-class) STORAGE_CLASS=$2;; --storage-size) STORAGE_SIZE=$2;;
    --timeout) TIMEOUT=$2;; --chart) CHART=$2;; --image-repository) IMAGE=$2;; --image-tag) IMAGE_TAG=$2;;
   esac; shift 2 ;;
  *) fail "Unknown option: $1 (use --help)" ;;
 esac
done
case "$COMMAND" in install|upgrade|status|open|uninstall) ;; *) fail "Unknown command: $COMMAND";; esac
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "Version must be a stable version such as 0.2.0."
[[ "$NAMESPACE" =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$ && ${#NAMESPACE} -le 63 ]] || fail "Invalid namespace."
[[ "$RELEASE" =~ ^[a-z0-9]([a-z0-9-]*[a-z0-9])?$ && ${#RELEASE} -le 53 ]] || fail "Invalid Helm release name."
[[ "$LOCAL_PORT" =~ ^[0-9]+$ && ${#LOCAL_PORT} -le 5 ]] || fail "Invalid local port."
((10#$LOCAL_PORT >= 1024 && 10#$LOCAL_PORT <= 65535)) || fail "Local port must be 1024–65535."
for program in kubectl helm; do command -v "$program" >/dev/null || fail "Install $program first, then run this installer again."; done
HELM_VERSION=$(helm version --template '{{.Version}}')
[[ "$HELM_VERSION" =~ ^v([0-9]+)\.([0-9]+) ]] || fail "Could not determine Helm version."
(( BASH_REMATCH[1] >= 4 || (BASH_REMATCH[1] == 3 && BASH_REMATCH[2] >= 12) )) || fail "Helm 3.12 or newer is required."
if [[ -z "$CONTEXT" ]]; then CONTEXT=$(kubectl config current-context) || fail "Select a Kubernetes context first."; fi
[[ -n "$CONTEXT" ]] || fail "No Kubernetes context selected."
K=(kubectl --context "$CONTEXT" --namespace "$NAMESPACE")
H=(helm --kube-context "$CONTEXT" --namespace "$NAMESPACE")
printf 'Kubernetes context: %s\nNamespace: %s\nRelease: %s\n' "$CONTEXT" "$NAMESPACE" "$RELEASE"
"${K[@]}" get --raw=/readyz >/dev/null || fail "Cannot connect to the selected cluster. Start it or check your kubeconfig."
access() {
 printf '\nOpen the dashboard (keep this command running):\n  '
 printf '%q ' "${K[@]}" port-forward --address 127.0.0.1 "deployment/$RELEASE" "$LOCAL_PORT:7878"
 printf '\nThen open http://127.0.0.1:%s\n\nFor first-run owner setup, retrieve the setup token locally:\n  ' "$LOCAL_PORT"
 printf '%q ' "${K[@]}" logs "deployment/$RELEASE" --container dashboard
 printf '\nThe first-run token changes on restart until the account is created. Existing owners sign in normally.\n'
}
if [[ "$COMMAND" == status ]]; then "${H[@]}" status "$RELEASE"; "${K[@]}" get deployment "$RELEASE"; "${K[@]}" get pvc; access; exit; fi
if [[ "$COMMAND" == open ]]; then exec "${K[@]}" port-forward --address 127.0.0.1 "deployment/$RELEASE" "$LOCAL_PORT:7878"; fi
if [[ "$COMMAND" == uninstall ]]; then
 MANIFEST=$("${H[@]}" get manifest "$RELEASE") || fail "Cannot inspect the installed release before uninstall."
 UNSAFE=$(printf '%s\n' "$MANIFEST" | awk '/^---/ {if (pvc && !keep) print "unsafe"; pvc=0;keep=0} /^kind: PersistentVolumeClaim/ {pvc=1} /helm.sh\/resource-policy: *"?keep"?/ {keep=1} END {if(pvc && !keep) print "unsafe"}')
 [[ -z "$UNSAFE" ]] || fail "This older release does not retain its account PVC. Upgrade with the new chart before uninstalling. Nothing was deleted."
 "${H[@]}" uninstall "$RELEASE" --wait --timeout "$TIMEOUT"
 printf '\nDashboard removed. Namespace, retained account volume, and deployed applications were not deleted.\nReinstall with the same namespace and release name to reuse the retained account volume.\n'
 exit
fi
EXISTS=false
# Listing releases detects API/permission failures rather than confusing them with absence.
RELEASES=$("${H[@]}" list --all --short --filter "^${RELEASE}$") || fail "Cannot inspect Helm releases in this namespace."
if [[ "$RELEASES" == "$RELEASE" ]]; then EXISTS=true; fi
if [[ "$COMMAND" == install && "$EXISTS" == true ]]; then printf 'Already installed. Use upgrade to change versions.\n'; access; exit; fi
if [[ "$COMMAND" == upgrade && "$EXISTS" == false ]]; then fail "Release not found. Use install first."; fi
if [[ -z "$CHART" ]]; then
 for program in curl tar; do command -v "$program" >/dev/null || fail "Install $program first."; done
 if command -v sha256sum >/dev/null; then HASH=(sha256sum); elif command -v shasum >/dev/null; then HASH=(shasum -a 256); else fail "Install sha256sum or shasum."; fi
 WORK=$(mktemp -d)
 BASE="https://github.com/$REPOSITORY/releases/download/v$VERSION"
 ASSET="kubeweasel-$VERSION.tgz"
 printf 'Downloading KubeWeasel %s…\n' "$VERSION"
 curl --proto '=https' --proto-redir '=https' -fLsS --retry 3 "$BASE/$ASSET" -o "$WORK/$ASSET" || fail "Release/chart unavailable. Check the version and network connection."
 curl --proto '=https' --proto-redir '=https' -fLsS --retry 3 "$BASE/SHA256SUMS" -o "$WORK/SHA256SUMS" || fail "Release checksum unavailable."
 EXPECTED=$(awk -v name="$ASSET" '$2==name {print $1}' "$WORK/SHA256SUMS")
 [[ "$EXPECTED" =~ ^[a-f0-9]{64}$ ]] || fail "No valid checksum found for chart."
 ACTUAL=$("${HASH[@]}" "$WORK/$ASSET"); ACTUAL=${ACTUAL%% *}
 [[ "$ACTUAL" == "$EXPECTED" ]] || fail "Chart checksum mismatch. Nothing was installed."
 CHART="$WORK/$ASSET"
fi
[[ -e "$CHART" ]] || fail "Chart path does not exist."
CHART_VERSION=$(helm show chart "$CHART" | awk '$1=="version:" {gsub(/"/,"",$2);print $2}')
[[ "$CHART_VERSION" == "$VERSION" ]] || fail "Chart version does not match requested release."
ARGS=(upgrade --install "$RELEASE" "$CHART" --wait --timeout "$TIMEOUT")
if [[ "$NAMESPACE_ONLY" == false ]]; then ARGS+=(--create-namespace); fi
if [[ "$EXISTS" == true ]]; then ARGS+=(--reuse-values); fi
ARGS+=(--set-string "image.repository=$IMAGE" --set-string "image.tag=${IMAGE_TAG:-$VERSION}")
if [[ "$NAMESPACE_ONLY" == true ]]; then ARGS+=(--set ingressClassDiscovery=false --set nodeMonitoring=false --set nodeManagement=false --set storageClassDiscovery=false --set clusterIssuerDiscovery=false); fi
if [[ "$EXISTS" == false ]]; then
 CLAIM=$("${K[@]}" get pvc "$RELEASE-data" --ignore-not-found -o name) || fail "Cannot inspect account storage."
 if [[ -n "$CLAIM" ]]; then
  printf 'Reusing retained account volume %s-data.\n' "$RELEASE"
  ARGS+=(--set-string "persistence.existingClaim=$RELEASE-data")
 fi
fi
[[ -z "$STORAGE_CLASS" ]] || ARGS+=(--set-string "persistence.storageClass=$STORAGE_CLASS")
[[ -z "$STORAGE_SIZE" ]] || ARGS+=(--set-string "persistence.size=$STORAGE_SIZE")
printf 'Installing release and waiting for persistent storage and dashboard readiness…\n'
if ! "${H[@]}" "${ARGS[@]}"; then
 printf '\nInstallation did not reach readiness. Resources were retained for inspection.\n' >&2
 "${K[@]}" get pods,pvc >&2 || true
 printf 'Inspect pod events for image-pull errors, or choose --storage-class if the account PVC is Pending.\nUse upgrade to retry a failed release; no namespace or data was automatically deleted.\n' >&2
 exit 1
fi
access
