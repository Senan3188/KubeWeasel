#!/usr/bin/python3 -I
"""Opt-in Linux host workaround for NetBird/K3s ingress mark collisions.

Only authorizes DNATed ingress requests already marked as authorized by NetBird.
Never rewrites packet marks or snapshots/restores Kubernetes' dynamic firewall.
"""
import argparse
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys

MARKER = 'Managed by KubeWeasel NetBird ingress helper'
AUTH_MARK = 0x0001BD20
# Ignore only kube-router's policy bits and kube-proxy's masquerade bit.
MASK = 0xFFFCBFFF
EXPECTED = AUTH_MARK & MASK
PREFIX = 'kw-netbird-ingress:v1:'
CHAIN = 'netbird-acl-forward-filter'
CONFIG = Path('/etc/kubeweasel-netbird-ingress.json')
EXECUTABLE = Path('/usr/local/sbin/kubeweasel-netbird-ingress')
UNIT_DIR = Path('/etc/systemd/system')
SERVICE = 'kubeweasel-netbird-ingress.service'
TIMER = 'kubeweasel-netbird-ingress.timer'
RUNTIME = Path('/run/kubeweasel-netbird-ingress')
SAFE_PATH = '/usr/sbin:/usr/bin:/sbin:/bin'


class Failure(RuntimeError):
    pass


class Runner:
    def __call__(self, *args, data=None, optional=False):
        result = subprocess.run(args, input=data, text=True, capture_output=True,
                                env={**os.environ, 'PATH': SAFE_PATH}, timeout=15)
        if result.returncode:
            if optional and 'No such file or directory' in result.stderr:
                return None
            raise Failure(f'{args[0]} failed: {result.stderr.strip()}')
        return result.stdout


def validate(interface, ports):
    if not re.fullmatch(r'[A-Za-z0-9_.:-]{1,15}', interface):
        raise Failure('Invalid NetBird interface name.')
    if (not ports or len(ports) > 16 or any(type(p) is not int or not 1 <= p <= 65535 for p in ports)):
        raise Failure('Choose between 1 and 16 TCP ports, each 1–65535.')
    return {'managed_by': MARKER, 'interface': interface, 'ports': sorted(set(ports))}


def peer_address(run, interface):
    output = json.loads(run('ip', '-j', '-4', 'address', 'show', 'dev', interface))
    addresses = {a['local'] for link in output for a in link.get('addr_info', [])
                 if a.get('family') == 'inet' and a.get('scope') == 'global'}
    if len(addresses) != 1:
        raise Failure('The NetBird interface must have exactly one global IPv4 address.')
    address = ipaddress.IPv4Address(addresses.pop())
    if not (address.is_private or address in ipaddress.IPv4Network('100.64.0.0/10')) or address.is_unspecified:
        raise Failure('The NetBird interface address must be private or CGNAT.')
    return str(address)


def rules(run, chain):
    output = run('nft', '-j', '-n', 'list', 'chain', 'ip', 'netbird', chain, optional=True)
    if output is None:
        return None
    return [item['rule'] for item in json.loads(output).get('nftables', []) if 'rule' in item]


def numeric(value):
    try:
        return int(value, 0) if isinstance(value, str) else int(value)
    except (ValueError, TypeError):
        return None


def compatible(forward, prerouting):
    """Recognize the observed NetBird authorization contract; fail closed otherwise."""
    if forward is None or prerouting is None:
        return False
    guard = False
    for rule in forward:
        expressions = [e for e in rule.get('expr', []) if 'counter' not in e]
        if len(expressions) != 2 or 'accept' not in expressions[-1]:
            continue
        match = expressions[0].get('match', {})
        if (match.get('op') == '==' and match.get('left') == {'meta': {'key': 'mark'}}
                and numeric(match.get('right')) == AUTH_MARK):
            guard = True
    setter = any(e.get('mangle', {}).get('key') == {'meta': {'key': 'mark'}}
                 and numeric(e.get('mangle', {}).get('value')) == AUTH_MARK
                 for rule in prerouting for e in rule.get('expr', []))
    return guard and setter


def comment_for(config, address):
    scope = json.dumps({**config, 'address': address, 'mask': MASK}, sort_keys=True)
    return PREFIX + hashlib.sha256(scope.encode()).hexdigest()[:16]


