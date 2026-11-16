# NetBird and K3s ingress forwarding

Some NetBird/K3s combinations reject VPN traffic to ingress while host services still work. NetBird authorizes a peer-to-local connection with packet mark `0x0001bd20`; kube-router or kube-proxy then changes bits in that mark during forwarding. NetBird's exact-match forwarding guard no longer recognizes it. See [upstream report #6022](https://github.com/netbirdio/netbird/issues/6022).

`scripts/netbird-ingress.py` is an **optional Ubuntu host workaround**, separate from the KubeWeasel Helm installer. It is intended for this diagnosed mark collision, not general DNS, routing or firewall troubleshooting.

## Scope

- Requires the expected NetBird authorization rule and prerouting mark setter. Unknown or changed semantics cause it to remove its own exception and fail closed.
- Detects the server's own IPv4 address on `wt0`; rejects public or ambiguous addresses. IPv6 is not handled.
- Permits only DNATed TCP connections originally addressed to that server's NetBird IP and the configured ingress ports (80 and 443 by default).
- Continues to require NetBird's authorization mark. Other devices work only when their current NetBird policy permits that connection. Source and port revocations continue to apply.
- Ignores only kube-router's `0x10000`/`0x20000` bits and kube-proxy's `0x4000` bit when recognizing the mark. It never changes those bits, so Kubernetes policy verdicts and masquerading remain intact.
- Inserts the scoped exception immediately before NetBird’s existing authorization guard, retaining any earlier native policy decisions.
- Changes only comment-marked helper rules in NetBird's forwarding chain. It does not disable NetBird filtering, change access policies, restart NetBird/K3s, or save/restore the complete dynamic firewall.

## Install on each Ubuntu ingress node

Download the script from a reviewed, pinned repository commit. Run:

```sh
sudo python3 -I netbird-ingress.py install
sudo kubeweasel-netbird-ingress status
```

Run the same commands on each ingress node you want to reach. The address is detected locally, so there is no Mac IP or server IP to edit. Advanced overrides are `--interface wt0 --ports 80,443`; reinstall without overrides preserves the existing configuration.

Installation replaces the earlier temporary rule with comment `kubeweasel-http80` after the new rule passes nftables' check. A single atomic batch removes that old exception and adds the policy-aware rule. Unrelated rules are retained.

The installer writes a root-owned helper, configuration, systemd oneshot service, and timer. It applies immediately, runs after boot and checks every 15 seconds. NetBird policy refreshes/restarts can cause a brief interruption until the next check (approximately 15 seconds plus timer accuracy), but no manual reapplication is needed. It does not replace an upstream NetBird fix.

Inspect with:

```sh
sudo systemctl status kubeweasel-netbird-ingress.timer --no-pager
sudo kubeweasel-netbird-ingress status
sudo journalctl -u kubeweasel-netbird-ingress.service -n 30 --no-pager
```

`status` reports the detected address, ports, guard compatibility and rule counters. If a NetBird update changes the authorization scheme, the helper stops applying exceptions and logs an error. Review compatibility instead of bypassing the check.

## Remove

```sh
sudo kubeweasel-netbird-ingress uninstall
```

This removes only the helper's rule, files and timer. The original NetBird filtering remains; ingress may become unreachable again if the underlying collision still exists. Kubernetes resources, application volumes and dashboard data are never changed.

## Verification

Unit tests use fake command runners and temporary files. The Linux integration test runs in a fresh network namespace and refuses to operate in the host network namespace. It reproduces the collision with real nftables and checks authorized and unauthorized devices, port restrictions, policy revocation, retained marks, idempotence, recovery after rule removal and fail-closed behavior after an upstream guard change. No real NetBird daemon, credentials, systemd service or cluster is modified by the test.
