"""Disposable kind-only installer lifecycle; never targets an existing cluster."""
import http.cookiejar
import json
import os
from pathlib import Path
import re
import subprocess
import time
import urllib.error
import urllib.request

CTX = 'kind-kubeweasel-access'
NS = 'kw-access-check'
ROOT = Path(__file__).resolve().parents[1]
ORIGIN = 'http://dashboard.example.internal:17887'
HOST = ORIGIN.split('://')[1]
EMAIL = 'owner@example.test'
PASSWORD = 'disposable-private-access-password'


def run(*args, data=None):
    result = subprocess.run(args, input=data, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f'{args[0]} {args[1]} failed (output withheld to protect setup credentials)')
    return result.stdout


def kube(*args, data=None):
    return run('kubectl', '--context', CTX, '-n', NS, *args, data=data)


def installer(command, *args):
    return run('bash', str(ROOT / 'scripts/install.sh'), command, '--context', CTX, '--namespace', NS, '--timeout', '5m', *args)


assert os.environ.get('KUBEWEASEL_RUN_PRIVATE_ACCESS_TEST') == '1'
assert run('kubectl', 'config', 'current-context').strip() == CTX
kube('create', 'namespace', NS)  # Fail if a previous namespace exists.
nodes = json.loads(kube('get', 'nodes', '-o', 'json'))['items']
primary = next(n for n in nodes if n['metadata']['name'].endswith('control-plane'))
node = primary['metadata']['name']
ip = next(a['address'] for a in primary['status']['addresses'] if a['type'] == 'InternalIP')
private_args = ('--private-http', ORIGIN, '--listen-address', ip, '--chart', str(ROOT / 'charts/kubeweasel'), '--image-repository', 'kubeweasel', '--image-tag', 'access-test')
cookies = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(cookies))
base = 'http://127.0.0.1:17886'
host = '127.0.0.1:7878'
csrf = ''


def api(path, body=None, extra=None, expected=200, method=None):
    headers = {'Host': host, 'content-type': 'application/json', 'x-kubeweasel-request': 'dashboard', 'x-kubeweasel-csrf': csrf}
    if extra:
        headers.update(extra)
    request = urllib.request.Request(base + path, data=None if body is None else json.dumps(body).encode(), headers=headers, method=method)
    try:
        response = opener.open(request, timeout=15)
    except urllib.error.HTTPError as error:
        assert error.code == expected, (path, error.code, expected)
        return {}
    assert response.status == expected, (path, response.status, expected)
    assert 'Strict-Transport-Security' not in response.headers
    return json.load(response)



def wait_for_entrypoint():
    # Pod readiness precedes kube-proxy/EndpointSlice convergence on other nodes.
    # Poll a safe GET through the actual gateway before sending login mutations.
    for _ in range(60):
        try:
            response = opener.open(urllib.request.Request(base + '/healthz', headers={'Host': host}), timeout=6)
            assert response.status == 200 and response.read() == b'ok'
            return
        except urllib.error.HTTPError as error:
            if error.code not in (502, 503, 504):
                raise
        except OSError:
            pass
        time.sleep(.5)
    raise RuntimeError('Direct entry point did not recover after rollout')


def login():
    global csrf
    csrf = api('/api/auth/login', {'email': EMAIL, 'password': PASSWORD})['csrf']


def uid(kind, name):
    return json.loads(kube('get', kind, name, '-o', 'json'))['metadata']['uid']


