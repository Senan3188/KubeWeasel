"""Exercise the host helper with real nftables in a disposable network namespace.

Run only as: sudo env KUBEWEASEL_RUN_NETBIRD_FIX_TEST=1 unshare --net python3 ...
Never uses the runner's host network, NetBird daemon, systemd, or system config.
"""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time

if os.environ.get('KUBEWEASEL_RUN_NETBIRD_FIX_TEST') != '1' or os.geteuid() != 0:
    raise SystemExit('Explicit isolated-root test opt-in required.')
if os.readlink('/proc/self/ns/net') == os.readlink('/proc/1/ns/net'):
    raise SystemExit('Refusing to change the host network namespace.')

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('netbird_fix', ROOT/'scripts/netbird-ingress.py')
fix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fix)
run = fix.Runner()
processes = []


def command(*args):
    return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT)


def inside(pid, *args):
    return command('nsenter', '-t', str(pid), '-n', *args)


def netns():
    child = subprocess.Popen(['unshare', '--net', 'sleep', '300'])
    processes.append(child)
    for _ in range(100):
        if os.readlink(f'/proc/{child.pid}/ns/net') != os.readlink('/proc/self/ns/net'):
            return child.pid
        time.sleep(.02)
    raise RuntimeError('Child network namespace did not start.')


def nft(data):
    run('nft', '-f', '-', data=data)


def address_client(address):
    inside(client, 'ip', '-4', 'addr', 'flush', 'dev', 'client0')
    inside(client, 'ip', 'addr', 'add', address+'/16', 'dev', 'client0')


def reachable(port=80):
    code = '''import urllib.request
try:
 r=urllib.request.build_opener(urllib.request.ProxyHandler({})).open('http://100.73.28.50:PORT/',timeout=.8)
 assert r.status==200 and r.read()==b'ok'
except Exception:raise SystemExit(1)
'''.replace('PORT', str(port))
    result = subprocess.run(['nsenter', '-t', str(client), '-n', sys.executable, '-c', code], capture_output=True, timeout=3)
    return result.returncode == 0


