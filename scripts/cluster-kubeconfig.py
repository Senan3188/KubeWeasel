#!/usr/bin/env python3
"""Create a private, self-contained kubeconfig for a remote namespace service account."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
from urllib.parse import urlsplit


def kubectl(context, *args):
    result = subprocess.run(["kubectl", "--context", context, *args], capture_output=True, text=True, check=False)
    if result.returncode:
        # kubectl errors may contain credentials or other kubeconfig values.
        raise ValueError("A Kubernetes command failed. Check context, namespace and token-creation permissions.")
    return result.stdout


def create(args):
    config = json.loads(kubectl(args.context, "config", "view", "--raw", "--flatten", "--minify", "-o", "json"))
    cluster = config["clusters"][0]["cluster"]
    url = urlsplit(cluster["server"])
    if url.scheme != "https" or not url.hostname or url.username or url.password or url.query or url.fragment:
        raise ValueError("The cluster endpoint must use HTTPS without URL credentials or parameters.")
    if cluster.get("insecure-skip-tls-verify") or not cluster.get("certificate-authority-data"):
        raise ValueError("The source context needs verified TLS and an embedded certificate authority.")
    if cluster.get("proxy-url"):
        raise ValueError("Proxy-based contexts are not supported for uploaded connections.")
    token = kubectl(args.context, "-n", args.namespace, "create", "token", args.service_account, "--duration=" + args.duration).strip()
    if not token:
        raise ValueError("The API server did not return a token.")
    output = {"apiVersion": "v1", "kind": "Config", "current-context": "remote",
              "clusters": [{"name": "remote", "cluster": {k: cluster[k] for k in ("server", "certificate-authority-data", "tls-server-name") if k in cluster}}],
              "contexts": [{"name": "remote", "context": {"cluster": "remote", "user": "remote", "namespace": args.namespace}}],
              "users": [{"name": "remote", "user": {"token": token}}]}
    # Exclusive creation prevents overwriting another connection or following a symlink.
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "w") as file:
            json.dump(output, file, indent=2)
            file.write("\n")
    except BaseException:
        Path(args.output).unlink(missing_ok=True)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--context", required=True)
    parser.add_argument("--namespace", default="kubeweasel")
    parser.add_argument("--service-account", default="kubeweasel-remote")
    parser.add_argument("--duration", default="1h")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    try:
        create(args)
    except (ValueError, KeyError, IndexError, OSError):
        print("Could not create connection file. Check the verified context, namespace, service account, permissions and that the output file does not already exist.", file=sys.stderr)
        return 1
    print("Created private connection file. Upload it through Manage clusters, then remove the local file. Rotate credentials before the token expires.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