def rule_text(config, address):
    ports = ', '.join(str(p) for p in config['ports'])
    port_set = str(config['ports'][0]) if len(config['ports']) == 1 else '{ ' + ports + ' }'
    return (f'insert rule ip netbird {CHAIN} iifname "{config["interface"]}" '
            f'ct status dnat ct original ip daddr {address} '
            f'meta l4proto tcp ct original proto-dst {port_set} '
            f'meta mark and 0x{MASK:08x} == 0x{EXPECTED:08x} '
            f'counter accept comment "{comment_for(config, address)}"')


def owned(rule):
    return str(rule.get('comment', '')).startswith(PREFIX)


def deletes(entries):
    commands = []
    for entry in entries:
        handle = entry.get('handle')
        if type(handle) is not int or handle <= 0:
            raise Failure('Cannot safely identify a managed rule handle.')
        commands.append(f'delete rule ip netbird {CHAIN} handle {handle}')
    return commands


def reconcile(run, config):
    current = rules(run, CHAIN)
    if current is None:
        return 'waiting'  # NetBird may be restarting; retry on the next timer tick.
    ours = [r for r in current if owned(r)]
    if not compatible(current, rules(run, 'netbird-mangle-prerouting')):
        if ours:
            run('nft', '-f', '-', data='\n'.join(deletes(ours)) + '\n')
        return 'unsupported'
    try:
        address = peer_address(run, config['interface'])
    except Failure:
        if ours:
            run('nft', '-f', '-', data='\n'.join(deletes(ours)) + '\n')
        raise
    expected_comment = comment_for(config, address)
    legacy = [r for r in current if r.get('comment') == 'kubeweasel-http80']
    if len(ours) == 1 and ours[0].get('comment') == expected_comment and not legacy:
        return 'active'
    # One atomic batch: keep all unrelated rules and replace only our exceptions.
    commands = deletes(ours + legacy) + [rule_text(config, address)]
    batch = '\n'.join(commands) + '\n'
    run('nft', '--check', '-f', '-', data=batch)
    run('nft', '-f', '-', data=batch)
    print(f'Ingress workaround active on {config["interface"]} ({address}), TCP {config["ports"]}; NetBird authorization remains required.')
    return 'active'


def private_file(path):
    meta = path.lstat()
    if not stat.S_ISREG(meta.st_mode) or meta.st_uid != 0 or meta.st_mode & 0o022:
        raise Failure(f'{path} must be a root-owned regular file without group/world write access.')


def load_config():
    private_file(CONFIG)
    data = json.loads(CONFIG.read_text())
    if data.get('managed_by') != MARKER:
        raise Failure('Configuration is not managed by this helper.')
    return validate(data['interface'], data['ports'])