try:
    client = netns()
    pod = netns()
    command('ip', 'link', 'set', 'lo', 'up')
    command('ip', 'link', 'add', 'wt0', 'type', 'veth', 'peer', 'name', 'client0')
    command('ip', 'link', 'set', 'client0', 'netns', str(client))
    command('ip', 'addr', 'add', '100.73.28.50/16', 'dev', 'wt0')
    command('ip', 'link', 'set', 'wt0', 'up')
    inside(client, 'ip', 'link', 'set', 'lo', 'up')
    inside(client, 'ip', 'link', 'set', 'client0', 'up')
    address_client('100.73.120.2')
    command('ip', 'link', 'add', 'cni0', 'type', 'veth', 'peer', 'name', 'pod0')
    command('ip', 'link', 'set', 'pod0', 'netns', str(pod))
    command('ip', 'addr', 'add', '10.42.0.1/24', 'dev', 'cni0')
    command('ip', 'link', 'set', 'cni0', 'up')
    inside(pod, 'ip', 'link', 'set', 'lo', 'up')
    inside(pod, 'ip', 'link', 'set', 'pod0', 'up')
    inside(pod, 'ip', 'addr', 'add', '10.42.0.2/24', 'dev', 'pod0')
    inside(pod, 'ip', 'route', 'add', 'default', 'via', '10.42.0.1')
    command('sysctl', '-w', 'net.ipv4.ip_forward=1')
    server_code = '''from http.server import BaseHTTPRequestHandler,HTTPServer
class Handler(BaseHTTPRequestHandler):
 def do_GET(self):
  self.send_response(200);self.end_headers();self.wfile.write(b'ok')
 def log_message(self,*args):pass
HTTPServer(('0.0.0.0',3000),Handler).serve_forever()
'''
    server = subprocess.Popen(['nsenter', '-t', str(pod), '-n', sys.executable, '-c', server_code], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    processes.append(server)
    for _ in range(60):
        result = subprocess.run(['nsenter', '-t', str(pod), '-n', sys.executable, '-c', "import socket;s=socket.create_connection(('127.0.0.1',3000),.1);s.close()"], capture_output=True)
        if result.returncode == 0:
            break
        time.sleep(.05)
    else:
        raise RuntimeError('Disposable HTTP server did not start.')
    nft('''add table ip netbird
add set ip netbird allowed_peers { type ipv4_addr; elements = { 100.73.120.2, 100.73.120.3 }; }
add set ip netbird allowed_ports { type inet_service; elements = {80,443,8080}; }
add chain ip netbird netbird-mangle-prerouting { type filter hook prerouting priority mangle; policy accept; }
add rule ip netbird netbird-mangle-prerouting iifname "wt0" ip saddr @allowed_peers tcp dport @allowed_ports fib daddr type local meta mark set 0x0001bd20
add chain ip netbird fixture_nat { type nat hook prerouting priority dstnat; policy accept; }
add rule ip netbird fixture_nat iifname "wt0" ip daddr 100.73.28.50 tcp dport {80,443,8080} dnat to 10.42.0.2:3000
add chain ip netbird fixture_kube_mark { type filter hook forward priority -10; policy accept; }
add rule ip netbird fixture_kube_mark iifname "wt0" ct status dnat meta mark set meta mark and 0xfffeffff or 0x00024000
add chain ip netbird netbird-acl-forward-filter { type filter hook forward priority filter; policy accept; }
add rule ip netbird netbird-acl-forward-filter meta mark 0x0001bd20 accept
add rule ip netbird netbird-acl-forward-filter iifname "wt0" drop
add chain ip netbird fixture_mark_preserved { type filter hook forward priority 10; policy accept; }
add rule ip netbird fixture_mark_preserved iifname "wt0" meta mark 0x0002fd20 counter comment "kubernetes-bits-retained"
''')
    config = fix.validate('wt0', [80,443])
    assert not reachable(), 'Fixture must reproduce the mark-collision failure first.'
    assert fix.reconcile(run, config) == 'active'
    assert reachable() and reachable(443), 'Authorized ingress must become reachable.'
    assert not reachable(8080), 'Other DNATed ports must remain outside the workaround.'
    nft('delete element ip netbird allowed_ports {443}\n')
    assert not reachable(443), 'A port excluded by NetBird policy must stay blocked.'
    nft('add element ip netbird allowed_ports {443}\n')
    assert reachable(443)
    ours = [r for r in fix.rules(run, fix.CHAIN) if fix.owned(r)]
    assert len(ours) == 1
    handle = ours[0]['handle']
    fix.reconcile(run, config)
    assert [r['handle'] for r in fix.rules(run, fix.CHAIN) if fix.owned(r)] == [handle]
    observed = fix.rules(run, 'fixture_mark_preserved')
    assert sum(e['counter']['packets'] for r in observed for e in r['expr'] if 'counter' in e) > 0
    address_client('100.73.120.3')
    assert reachable(), 'A second authorized device must work without a per-device exception.'
    address_client('100.73.120.4')
    assert not reachable(), 'Unknown peers must remain blocked.'
    address_client('100.73.120.2')
    nft('delete element ip netbird allowed_peers {100.73.120.2}\n')
    assert not reachable(), 'Policy revocation must still take effect.'
    nft('add element ip netbird allowed_peers {100.73.120.2}\n')
    assert reachable()
    nft(f'delete rule ip netbird {fix.CHAIN} handle {handle}\n')
    assert not reachable(), 'Removing the exception must reproduce the failure.'
    assert fix.reconcile(run, config) == 'active' and reachable()
    # A future NetBird guard change must remove an obsolete exception.
    current = fix.rules(run, fix.CHAIN)
    original = next(r for r in current if not fix.owned(r) and any('accept' in e for e in r['expr']))
    nft(f'delete rule ip netbird {fix.CHAIN} handle {original["handle"]}\n')
    assert fix.reconcile(run, config) == 'unsupported'
    assert not any(fix.owned(r) for r in fix.rules(run, fix.CHAIN))
    assert not reachable()
    print('Real nftables tests passed: collision reproduced; authorized devices allowed on 80/443; other ports and unauthorized/revoked peers blocked; marks retained; idempotence, refresh recovery and fail-closed upgrade behavior verified.')
finally:
    for process in reversed(processes):
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)