forward = None
try:
    # Place the old account volume on the worker, then expose through the primary.
    # This proves enabling access does not try to move node-local account storage.
    kube('cordon', node)
    installer('install', '--version', '0.2.1')
    print('Published v0.2.1 installed on worker.', flush=True)
    kube('uncordon', node)
    old_pod = json.loads(kube('get', 'pods', '-l', 'app=kubeweasel', '-o', 'json'))['items'][0]
    storage_node = old_pod['spec']['nodeName']
    assert storage_node != node
    forward = subprocess.Popen(['kubectl', '--context', CTX, '-n', NS, 'port-forward', 'deployment/kubeweasel', '17886:7878'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(60):
        try:
            api('/api/auth/status')
            break
        except OSError:
            time.sleep(.5)
    setup_token = re.search(r'first-run setup token: ([a-f0-9]+)', kube('logs', 'deployment/kubeweasel', '--container', 'dashboard')).group(1)
    csrf = api('/api/auth/setup', {'email': EMAIL, 'password': PASSWORD, 'setup_token': setup_token})['csrf']
    project = api('/api/projects', {'name': 'Preserved project', 'description': 'Access upgrade check'})['project']['id']
    claim = uid('pvc', 'kubeweasel-data')
    kube('apply', '-f', '-', data=json.dumps({'apiVersion': 'apps/v1', 'kind': 'Deployment', 'metadata': {'name': 'retained-app'}, 'spec': {'replicas': 0, 'selector': {'matchLabels': {'app': 'retained-app'}}, 'template': {'metadata': {'labels': {'app': 'retained-app'}}, 'spec': {'containers': [{'name': 'app', 'image': 'registry.k8s.io/pause:3.10'}]}}}}))
    workload = uid('deployment', 'retained-app')
    forward.terminate()
    forward.wait(timeout=10)
    forward = None
    output = installer('upgrade', *private_args)
    print('Private gateway upgrade reached readiness.', flush=True)
    assert 'Dashboard ready: ' + ORIGIN in output
    assert 'No port-forward or SSH tunnel is needed.' in output
    assert uid('pvc', 'kubeweasel-data') == claim
    new_pod = json.loads(kube('get', 'pods', '-l', 'app=kubeweasel', '-o', 'json'))['items'][0]
    assert new_pod['spec']['nodeName'] == storage_node
    base = f'http://{ip}:17887'
    host = HOST
    cookies.clear()
    api('/api/projects', expected=401)
    login()
    assert any(p['id'] == project for p in api('/api/projects')['projects'])
    api('/api/projects', extra={'Host': 'evil.example:17887'}, expected=403)
    api('/api/projects', {'name': 'Blocked'}, {'Origin': 'http://evil.example:17887'}, expected=403)
    api('/api/projects', {'name': 'Blocked'}, {'x-kubeweasel-csrf': ''}, expected=403)
    assert ORIGIN in installer('open')
    assert 'port-forward' not in installer('open')
    # The gateway and backend can both restart without a terminal or tunnel.
    kube('rollout', 'restart', 'deployment/kubeweasel-access')
    kube('rollout', 'status', 'deployment/kubeweasel-access', '--timeout=3m')
    kube('rollout', 'restart', 'deployment/kubeweasel')
    kube('rollout', 'status', 'deployment/kubeweasel', '--timeout=3m')
    wait_for_entrypoint()
    login()
    print('Direct login, guards and independent restarts passed.', flush=True)
    installer('uninstall')
    assert uid('pvc', 'kubeweasel-data') == claim
    assert uid('deployment', 'retained-app') == workload
    installer('install', *private_args)
    cookies.clear()
    login()
    assert any(p['id'] == project for p in api('/api/projects')['projects'])
    assert uid('pvc', 'kubeweasel-data') == claim
    assert uid('deployment', 'retained-app') == workload
    print('Private access: old-release upgrade, cross-node retained storage, direct login, Host/Origin/CSRF guards, restarts and retained-data reinstall passed.')
except Exception:
    print(kube('get', 'pods,pvc', '-o', 'wide'))
    print(kube('get', 'events', '--sort-by=.lastTimestamp'))
    # This tokenless gateway never runs owner setup or logs request bodies.
    diagnostic = subprocess.run(['kubectl', '--context', CTX, '-n', NS, 'logs', 'deployment/kubeweasel-access', '--container', 'access', '--tail=30'], text=True, capture_output=True)
    print(diagnostic.stdout)
    raise
finally:
    if forward:
        forward.terminate()
        forward.wait(timeout=10)
    # Only this explicitly disposable namespace is ever removed.
    kube('delete', 'namespace', NS, '--wait=false')