def lock():
    RUNTIME.mkdir(mode=0o700, parents=True, exist_ok=True)
    meta = RUNTIME.lstat()
    if not stat.S_ISDIR(meta.st_mode) or meta.st_uid != 0 or meta.st_mode & 0o022:
        raise Failure('Unsafe runtime directory.')
    descriptor = os.open(RUNTIME / 'lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    meta = os.fstat(descriptor)
    if not stat.S_ISREG(meta.st_mode) or meta.st_uid != 0 or meta.st_mode & 0o022:
        os.close(descriptor)
        raise Failure('Unsafe lock file.')
    fcntl.flock(descriptor, fcntl.LOCK_EX)
    return descriptor


def unit_contents():
    service = f'''# {MARKER}
[Unit]
Description=Maintain authorized NetBird access to Kubernetes ingress
After=network-online.target netbird.service k3s.service k3s-agent.service
Wants=network-online.target

[Service]
Type=oneshot
ExecStart={EXECUTABLE} reconcile
TimeoutStartSec=45
RuntimeDirectory=kubeweasel-netbird-ingress
RuntimeDirectoryMode=0700
RuntimeDirectoryPreserve=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
CapabilityBoundingSet=CAP_NET_ADMIN
'''
    timer = f'''# {MARKER}
[Unit]
Description=Restore the scoped NetBird ingress workaround after rule refreshes

[Timer]
OnBootSec=10s
OnUnitInactiveSec=15s
AccuracySec=1s
Unit={SERVICE}

[Install]
WantedBy=timers.target
'''
    return service, timer


def check_managed_file(path, kind):
    if not path.exists() and not path.is_symlink():
        return
    private_file(path)
    if kind == 'config':
        if json.loads(path.read_text()).get('managed_by') != MARKER:
            raise Failure(f'Refusing to replace unmanaged file {path}.')
    elif MARKER not in path.read_text():
        raise Failure(f'Refusing to replace unmanaged file {path}.')


def write_file(path, data, mode):
    # Parents are standard root-owned system directories; refuse symlink targets.
    if path.is_symlink():
        raise Failure(f'Refusing symbolic link {path}.')
    temporary = path.with_name(path.name + '.kw-new')
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
    try:
        with os.fdopen(descriptor, 'w') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if temporary.exists():
            temporary.unlink()


def install(run, args):
    for program in ('nft', 'ip', 'systemctl'):
        if shutil.which(program, path=SAFE_PATH) is None:
            raise Failure(f'Install {program} first.')
    paths = ((CONFIG, 'config'), (EXECUTABLE, 'script'), (UNIT_DIR / SERVICE, 'unit'), (UNIT_DIR / TIMER, 'unit'))
    for path, kind in paths:
        check_managed_file(path, kind)
    run('systemctl', 'show', '--property=Version', '--value')
    previous = load_config() if CONFIG.exists() else validate('wt0', [80, 443])
    config = validate(args.interface or previous['interface'], args.ports or previous['ports'])
    if not compatible(rules(run, CHAIN), rules(run, 'netbird-mangle-prerouting')):
        raise Failure('The expected NetBird mark-based firewall is absent or unsupported; nothing was installed.')
    peer_address(run, config['interface'])
    service, timer = unit_contents()
    # No firewall snapshot, NetBird restart, or Kubernetes service changes.
    write_file(EXECUTABLE, Path(__file__).read_text(), 0o755)
    write_file(CONFIG, json.dumps(config, indent=2) + '\n', 0o644)
    write_file(UNIT_DIR / SERVICE, service, 0o644)
    write_file(UNIT_DIR / TIMER, timer, 0o644)
    run('systemctl', 'daemon-reload')
    run('systemctl', 'enable', '--now', TIMER)
    run('systemctl', 'start', SERVICE)
    print('Installed. Boot and 15-second reconciliation enabled. Check: sudo kubeweasel-netbird-ingress status')


def uninstall(run):
    for path, kind in ((CONFIG, 'config'), (EXECUTABLE, 'script'), (UNIT_DIR / SERVICE, 'unit'), (UNIT_DIR / TIMER, 'unit')):
        check_managed_file(path, kind)
    if (UNIT_DIR / TIMER).exists():
        run('systemctl', 'disable', '--now', TIMER)
    if (UNIT_DIR / SERVICE).exists():
        run('systemctl', 'stop', SERVICE)
    descriptor = lock()
    try:
        current = rules(run, CHAIN) or []
        commands = deletes([r for r in current if owned(r)])
        if commands:
            run('nft', '-f', '-', data='\n'.join(commands) + '\n')
        for path in (CONFIG, EXECUTABLE, UNIT_DIR / SERVICE, UNIT_DIR / TIMER):
            path.unlink(missing_ok=True)
    finally:
        os.close(descriptor)
    run('systemctl', 'daemon-reload')
    print('Removed only the managed workaround and timer. NetBird and Kubernetes rules were retained.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('install', 'reconcile', 'status', 'uninstall'))
    parser.add_argument('--interface', help='NetBird interface (default wt0; existing configuration is retained)')
    parser.add_argument('--ports', type=lambda s: [int(p) for p in s.split(',')], help='TCP ingress ports (default 80,443)')
    args = parser.parse_args()
    if sys.platform != 'linux' or os.geteuid() != 0:
        raise Failure('Run this opt-in host helper as root on the Ubuntu/K3s server.')
    run = Runner()
    if args.command == 'install':
        install(run, args)
    elif args.command == 'uninstall':
        uninstall(run)
    else:
        if args.command == 'reconcile':
            descriptor = lock()
            try:
                outcome = reconcile(run, load_config())
            finally:
                os.close(descriptor)
            if outcome == 'unsupported':
                raise Failure('NetBird firewall semantics changed; managed exception removed. Review before re-enabling.')
        else:
            config = load_config()
            current = rules(run, CHAIN)
            supported = compatible(current, rules(run, 'netbird-mangle-prerouting'))
            address = peer_address(run, config['interface'])
            active = [r for r in current or [] if r.get('comment') == comment_for(config, address)]
            print(json.dumps({'interface': config['interface'], 'address': address, 'tcp_ports': config['ports'], 'supported_guard': supported, 'active_rules': len(active), 'counters': [e['counter'] for r in active for e in r.get('expr', []) if 'counter' in e]}, indent=2))


if __name__ == '__main__':
    try:
        main()
    except (Failure, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(f'Error: {error}', file=sys.stderr)
        sys.exit(1)
